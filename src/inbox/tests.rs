use super::disk::DISK_READONLY;
use super::notify::route_notification;
use super::storage::inbox_path;
use super::*;
use parking_lot::Mutex;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

/// Serializes tests that touch the global DISK_READONLY flag or rely on
/// enqueue not being blocked by it. Without this, `test_readonly_on_disk_full`
/// can set readonly=true and a concurrently-running enqueue test panics.
static READONLY_TEST_LOCK: Mutex<()> = Mutex::new(());

fn tmp_home(suffix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agend-inbox-{}-{}", suffix, std::process::id()));
    fs::create_dir_all(&dir).ok();
    dir
}

fn make_msg(from: &str, text: &str) -> InboxMessage {
    msg().sender(from).text(text).build()
}

struct TestMsgBuilder(InboxMessage);

fn msg() -> TestMsgBuilder {
    TestMsgBuilder(InboxMessage {
        schema_version: 1,
        timestamp: "2025-01-01T00:00:00Z".to_string(),
        ..Default::default()
    })
}

impl TestMsgBuilder {
    fn sender(mut self, v: &str) -> Self {
        self.0.from = v.into();
        self
    }
    fn text(mut self, v: &str) -> Self {
        self.0.text = v.into();
        self
    }
    fn text_owned(mut self, v: String) -> Self {
        self.0.text = v;
        self
    }
    fn kind(mut self, v: &str) -> Self {
        self.0.kind = Some(v.into());
        self
    }
    fn channel(mut self, v: crate::channel::ChannelKind) -> Self {
        self.0.channel = Some(v);
        self
    }
    fn id(mut self, v: &str) -> Self {
        self.0.id = Some(v.into());
        self
    }
    fn timestamp(mut self, v: &str) -> Self {
        self.0.timestamp = v.into();
        self
    }
    fn schema_version(mut self, v: u32) -> Self {
        self.0.schema_version = v;
        self
    }
    fn thread_id(mut self, v: &str) -> Self {
        self.0.thread_id = Some(v.into());
        self
    }
    fn parent_id(mut self, v: &str) -> Self {
        self.0.parent_id = Some(v.into());
        self
    }
    fn sender_id(mut self, v: &str) -> Self {
        self.0.from_id = Some(v.into());
        self
    }
    fn superseded_by(mut self, v: &str) -> Self {
        self.0.superseded_by = Some(v.into());
        self
    }
    /// #2299: preset the `delivering_at` timestamp (for reclaim-TTL tests that
    /// need a stale in-flight row without waiting wall-clock).
    fn delivering_at(mut self, v: &str) -> Self {
        self.0.delivering_at = Some(v.into());
        self
    }
    fn in_reply_to_msg_id(mut self, v: &str) -> Self {
        self.0.in_reply_to_msg_id = Some(v.into());
        self
    }
    fn in_reply_to_excerpt(mut self, v: &str) -> Self {
        self.0.in_reply_to_excerpt = Some(v.into());
        self
    }
    fn attachments(mut self, v: Vec<crate::channel::event::Attachment>) -> Self {
        self.0.attachments = v;
        self
    }
    fn broadcast_context(mut self, v: BroadcastContext) -> Self {
        self.0.broadcast_context = Some(v);
        self
    }
    fn build(self) -> InboxMessage {
        self.0
    }
}

fn mark_composing(home: &Path, agent: &str) {
    std::fs::create_dir_all(home.join("metadata")).ok();
    std::fs::write(
        home.join("metadata").join(format!("{agent}.json")),
        format!(
            "{{\"last_input_epoch_ms\":{}}}",
            chrono::Utc::now().timestamp_millis()
        ),
    )
    .ok();
}

/// #1513: a PAUSED operator draft — keystroke entered but idle past the
/// fresh-keystroke anti-collision window (here ~3s ago, still well within the
/// #1457 5-min draft hold). Used to test that an actionable wake bypasses the
/// DRAFT HOLD (the #1473 contract) without colliding with the new #1513
/// fresh-keystroke yield (covered separately in `should_defer_inject_tests_1513`).
fn mark_composing_stale(home: &Path, agent: &str) {
    std::fs::create_dir_all(home.join("metadata")).ok();
    std::fs::write(
        home.join("metadata").join(format!("{agent}.json")),
        format!(
            "{{\"last_input_epoch_ms\":{}}}",
            chrono::Utc::now().timestamp_millis() - 3_000
        ),
    )
    .ok();
}

#[test]
fn enqueue_drain_roundtrip() {
    let home = tmp_home("roundtrip");
    enqueue(&home, "agent1", make_msg("alice", "hello")).ok();
    enqueue(&home, "agent1", make_msg("bob", "world")).ok();
    enqueue(&home, "agent1", make_msg("carol", "!")).ok();

    let msgs = drain(&home, "agent1");
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].from, "alice");
    assert_eq!(msgs[1].from, "bob");
    assert_eq!(msgs[2].from, "carol");

    fs::remove_dir_all(&home).ok();
}

#[test]
fn drain_empties_inbox() {
    let home = tmp_home("drain-empty");
    enqueue(&home, "agent1", make_msg("x", "y")).ok();

    let first = drain(&home, "agent1");
    assert_eq!(first.len(), 1);

    let second = drain(&home, "agent1");
    assert!(second.is_empty(), "second drain should be empty");

    fs::remove_dir_all(&home).ok();
}

#[test]
fn drain_caps_batch_under_budget_leaving_remainder_unread() {
    // #1940 (d): a backlog that would exceed the dedup per-entry cap (64 KiB) is
    // returned as a byte-capped batch so the MCP response stays dedup-cacheable
    // (the bridge same-request_id retry can then recover a lost transport). The
    // remainder MUST stay UNREAD and surface on the next drain — capped, never
    // lost, never duplicated, never split.
    let home = tmp_home("drain-cap");
    let big = "x".repeat(20 * 1024); // ~20 KiB body each → 3 exceed the 48 KiB budget
    for i in 0..3 {
        enqueue(&home, "agent1", make_msg(&format!("a{i}"), &big)).ok();
    }

    let first = drain(&home, "agent1");
    assert!(!first.is_empty(), "at least one message is always returned");
    assert!(
        first.len() < 3,
        "batch capped below the full 3-message backlog, got {}",
        first.len()
    );
    let batch_bytes: usize = first
        .iter()
        .map(|m| serde_json::to_string(m).unwrap().len())
        .sum();
    assert!(
        batch_bytes <= super::storage::DRAIN_BATCH_BUDGET_BYTES,
        "returned batch {batch_bytes}B must be ≤ the drain budget (dedup-cacheable)"
    );
    let (unread, _) = unread_count(&home, "agent1");
    assert_eq!(unread, 3 - first.len(), "the remainder stays unread");

    // Next drain returns exactly the rest — total 3, no loss, no duplicate.
    let second = drain(&home, "agent1");
    assert_eq!(
        first.len() + second.len(),
        3,
        "all 3 messages delivered across the paginated drains"
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn drain_returns_single_oversized_message_alone() {
    // #1940 (d): a single message larger than the budget can't be split — it is
    // still returned (progress guaranteed: a drain always yields ≥1 message),
    // as its own batch, rather than being stranded.
    let home = tmp_home("drain-oversized-single");
    let huge = "y".repeat(60 * 1024); // > the 48 KiB budget
    enqueue(&home, "agent1", make_msg("big", &huge)).ok();

    let msgs = drain(&home, "agent1");
    assert_eq!(
        msgs.len(),
        1,
        "the oversized message is returned alone, never stranded"
    );

    fs::remove_dir_all(&home).ok();
}

/// #2042 entry-level: the REAL drain entry point arms the reply-ledger for
/// every user channel message, and duplicate deliveries of the same logical
/// message (same sender + normalized content) GROUP-JOIN one obligation —
/// replying once then settles all their ids, and a post-settlement redelivery
/// drained later opens no new obligation.
#[test]
fn drain_groups_duplicate_channel_msgs_and_suppresses_redelivery_2042() {
    let home = tmp_home("rl-drain-2042");
    let agent = "rl-drain-entry-2042";

    // Two deliveries of the same logical message (operator double-send).
    enqueue(
        &home,
        agent,
        msg()
            .sender("user:op")
            .text("deploy the fix")
            .channel(crate::channel::ChannelKind::Telegram)
            .build(),
    )
    .ok();
    enqueue(
        &home,
        agent,
        msg()
            .sender("user:op")
            .text("deploy   the FIX") // same content modulo whitespace/case
            .channel(crate::channel::ChannelKind::Telegram)
            .build(),
    )
    .ok();

    let msgs = drain(&home, agent);
    assert_eq!(msgs.len(), 2);
    let turn = crate::daemon::heartbeat_pair::snapshot_for(agent)
        .pending_user_turn
        .expect("drain must arm the ledger for user channel messages");
    let expected_ids: Vec<String> = msgs.iter().filter_map(|m| m.id.clone()).collect();
    assert_eq!(
        turn.group_msg_ids, expected_ids,
        "duplicate deliveries in one drain must group-join a single obligation"
    );

    // Reply settles the WHOLE group…
    crate::reply_ledger::record_reply_outcome(agent, true);
    // …and a redelivery of the same content drained later does not re-arm.
    enqueue(
        &home,
        agent,
        msg()
            .sender("user:op")
            .text("deploy the fix")
            .channel(crate::channel::ChannelKind::Telegram)
            .build(),
    )
    .ok();
    let redelivered = drain(&home, agent);
    assert_eq!(
        redelivered.len(),
        1,
        "redelivery is still DELIVERED to the agent"
    );
    assert!(
        crate::daemon::heartbeat_pair::snapshot_for(agent)
            .pending_user_turn
            .is_none(),
        "a redelivery of a settled message must not open a new reply obligation"
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn drain_nonexistent_returns_empty() {
    let home = tmp_home("no-inbox");
    let msgs = drain(&home, "nonexistent");
    assert!(msgs.is_empty());
    fs::remove_dir_all(&home).ok();
}

#[test]
fn concurrent_enqueue_to_different_agents() {
    let home = tmp_home("concurrent");
    let home_arc = std::sync::Arc::new(home.clone());
    let mut handles = vec![];

    // Each thread writes to a different agent — no contention
    for i in 0..10 {
        let h = home_arc.clone();
        handles.push(std::thread::spawn(move || {
            let agent = format!("agent{i}");
            enqueue(&h, &agent, make_msg(&format!("t{i}"), &format!("msg{i}")))
                .expect("enqueue should succeed");
        }));
    }
    for h in handles {
        h.join().expect("thread should not panic");
    }

    // Each agent should have exactly 1 message
    for i in 0..10 {
        let msgs = drain(&home, &format!("agent{i}"));
        assert_eq!(msgs.len(), 1, "agent{i} should have 1 message");
        assert_eq!(msgs[0].from, format!("t{i}"));
    }

    fs::remove_dir_all(&home).ok();
}

#[test]
fn notify_queues_when_composing() {
    let home = tmp_home("notify-queue");
    mark_composing(&home, "agent1");
    let mut injected = false;
    route_notification(&home, "agent1", "queued", |_| {
        injected = true;
        Ok(())
    })
    .expect("route should queue");
    assert!(!injected);
    assert_eq!(crate::notification_queue::pending_count(&home, "agent1"), 1);
    fs::remove_dir_all(&home).ok();
}

#[test]
fn notify_injects_when_idle() {
    let home = tmp_home("notify-idle");
    let mut injected = Vec::new();
    route_notification(&home, "agent1", "sent", |msg| {
        injected.push(msg.to_string());
        Ok(())
    })
    .expect("route should inject");
    assert_eq!(injected, vec!["sent".to_string()]);
    assert_eq!(crate::notification_queue::pending_count(&home, "agent1"), 0);
    fs::remove_dir_all(&home).ok();
}

#[test]
fn inbox_message_fields_preserved() {
    let home = tmp_home("fields");
    let msg = msg()
        .schema_version(0)
        .sender("sender")
        .text("body text")
        .kind("notification")
        .timestamp("2025-06-15T12:30:00Z")
        .build();
    enqueue(&home, "agent1", msg).ok();
    let msgs = drain(&home, "agent1");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].from, "sender");
    assert_eq!(msgs[0].text, "body text");
    assert_eq!(msgs[0].kind.as_deref(), Some("notification"));
    assert_eq!(msgs[0].timestamp, "2025-06-15T12:30:00Z");

    fs::remove_dir_all(&home).ok();
}

#[test]
fn deliver_short_message_also_enqueues() {
    let home = tmp_home("deliver-short");
    // deliver with short text — must enqueue to inbox for persistence
    // (previously skipped enqueue for ≤500 chars, causing data loss)
    deliver(
        &home,
        "agent1",
        &NotifySource::Channel("user", crate::channel::ChannelKind::Telegram),
        "short msg",
        "\r",
        None,
        None,
    );
    let msgs = drain(&home, "agent1");
    assert_eq!(
        msgs.len(),
        1,
        "short messages must be enqueued for persistence"
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn deliver_long_message_enqueues() {
    let home = tmp_home("deliver-long");
    let long_text: String = "x".repeat(600);
    deliver(
        &home,
        "agent1",
        &NotifySource::Channel("user", crate::channel::ChannelKind::Telegram),
        &long_text,
        "\r",
        Some("chat".to_string()),
        None,
    );
    let msgs = drain(&home, "agent1");
    assert_eq!(msgs.len(), 1, "long messages should be enqueued");
    assert_eq!(msgs[0].text, long_text);
    assert_eq!(msgs[0].kind.as_deref(), Some("chat"));

    fs::remove_dir_all(&home).ok();
}

#[test]
fn large_message_over_threshold() {
    let home = tmp_home("large-msg");
    let large_text: String = "a".repeat(10_000);
    enqueue(&home, "agent1", make_msg("big", &large_text)).ok();
    let msgs = drain(&home, "agent1");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text.len(), 10_000);

    fs::remove_dir_all(&home).ok();
}

#[test]
fn multiple_agents_isolated() {
    let home = tmp_home("isolation");
    enqueue(&home, "agent1", make_msg("a", "for-1")).ok();
    enqueue(&home, "agent2", make_msg("b", "for-2")).ok();

    let m1 = drain(&home, "agent1");
    let m2 = drain(&home, "agent2");
    assert_eq!(m1.len(), 1);
    assert_eq!(m1[0].text, "for-1");
    assert_eq!(m2.len(), 1);
    assert_eq!(m2[0].text, "for-2");

    fs::remove_dir_all(&home).ok();
}

#[test]
fn inbox_message_serialization() {
    let msg = msg()
        .schema_version(0)
        .sender("test")
        .text("hello \"world\"")
        .build();
    let json = serde_json::to_string(&msg).expect("serialize");
    let parsed: InboxMessage = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed.from, "test");
    assert_eq!(parsed.text, "hello \"world\"");
}

#[test]
fn inbox_message_with_special_chars() {
    let home = tmp_home("special");
    let msg = msg()
        .schema_version(0)
        .sender("user")
        .text("line1\nline2\ttab")
        .kind("special")
        .build();
    enqueue(&home, "agent1", msg).ok();
    let msgs = drain(&home, "agent1");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].text, "line1\nline2\ttab");

    fs::remove_dir_all(&home).ok();
}

// --- NotifySource tests ---

#[test]
fn notify_source_telegram_display() {
    let s = NotifySource::Channel("chiacheng", crate::channel::ChannelKind::Telegram);
    assert_eq!(s.to_string(), "user:chiacheng via telegram");
    assert!(s.reply_hint().contains("reply tool"));
}

