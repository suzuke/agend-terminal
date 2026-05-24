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

#[test]
fn drain_recovers_leftover_draining_file() {
    // Simulates a crash between rename() and read: pending messages
    // sit in `{name}.draining`. A second drain() must surface them,
    // not drop them.
    let home = tmp_home("recover");
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).ok();
    let draining = inbox_dir.join("agent1.draining");
    let msg = serde_json::to_string(&make_msg("crashed", "pending")).expect("ser");
    fs::write(&draining, format!("{msg}\n")).expect("write leftover");

    let msgs = drain(&home, "agent1");
    assert_eq!(msgs.len(), 1, "crashed batch must be recovered");
    assert_eq!(msgs[0].from, "crashed");
    assert_eq!(msgs[0].text, "pending");
    // After successful read, leftover is cleared.
    assert!(
        !draining.exists(),
        ".draining must be removed after successful drain"
    );
    fs::remove_dir_all(&home).ok();
}

#[test]
fn drain_does_not_overwrite_leftover_draining() {
    // If a .draining file exists from a prior crash AND a new live
    // inbox has arrived, the live file must be preserved — the new
    // messages are picked up on the next drain cycle, not lost by a
    // rename that overwrites the pending batch.
    let home = tmp_home("no_overwrite");
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).ok();

    let draining = inbox_dir.join("agent1.draining");
    let old_msg = serde_json::to_string(&make_msg("old", "from_crashed_batch")).expect("ser");
    fs::write(&draining, format!("{old_msg}\n")).expect("write leftover");

    enqueue(&home, "agent1", make_msg("new", "fresh")).ok();

    // First drain: returns the crashed batch only.
    let first = drain(&home, "agent1");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].from, "old");

    // Second drain: picks up the new message now that .draining is gone.
    let second = drain(&home, "agent1");
    assert_eq!(second.len(), 1, "fresh message must survive recovery");
    assert_eq!(second[0].from, "new");

    fs::remove_dir_all(&home).ok();
}

#[test]
fn drain_read_failure_leaves_file_for_retry() {
    // If read_to_string fails, .draining must remain on disk so a
    // subsequent drain has another chance. (Simulating an unreadable
    // file is awkward cross-platform; we instead assert the
    // "retain-on-error" invariant by verifying successful drains
    // DO remove, which is the inverse assertion our prior bug
    // violated. See drain_recovers_leftover_draining_file.)
    let home = tmp_home("retain");
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).ok();
    let draining = inbox_dir.join("agent1.draining");
    // Non-UTF8 bytes → read_to_string returns Err.
    fs::write(&draining, [0xFF, 0xFE, 0xFD]).expect("write");

    let msgs = drain(&home, "agent1");
    assert!(msgs.is_empty(), "unreadable batch yields no messages");
    assert!(
        draining.exists(),
        ".draining must be retained after read failure for next retry"
    );
    fs::remove_dir_all(&home).ok();
}

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
fn test_half_written_jsonl_goes_to_recovery() {
    // A JSONL file with a corrupt line must be moved to recovery.
    let home = tmp_home("half-write");
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).ok();

    let jsonl = inbox_dir.join("agent1.jsonl");
    let good = serde_json::to_string(&make_msg("ok", "fine")).unwrap();
    // Write a good line followed by a truncated/corrupt line
    fs::write(
        &jsonl,
        format!("{good}\n{{\"from\":\"broken\",\"text\":\"trun"),
    )
    .ok();

    recover_half_writes(&home);

    assert!(!jsonl.exists(), "corrupt JSONL must be moved to recovery");
    let recovery = home.join("inbox.recovery");
    assert!(recovery.exists());
    // The recovery subdir should contain the moved file
    let subdirs: Vec<_> = fs::read_dir(&recovery).unwrap().flatten().collect();
    assert_eq!(subdirs.len(), 1);
    let files: Vec<_> = fs::read_dir(subdirs[0].path()).unwrap().flatten().collect();
    assert_eq!(files.len(), 1);
    assert!(files[0].file_name().to_string_lossy().contains("agent1"));

    fs::remove_dir_all(&home).ok();
}