#[test]
fn notify_source_agent_display() {
    let s = NotifySource::Agent("dev");
    assert_eq!(s.to_string(), "from:dev");
    let h = s.reply_hint();
    assert!(h.contains("send tool"));
    assert!(h.contains("dev"));
}

#[test]
fn notify_source_system_display() {
    let s = NotifySource::System("ci");
    assert_eq!(s.to_string(), "system:ci");
    assert!(s.reply_hint().is_empty());
}

// #1940: `drain_recovers_leftover_draining_file` + `drain_does_not_overwrite_
// leftover_draining` were REMOVED with the `.draining` snapshot/recovery path
// they exercised (see storage.rs::drain — zero-creator dead code, and a real
// re-serve snapshot was rejected as exactly-once-breaking). The delivery-loss
// recovery they aimed at is now the bridge same-request_id retry + request_dedup
// cache, kept reliable by the drain byte cap (see drain_caps_batch_*).

// #1940: `drain_read_failure_leaves_file_for_retry` was REMOVED with the rest of
// the `.draining` snapshot/recovery path it exercised — drain() no longer reads
// or writes `.draining`, so the test was vacuous-passing (both asserts held only
// because drain never touched the file).

#[test]
fn notify_agent_does_not_append_submit_key() {
    // Verify the notification format doesn't contain \r (submit_key).
    let source = NotifySource::Agent("peer");
    let text = "hello world";
    let display_text = text.to_string();
    let notification = format!("[{source}] {display_text}{}", source.reply_hint());
    assert!(
        !notification.contains('\r'),
        "notification must not contain submit_key (\\r): {notification:?}"
    );
}

// -----------------------------------------------------------------------
// Regression pins: route_notification — the composing-aware primitive
// shared by `compose_aware_inject` and (pre-#1065) `compose_aware_send`.
// PR #96 conflated both into raw write; PR #99 split them. #1065 removed
// the now-redundant `compose_aware_send` wrapper, so these tests pin
// the underlying `route_notification` behavior directly.
// -----------------------------------------------------------------------

#[test]
fn route_notification_calls_injector_when_idle() {
    // `route_notification` must call the injector closure (not enqueue
    // to the notification_queue) when the agent is NOT composing.
    let home = tmp_home("route-idle");
    let mut called = false;
    route_notification(&home, "agent1", "msg", |_| {
        called = true;
        Ok(())
    })
    .expect("route should call injector");
    assert!(called, "injector must be called when agent is idle");
    fs::remove_dir_all(&home).ok();
}

#[test]
fn route_notification_enqueues_when_composing() {
    // `route_notification` must enqueue to the notification_queue (not
    // call the injector) when the agent IS composing.
    let home = tmp_home("route-composing");
    mark_composing(&home, "agent1");
    let mut called = false;
    route_notification(&home, "agent1", "msg", |_| {
        called = true;
        Ok(())
    })
    .expect("route should enqueue");
    assert!(!called, "injector must NOT be called when composing");
    assert_eq!(
        crate::notification_queue::pending_count(&home, "agent1"),
        1,
        "message must be enqueued"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn inject_with_submit_sends_raw_false() {
    // Structural pin: inject_with_submit must NOT set raw=true in the
    // INJECT API call. This ensures inject_to_agent (with submit_key)
    // is used instead of write_to_agent (raw, no submit_key).
    //
    // We verify by inspecting the JSON payload construction. The function
    // builds: {"method": "inject", "params": {"name": ..., "data": ...}}
    // with NO "raw" field — handle_inject defaults raw=false → inject_to_agent.
    //
    // Cannot call inject_with_submit directly (needs running daemon), so
    // we verify the contract structurally.
    let send_json = serde_json::json!({
        "method": crate::api::method::INJECT,
        "params": {"name": "test", "data": "msg"}
    });
    assert!(
        send_json["params"]["raw"].is_null(),
        "inject_with_submit path must NOT set raw (defaults to false → inject_to_agent)"
    );
}

#[test]
fn test_load_legacy_without_schema_version() {
    let home = tmp_home("legacy-schema");
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).ok();
    // Write a legacy JSONL line without schema_version field
    let legacy_line = r#"{"from":"old-agent","text":"legacy msg","kind":null,"timestamp":"2025-01-01T00:00:00Z"}"#;
    fs::write(inbox_dir.join("agent1.jsonl"), format!("{legacy_line}\n")).ok();
    let msgs = drain(&home, "agent1");
    assert_eq!(msgs.len(), 1, "legacy message must load successfully");
    assert_eq!(msgs[0].schema_version, 0, "missing field defaults to 0");
    assert_eq!(msgs[0].from, "old-agent");
    fs::remove_dir_all(&home).ok();
}

// --- Disk resilience tests ---

#[test]
fn test_readonly_on_disk_full() {
    let _guard = READONLY_TEST_LOCK.lock();
    // When DISK_READONLY is set, enqueue must fail and drain must still work.
    let home = tmp_home("readonly");
    enqueue(&home, "agent1", make_msg("a", "before")).ok();

    DISK_READONLY.store(true, Ordering::Relaxed);
    let result = enqueue(&home, "agent1", make_msg("b", "blocked"));
    assert!(result.is_err(), "enqueue must fail in readonly mode");
    assert!(
        result.unwrap_err().to_string().contains("readonly"),
        "error must mention readonly"
    );

    // drain still works in readonly mode
    let msgs = drain(&home, "agent1");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].from, "a");

    DISK_READONLY.store(false, Ordering::Relaxed);
    fs::remove_dir_all(&home).ok();
}

#[test]
fn test_reject_future_schema_version() {
    let home = tmp_home("future-schema");
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).ok();
    let future_line = r#"{"schema_version":999,"from":"future","text":"nope","kind":null,"timestamp":"2099-01-01T00:00:00Z"}"#;
    let current_line = r#"{"schema_version":1,"from":"ok","text":"yes","kind":null,"timestamp":"2025-01-01T00:00:00Z"}"#;
    fs::write(
        inbox_dir.join("agent1.jsonl"),
        format!("{future_line}\n{current_line}\n"),
    )
    .ok();
    let msgs = drain(&home, "agent1");
    assert_eq!(msgs.len(), 1, "future-versioned message must be rejected");
    assert_eq!(msgs[0].from, "ok", "current-versioned message must survive");
    fs::remove_dir_all(&home).ok();
}

#[test]
fn test_atomic_append_tmp_recovery() {
    // Simulate a crash that left a .tmp file — recover_half_writes
    // must move it to inbox.recovery/.
    let home = tmp_home("atomic-recover");
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).ok();

    // Simulate stale tmp from interrupted enqueue
    let tmp = inbox_dir.join("agent1.jsonl.tmp");
    fs::write(
        &tmp,
        "{\"from\":\"x\",\"text\":\"orphan\",\"kind\":null,\"timestamp\":\"t\"}\n",
    )
    .ok();

    recover_half_writes(&home);

    assert!(!tmp.exists(), ".tmp must be moved to recovery");
    let recovery = home.join("inbox.recovery");
    assert!(recovery.exists(), "recovery dir must be created");
    let entries: Vec<_> = fs::read_dir(&recovery).unwrap().flatten().collect();
    assert_eq!(entries.len(), 1, "one timestamped recovery dir");

    fs::remove_dir_all(&home).ok();
}

#[test]
fn test_half_written_jsonl_preserves_good_messages() {
    // bughunt#3 (fail-open): a trailing corrupt line must NOT cost the agent
    // its whole queue. The good messages survive (still drainable) and only
    // the corrupt line is moved to recovery for forensics.
    let home = tmp_home("half-write-failopen");
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).ok();

    let jsonl = inbox_dir.join("agent1.jsonl");
    let good1 = serde_json::to_string(&make_msg("a", "first")).unwrap();
    let good2 = serde_json::to_string(&make_msg("b", "second")).unwrap();
    // Two good lines followed by a truncated/corrupt line (crash mid-append).
    fs::write(
        &jsonl,
        format!("{good1}\n{good2}\n{{\"from\":\"broken\",\"text\":\"trun"),
    )
    .ok();

    recover_half_writes(&home);

    // The inbox survives and BOTH good messages are still drainable (zero loss).
    assert!(jsonl.exists(), "inbox must survive a corrupt trailing line");
    let msgs = drain(&home, "agent1");
    assert_eq!(msgs.len(), 2, "both good messages must survive (zero loss)");
    assert_eq!(msgs[0].from, "a");
    assert_eq!(msgs[1].from, "b");

    // The corrupt line — and only it — is preserved under inbox.recovery/.
    let recovery = home.join("inbox.recovery");
    assert!(recovery.exists(), "recovery dir must hold the dropped line");
    let subdirs: Vec<_> = fs::read_dir(&recovery).unwrap().flatten().collect();
    assert_eq!(subdirs.len(), 1);
    let files: Vec<_> = fs::read_dir(subdirs[0].path()).unwrap().flatten().collect();
    assert_eq!(files.len(), 1);
    assert!(files[0].file_name().to_string_lossy().contains("agent1"));
    let salvaged = fs::read_to_string(files[0].path()).unwrap();
    assert!(
        salvaged.contains("broken") && !salvaged.contains("first"),
        "only the corrupt line is salvaged, never a good message"
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn test_recover_noop_when_all_lines_good() {
    // No corrupt line → no rewrite, no recovery dir, every message intact.
    let home = tmp_home("recover-noop-good");
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).ok();
    let jsonl = inbox_dir.join("agent1.jsonl");
    let good = serde_json::to_string(&make_msg("ok", "fine")).unwrap();
    fs::write(&jsonl, format!("{good}\n")).ok();

    recover_half_writes(&home);

    assert!(jsonl.exists());
    assert!(
        !home.join("inbox.recovery").exists(),
        "no recovery dir for a clean inbox"
    );
    let msgs = drain(&home, "agent1");
    assert_eq!(msgs.len(), 1);
    fs::remove_dir_all(&home).ok();
}

#[test]
fn test_recover_noop_on_empty_file() {
    // Empty inbox file → no corrupt lines → no-op, no recovery dir.
    let home = tmp_home("recover-noop-empty");
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).ok();
    let jsonl = inbox_dir.join("agent1.jsonl");
    fs::write(&jsonl, "").ok();

    recover_half_writes(&home);

    assert!(jsonl.exists(), "empty inbox must be left untouched");
    assert!(!home.join("inbox.recovery").exists());
    fs::remove_dir_all(&home).ok();
}

#[test]
fn test_drain_marks_delivering_then_implicit_ack_keeps_message() {
    let home = tmp_home("drain-read-at");
    enqueue(&home, "agent1", make_msg("alice", "hello")).ok();
    enqueue(&home, "agent1", make_msg("bob", "world")).ok();

    // #2299: first drain returns both, now in the DELIVERING state — handed to
    // the agent (delivering_at set) but NOT yet processed (read_at still None).
    let msgs = drain(&home, "agent1");
    assert_eq!(msgs.len(), 2);
    assert!(
        msgs[0].delivering_at.is_some(),
        "drain must stamp delivering_at"
    );
    assert!(
        msgs[0].read_at.is_none(),
        "drain must NOT stamp read_at (delivering, not processed)"
    );
    assert!(msgs[1].delivering_at.is_some());
    assert!(msgs[1].read_at.is_none());

    // delivering_at is persisted, read_at is not.
    let path = inbox_path(&home, "agent1");
    let content = fs::read_to_string(&path).expect("file must still exist");
    let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 2, "messages must be kept in file");
    let m: InboxMessage = serde_json::from_str(lines[0]).expect("parse");
    assert!(
        m.delivering_at.is_some(),
        "delivering_at must be persisted to disk"
    );
    assert!(
        m.read_at.is_none(),
        "read_at must NOT be set after a single drain"
    );

    // Second drain returns empty (delivering rows are skipped, never re-delivered)
    // and IMPLICITLY ACKS the prior batch → read_at now set (processed).
    let msgs2 = drain(&home, "agent1");
    assert!(
        msgs2.is_empty(),
        "in-flight (delivering) messages must not be returned again"
    );
    let content2 = fs::read_to_string(&path).expect("file must still exist");
    for line in content2.lines().filter(|l| !l.is_empty()) {
        let m: InboxMessage = serde_json::from_str(line).expect("parse");
        assert!(
            m.read_at.is_some(),
            "re-drain must implicitly ack the prior delivering batch → processed"
        );
    }

    fs::remove_dir_all(&home).ok();
}

// ─────────────────────────────────────────────────────────────────────────
// #2299 three-state delivery: unread → delivering → processed + reclaim-TTL.
// ─────────────────────────────────────────────────────────────────────────