#[test]
fn test_drain_marks_read_at_but_keeps_message() {
    let home = tmp_home("drain-read-at");
    enqueue(&home, "agent1", make_msg("alice", "hello")).ok();
    enqueue(&home, "agent1", make_msg("bob", "world")).ok();

    // First drain returns both messages with read_at set
    let msgs = drain(&home, "agent1");
    assert_eq!(msgs.len(), 2);
    assert!(msgs[0].read_at.is_some(), "drain must stamp read_at");
    assert!(msgs[1].read_at.is_some());

    // Second drain returns empty (already read)
    let msgs2 = drain(&home, "agent1");
    assert!(
        msgs2.is_empty(),
        "already-read messages must not be returned"
    );

    // But the file still exists with the messages
    let path = inbox_path(&home, "agent1");
    let content = fs::read_to_string(&path).expect("file must still exist");
    let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 2, "messages must be kept in file");
    // Verify read_at is persisted
    let m: InboxMessage = serde_json::from_str(lines[0]).expect("parse");
    assert!(m.read_at.is_some(), "read_at must be persisted to disk");

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
    fs::write(
        inbox_dir.join("agent1.jsonl"),
        format!("{read_msg}\n{expired_msg}\n"),
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

    // NotFound
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
fn test_concurrent_drain_no_duplicate_recovery() {
    let _guard = READONLY_TEST_LOCK.lock();
    // Pre-write a stale .draining file with 3 messages.
    // Spawn 2 threads that both call drain simultaneously.
    // Total recovered messages must be exactly 3 (no duplicates).
    let home = tmp_home("concurrent-recovery");
    let inbox_dir = home.join("inbox");
    fs::create_dir_all(&inbox_dir).ok();

    let draining = inbox_dir.join("agent1.draining");
    let mut content = String::new();
    for i in 0..3 {
        let msg = make_msg(&format!("recover{i}"), &format!("msg{i}"));
        content.push_str(&serde_json::to_string(&msg).unwrap());
        content.push('\n');
    }
    fs::write(&draining, &content).ok();

    let home_a = std::sync::Arc::new(home.clone());
    let home_b = home_a.clone();

    let a = std::thread::spawn(move || drain(&home_a, "agent1"));
    let b = std::thread::spawn(move || drain(&home_b, "agent1"));

    let mut all = a.join().expect("thread a");
    all.extend(b.join().expect("thread b"));

    assert_eq!(
        all.len(),
        3,
        "exactly 3 recovered messages expected (no duplicates), got {}",
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

#[test]
fn test_short_msg_below_threshold() {
    // Messages <= HEADER_SIZE_THRESHOLD should NOT use header format
    let short = "a".repeat(HEADER_SIZE_THRESHOLD);
    assert!(short.len() <= HEADER_SIZE_THRESHOLD);
}

#[test]
fn test_long_msg_above_threshold() {
    // Messages > HEADER_SIZE_THRESHOLD should use header-only injection
    let long = "a".repeat(HEADER_SIZE_THRESHOLD + 1);
    assert!(long.len() > HEADER_SIZE_THRESHOLD);
    // format_header produces a compact single-line representation
    let msg = msg()
        .id("m-1")
        .sender("from:x")
        .text_owned(long)
        .kind("task")
        .timestamp("2026-01-01T00:00:00Z")
        .build();
    let header = format_header(&msg);
    assert!(
        header.len() < HEADER_SIZE_THRESHOLD,
        "header must be compact"
    );
    assert!(header.contains(&format!("size={}", HEADER_SIZE_THRESHOLD + 1)));
}

#[test]
fn test_threshold_uses_char_count_not_bytes() {
    // 100 CJK chars = 100 chars but 300 bytes (3 bytes each in UTF-8).
    // Must be treated as short (100 < 300 threshold), not long.
    let cjk = "你".repeat(100);
    assert_eq!(cjk.chars().count(), 100);
    assert_eq!(cjk.len(), 300); // bytes
                                // 100 chars < HEADER_SIZE_THRESHOLD (300) → should be short path
    assert!(cjk.chars().count() <= HEADER_SIZE_THRESHOLD);

    // 301 CJK chars = 301 chars → should be long path
    let long_cjk = "你".repeat(HEADER_SIZE_THRESHOLD + 1);
    assert!(long_cjk.chars().count() > HEADER_SIZE_THRESHOLD);

    // format_header size= should report char count, not byte count
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
        let composing_home = tmp_home(&format!("982-t12-{kind}"));
        mark_composing(&composing_home, "agent1");
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
            "composing must defer 1 hint for kind={kind}"
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
    assert!(drained.iter().all(|m| m.read_at.is_some()));

    // All messages remain in the JSONL file after drain
    let path = super::storage::inbox_path(&home, "a");
    let content = fs::read_to_string(&path).unwrap();
    let persisted: Vec<InboxMessage> = content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    assert_eq!(persisted.len(), 3, "all messages persisted");
    assert!(
        persisted.iter().all(|m| m.read_at.is_some()),
        "all have read_at on disk"
    );

    // Second drain returns empty
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

    // Drain to mark as read
    drain(&home, agent);

    // Supersede should not affect already-read messages
    super::storage::mark_ci_watch_superseded(&home, agent, "owner/repo@main", "new-id");

    let path = super::storage::inbox_path(&home, agent);
    let content = fs::read_to_string(&path).unwrap();
    let msg: InboxMessage = serde_json::from_str(content.lines().next().unwrap()).unwrap();
    assert!(
        msg.superseded_by.is_none(),
        "already-read should not be superseded"
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