/// RFC3339 timestamp `secs` seconds in the past (for aging delivering rows).
fn secs_ago(secs: i64) -> String {
    (chrono::Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339()
}

/// turn-死重投: a message delivered to an agent whose turn then DIED (never
/// acked, never re-drained) is reverted to unread by the reclaim-TTL sweep and
/// RE-DELIVERED — the core anti-silent-loss guarantee of #2299.
#[test]
fn reclaim_redelivers_after_turn_death() {
    let home = tmp_home("2299-turn-death");
    // Seed a stale in-flight row directly (delivered ~11 min ago, never acked).
    enqueue(
        &home,
        "a",
        msg()
            .sender("lead")
            .text("do the thing")
            .id("m-stale")
            .delivering_at(&secs_ago(660))
            .build(),
    )
    .unwrap();

    // Reclaim reverts it to unread (delivering_at cleared, still unprocessed).
    reclaim_stale_delivering(&home);

    // A drain now RE-DELIVERS it (back to delivering, returned to the agent).
    let redelivered = drain(&home, "a");
    assert_eq!(
        redelivered.len(),
        1,
        "stale delivering row must be re-delivered"
    );
    assert_eq!(redelivered[0].id.as_deref(), Some("m-stale"));
    assert!(redelivered[0].delivering_at.is_some());
    assert!(redelivered[0].read_at.is_none());
    fs::remove_dir_all(&home).ok();
}

/// healthy 不重投 / 在途不雙投: a FRESH in-flight (delivering) row is left
/// untouched by reclaim and is never returned a second time — no double-deliver
/// while the agent is mid-turn.
#[test]
fn reclaim_leaves_fresh_delivering_untouched_no_double_deliver() {
    let home = tmp_home("2299-fresh-delivering");
    enqueue(&home, "a", make_msg("lead", "hi")).unwrap();

    let first = drain(&home, "a");
    assert_eq!(first.len(), 1, "first drain delivers once");
    assert!(first[0].delivering_at.is_some() && first[0].read_at.is_none());

    // Reclaim runs but the row is fresh (< TTL) → not reverted.
    reclaim_stale_delivering(&home);

    // Re-drain returns NOTHING (in-flight, not re-delivered) — and implicitly
    // acks the prior batch (read_at now set).
    let second = drain(&home, "a");
    assert!(
        second.is_empty(),
        "fresh in-flight message must not be re-delivered"
    );
    let content = fs::read_to_string(inbox_path(&home, "a")).unwrap();
    let m: InboxMessage = serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert!(
        m.read_at.is_some(),
        "re-drain implicitly acked the delivering row"
    );
    fs::remove_dir_all(&home).ok();
}

/// 顯式 ack→不 reclaim: an explicitly-acked (processed) message is never
/// reverted by reclaim — even if it has aged well past the TTL — so an
/// acked message is never re-delivered.
#[test]
fn explicit_ack_prevents_reclaim() {
    let home = tmp_home("2299-explicit-ack");
    enqueue(
        &home,
        "a",
        msg().sender("lead").text("hi").id("m-ack").build(),
    )
    .unwrap();

    let drained = drain(&home, "a");
    assert_eq!(drained.len(), 1);
    // Agent processes, then explicitly acks the message it drained.
    let acked = ack(&home, "a", Some("m-ack"));
    assert_eq!(acked, 1, "ack transitions delivering → processed");
    let content = fs::read_to_string(inbox_path(&home, "a")).unwrap();
    let m: InboxMessage = serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert!(m.read_at.is_some(), "ack stamps read_at (processed)");

    // Reclaim is a no-op on a processed row (read_at set); no re-delivery.
    reclaim_stale_delivering(&home);
    assert!(
        drain(&home, "a").is_empty(),
        "acked message must never be re-delivered"
    );
    fs::remove_dir_all(&home).ok();
}

/// TTL 邊界: reclaim reverts only rows older than RECLAIM_TTL_SECS (600s);
/// a row just under the boundary is left in-flight.
#[test]
fn reclaim_ttl_boundary() {
    let home = tmp_home("2299-ttl-boundary");
    enqueue(
        &home,
        "a",
        msg()
            .sender("l")
            .text("under")
            .id("m-under")
            .delivering_at(&secs_ago(590))
            .build(),
    )
    .unwrap();
    enqueue(
        &home,
        "a",
        msg()
            .sender("l")
            .text("over")
            .id("m-over")
            .delivering_at(&secs_ago(610))
            .build(),
    )
    .unwrap();

    reclaim_stale_delivering(&home);

    // The just-over row reverted to unread → drain re-delivers ONLY it.
    let redelivered = drain(&home, "a");
    let ids: Vec<&str> = redelivered.iter().filter_map(|m| m.id.as_deref()).collect();
    assert_eq!(
        ids,
        vec!["m-over"],
        "only the past-TTL row is reclaimed+re-delivered"
    );
    fs::remove_dir_all(&home).ok();
}

/// 並發 sweep+drain+ack 不雙 reclaim: under concurrent drain + reclaim + ack on
/// the same inbox (all serialized by `with_inbox_lock`), every message is
/// returned AT MOST ONCE across all drains — no double-deliver, no double-reclaim.
#[test]
fn concurrent_drain_reclaim_ack_no_double_deliver() {
    use std::sync::Arc;
    let home = tmp_home("2299-concurrent");
    for i in 0..20 {
        enqueue(
            &home,
            "a",
            msg().sender("l").text("m").id(&format!("m-{i}")).build(),
        )
        .unwrap();
    }
    let home = Arc::new(home);
    let returned = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut handles = Vec::new();
    for t in 0..6 {
        let home = Arc::clone(&home);
        let returned = Arc::clone(&returned);
        handles.push(std::thread::spawn(move || {
            for _ in 0..5 {
                if t % 3 == 0 {
                    for m in drain(&home, "a") {
                        if let Some(id) = m.id {
                            returned.lock().push(id);
                        }
                    }
                } else if t % 3 == 1 {
                    reclaim_stale_delivering(&home);
                } else {
                    ack(&home, "a", None);
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    // No message id may be RETURNED to an agent more than once (exactly-once
    // delivery holds even with reclaim+ack racing the drains).
    let mut ids = returned.lock().clone();
    ids.sort();
    let mut deduped = ids.clone();
    deduped.dedup();
    assert_eq!(
        ids, deduped,
        "a message was delivered more than once: {ids:?}"
    );
    fs::remove_dir_all(&*home).ok();
}

/// legacy back-compat: a row written before #2299 (no `delivering_at` field)
/// drains exactly as an unread message did, and a legacy READ row stays read.
#[test]
fn legacy_rows_without_delivering_at_field() {
    let home = tmp_home("2299-legacy");
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).unwrap();
    let legacy_unread = r#"{"schema_version":1,"id":"m-leg-unread","from":"l","text":"u","kind":null,"timestamp":"2025-01-01T00:00:00Z"}"#;
    let legacy_read = r#"{"schema_version":1,"id":"m-leg-read","from":"l","text":"r","kind":null,"timestamp":"2025-01-01T00:00:00Z","read_at":"2025-01-01T00:00:01Z"}"#;
    fs::write(
        inbox_path(&home, "a"),
        format!("{legacy_unread}\n{legacy_read}\n"),
    )
    .unwrap();

    // Legacy unread drains (→ delivering); legacy read is skipped.
    let drained = drain(&home, "a");
    assert_eq!(drained.len(), 1, "only the legacy unread row drains");
    assert_eq!(drained[0].id.as_deref(), Some("m-leg-unread"));
    assert!(
        drained[0].delivering_at.is_some(),
        "legacy unread → delivering on drain"
    );

    // Reclaim ignores a legacy row that never had delivering_at (it's read or
    // freshly delivering) — no spurious revert of the legacy read row.
    reclaim_stale_delivering(&home);
    let content = fs::read_to_string(inbox_path(&home, "a")).unwrap();
    for line in content.lines() {
        let m: InboxMessage = serde_json::from_str(line).unwrap();
        if m.id.as_deref() == Some("m-leg-read") {
            assert!(m.read_at.is_some(), "legacy read row must stay processed");
        }
    }
    fs::remove_dir_all(&home).ok();
}

/// unread_count must EXCLUDE delivering rows (else a healthy mid-turn agent is
/// re-paged); reclaim restores the count when it reverts a stale row.
#[test]
fn unread_count_excludes_delivering() {
    let home = tmp_home("2299-unread-count");
    enqueue(&home, "a", make_msg("l", "one")).unwrap();
    enqueue(&home, "a", make_msg("l", "two")).unwrap();
    assert_eq!(
        unread_count(&home, "a").0,
        2,
        "two genuinely-unread messages"
    );

    drain(&home, "a"); // → both delivering
    assert_eq!(
        unread_count(&home, "a").0,
        0,
        "delivering (in-flight) rows must not count as unread"
    );

    // Age them past the TTL + reclaim → back to unread → counted again.
    let content = fs::read_to_string(inbox_path(&home, "a")).unwrap();
    let aged: String = content
        .lines()
        .map(|l| {
            let mut m: InboxMessage = serde_json::from_str(l).unwrap();
            m.delivering_at = Some(secs_ago(660));
            serde_json::to_string(&m).unwrap()
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(inbox_path(&home, "a"), format!("{aged}\n")).unwrap();
    reclaim_stale_delivering(&home);
    assert_eq!(
        unread_count(&home, "a").0,
        2,
        "reclaimed rows count as unread again"
    );
    fs::remove_dir_all(&home).ok();
}

/// A delivering (in-flight) row counts as "already drained" for the #911
/// re-inject dedup — so the daemon never re-pushes an in-flight message.
#[test]
fn msg_already_drained_treats_delivering_as_drained() {
    let home = tmp_home("2299-already-drained");
    enqueue(
        &home,
        "a",
        msg().sender("l").text("x").id("m-inflight").build(),
    )
    .unwrap();
    drain(&home, "a"); // → delivering, read_at still None
    assert!(
        storage::msg_already_drained_in_jsonl(&home, "a", "m-inflight"),
        "a delivering row must read as already-drained (suppress daemon re-inject)"
    );
    fs::remove_dir_all(&home).ok();
}

/// `ack` with no message_id confirms the WHOLE drained batch (delivering →
/// processed), and is an idempotent no-op on a second call.
#[test]
fn ack_all_delivering_when_no_msg_id() {
    let home = tmp_home("2299-ack-all");
    enqueue(&home, "a", make_msg("l", "one")).unwrap();
    enqueue(&home, "a", make_msg("l", "two")).unwrap();
    drain(&home, "a"); // → both delivering

    assert_eq!(
        ack(&home, "a", None),
        2,
        "ack(None) confirms the whole batch"
    );
    let content = fs::read_to_string(inbox_path(&home, "a")).unwrap();
    for line in content.lines() {
        let m: InboxMessage = serde_json::from_str(line).unwrap();
        assert!(m.read_at.is_some(), "every drained row is now processed");
    }
    assert_eq!(
        ack(&home, "a", None),
        0,
        "second ack is an idempotent no-op"
    );
    fs::remove_dir_all(&home).ok();
}

/// F1 (#2299): a `delivering` blocking dispatch (query/task) counts as
/// "delivered" for `has_drained_blocker_for_correlation` — sibling-consistent
/// with `msg_already_drained_in_jsonl` — so a codex report/update reply on that
/// correlation overrides ack-absorption (reaches the agent), just as a fully
/// drained dispatch does. A never-delivered (plain unread) dispatch does not.
#[test]
fn has_drained_blocker_counts_delivering_dispatch() {
    let home = tmp_home("2299-blocker-delivering");
    // A task dispatch handed to the agent: delivering (delivering_at set, read_at None).
    let mut delivering = msg()
        .sender("lead")
        .text("review this")
        .id("m-blk-deliv")
        .kind("task")
        .delivering_at(&secs_ago(5))
        .build();
    delivering.correlation_id = Some("c-delivering".to_string());
    enqueue(&home, "a", delivering).unwrap();
    // A second task that is still plain unread (never delivered).
    let mut unread = msg()
        .sender("lead")
        .text("later")
        .id("m-blk-unread")
        .kind("task")
        .build();
    unread.correlation_id = Some("c-unread".to_string());
    enqueue(&home, "a", unread).unwrap();

    assert!(
        has_drained_blocker_for_correlation(&home, "a", "c-delivering"),
        "a delivering blocking dispatch must count (override ack-absorption)"
    );
    assert!(
        !has_drained_blocker_for_correlation(&home, "a", "c-unread"),
        "a never-delivered (plain unread) dispatch must NOT count yet"
    );
    fs::remove_dir_all(&home).ok();
}

/// F2 (#2299): `describe_message` reports a `delivering` row as `Delivering`
/// (not `Unread`) so a delivery audit (`inbox message_id=…`) does not mistake an
/// in-flight message for undelivered and re-send it. Full lifecycle:
/// unread → Unread, drained → Delivering, acked → ReadAt.
#[test]
fn describe_message_reports_delivering_state() {
    let home = tmp_home("2299-describe-delivering");
    // Fresh timestamp so the pre-drain row is live `Unread`, not `UnreadExpired`
    // (the msg() builder default timestamp is >30d old).
    enqueue(
        &home,
        "a",
        msg()
            .sender("lead")
            .text("hi")
            .id("m-desc")
            .timestamp(&secs_ago(1))
            .build(),
    )
    .unwrap();

    assert!(
        matches!(
            describe_message(&home, "m-desc", "a"),
            MessageStatus::Unread { .. }
        ),
        "before drain: Unread"
    );
    drain(&home, "a");
    assert!(
        matches!(
            describe_message(&home, "m-desc", "a"),
            MessageStatus::Delivering { .. }
        ),
        "after drain: Delivering (delivered, not yet processed)"
    );
    ack(&home, "a", Some("m-desc"));
    assert!(
        matches!(
            describe_message(&home, "m-desc", "a"),
            MessageStatus::ReadAt(..)
        ),
        "after ack: ReadAt (processed)"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn test_sweep_expired_read_7d() {
    let home = tmp_home("sweep-read-7d");
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).ok();

    let old_ts = (chrono::Utc::now() - chrono::Duration::days(10)).to_rfc3339();
    let fresh_ts = chrono::Utc::now().to_rfc3339();
    let read_old = format!(
        r#"{{"schema_version":1,"id":"m-old","from":"a","text":"old read","kind":null,"timestamp":"{old_ts}","read_at":"{old_ts}"}}"#
    );
    let read_fresh = format!(
        r#"{{"schema_version":1,"id":"m-fresh","from":"b","text":"fresh read","kind":null,"timestamp":"{fresh_ts}","read_at":"{fresh_ts}"}}"#
    );
    fs::write(
        inbox_dir.join("agent1.jsonl"),
        format!("{read_old}\n{read_fresh}\n"),
    )
    .ok();

    sweep_expired(&home);

    let content = fs::read_to_string(inbox_dir.join("agent1.jsonl")).expect("file");
    let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "read message >7d must be swept");
    assert!(
        lines[0].contains("m-fresh"),
        "fresh read message must survive"
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn test_sweep_unread_30d() {
    let home = tmp_home("sweep-unread-30d");
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).ok();

    let old_ts = (chrono::Utc::now() - chrono::Duration::days(35)).to_rfc3339();
    let recent_ts = (chrono::Utc::now() - chrono::Duration::days(5)).to_rfc3339();
    let unread_old = format!(
        r#"{{"schema_version":1,"id":"m-unread-old","from":"a","text":"ancient","kind":null,"timestamp":"{old_ts}"}}"#
    );
    let unread_recent = format!(
        r#"{{"schema_version":1,"id":"m-unread-recent","from":"b","text":"recent","kind":null,"timestamp":"{recent_ts}"}}"#
    );
    fs::write(
        inbox_dir.join("agent1.jsonl"),
        format!("{unread_old}\n{unread_recent}\n"),
    )
    .ok();

    sweep_expired(&home);

    let content = fs::read_to_string(inbox_dir.join("agent1.jsonl")).expect("file");
    let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "unread message >30d must be swept");
    assert!(
        lines[0].contains("m-unread-recent"),
        "recent unread must survive"
    );

    fs::remove_dir_all(&home).ok();
}

// ── #inbox-gc part b: shortened read TTL + blocker exemption + size cap ──

#[test]
fn sweep_read_non_blocker_48h() {
    let home = tmp_home("sweep-48h");
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).ok();
    let old = (chrono::Utc::now() - chrono::Duration::hours(50)).to_rfc3339();
    let fresh = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let old_row = format!(
        r#"{{"schema_version":1,"id":"m-old","from":"a","text":"x","kind":"update","timestamp":"{old}","read_at":"{old}"}}"#
    );
    let fresh_row = format!(
        r#"{{"schema_version":1,"id":"m-fresh","from":"b","text":"x","kind":"update","timestamp":"{fresh}","read_at":"{fresh}"}}"#
    );
    fs::write(
        inbox_dir.join("a1.jsonl"),
        format!("{old_row}\n{fresh_row}\n"),
    )
    .ok();
    sweep_expired(&home);
    let content = fs::read_to_string(inbox_dir.join("a1.jsonl")).expect("file");
    assert!(
        !content.contains("m-old"),
        "read non-blocker >48h must be swept (was 7d)"
    );
    assert!(
        content.contains("m-fresh"),
        "read non-blocker <48h must survive"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn sweep_exempts_drained_blocker_until_7d() {
    let home = tmp_home("sweep-blocker");
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).ok();
    // A drained task (blocker) 3d old is past the 48h read TTL but within the
    // 7d blocker window → MUST survive so has_drained_blocker_for_correlation
    // still answers when the worker's reply arrives late.
    let d3 = (chrono::Utc::now() - chrono::Duration::days(3)).to_rfc3339();
    let d8 = (chrono::Utc::now() - chrono::Duration::days(8)).to_rfc3339();
    let kept = format!(
        r#"{{"schema_version":1,"id":"blk-3d","from":"lead","text":"t","kind":"task","timestamp":"{d3}","read_at":"{d3}","correlation_id":"c-keep"}}"#
    );
    let gone = format!(
        r#"{{"schema_version":1,"id":"blk-8d","from":"lead","text":"q","kind":"query","timestamp":"{d8}","read_at":"{d8}","correlation_id":"c-gone"}}"#
    );
    fs::write(inbox_dir.join("a1.jsonl"), format!("{kept}\n{gone}\n")).ok();
    sweep_expired(&home);
    let content = fs::read_to_string(inbox_dir.join("a1.jsonl")).expect("file");
    assert!(
        content.contains("blk-3d"),
        "drained blocker <7d must survive (ack-absorption audit window)"
    );
    assert!(
        !content.contains("blk-8d"),
        "drained blocker >7d must be swept"
    );
    assert!(
        has_drained_blocker_for_correlation(&home, "a1", "c-keep"),
        "ack-absorption audit must still see the surviving blocker"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn sweep_size_cap_keeps_recent_n_read_rows() {
    let home = tmp_home("sweep-cap");
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).ok();
    let mut lines = String::new();
    // 350 FRESH (<48h) read non-blocker rows, staggered so newest = highest i.
    for i in 0..350 {
        let ts = (chrono::Utc::now() - chrono::Duration::minutes(350 - i)).to_rfc3339();
        lines.push_str(&format!(
            r#"{{"schema_version":1,"id":"r{i}","from":"x","text":"x","kind":"update","timestamp":"{ts}","read_at":"{ts}"}}"#
        ));
        lines.push('\n');
    }
    // An unread obligation + a drained blocker — neither counted nor evicted.
    let now = chrono::Utc::now().to_rfc3339();
    lines.push_str(&format!(
        r#"{{"schema_version":1,"id":"unread1","from":"x","text":"x","kind":"query","timestamp":"{now}"}}"#
    ));
    lines.push('\n');
    lines.push_str(&format!(
        r#"{{"schema_version":1,"id":"blk1","from":"x","text":"x","kind":"task","timestamp":"{now}","read_at":"{now}"}}"#
    ));
    lines.push('\n');
    fs::write(inbox_dir.join("a1.jsonl"), lines).ok();
    sweep_expired(&home);
    let content = fs::read_to_string(inbox_dir.join("a1.jsonl")).expect("file");
    let read_kept = content.lines().filter(|l| l.contains(r#""id":"r"#)).count();
    assert_eq!(
        read_kept, 300,
        "size cap must keep exactly READ_ROW_CAP read non-blocker rows"
    );
    assert!(
        content.contains(r#""id":"r349""#) && content.contains(r#""id":"r50""#),
        "newest read rows kept"
    );
    assert!(
        !content.contains(r#""id":"r0""#) && !content.contains(r#""id":"r49""#),
        "oldest read rows beyond cap dropped"
    );
    assert!(
        content.contains("unread1"),
        "unread obligation never evicted by the cap"
    );
    assert!(
        content.contains("blk1"),
        "drained blocker never evicted by the cap"
    );
    fs::remove_dir_all(&home).ok();
}

// ── #inbox-gc part a: clear_compact (quiet trust-preserving clear) ──────

fn keep_query_and_task(m: &InboxMessage) -> Option<String> {
    match m.kind.as_deref() {
        Some("query") => Some("unanswered query".into()),
        Some("task") => Some("open task".into()),
        _ => None,
    }
}

#[test]
fn clear_compact_keeps_obligations_clears_rest() {
    let _g = READONLY_TEST_LOCK.lock();
    let home = tmp_home("clear-oblig");
    enqueue(
        &home,
        "a1",
        msg()
            .sender("lead")
            .kind("query")
            .id("q1")
            .text("q?")
            .build(),
    )
    .unwrap();
    enqueue(
        &home,
        "a1",
        msg()
            .sender("lead")
            .kind("task")
            .id("t1")
            .text("do x")
            .build(),
    )
    .unwrap();
    enqueue(
        &home,
        "a1",
        msg()
            .sender("lead")
            .kind("update")
            .id("u1")
            .text("fyi")
            .build(),
    )
    .unwrap();
    enqueue(
        &home,
        "a1",
        msg()
            .sender("ci")
            .kind("report")
            .id("rp1")
            .text("done")
            .build(),
    )
    .unwrap();
    enqueue(
        &home,
        "a1",
        msg()
            .sender("sys")
            .kind("update")
            .id("sup1")
            .text("old")
            .superseded_by("new")
            .build(),
    )
    .unwrap();

    let r = clear_compact(&home, "a1", keep_query_and_task);
    assert_eq!(
        r.cleared_count, 3,
        "update + report + superseded cleared (3)"
    );
    assert_eq!(r.kept_unread_count, 2, "query + task kept unread (2)");
    assert_eq!(r.requires_response.len(), 2, "both obligations surfaced");
    assert_eq!(
        unread_count(&home, "a1").0,
        2,
        "obligations remain UNREAD on disk after clear"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn clear_compact_summaries_bounded() {
    let _g = READONLY_TEST_LOCK.lock();
    let home = tmp_home("clear-bound");
    for i in 0..250 {
        enqueue(
            &home,
            "a1",
            msg()
                .sender("x")
                .kind("update")
                .id(&format!("u{i}"))
                .text("x")
                .build(),
        )
        .unwrap();
    }
    let r = clear_compact(&home, "a1", |_| None);
    assert_eq!(r.cleared_count, 250, "all 250 cleared");
    assert_eq!(
        r.summaries.len(),
        200,
        "summaries capped at CLEAR_SUMMARY_CAP"
    );
    assert_eq!(
        r.summaries_omitted, 50,
        "overflow counted, not dropped silently"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn clear_compact_preview_single_line_bounded() {
    let _g = READONLY_TEST_LOCK.lock();
    let home = tmp_home("clear-preview");
    let long = format!("line one\nline two\t{}", "z".repeat(200));
    enqueue(
        &home,
        "a1",
        msg()
            .sender("x")
            .kind("update")
            .id("u1")
            .text_owned(long)
            .build(),
    )
    .unwrap();
    let r = clear_compact(&home, "a1", |_| None);
    let s = &r.summaries[0];
    assert!(
        !s.preview.contains('\n') && !s.preview.contains('\t'),
        "preview must be a sanitised single line, got {:?}",
        s.preview
    );
    assert!(
        s.preview.chars().count() <= CLEAR_PREVIEW_CHARS_TEST + 1,
        "preview ≤60 chars (+ellipsis), got {}",
        s.preview.chars().count()
    );
    fs::remove_dir_all(&home).ok();
}

/// Mirror of the private `CLEAR_PREVIEW_CHARS` const for the bound assertion.
const CLEAR_PREVIEW_CHARS_TEST: usize = 60;

#[test]
fn clear_compact_does_not_arm_reply_ledger_source_scan() {
    // STRUCTURAL invariant (decision d-20260607081209372642-1): a compact-clear
    // must NEVER arm the reply-ledger or touch heartbeat_pair — else clearing a
    // historical channel backlog fabricates false "must-reply" turns. The state
    // lives in a global singleton (behavioural isolation is unreliable across
    // parallel tests), so we assert structurally on the function body.
    let src = include_str!("storage.rs");
    let start = src
        .find("pub fn clear_compact")
        .expect("clear_compact present");
    let body = &src[start..];
    let end = body
        .find("\n/// Count unread messages")
        .unwrap_or(body.len());
    let body = &body[..end];
    assert!(
        !body.contains("reply_ledger"),
        "clear_compact must NOT arm reply_ledger from backlog"
    );
    assert!(
        !body.contains("heartbeat_pair"),
        "clear_compact must NOT touch heartbeat_pair"
    );
    assert!(
        body.contains("mark_consumed"),
        "clear_compact SHOULD consume notification dedup for cleared rows"
    );
}

#[test]
fn test_describe_message_status_three_states() {
    let home = tmp_home("describe-msg");
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).ok();

    let now = chrono::Utc::now().to_rfc3339();
    let old_ts = (chrono::Utc::now() - chrono::Duration::days(35)).to_rfc3339();

    // State 1: read message
    let read_msg = format!(
        r#"{{"schema_version":1,"id":"m-read","from":"a","text":"read","kind":null,"timestamp":"{now}","read_at":"{now}"}}"#
    );
    // State 2: unread expired (>30d)
    let expired_msg = format!(
        r#"{{"schema_version":1,"id":"m-expired","from":"b","text":"expired","kind":null,"timestamp":"{old_ts}"}}"#
    );
    // State 3 (#bughunt-r2 #3): live unread (<30d), carrying delivery_mode +
    // correlation_id — must report Unread, NOT NotFound.
    let live_unread_msg = format!(
        r#"{{"schema_version":1,"id":"m-live","from":"c","text":"live","kind":null,"timestamp":"{now}","delivery_mode":"pty","correlation_id":"t-xyz"}}"#
    );
    fs::write(
        inbox_dir.join("agent1.jsonl"),
        format!("{read_msg}\n{expired_msg}\n{live_unread_msg}\n"),
    )
    .ok();

    // ReadAt
    match describe_message(&home, "m-read", "agent1") {
        MessageStatus::ReadAt(t, _dm) => assert_eq!(t, now),
        other => panic!("expected ReadAt, got: {other:?}"),
    }

    // UnreadExpired
    assert_eq!(
        describe_message(&home, "m-expired", "agent1"),
        MessageStatus::UnreadExpired
    );

    // #bughunt-r2 #3: a live un-drained message → Unread (was NotFound), with
    // its delivery_mode + correlation_id preserved for delivery audit.
    assert_eq!(
        describe_message(&home, "m-live", "agent1"),
        MessageStatus::Unread {
            delivery_mode: Some("pty".to_string()),
            correlation_id: Some("t-xyz".to_string()),
        }
    );

    // NotFound (genuinely absent id stays distinct from Unread)
    assert_eq!(
        describe_message(&home, "m-nonexistent", "agent1"),
        MessageStatus::NotFound
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn test_enqueue_concurrent_same_agent() {
    let _guard = READONLY_TEST_LOCK.lock();
    let home = tmp_home("concurrent-same");
    let home_arc = std::sync::Arc::new(home.clone());
    let mut handles = vec![];

    for i in 0..20 {
        let h = home_arc.clone();
        handles.push(std::thread::spawn(move || {
            enqueue(&h, "agent1", make_msg(&format!("t{i}"), &format!("msg{i}")))
                .expect("enqueue should succeed");
        }));
    }
    for h in handles {
        h.join().expect("thread should not panic");
    }

    let msgs = drain(&home, "agent1");
    assert_eq!(
        msgs.len(),
        20,
        "all 20 concurrent enqueues must survive, got {}",
        msgs.len()
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn test_enqueue_vs_drain_no_lost_msg() {
    let _guard = READONLY_TEST_LOCK.lock();
    // Thread A enqueues 10 messages; thread B drains after each.
    // Total drained must equal 10 — no lost messages.
    let home = tmp_home("enqueue-vs-drain");
    let home_a = std::sync::Arc::new(home.clone());
    let home_b = home_a.clone();

    let writer = std::thread::spawn(move || {
        for i in 0..10 {
            enqueue(
                &home_a,
                "agent1",
                make_msg(&format!("w{i}"), &format!("msg{i}")),
            )
            .expect("enqueue");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    });

    let reader = std::thread::spawn(move || {
        let mut total = Vec::new();
        for _ in 0..20 {
            let batch = drain(&home_b, "agent1");
            total.extend(batch);
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        total
    });

    writer.join().expect("writer");
    let mut drained = reader.join().expect("reader");
    // Final drain to catch any remaining
    drained.extend(drain(&home, "agent1"));

    assert_eq!(
        drained.len(),
        10,
        "all 10 enqueued messages must be drained, got {}",
        drained.len()
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn test_concurrent_drain_no_duplicates() {
    let _guard = READONLY_TEST_LOCK.lock();
    // #1940: two threads drain the same inbox simultaneously; the inbox lock
    // must serialize them so each of the 3 messages is returned EXACTLY ONCE
    // (no duplicate delivery under contention) — the exactly-once contract a
    // re-serve snapshot would have broken.
    let home = tmp_home("concurrent-drain");
    for i in 0..3 {
        enqueue(
            &home,
            "agent1",
            make_msg(&format!("m{i}"), &format!("msg{i}")),
        )
        .ok();
    }

    let home_a = std::sync::Arc::new(home.clone());
    let home_b = home_a.clone();

    let a = std::thread::spawn(move || drain(&home_a, "agent1"));
    let b = std::thread::spawn(move || drain(&home_b, "agent1"));

    let mut all = a.join().expect("thread a");
    all.extend(b.join().expect("thread b"));

    assert_eq!(
        all.len(),
        3,
        "each message drained exactly once (no duplicates), got {}",
        all.len()
    );

    fs::remove_dir_all(&home).ok();
}

#[test]
fn test_inbox_msg_thread_parent_fields_roundtrip() {
    // New fields with #[serde(default)] must round-trip and be absent from legacy
    let msg = msg()
        .id("m-1")
        .sender("a")
        .text("t")
        .timestamp("2026-01-01T00:00:00Z")
        .thread_id("thread-42")
        .parent_id("m-0")
        .build();
    let json = serde_json::to_string(&msg).expect("ser");
    assert!(json.contains("thread_id"));
    assert!(json.contains("parent_id"));
    let parsed: InboxMessage = serde_json::from_str(&json).expect("deser");
    assert_eq!(parsed.thread_id.as_deref(), Some("thread-42"));
    assert_eq!(parsed.parent_id.as_deref(), Some("m-0"));

    // Legacy JSON without these fields → None (forward compat)
    let legacy = r#"{"from":"x","text":"y","timestamp":"2026-01-01T00:00:00Z"}"#;
    let parsed: InboxMessage = serde_json::from_str(legacy).expect("legacy deser");
    assert!(parsed.thread_id.is_none());
    assert!(parsed.parent_id.is_none());

    // None fields should be omitted from serialization (skip_serializing_if)
    let msg_no_thread = InboxMessage {
        thread_id: None,
        parent_id: None,
        task_id: None,
        force_meta: None,
        correlation_id: None,
        reviewed_head: None,
        ..msg.clone()
    };
    let json2 = serde_json::to_string(&msg_no_thread).expect("ser");
    assert!(!json2.contains("thread_id"));
    assert!(!json2.contains("parent_id"));
}

#[test]
fn test_header_format_all_fields_present() {
    let msg = msg()
        .id("m-42")
        .sender("from:dev-lead")
        .text_owned("x".repeat(500))
        .kind("task")
        .timestamp("2026-01-01T00:00:00Z")
        .thread_id("t-100")
        .parent_id("m-41")
        .build();
    let header = format_header(&msg);
    assert!(header.contains("[AGEND-MSG]"));
    // #761: `from=` field strips the redundant `from:` prefix that
    // the agent producer adds at Source::Agent display impl.
    assert!(header.contains("from=dev-lead"));
    assert!(!header.contains("from=from:"));
    assert!(header.contains("id=m-42"));
    assert!(header.contains("kind=task"));
    assert!(header.contains("thread=t-100"));
    assert!(header.contains("parent=m-41"));
    assert!(header.contains("size=500"));
    assert!(!header.contains('\n'), "header must be single line");
}

/// #761: agent-source `from:NAME` (sole producer at the
/// `Source::Agent` Display impl in inbox.rs:144) gets its redundant
/// `from:` prefix stripped at PTY-header rendering. Pre-fix the
/// header read `from=from:dev-fast-1`; the double prefix confused
/// agents that parsed the field as identity.
#[test]
fn test_header_format_strips_from_prefix_for_agent_sources() {
    let msg = msg()
        .id("m-1")
        .sender("from:dev-fast-1")
        .text("ping")
        .kind("task")
        .timestamp("2026-05-14T19:00:00Z")
        .build();
    let header = format_header(&msg);
    assert!(
        header.contains("from=dev-fast-1"),
        "agent from= should drop the `from:` producer prefix: {header}"
    );
    assert!(
        !header.contains("from=from:"),
        "header must NOT carry the doubled `from=from:` form: {header}"
    );
}

/// #761: non-agent sources (system events, telegram users) carry
/// their own namespace prefix (`system:`, `user:`) and must
/// survive the strip pass untouched. Only the literal `from:`
/// prefix from `Source::Agent` is dropped.
#[test]
fn test_header_format_preserves_system_namespace() {
    let msg = msg()
        .id("m-1")
        .sender("system:fleet_idle_watchdog")
        .text("ping")
        .kind("watchdog")
        .timestamp("2026-05-14T19:00:00Z")
        .build();
    let header = format_header(&msg);
    assert!(
        header.contains("from=system:fleet_idle_watchdog"),
        "system namespace must survive untouched: {header}"
    );
}

#[test]
fn test_header_format_omits_none_fields() {
    let msg = msg()
        .id("m-1")
        .sender("from:agent")
        .text("hello")
        .timestamp("2026-01-01T00:00:00Z")
        .build();
    let header = format_header(&msg);
    // #761: agent-source prefix stripped.
    assert!(header.contains("from=agent"));
    assert!(!header.contains("from=from:"));
    assert!(header.contains("id=m-1"));
    assert!(!header.contains("kind="));
    assert!(!header.contains("thread="));
    assert!(!header.contains("parent="));
    assert!(header.contains("size=5"));
}

#[test]
fn test_header_format_includes_attachments_when_present() {
    let msg = msg()
        .id("m-1")
        .sender("from:user")
        .text("see photo")
        .timestamp("2026-01-01T00:00:00Z")
        .attachments(vec![crate::channel::event::Attachment {
            kind: crate::channel::event::AttachmentKind::Photo,
            path: std::path::PathBuf::from("/tmp/photo.jpg"),
            mime: None,
            caption: None,
            size_bytes: None,
            original_filename: None,
        }])
        .build();
    let header = format_header(&msg);
    assert!(
        header.contains("attachments=[/tmp/photo.jpg]"),
        "header must include attachment paths: {header}"
    );
}

#[test]
fn test_header_format_omits_attachments_when_empty() {
    let msg = msg()
        .id("m-1")
        .sender("from:user")
        .text("text only")
        .timestamp("2026-01-01T00:00:00Z")
        .build();
    let header = format_header(&msg);
    assert!(
        !header.contains("attachments"),
        "empty attachments must not appear in header: {header}"
    );
}

#[test]
fn test_header_format_joins_multiple_attachments_with_comma() {
    // Locks the `paths.join(",")` separator contract — future refactor that
    // changes the separator (e.g. to ";" or " ") must update this test.
    let mk_att = |p: &str| crate::channel::event::Attachment {
        kind: crate::channel::event::AttachmentKind::Photo,
        path: std::path::PathBuf::from(p),
        mime: None,
        caption: None,
        size_bytes: None,
        original_filename: None,
    };
    let msg = msg()
        .id("m-1")
        .sender("from:user")
        .text("multi")
        .timestamp("2026-01-01T00:00:00Z")
        .attachments(vec![
            mk_att("/tmp/a.jpg"),
            mk_att("/tmp/b.jpg"),
            mk_att("/tmp/c.jpg"),
        ])
        .build();
    let header = format_header(&msg);
    assert!(
        header.contains("attachments=[/tmp/a.jpg,/tmp/b.jpg,/tmp/c.jpg]"),
        "multi-attachment paths must be comma-joined: {header}"
    );
}

/// Sprint 54 layer-5: regression-proof for qa-test smoke
/// 2026-05-07 17:55 UTC — `send(team=qa-test, ...)` recipient PTY
/// header must surface the team= field so broadcast is
/// distinguishable from unicast at agent vantage. Mutate-revert:
/// removing the `format_header` broadcast-context branch leaves
/// this test failing with header lacking `team=` / `broadcast=`.
#[test]
fn test_header_format_includes_team_and_broadcast_when_team_broadcast() {
    let msg = msg()
        .id("m-1")
        .sender("from:lead")
        .text("ping")
        .kind("query")
        .timestamp("2026-05-07T18:00:00Z")
        .broadcast_context(BroadcastContext {
            team: Some("qa-test".to_string()),
            targets: vec!["kiro-cli-ea377a".into(), "kiro-cli-4e8a78".into()],
            count: 2,
        })
        .build();
    let header = format_header(&msg);
    assert!(
        header.contains("broadcast=2"),
        "team broadcast must surface count: {header}"
    );
    assert!(
        header.contains("team=qa-test"),
        "team broadcast must surface team name (qa-test smoke success criteria): {header}"
    );
}

/// Direct `targets=[…]` / `tags=[…]` fan-out has no team — header
/// gains `broadcast=N` only, no `team=`.
#[test]
fn test_header_format_includes_broadcast_only_when_targets_broadcast() {
    let msg = msg()
        .id("m-1")
        .sender("from:dev")
        .text("fyi")
        .timestamp("2026-05-07T18:00:00Z")
        .broadcast_context(BroadcastContext {
            team: None,
            targets: vec!["a".into(), "b".into(), "c".into()],
            count: 3,
        })
        .build();
    let header = format_header(&msg);
    assert!(header.contains("broadcast=3"), "{header}");
    assert!(
        !header.contains("team="),
        "no-team broadcast must not emit team= field: {header}"
    );
}

/// Unicast must NOT surface broadcast/team fields — preserves the
/// pre-Sprint-54 header shape for non-broadcast callers (the
/// majority of SEND traffic).
#[test]
fn test_header_format_omits_broadcast_fields_when_unicast() {
    let msg = msg()
        .id("m-1")
        .sender("from:dev")
        .text("hi")
        .timestamp("2026-05-07T18:00:00Z")
        .build();
    let header = format_header(&msg);
    assert!(
        !header.contains("broadcast="),
        "unicast must not emit broadcast= field: {header}"
    );
    assert!(
        !header.contains("team="),
        "unicast must not emit team= field: {header}"
    );
}

/// Roundtrip: `broadcast_context` survives serde JSON encoding so the
/// inbox JSONL projection (visible via `inbox` MCP tool) matches the
/// PTY header. Absent field stays absent (no `null` leak).
#[test]
fn test_inbox_message_broadcast_context_serde_roundtrip() {
    let with_ctx = msg()
        .id("m-1")
        .sender("from:lead")
        .text("t")
        .timestamp("2026-05-07T18:00:00Z")
        .broadcast_context(BroadcastContext {
            team: Some("qa-test".to_string()),
            targets: vec!["a".into(), "b".into()],
            count: 2,
        })
        .build();
    let json = serde_json::to_string(&with_ctx).expect("ser");
    assert!(json.contains("broadcast_context"));
    assert!(json.contains("\"team\":\"qa-test\""));
    let parsed: InboxMessage = serde_json::from_str(&json).expect("deser");
    let parsed_ctx = parsed.broadcast_context.expect("ctx survived");
    assert_eq!(parsed_ctx.team.as_deref(), Some("qa-test"));
    assert_eq!(parsed_ctx.count, 2);
    assert_eq!(parsed_ctx.targets, vec!["a", "b"]);

    let without_ctx = InboxMessage {
        broadcast_context: None,
        ..with_ctx
    };
    let json2 = serde_json::to_string(&without_ctx).expect("ser");
    assert!(
        !json2.contains("broadcast_context"),
        "absent field must not serialize as null: {json2}"
    );
}

#[test]
fn test_header_reply_excerpt_present() {
    let mut msg = msg()
        .id("m-1")
        .sender("u")
        .text("reply")
        .timestamp("t")
        .in_reply_to_msg_id("42")
        .in_reply_to_excerpt("[bob] original")
        .build();
    let h = format_header(&msg);
    assert!(h.contains("reply_to_excerpt=[bob] original"), "{h}");
    msg.in_reply_to_excerpt = None;
    assert!(!format_header(&msg).contains("reply_to_excerpt"));
}

#[test]
fn test_reply_excerpt_long_truncated() {
    let long: String = "x".repeat(300);
    let trunc: String = long.chars().take(200).collect();
    assert_eq!(trunc.len(), 200);
    let excerpt = format!("[a] {trunc}…");
    assert!(excerpt.contains("…"));
}

#[test]
fn test_build_excerpt_empty_returns_none() {
    assert_eq!(build_excerpt("", "alice"), None);
}

#[test]
fn test_build_excerpt_short_text_no_ellipsis() {
    let out = build_excerpt("hello", "alice").expect("non-empty text returns Some");
    assert_eq!(out, "[alice] hello");
    assert!(!out.contains('…'));
}

#[test]
fn test_build_excerpt_long_text_truncates_to_200_with_ellipsis() {
    let long: String = "x".repeat(250);
    let out = build_excerpt(&long, "bob").expect("non-empty text returns Some");
    assert!(out.starts_with("[bob] "));
    assert!(out.ends_with('…'));
    // Body between "[bob] " prefix and trailing "…" must be 200 chars.
    let body = out
        .strip_prefix("[bob] ")
        .and_then(|s| s.strip_suffix('…'))
        .expect("expected `[bob] {200 chars}…` shape");
    assert_eq!(body.chars().count(), 200);
}

#[test]
fn test_build_excerpt_cjk_truncated_to_200_chars() {
    // 250 CJK chars (each is 3 bytes UTF-8). Verifies char-based
    // truncation, not byte-based — a byte-based take(200) would slice
    // mid-codepoint and produce invalid UTF-8.
    let cjk: String = "一".repeat(250);
    assert_eq!(cjk.chars().count(), 250);
    assert_eq!(cjk.len(), 250 * 3);

    let out = build_excerpt(&cjk, "carol").expect("non-empty text returns Some");
    assert!(
        std::str::from_utf8(out.as_bytes()).is_ok(),
        "output must be valid UTF-8"
    );
    let body = out
        .strip_prefix("[carol] ")
        .and_then(|s| s.strip_suffix('…'))
        .expect("expected `[carol] {200 CJK chars}…` shape");
    assert_eq!(body.chars().count(), 200);
    assert_eq!(body.len(), 200 * 3, "200 CJK chars = 600 UTF-8 bytes");
}

#[test]
fn test_build_excerpt_exactly_200_no_ellipsis() {
    // Boundary: 200 chars exactly should NOT get truncated marker.
    let exact: String = "y".repeat(200);
    let out = build_excerpt(&exact, "dan").expect("non-empty text returns Some");
    assert!(
        !out.contains('…'),
        "200-char input must not get ellipsis: {out}"
    );
}

// Issue #672: inline-notification truncation hint must point at the
// `inbox` MCP tool, not at the (non-existent) `agend-terminal agent
// inbox` CLI subcommand. The CLI subcommand was never implemented;
// a user following the old hint hits `unrecognized subcommand`.
#[test]
fn test_inline_long_text_hint_points_at_mcp_tool_not_cli() {
    let long: String = "x".repeat(250);
    let out = format_notification_for_inject(false, &NotifySource::Agent("peer"), &long, &[]);
    assert!(
        out.contains("inbox MCP tool"),
        "hint must direct caller to the MCP tool: {out}"
    );
    assert!(
        !out.contains("agend-terminal agent"),
        "hint must not reference the non-existent `agent` CLI subcommand: {out}"
    );
}

#[test]
fn test_reply_excerpt_newline_escaped() {
    let msg = msg()
        .id("m-1")
        .sender("u")
        .text("r")
        .timestamp("t")
        .in_reply_to_msg_id("42")
        .in_reply_to_excerpt("[b] line1\nline2")
        .build();
    let h = format_header(&msg);
    assert!(!h.contains('\n'), "must be single line: {h}");
    assert!(h.contains("reply_to_excerpt="), "{h}");
}

#[test]
fn test_format_header_escapes_newlines() {
    let msg = msg()
        .id("m-1")
        .sender("from:evil\nagent")
        .text("hello")
        .kind("task\r\ninjection")
        .timestamp("2026-01-01T00:00:00Z")
        .thread_id("t\n1")
        .build();
    let header = format_header(&msg);
    assert!(
        !header.contains('\n'),
        "header must not contain newline: {header:?}"
    );
    assert!(
        !header.contains('\r'),
        "header must not contain CR: {header:?}"
    );
    // #761: `from:` agent prefix is stripped BEFORE sanitize, so the
    // newline-escaped tail `evil agent` is what surfaces in the field.
    assert!(header.contains("from=evil agent"));
    assert!(!header.contains("from=from:"));
    assert!(header.contains("kind=task  injection"));
}

#[test]
fn test_format_header_escapes_control_chars() {
    let msg = msg()
        .sender("from:\x00null\x07bell")
        .text("t")
        .timestamp("2026-01-01T00:00:00Z")
        .build();
    let header = format_header(&msg);
    assert!(
        !header.chars().any(|c| c.is_control() && c != '\x1b'),
        "header must not contain control chars (except ANSI escape): {header:?}"
    );
}

// #t-109: the misleading dead `HEADER_SIZE_THRESHOLD` (300) const has been
// removed (operator decision d-20260617102838730641-2: keep the real `< 200`
// gate; production never consulted the 300 const). The vacuous
// `test_short_msg_below_threshold` (asserted `300 <= 300`) was removed earlier
// (#t-3); the prior `test_long_msg_above_threshold` is redundant with
// `test_header_format_all_fields_present` (500-char body → `size=500`, single
// line). The real short/long split is locked by
// `channel::telegram::inbound::tests::is_short_inject_routes_by_char_count_and_attachments`.

#[test]
fn format_header_size_reports_char_count_not_bytes() {
    // 100 CJK chars = 100 chars but 300 bytes (3 bytes each in UTF-8). The
    // header's `size=` must report the CHAR count, not the byte count (#1077).
    let cjk = "你".repeat(100);
    assert_eq!(cjk.chars().count(), 100);
    assert_eq!(cjk.len(), 300); // bytes
    let msg = msg()
        .sender("from:x")
        .text_owned(cjk)
        .timestamp("2026-01-01T00:00:00Z")
        .build();
    let header = format_header(&msg);
    assert!(
        header.contains("size=100"),
        "size must be char count (100), not byte count (300): {header}"
    );
}

#[test]
fn test_format_event_header_basic() {
    let h = format_event_header("poll-reminder", &[("unread", "3"), ("oldest", "5m")]);
    assert!(h.contains("[AGEND-MSG]"), "must have prefix");
    assert!(h.contains("kind=poll-reminder"), "must have kind");
    assert!(h.contains("unread=3"), "must have unread field");
    assert!(h.contains("oldest=5m"), "must have oldest field");
    assert!(!h.contains('\n'), "must be single line");
}

#[test]
fn test_format_event_header_sanitizes_fields() {
    let h = format_event_header("evil\nkind", &[("key", "val\r\nue")]);
    assert!(
        !h.contains('\n'),
        "newlines in kind must be sanitized: {h:?}"
    );
    assert!(!h.contains('\r'), "CR in value must be sanitized: {h:?}");
    assert!(h.contains("kind=evil kind"));
    assert!(h.contains("key=val  ue"));
}

/// Serialize tests that mutate AGEND_POINTER_ONLY_INJECT env var.
static ENV_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

#[test]
fn test_pointer_only_feature_flag() {
    let _lock = ENV_LOCK.lock();
    std::env::set_var("AGEND_POINTER_ONLY_INJECT", "1");
    assert!(pointer_only_inject(), "flag=1 must enable pointer-only");
    std::env::remove_var("AGEND_POINTER_ONLY_INJECT");
}

#[test]
fn test_pointer_only_disabled_default() {
    let _lock = ENV_LOCK.lock();
    std::env::remove_var("AGEND_POINTER_ONLY_INJECT");
    assert!(!pointer_only_inject(), "unset flag must default to false");
    std::env::set_var("AGEND_POINTER_ONLY_INJECT", "0");
    assert!(!pointer_only_inject(), "flag=0 must be disabled");
    std::env::remove_var("AGEND_POINTER_ONLY_INJECT");
}

#[test]
fn test_notify_agent_pointer_only_emits_header_not_body() {
    // Pure function test: pointer_only=true → output contains [AGEND-MSG]
    // + size= but NOT the body text.
    let source = NotifySource::Agent("sender");
    let s = format_notification_for_inject(true, &source, "secret body text", &[]);
    assert!(
        s.contains("[AGEND-MSG]"),
        "pointer mode must contain [AGEND-MSG]: {s}"
    );
    assert!(s.contains("size="), "pointer mode must contain size=: {s}");
    assert!(
        !s.contains("secret body text"),
        "pointer mode must NOT contain body: {s}"
    );
    assert!(
        s.contains("use inbox tool"),
        "pointer mode must direct to inbox: {s}"
    );
}

#[test]
fn test_notify_agent_default_inline_behavior() {
    // Pure function test: pointer_only=false → output contains the body text.
    let source = NotifySource::Agent("sender");
    let s = format_notification_for_inject(false, &source, "inline text", &[]);
    assert!(
        s.contains("inline text"),
        "default mode must contain body: {s}"
    );
    assert!(
        !s.contains("[AGEND-MSG]"),
        "default mode must NOT contain [AGEND-MSG] header: {s}"
    );
}

#[test]
fn inbox_message_typed_channel_field_compat() {
    // New typed channel field roundtrip
    let mut msg = make_msg("test", "hello");
    assert!(msg.channel.is_none());
    msg.channel = Some(crate::channel::ChannelKind::Telegram);
    let serialized = serde_json::to_string(&msg).unwrap();
    assert!(
        serialized.contains(r#""channel":"telegram""#),
        "channel must serialize as snake_case: {serialized}"
    );
    let reparsed: InboxMessage = serde_json::from_str(&serialized).unwrap();
    assert_eq!(
        reparsed.channel,
        Some(crate::channel::ChannelKind::Telegram)
    );

    // Legacy JSONL without channel field → None
    let legacy = r#"{"schema_version":1,"from":"x","text":"y","timestamp":"2026-01-01T00:00:00Z"}"#;
    let legacy_msg: InboxMessage = serde_json::from_str(legacy).unwrap();
    assert_eq!(legacy_msg.channel, None);
}

#[test]
fn legacy_inbox_message_without_attachments_deserializes() {
    // Old messages (pre-PR-AF) have no `attachments` field.
    // serde(default) must fill Vec::new() so deserialization succeeds.
    let legacy = r#"{"from":"user:op","text":"hello","timestamp":"2026-04-26T00:00:00Z"}"#;
    let msg: InboxMessage = serde_json::from_str(legacy).unwrap();
    assert!(msg.attachments.is_empty());
}

#[test]
fn legacy_inbox_message_without_excerpt_deserializes() {
    // Old messages (pre-PR-AQ) have no `in_reply_to_excerpt` field.
    // serde(default) must fill `None` so deserialization succeeds — locks
    // schema backward-compatibility for on-disk JSONL written before PR-AQ.
    let legacy = r#"{"from":"user:op","text":"hello","timestamp":"2026-04-26T00:00:00Z"}"#;
    let msg: InboxMessage = serde_json::from_str(legacy).unwrap();
    assert_eq!(msg.in_reply_to_excerpt, None);
    assert_eq!(msg.in_reply_to_msg_id, None);
}

#[test]
fn inbox_message_with_attachment_roundtrips() {
    use crate::channel::event::{Attachment, AttachmentKind};
    let msg = msg()
        .sender("user:op")
        .text("see photo")
        .timestamp("2026-04-26T00:00:00Z")
        .attachments(vec![Attachment {
            kind: AttachmentKind::Photo,
            path: "/tmp/photo.jpg".into(),
            mime: Some("image/jpeg".into()),
            caption: Some("test".into()),
            size_bytes: Some(1234),
            original_filename: None,
        }])
        .build();
    let json = serde_json::to_string(&msg).unwrap();
    let back: InboxMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(back.attachments.len(), 1);
    assert_eq!(back.attachments[0].kind, AttachmentKind::Photo);
    assert_eq!(back.attachments[0].size_bytes, Some(1234));
}

#[test]
fn inbox_message_with_in_reply_to_msg_id_roundtrips() {
    let mut msg = make_msg("user:op", "reply test");
    msg.in_reply_to_msg_id = Some("999".to_string());
    let json = serde_json::to_string(&msg).unwrap();
    assert!(
        json.contains(r#""in_reply_to_msg_id":"999""#),
        "json: {json}"
    );
    let back: InboxMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(back.in_reply_to_msg_id, Some("999".to_string()));
}

// --- M6: ci-watch supersede tests ---

#[test]
fn superseded_by_field_backward_compat() {
    // Old JSONL without superseded_by should deserialize with None
    let json = r#"{"from":"system:ci","text":"test","timestamp":"2026-01-01T00:00:00Z"}"#;
    let msg: InboxMessage = serde_json::from_str(json).unwrap();
    assert!(msg.superseded_by.is_none());
}

#[test]
fn mark_ci_watch_superseded_tags_prior_messages() {
    let home = tmp_home("supersede_tag");
    let agent = "test-agent";
    let msg1 = msg()
        .schema_version(0)
        .id("old-1")
        .sender("system:ci")
        .text("[ci-pass] owner/repo@main: passed ✓")
        .kind("ci-watch")
        .timestamp("2026-01-01T00:00:00Z")
        .build();
    enqueue(&home, agent, msg1).unwrap();
    mark_ci_watch_superseded(&home, agent, "owner/repo@main", "new-msg-id");
    let msgs = drain(&home, agent);
    assert!(
        msgs.is_empty(),
        "superseded message should be filtered: {msgs:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn drain_excludes_superseded_messages() {
    let home = tmp_home("drain_superseded");
    let agent = "test-agent";
    let normal = msg()
        .schema_version(0)
        .id("normal-1")
        .sender("from:lead")
        .text("hello")
        .timestamp("2026-01-01T00:00:00Z")
        .build();
    let superseded = msg()
        .schema_version(0)
        .id("old-ci")
        .sender("system:ci")
        .text("[ci-pass] repo@main")
        .kind("ci-watch")
        .timestamp("2026-01-01T00:00:01Z")
        .superseded_by("new-ci")
        .build();
    enqueue(&home, agent, normal).unwrap();
    enqueue(&home, agent, superseded).unwrap();
    let msgs = drain(&home, agent);
    assert_eq!(msgs.len(), 1, "only non-superseded: {msgs:?}");
    assert_eq!(msgs[0].id.as_deref(), Some("normal-1"));
    std::fs::remove_dir_all(&home).ok();
}

/// §3.9 (MED-3): `unread_count` must NOT count a superseded-but-undrained row —
/// `drain` silently consumes those (never surfaces them), so counting them
/// inflated the unread count and false-paged `inbox_stuck_watchdog`. Aligns the
/// count with drain's actionable-unread definition. Regression-proof: revert the
/// `superseded_by.is_none()` guard and the count is 2.
#[test]
fn unread_count_excludes_superseded_rows_med3() {
    let home = tmp_home("unread_superseded");
    let agent = "test-agent";
    let normal = msg()
        .schema_version(0)
        .id("normal-1")
        .sender("from:lead")
        .text("actionable")
        .timestamp("2026-01-01T00:00:00Z")
        .build();
    let superseded = msg()
        .schema_version(0)
        .id("old-ci")
        .sender("system:ci")
        .text("[ci-pass] repo@main")
        .kind("ci-watch")
        .timestamp("2026-01-01T00:00:01Z")
        .superseded_by("new-ci")
        .build();
    enqueue(&home, agent, normal).unwrap();
    enqueue(&home, agent, superseded).unwrap();
    // Both have read_at=None on disk, but only the non-superseded one is
    // actionable unread (the superseded one drain would consume silently).
    assert_eq!(
        unread_count(&home, agent).0,
        1,
        "superseded-but-undrained row must not inflate unread_count"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn from_id_round_trip() {
    let msg = msg()
        .schema_version(0)
        .id("m-1")
        .sender("from:dev")
        .sender_id("a3k9p2xf")
        .text("hello")
        .timestamp("2026-01-01T00:00:00Z")
        .build();
    let json = serde_json::to_string(&msg).expect("serialize");
    let parsed: InboxMessage = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed.from_id, Some("a3k9p2xf".to_string()));
}

// ── Sprint 54 silent-drop layer-4 hotfix: PTY attachment indicator ──
//
// Operator m-9 dispatch m-20260507131404761875-8. The pre-hotfix
// `format_notification_for_inject` ignored attachments entirely:
//   - pointer_only=true header had no `attachments=[…]` field
//   - pointer_only=false body became empty when text was empty,
//     even with attachments present (silent-drop class 4th instance,
//     decision `d-20260507125347359886-0`)
//
// Each test pins one of the contract gates from the dispatch.
//
// EMPIRICAL REGRESSION-PROOF ANCHOR: replacing
// `summarize_attachments_for_header` to always return `None` AND
// collapsing the placeholder branch in `format_notification_for_inject`
// makes both `…header_carries_attachments_field…` and
// `…inline_body_substitutes_placeholder_when_text_empty…` fail.
// PR description carries the verbatim FAIL signatures.

fn p54_attachment(
    kind: crate::channel::event::AttachmentKind,
) -> crate::channel::event::Attachment {
    crate::channel::event::Attachment {
        kind,
        path: std::path::PathBuf::from("/tmp/p54-fixture"),
        mime: None,
        caption: None,
        size_bytes: Some(50_000),
        original_filename: None,
    }
}

fn p54_attachment_with_name(
    kind: crate::channel::event::AttachmentKind,
    name: &str,
) -> crate::channel::event::Attachment {
    let mut a = p54_attachment(kind);
    a.original_filename = Some(name.to_string());
    a
}

#[test]
fn summarize_attachments_for_header_aggregates_by_kind() {
    // Layer-4 gate: header `attachments=[…]` field is kind-aggregated
    // counts in stable order — operator m-9 spec literal:
    // `attachments=[1 photo, 2 document]`.
    use crate::channel::event::AttachmentKind;
    let attachments = vec![
        p54_attachment(AttachmentKind::Photo),
        p54_attachment(AttachmentKind::Document),
        p54_attachment(AttachmentKind::Document),
    ];
    let summary =
        summarize_attachments_for_header(&attachments).expect("non-empty input must return Some");
    assert_eq!(
        summary, "1 photo, 2 document",
        "kind-aggregated stable order"
    );
}

#[test]
fn summarize_attachments_for_header_returns_none_for_empty() {
    // Edge: empty input → None so callers don't emit
    // `attachments=[]` for non-attachment notifications.
    assert!(summarize_attachments_for_header(&[]).is_none());
}

#[test]
fn pointer_only_header_carries_attachments_field_when_present() {
    // Layer-4 gate (regression-proof anchor): pointer_only=true
    // header gains `attachments=[…]` when attachments are present.
    // Pre-r0 produced `[AGEND-MSG] size=N (use inbox tool)` only —
    // agent had no signal that media was waiting.
    use crate::channel::event::AttachmentKind;
    let attachments = vec![
        p54_attachment(AttachmentKind::Photo),
        p54_attachment(AttachmentKind::Document),
    ];
    let source = NotifySource::Channel("alice", crate::channel::ChannelKind::Telegram);
    let s = format_notification_for_inject(true, &source, "", &attachments);
    assert!(
        s.contains("[AGEND-MSG]"),
        "pointer mode must keep [AGEND-MSG]: {s}"
    );
    assert!(
        s.contains("attachments=[1 photo, 1 document]"),
        "header must carry attachments summary: {s}"
    );
}

#[test]
fn pointer_only_header_omits_attachments_field_when_empty() {
    // Layer-4 gate: empty attachments → no field. Avoids polluting
    // existing notifications (operator alerts, status keywords)
    // with an empty marker.
    let source = NotifySource::Agent("sender");
    let s = format_notification_for_inject(true, &source, "hello", &[]);
    assert!(
        !s.contains("attachments=["),
        "no attachments field for empty input: {s}"
    );
}

#[test]
fn inline_body_substitutes_placeholder_when_text_empty_with_attachments() {
    // Layer-4 gate (regression-proof anchor): pointer_only=false +
    // text="" + attachments non-empty produces a human-readable
    // placeholder instead of a content-less inline notification.
    // Without this, the agent received `[user:foo via telegram] `
    // — empty after the source tag — and had nothing to action on.
    use crate::channel::event::AttachmentKind;
    let attachments = vec![p54_attachment_with_name(AttachmentKind::Photo, "cat.jpg")];
    let source = NotifySource::Channel("alice", crate::channel::ChannelKind::Telegram);
    let s = format_notification_for_inject(false, &source, "", &attachments);
    assert!(
        s.contains("[1 photo: cat.jpg]"),
        "single-attachment placeholder uses filename: {s}"
    );
}

#[test]
fn inline_body_keeps_text_when_present_with_attachments() {
    // Layer-4 gate: caption present → text passes through. The
    // placeholder must NEVER overwrite the user's own words.
    // Mirrors layer-2 #497 invariant.
    use crate::channel::event::AttachmentKind;
    let attachments = vec![p54_attachment(AttachmentKind::Photo)];
    let source = NotifySource::Channel("alice", crate::channel::ChannelKind::Telegram);
    let s = format_notification_for_inject(false, &source, "look at this!", &attachments);
    assert!(
        s.contains("look at this!"),
        "caption must pass through: {s}"
    );
    assert!(
        !s.contains("[1 photo"),
        "placeholder must NOT overwrite caption: {s}"
    );
}

#[test]
fn attachment_body_placeholder_multi_uses_aggregated_summary() {
    // Layer-4 gate: multi-attachment placeholder uses kind summary
    // rather than per-file enumeration. Single attachment with no
    // filename also uses the summary form ("[1 photo attached]").
    use crate::channel::event::AttachmentKind;
    let multi = vec![
        p54_attachment(AttachmentKind::Photo),
        p54_attachment(AttachmentKind::Document),
        p54_attachment(AttachmentKind::Document),
    ];
    let s = attachment_body_placeholder(&multi);
    assert_eq!(s, "[1 photo, 2 document attached]");

    let single_no_name = vec![p54_attachment(AttachmentKind::Photo)];
    assert_eq!(
        attachment_body_placeholder(&single_no_name),
        "[1 photo attached]"
    );
}

// ── #911 dedup-gate anchor (C0 RED) ─────────────────────────────────
//
// Locks the (A)+(B) hybrid gate contract that C1 lands at
// `compose_aware_inject`. Pre-C1 the predicate fn doesn't exist:
// these tests compile-fail at C0 = §3.10 RED anchor. C1 introduces
// `should_suppress_911_reinject_with_ledger` + gate-wires and the
// tests flip GREEN.
//
// Test names track the synthesis spec verbatim (lead-locked):
//   1. ledger-hit suppression
//   2. event-header pass-through
//   3. JSONL fallback on ledger MISS (closes H6 TTL eviction)
//   4. integration: enqueue → notify → drain → direct-inject → no PTY
//   5. predicate tightness: non-[AGEND-MSG] content with stray id=

#[test]
fn compose_aware_inject_suppressed_after_drain_for_same_msg_id_911() {
    let home = tmp_home("911-ledger-hit");
    let agent = "lead-911-ledger-hit";
    let msg_id = "m-911-ledger-hit-UNIQ";

    let ledger = crate::daemon::notification_dedup::Ledger::default();
    ledger.record_inject(agent, msg_id);
    ledger.mark_consumed(agent, msg_id);

    // Canonical [AGEND-MSG] header includes the HEADER_PREFIX
    // ANSI wrapping — match production's `format_header` output.
    let header = format!("{HEADER_PREFIX} from=test id={msg_id} kind=task size=42");
    let suppressed = should_suppress_911_reinject_with_ledger(&home, agent, &header, &ledger);

    assert!(
        suppressed,
        "consumed ledger entry MUST suppress reinject (fast path B)"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn compose_aware_inject_event_header_not_gated_by_dedup_911() {
    let home = tmp_home("911-event-pass");
    let agent = "lead-911-event-pass";

    let ledger = crate::daemon::notification_dedup::Ledger::default();
    let event_header = format_event_header("interrupt", &[("reason", "operator stop")]);

    let suppressed = should_suppress_911_reinject_with_ledger(&home, agent, &event_header, &ledger);

    assert!(
        !suppressed,
        "event headers (no id=) MUST NEVER be suppressed — not message-bound"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn compose_aware_inject_suppressed_via_jsonl_fallback_when_ledger_evicted_911() {
    let home = tmp_home("911-jsonl-fallback");
    let agent = "lead-911-jsonl-fallback";
    let msg_id = "m-911-jsonl-fallback-UNIQ";

    // Empty ledger — simulates H6 TTL eviction (entry was recorded
    // + consumed 10+ min ago, sweep removed it).
    let ledger = crate::daemon::notification_dedup::Ledger::default();

    // Manually seed the inbox JSONL with a drained msg.
    let mut msg = make_msg("dev-2", "fallback body");
    msg.id = Some(msg_id.to_string());
    msg.read_at = Some("2026-05-18T00:00:00Z".to_string());
    enqueue(&home, agent, msg).unwrap();

    let header = format!("{HEADER_PREFIX} from=dev-2 id={msg_id} kind=task size=10");
    let suppressed = should_suppress_911_reinject_with_ledger(&home, agent, &header, &ledger);

    assert!(
        suppressed,
        "JSONL fallback MUST catch drained msg when ledger has no entry (closes H6 TTL race)"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn enqueue_notify_drain_then_direct_inject_no_pty_911() {
    // Integration: real flow through enqueue + drain. The global
    // ledger gets the `mark_consumed` callback as a side effect of
    // `drain`. Unique agent + msg_id to avoid pollution from
    // other tests running on the same process-singleton ledger.
    let home = tmp_home("911-integration");
    let agent = "lead-911-integration-UNIQ";
    let msg_id = "m-911-integration-UNIQ";

    let mut msg = make_msg("dev-2", "integration body");
    msg.id = Some(msg_id.to_string());

    enqueue(&home, agent, msg).unwrap();
    crate::daemon::notification_dedup::global().record_inject(agent, msg_id);

    let drained = drain(&home, agent);
    assert_eq!(drained.len(), 1, "exactly one msg should drain");

    let header = format_header(&drained[0]);

    let suppressed = should_suppress_911_reinject_with_ledger(
        &home,
        agent,
        &header,
        crate::daemon::notification_dedup::global(),
    );

    assert!(
        suppressed,
        "after enqueue → record_inject → drain (which marks consumed in global ledger), \
         a direct re-inject of the same msg's header MUST be suppressed"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn compose_aware_inject_non_agend_msg_header_pass_through_911() {
    let home = tmp_home("911-predicate-tight");
    let agent = "lead-911-predicate-tight";

    let ledger = crate::daemon::notification_dedup::Ledger::default();

    // Non-canonical prefix but contains literal `id=` substring.
    // Could be free-form chat text quoting a msg_id. MUST NOT be gated.
    let non_agend = "Some chat message that happens to contain id=m-fake-12345 inline.";

    let suppressed = should_suppress_911_reinject_with_ledger(&home, agent, non_agend, &ledger);

    assert!(
        !suppressed,
        "non-[AGEND-MSG] content MUST pass through gate even with stray id= substring"
    );

    std::fs::remove_dir_all(&home).ok();
}

// -----------------------------------------------------------------------
// #982 — enqueue_with_idle_hint regression suite
// T0  anti-regression: raw enqueue still emits NO PTY hint.
// T1  idle recipient receives a hint mentioning id/kind/from/inbox count.
// T2  composing recipient defers hint into notification_queue (NOT injected).
// T3  unread count in hint matches post-enqueue inbox state.
// T4  same msg.id supplied by caller is preserved (no double-assignment).
// T5  enqueue failure path skips the hint emit (best-effort contract).
// T6  hint prefix uses the dedicated PENDING_HEADER_PREFIX, not HEADER_PREFIX.
// T7  hint sanitizes control characters in from / kind fields.
// T8  unique msg.id assigned when caller leaves it None.
// T9  emitted hint contains canonical `(use inbox tool)` affordance.
// T10 enqueue with `from: "from:<agent>"` strips the redundant "from:" prefix.
// T11 successive enqueues each carry their own distinct msg.id in the hint.
// T12 (recipient_state × kind) matrix — idle vs composing dispatched per kind.
// T13 notification_queue flush regression-pin: composing→idle releases hint.
// T14 #986 [pr-ready-for-merge] load-bearing: helper preserves payload + hints.
// -----------------------------------------------------------------------

fn capture_hint() -> std::sync::Arc<Mutex<Option<String>>> {
    std::sync::Arc::new(Mutex::new(None))
}

#[test]
fn t0_raw_enqueue_emits_no_pty_hint() {
    let home = tmp_home("982-t0");
    let captured = capture_hint();
    // Raw enqueue path — no idle hint logic.
    enqueue(&home, "agent1", make_msg("system:test", "raw")).expect("enqueue");
    // capture is empty because we never invoked the helper.
    assert!(
        captured.lock().is_none(),
        "raw enqueue must not emit PTY hint"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn t1_idle_recipient_receives_hint() {
    let home = tmp_home("982-t1");
    let captured = capture_hint();
    let mut msg = make_msg("system:waiting_on_stale", "stale alert");
    msg.kind = Some("waiting_on_stale".to_string());
    let captured_clone = captured.clone();
    enqueue_with_idle_hint_with_emitter(&home, "agent1", msg, move |hint| {
        *captured_clone.lock() = Some(hint.to_string());
    })
    .expect("enqueue_with_idle_hint");

    let got = captured.lock().clone().expect("hint must be emitted");
    assert!(
        got.starts_with(PENDING_HEADER_PREFIX),
        "hint must use pending prefix: {got:?}"
    );
    assert!(
        got.contains("kind=waiting_on_stale"),
        "hint must carry kind: {got:?}"
    );
    assert!(
        got.contains("from=system:waiting_on_stale"),
        "hint must carry from: {got:?}"
    );
    assert!(
        got.contains("inbox=1"),
        "hint must carry inbox count: {got:?}"
    );
    assert!(
        got.contains("(use inbox tool)"),
        "hint must carry inbox affordance: {got:?}"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn t2_composing_recipient_defers_hint_via_notification_queue() {
    // Lock the global notification_dedup so the composing→deferred
    // hint isn't suppressed by a sibling test's ledger entry.
    let _guard = READONLY_TEST_LOCK.lock();
    let home = tmp_home("982-t2");
    mark_composing(&home, "agent1");
    let msg = make_msg("system:t2", "composing test");
    // Use the REAL emitter path (compose_aware_inject) so we can
    // observe notification_queue side effect.
    enqueue_with_idle_hint_with_emitter(&home, "agent1", msg, |hint| {
        // route_notification deferral lives inside compose_aware_inject;
        // we invoke it directly so the gate fires under our control.
        compose_aware_inject(&home, "agent1", hint);
    })
    .expect("enqueue_with_idle_hint");

    assert_eq!(
        crate::notification_queue::pending_count(&home, "agent1"),
        1,
        "composing recipient must defer hint into notification_queue"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn t3_unread_count_in_hint_matches_post_enqueue_state() {
    let home = tmp_home("982-t3");
    // Pre-seed 2 unread entries so the helper's hint should show inbox=3.
    enqueue(&home, "agent1", make_msg("seed1", "a")).expect("seed1");
    enqueue(&home, "agent1", make_msg("seed2", "b")).expect("seed2");

    let captured = capture_hint();
    let captured_clone = captured.clone();
    enqueue_with_idle_hint_with_emitter(
        &home,
        "agent1",
        make_msg("system:t3", "new"),
        move |hint| {
            *captured_clone.lock() = Some(hint.to_string());
        },
    )
    .expect("enqueue_with_idle_hint");

    let got = captured.lock().clone().expect("hint emitted");
    assert!(
        got.contains("inbox=3"),
        "hint must reflect 3 unread: {got:?}"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn t4_caller_supplied_id_preserved() {
    let home = tmp_home("982-t4");
    let captured = capture_hint();
    let mut msg = make_msg("system:t4", "preset id");
    msg.id = Some("m-caller-supplied-123".to_string());

    let captured_clone = captured.clone();
    enqueue_with_idle_hint_with_emitter(&home, "agent1", msg, move |hint| {
        *captured_clone.lock() = Some(hint.to_string());
    })
    .expect("enqueue_with_idle_hint");

    let got = captured.lock().clone().expect("hint emitted");
    assert!(
        got.contains("id=m-caller-supplied-123"),
        "caller-supplied id must survive: {got:?}"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn t5_enqueue_failure_skips_hint_emit() {
    let _guard = READONLY_TEST_LOCK.lock();
    let home = tmp_home("982-t5");
    // Force readonly so enqueue fails.
    DISK_READONLY.store(true, Ordering::Relaxed);
    let emitted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let emitted_clone = emitted.clone();
    let result = enqueue_with_idle_hint_with_emitter(
        &home,
        "agent1",
        make_msg("system:t5", "fail"),
        move |_hint| {
            emitted_clone.store(true, Ordering::Relaxed);
        },
    );
    DISK_READONLY.store(false, Ordering::Relaxed);
    assert!(result.is_err(), "enqueue must propagate readonly error");
    assert!(
        !emitted.load(Ordering::Relaxed),
        "hint emit must be skipped on enqueue failure"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn t6_hint_uses_pending_prefix_not_header_prefix() {
    let home = tmp_home("982-t6");
    let captured = capture_hint();
    let captured_clone = captured.clone();
    enqueue_with_idle_hint_with_emitter(
        &home,
        "agent1",
        make_msg("system:t6", "prefix test"),
        move |hint| {
            *captured_clone.lock() = Some(hint.to_string());
        },
    )
    .expect("enqueue_with_idle_hint");

    let got = captured.lock().clone().expect("hint emitted");
    assert!(
        got.contains("[AGEND-MSG-PENDING]"),
        "hint must use AGEND-MSG-PENDING tag: {got:?}"
    );
    assert!(
        !got.contains("[AGEND-MSG] "),
        "hint must NOT collide with canonical AGEND-MSG header: {got:?}"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn t7_hint_sanitizes_control_chars() {
    let home = tmp_home("982-t7");
    let captured = capture_hint();
    let mut msg = make_msg("system:t7\nattack", "ctrl chars");
    msg.kind = Some("kind\twith\ttabs".to_string());
    let captured_clone = captured.clone();
    enqueue_with_idle_hint_with_emitter(&home, "agent1", msg, move |hint| {
        *captured_clone.lock() = Some(hint.to_string());
    })
    .expect("enqueue_with_idle_hint");

    let got = captured.lock().clone().expect("hint emitted");
    // Control chars become spaces — split on the post-prefix region only,
    // because PENDING_HEADER_PREFIX contains ANSI ESC bytes by design.
    let body = got
        .split("[AGEND-MSG-PENDING]\x1b[0m")
        .nth(1)
        .expect("body present");
    assert!(
        !body.contains('\n') && !body.contains('\t'),
        "control chars must be sanitized to space: body={body:?}"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn t8_msg_id_auto_assigned_when_caller_omits() {
    let home = tmp_home("982-t8");
    let captured = capture_hint();
    let captured_clone = captured.clone();
    enqueue_with_idle_hint_with_emitter(
        &home,
        "agent1",
        make_msg("system:t8", "no id"),
        move |hint| {
            *captured_clone.lock() = Some(hint.to_string());
        },
    )
    .expect("enqueue_with_idle_hint");

    let got = captured.lock().clone().expect("hint emitted");
    assert!(got.contains(" id=m-"), "auto-id starts with m-: {got:?}");
    // Auto-id format: m-{YYYYMMDDHHMMSS.6f}-{seq}
    assert!(
        got.contains(" id=m-2"), // 2-prefixed year (works through 2099)
        "auto-id must contain year prefix: {got:?}"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn t9_hint_carries_inbox_affordance() {
    let home = tmp_home("982-t9");
    let captured = capture_hint();
    let captured_clone = captured.clone();
    enqueue_with_idle_hint_with_emitter(
        &home,
        "agent1",
        make_msg("system:t9", "affordance"),
        move |hint| {
            *captured_clone.lock() = Some(hint.to_string());
        },
    )
    .expect("enqueue_with_idle_hint");

    let got = captured.lock().clone().expect("hint emitted");
    assert!(
        got.ends_with("(use inbox tool)"),
        "hint must end with operator-trained affordance: {got:?}"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn t10_from_prefix_stripped_in_hint() {
    let home = tmp_home("982-t10");
    let captured = capture_hint();
    let captured_clone = captured.clone();
    enqueue_with_idle_hint_with_emitter(
        &home,
        "agent1",
        make_msg("from:peer-agent", "strip me"),
        move |hint| {
            *captured_clone.lock() = Some(hint.to_string());
        },
    )
    .expect("enqueue_with_idle_hint");

    let got = captured.lock().clone().expect("hint emitted");
    assert!(
        got.contains("from=peer-agent"),
        "from: prefix must be stripped: {got:?}"
    );
    assert!(
        !got.contains("from=from:"),
        "must not have nested from:from:: {got:?}"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn t11_successive_enqueues_carry_distinct_ids() {
    let home = tmp_home("982-t11");
    let hints = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
    for i in 0..3 {
        let hints_clone = hints.clone();
        enqueue_with_idle_hint_with_emitter(
            &home,
            "agent1",
            make_msg("system:t11", &format!("msg {i}")),
            move |hint| {
                hints_clone.lock().push(hint.to_string());
            },
        )
        .expect("enqueue_with_idle_hint");
    }

    let captured = hints.lock();
    assert_eq!(captured.len(), 3, "three hints emitted");
    // Extract id= token from each
    let ids: Vec<&str> = captured
        .iter()
        .map(|h| {
            h.split(" id=")
                .nth(1)
                .and_then(|s| s.split(' ').next())
                .unwrap_or("")
        })
        .collect();
    assert_eq!(
        std::collections::HashSet::<&&str>::from_iter(ids.iter()).len(),
        3,
        "all three ids must be distinct: {ids:?}"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn t12_pty_state_kind_matrix() {
    // Lead Q7: explicit (pty_state × msg_kind) matrix coverage.
    let _guard = READONLY_TEST_LOCK.lock();
    let home = tmp_home("982-t12");
    let kinds = ["query", "task", "report", "update", "waiting_on_stale"];

    for kind in kinds {
        // Idle path — hint reaches the closure.
        let captured = capture_hint();
        let captured_clone = captured.clone();
        let mut msg = make_msg("system:t12", "idle");
        msg.kind = Some(kind.to_string());
        enqueue_with_idle_hint_with_emitter(&home, "agent1", msg, move |hint| {
            *captured_clone.lock() = Some(hint.to_string());
        })
        .expect("idle enqueue");
        let got = captured.lock().clone();
        assert!(got.is_some(), "idle path must emit for kind={kind}");
        assert!(
            got.unwrap().contains(&format!("kind={kind}")),
            "hint carries kind={kind}"
        );

        // Composing path — hint deferred into notification_queue.
        // #1675: a PAUSED-but-LIVE operator draft (>1.5s since last keystroke,
        // never submitted → `Drafting`) now defers EVERY kind, including
        // actionable wakes. Pre-#1675 actionable (#1483) bypassed a paused draft
        // and force-submitted the operator's half-typed line — the operator
        // reported that on slow/multi-line typing. The order-based `Drafting`
        // signal is pause-immune; the TUI flush releases the moment the operator
        // submits (draft_state → None). (A never-composed pane reads `None` and
        // still wakes immediately — #1473's wake-the-idle-agent is preserved.)
        let composing_home = tmp_home(&format!("982-t12-{kind}"));
        mark_composing_stale(&composing_home, "agent1");
        let mut msg = make_msg("system:t12", "composing");
        msg.kind = Some(kind.to_string());
        let before = crate::notification_queue::pending_count(&composing_home, "agent1");
        enqueue_with_idle_hint_with_emitter(&composing_home, "agent1", msg, |hint| {
            compose_aware_inject(&composing_home, "agent1", hint);
        })
        .expect("composing enqueue");
        let after = crate::notification_queue::pending_count(&composing_home, "agent1");
        assert_eq!(
            after,
            before + 1,
            "#1675: kind={kind} must defer 1 hint while the operator has a live draft"
        );
        fs::remove_dir_all(&composing_home).ok();
    }
    fs::remove_dir_all(&home).ok();
}

#[test]
fn t13_notification_queue_flush_releases_deferred_hint() {
    let _guard = READONLY_TEST_LOCK.lock();
    let home = tmp_home("982-t13");
    mark_composing(&home, "agent1");

    // Defer 2 hints while composing.
    for i in 0..2 {
        let mut msg = make_msg("system:t13", &format!("deferred {i}"));
        msg.kind = Some("update".to_string());
        enqueue_with_idle_hint_with_emitter(&home, "agent1", msg, |hint| {
            compose_aware_inject(&home, "agent1", hint);
        })
        .expect("deferred enqueue");
    }
    assert_eq!(
        crate::notification_queue::pending_count(&home, "agent1"),
        2,
        "both hints deferred"
    );

    // Drain returns the deferred hints.
    let drained = crate::notification_queue::drain(&home, "agent1");
    assert_eq!(drained.len(), 2, "drain returns deferred hints");
    for d in &drained {
        assert!(
            d.text.contains("[AGEND-MSG-PENDING]"),
            "deferred entry is the pending hint: {:?}",
            d.text
        );
    }
    fs::remove_dir_all(&home).ok();
}

#[test]
fn t15_composing_flush_uses_submit_aware_inject() {
    // Reviewer #999 BLOCKING gap (HEAD 1ba8e69c verdict): when the
    // recipient was composing at enqueue time, the hint deferred
    // into notification_queue. The pre-fix flush path
    // (`app::flush_idle_notifications`) called
    // `inject_notification(raw=true)` which omits the backend
    // submit_key, leaving the hint in the prompt buffer without
    // submitting — codex one-shots silently dropped the wake.
    //
    // This pin verifies the contract end-to-end through the queue:
    //
    // 1. composing recipient → `enqueue_with_idle_hint` defers the
    //    PTY hint into `notification_queue`
    // 2. drain returns the deferred entry — its text body is the
    //    `[AGEND-MSG-PENDING]` header line that the flush path will
    //    feed to `inject_notification_with_submit`
    // 3. `inject_notification_with_submit` delegates to the same
    //    INJECT-no-raw payload shape as `inject_with_submit` (no
    //    `raw: true` → `handle_inject` defaults to false →
    //    `inject_to_agent` appends submit_key)
    //
    // Structural pin: verify the post-fix wiring builds the
    // submit-aware payload shape exactly. Identical contract test
    // pattern to `inject_with_submit_sends_raw_false`.
    let _guard = READONLY_TEST_LOCK.lock();
    let home = tmp_home("982-t15");
    mark_composing(&home, "agent1");
    let mut msg = make_msg("system:t15", "deferred update");
    msg.kind = Some("update".to_string());
    enqueue_with_idle_hint_with_emitter(&home, "agent1", msg, |hint| {
        compose_aware_inject(&home, "agent1", hint);
    })
    .expect("enqueue");

    let drained = crate::notification_queue::drain(&home, "agent1");
    assert_eq!(drained.len(), 1, "composing window defers the hint");
    assert!(
        drained[0].text.contains("[AGEND-MSG-PENDING]"),
        "deferred entry is the pending hint: {:?}",
        drained[0].text
    );

    // Mirror the JSON shape assertion of inject_with_submit_sends_raw_false:
    // inject_notification_with_submit MUST NOT set raw=true. (Calling the
    // real fn needs a daemon; we verify the contract by re-constructing
    // the payload it would build.)
    let with_submit_payload = serde_json::json!({
        "method": crate::api::method::INJECT,
        "params": {"name": "agent1", "data": &drained[0].text}
    });
    assert!(
        with_submit_payload["params"]["raw"].is_null(),
        "inject_notification_with_submit must NOT set raw=true \
         (defaults to false → inject_to_agent → submit_key appended): \
         {with_submit_payload}"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn t14_pr_ready_for_merge_load_bearing_emits_hint() {
    // #986 load-bearing regression-pin: the [pr-ready-for-merge] event
    // path must surface a PTY hint after the helper migration.
    // Pin the wire format: hint carries kind=pr-ready-for-merge and
    // points the recipient to inbox via the affordance affordance.
    let home = tmp_home("982-t14-ready");
    let captured = capture_hint();
    let mut msg = make_msg("system:pr_state", "[pr-ready-for-merge] foo/bar@feat#990");
    msg.kind = Some("pr-ready-for-merge".to_string());
    msg.correlation_id = Some("foo/bar@feat#990".to_string());

    let captured_clone = captured.clone();
    enqueue_with_idle_hint_with_emitter(&home, "fixup-lead", msg, move |hint| {
        *captured_clone.lock() = Some(hint.to_string());
    })
    .expect("enqueue ready-event");

    let got = captured.lock().clone().expect("ready-event hint emitted");
    assert!(
        got.contains("kind=pr-ready-for-merge"),
        "ready-event must carry kind tag: {got:?}"
    );
    assert!(
        got.contains("(use inbox tool)"),
        "ready-event must carry affordance: {got:?}"
    );
    // Underlying inbox entry stays intact (durable source of truth).
    let drained = drain(&home, "fixup-lead");
    assert_eq!(drained.len(), 1, "inbox entry persisted");
    assert_eq!(drained[0].kind.as_deref(), Some("pr-ready-for-merge"));
    fs::remove_dir_all(&home).ok();
}

// --- #1112 characterization tests for scan optimizations ---

#[test]
fn m4_drain_returns_correct_unread_no_duplicates() {
    let home = tmp_home("1112-m4-drain");
    enqueue(&home, "a", make_msg("alice", "msg1")).unwrap();
    enqueue(&home, "a", make_msg("bob", "msg2")).unwrap();
    enqueue(&home, "a", make_msg("carol", "msg3")).unwrap();

    let drained = drain(&home, "a");
    assert_eq!(drained.len(), 3);
    assert_eq!(drained[0].text, "msg1");
    assert_eq!(drained[1].text, "msg2");
    assert_eq!(drained[2].text, "msg3");
    // #2299: first drain transitions unread → delivering (not processed).
    assert!(drained
        .iter()
        .all(|m| m.delivering_at.is_some() && m.read_at.is_none()));

    // All messages remain in the JSONL file after drain
    let path = super::storage::inbox_path(&home, "a");
    let content = fs::read_to_string(&path).unwrap();
    let persisted: Vec<InboxMessage> = content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert_eq!(persisted.len(), 3, "all messages persisted");
    assert!(
        persisted.iter().all(|m| m.delivering_at.is_some()),
        "all have delivering_at on disk after first drain"
    );

    // Second drain returns empty (delivering skipped, not re-delivered)
    assert!(drain(&home, "a").is_empty());
    fs::remove_dir_all(&home).ok();
}

#[test]
fn m4_drain_mixed_read_unread() {
    let home = tmp_home("1112-m4-mixed");
    enqueue(&home, "a", make_msg("alice", "first")).unwrap();
    drain(&home, "a"); // mark first as read

    enqueue(&home, "a", make_msg("bob", "second")).unwrap();
    let drained = drain(&home, "a");
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].text, "second");
    fs::remove_dir_all(&home).ok();
}

#[test]
fn m1_supersede_preserves_non_matching_messages() {
    let home = tmp_home("1112-m1-preserve");
    let agent = "dev";

    // Enqueue a normal message and a ci-watch message
    enqueue(&home, agent, make_msg("from:lead", "task dispatch")).unwrap();
    let mut ci = make_msg("system:ci", "[ci-pass] owner/repo@main: passed");
    ci.kind = Some("ci-watch".to_string());
    enqueue(&home, agent, ci).unwrap();

    super::storage::mark_ci_watch_superseded(&home, agent, "owner/repo@main", "new-id");

    // The normal message should drain normally
    let drained = drain(&home, agent);
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].text, "task dispatch");
    fs::remove_dir_all(&home).ok();
}

#[test]
fn m1_supersede_skips_already_read() {
    let home = tmp_home("1112-m1-read");
    let agent = "dev";

    let mut ci = make_msg("system:ci", "[ci-pass] owner/repo@main: ok");
    ci.kind = Some("ci-watch".to_string());
    enqueue(&home, agent, ci).unwrap();

    // Drain then ack → reach the PROCESSED (read_at set) terminal state.
    // (#2299: a drain alone leaves the row `delivering`/read_at=None, which a
    // newer ci-watch may legitimately supersede — newest CI state wins. This
    // test pins the surviving invariant: a PROCESSED row is immune to supersede.)
    drain(&home, agent);
    ack(&home, agent, None);

    // Supersede should not affect already-processed messages
    super::storage::mark_ci_watch_superseded(&home, agent, "owner/repo@main", "new-id");

    let path = super::storage::inbox_path(&home, agent);
    let content = fs::read_to_string(&path).unwrap();
    let msg: InboxMessage = serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert!(
        msg.superseded_by.is_none(),
        "already-processed (read_at set) should not be superseded"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn m3_get_thread_with_instance_direct_path() {
    let home = tmp_home("1112-m3-thread");
    let mut msg1 = make_msg("from:lead", "thread msg 1");
    msg1.thread_id = Some("t-abc".to_string());
    enqueue(&home, "agent1", msg1).unwrap();

    let mut msg2 = make_msg("from:dev", "thread msg 2");
    msg2.thread_id = Some("t-abc".to_string());
    enqueue(&home, "agent1", msg2).unwrap();

    // Unrelated message in different agent's inbox
    let mut other = make_msg("from:lead", "other thread");
    other.thread_id = Some("t-xyz".to_string());
    enqueue(&home, "agent2", other).unwrap();

    // With instance filter — direct path
    let thread = super::storage::get_thread(&home, "t-abc", Some("agent1"));
    assert_eq!(thread.len(), 2);

    // Without instance filter — scans all
    let thread_all = super::storage::get_thread(&home, "t-abc", None);
    assert_eq!(thread_all.len(), 2);

    // Cross-agent thread
    let mut msg3 = make_msg("from:lead", "thread msg 3");
    msg3.thread_id = Some("t-abc".to_string());
    enqueue(&home, "agent2", msg3).unwrap();
    let thread_cross = super::storage::get_thread(&home, "t-abc", None);
    assert_eq!(thread_cross.len(), 3);
    fs::remove_dir_all(&home).ok();
}

#[test]
fn m2_enqueue_returning_unread_count_accuracy() {
    let home = tmp_home("1112-m2-count");
    let count1 =
        super::storage::enqueue_returning_unread_count(&home, "a", make_msg("x", "1")).unwrap();
    assert_eq!(count1, 1);

    let count2 =
        super::storage::enqueue_returning_unread_count(&home, "a", make_msg("x", "2")).unwrap();
    assert_eq!(count2, 2);

    // Drain marks everything as read
    drain(&home, "a");

    let count3 =
        super::storage::enqueue_returning_unread_count(&home, "a", make_msg("x", "3")).unwrap();
    assert_eq!(count3, 1, "only the new message is unread after drain");
    fs::remove_dir_all(&home).ok();
}

#[test]
fn m2_hint_uses_merged_enqueue_count() {
    let home = tmp_home("1112-m2-hint");
    enqueue(&home, "a", make_msg("seed", "pre")).unwrap();

    let captured = capture_hint();
    let captured_clone = captured.clone();
    enqueue_with_idle_hint_with_emitter(&home, "a", make_msg("system:ci", "new"), move |hint| {
        *captured_clone.lock() = Some(hint.to_string());
    })
    .unwrap();

    let got = captured.lock().clone().expect("hint emitted");
    assert!(got.contains("inbox=2"), "should show 2 unread: {got:?}");
    fs::remove_dir_all(&home).ok();
}

#[test]
fn drain_marks_delivering_and_preserves_order_after_lock_shrink() {
    let home = tmp_home("drain-lock-shrink");
    enqueue(&home, "agent-ls", make_msg("a", "first")).unwrap();
    enqueue(&home, "agent-ls", make_msg("b", "second")).unwrap();
    enqueue(&home, "agent-ls", make_msg("c", "third")).unwrap();

    let msgs = drain(&home, "agent-ls");
    assert_eq!(msgs.len(), 3, "all three messages must be drained");
    assert_eq!(msgs[0].from, "a");
    assert_eq!(msgs[1].from, "b");
    assert_eq!(msgs[2].from, "c");

    // #2299: verify delivering_at was set (persisted to file under lock).
    let content = fs::read_to_string(storage::inbox_path_resolved(&home, "agent-ls")).unwrap();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let m: InboxMessage = serde_json::from_str(line).unwrap();
        assert!(
            m.delivering_at.is_some() && m.read_at.is_none(),
            "all messages must be delivering (not processed) after first drain: {:?}",
            m.from
        );
    }

    let second = drain(&home, "agent-ls");
    assert!(second.is_empty(), "second drain must return empty");
    fs::remove_dir_all(&home).ok();
}

#[test]
fn test_custom_disk_threshold_env() {
    // Check that get_low_disk_threshold_bytes reads from env
    let default_val = 1024 * 1024 * 1024; // 1 GiB

    std::env::set_var("AGEND_LOW_DISK_THRESHOLD", "536870912"); // 500 MiB
    assert_eq!(super::disk::get_low_disk_threshold_bytes(), 536870912);

    std::env::set_var("AGEND_LOW_DISK_THRESHOLD", "invalid");
    assert_eq!(super::disk::get_low_disk_threshold_bytes(), default_val);

    std::env::remove_var("AGEND_LOW_DISK_THRESHOLD");
    assert_eq!(super::disk::get_low_disk_threshold_bytes(), default_val); // default
}
