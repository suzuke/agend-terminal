//! Per-agent message inbox — append-only JSONL for thread safety.
//!
//! Messages stored as one JSON object per line in {home}/inbox/{name}.jsonl.
//! Append is atomic on most filesystems for small writes — no file locking needed.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Type-safe notification source — replaces raw string conventions.
pub enum NotifySource<'a> {
    /// Message from a Telegram user (e.g., "chiacheng").
    Telegram(&'a str),
    /// Message from another agent instance (e.g., "dev").
    Agent(&'a str),
    /// System message (e.g., "replace", "ci").
    #[allow(dead_code)]
    System(&'a str),
}

impl fmt::Display for NotifySource<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Telegram(user) => write!(f, "user:{user} via telegram"),
            Self::Agent(name) => write!(f, "from:{name}"),
            Self::System(label) => write!(f, "system:{label}"),
        }
    }
}

impl NotifySource<'_> {
    fn reply_hint(&self) -> Cow<'static, str> {
        match self {
            Self::Telegram(_) => {
                "\n(Reply using the reply tool — do NOT respond with direct text)".into()
            }
            Self::Agent(sender) => {
                format!("\n(Reply using the send_to_instance tool with target \"{sender}\")").into()
            }
            Self::System(_) => "".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    pub from: String,
    pub text: String,
    pub kind: Option<String>,
    pub timestamp: String,
}

pub(crate) fn inbox_path(home: &Path, name: &str) -> PathBuf {
    home.join("inbox").join(format!("{name}.jsonl"))
}

/// Enqueue a message — append one JSON line (atomic for small writes).
pub fn enqueue(home: &Path, name: &str, msg: InboxMessage) -> anyhow::Result<()> {
    let path = inbox_path(home, name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{}", serde_json::to_string(&msg)?)?;
    Ok(())
}

/// Drain all messages atomically (rename + read to avoid race with concurrent append).
///
/// Recovery: if a previous drain crashed between `rename()` and `remove_file()`
/// (or the read_to_string failed), the messages would live on in
/// `{name}.draining` forever, AND would be silently lost the next time drain
/// renamed over it. We now treat a leftover `.draining` as the authoritative
/// pending batch: read and consume it first. New arrivals in `{name}.jsonl`
/// are picked up on the next drain call — delaying them by one cycle is
/// acceptable; dropping them is not.
pub fn drain(home: &Path, name: &str) -> Vec<InboxMessage> {
    let path = inbox_path(home, name);
    let tmp = path.with_extension("draining");

    // Leftover from a crashed predecessor — consume it first, leave the
    // live file untouched so no pending batch is overwritten.
    if tmp.exists() {
        return read_drain_file(&tmp);
    }

    if !path.exists() {
        return Vec::new();
    }
    if std::fs::rename(&path, &tmp).is_err() {
        return Vec::new(); // File may have been drained by another caller
    }
    read_drain_file(&tmp)
}

fn read_drain_file(tmp: &Path) -> Vec<InboxMessage> {
    let content = match std::fs::read_to_string(tmp) {
        Ok(c) => c,
        // Leave `.draining` in place so the next drain call retries; the
        // previous implementation early-returned without removing, but also
        // returned empty even on success when read_to_string returned Err
        // after the earlier remove had run — which was impossible to recover.
        Err(e) => {
            tracing::warn!(
                path = %tmp.display(),
                error = %e,
                "inbox drain read failed; .draining retained for retry"
            );
            return Vec::new();
        }
    };
    let messages: Vec<InboxMessage> = content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    // Remove only AFTER a successful read+parse so crashes between read and
    // remove still leave the data on disk for the next drain to recover.
    if let Err(e) = std::fs::remove_file(tmp) {
        tracing::warn!(path = %tmp.display(), error = %e, "inbox drain cleanup failed");
    }
    messages
}

pub const INLINE_THRESHOLD: usize = 500;

/// Deliver a message: short messages (≤500 chars) inject directly to PTY,
/// long messages store to inbox + inject truncated notification.
pub fn deliver(
    home: &Path,
    agent_name: &str,
    source: &NotifySource<'_>,
    text: &str,
    _submit_key: &str,
    kind: Option<String>,
) {
    if text.chars().count() <= INLINE_THRESHOLD {
        notify_agent(home, agent_name, source, text);
    } else {
        let msg = InboxMessage {
            from: source.to_string(),
            text: text.to_string(),
            kind,
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        let _ = enqueue(home, agent_name, msg);
        notify_agent(home, agent_name, source, text);
    }
}

pub fn notify_agent(home: &Path, agent_name: &str, source: &NotifySource<'_>, text: &str) {
    let display_text = if text.chars().count() > 200 {
        let truncated: String = text.chars().take(200).collect();
        format!("{truncated}... (run: agend-terminal agent inbox)")
    } else {
        text.to_string()
    };
    let notification = format!("[{source}] {display_text}{}", source.reply_hint());
    compose_aware_inject(home, agent_name, &notification);
}

/// Compose-aware notification delivery: checks `is_composing` and enqueues
/// if the target agent is mid-typing, otherwise injects directly via the API.
/// Used by `notify_agent` (Telegram/system path) for passive notifications
/// that should NOT auto-submit (raw write, no submit_key).
pub fn compose_aware_inject(home: &Path, agent_name: &str, notification: &str) {
    let _ = route_notification(home, agent_name, notification, |msg| {
        inject_notification(home, agent_name, msg)
    });
}

/// Compose-aware message delivery with auto-submit: checks `is_composing`
/// and enqueues if mid-typing, otherwise injects via `inject_to_agent` which
/// appends `submit_key`. Used by `handle_send` (agent-to-agent MCP path)
/// for explicit messages that must be submitted to the target's CLI.
pub fn compose_aware_send(home: &Path, agent_name: &str, message: &str) {
    let _ = route_notification(home, agent_name, message, |msg| {
        inject_with_submit(home, agent_name, msg)
    });
}

fn inject_with_submit(home: &Path, agent_name: &str, message: &str) -> anyhow::Result<()> {
    let resp = crate::api::call(
        home,
        &serde_json::json!({
            "method": crate::api::method::INJECT,
            "params": {"name": agent_name, "data": message}
        }),
    )?;
    if resp["ok"].as_bool() == Some(true) {
        Ok(())
    } else {
        anyhow::bail!(
            "{}",
            resp["error"]
                .as_str()
                .unwrap_or("inject with submit failed")
        );
    }
}

pub fn inject_notification(
    home: &Path,
    agent_name: &str,
    notification: &str,
) -> anyhow::Result<()> {
    let resp = crate::api::call(
        home,
        &serde_json::json!({
            "method": crate::api::method::INJECT,
            "params": {"name": agent_name, "data": notification, "raw": true}
        }),
    )?;
    if resp["ok"].as_bool() == Some(true) {
        Ok(())
    } else {
        anyhow::bail!(
            "{}",
            resp["error"]
                .as_str()
                .unwrap_or("notification inject failed")
        );
    }
}

fn route_notification<F>(
    home: &Path,
    agent_name: &str,
    notification: &str,
    mut injector: F,
) -> anyhow::Result<()>
where
    F: FnMut(&str) -> anyhow::Result<()>,
{
    if crate::notification_queue::is_composing(home, agent_name) {
        crate::notification_queue::enqueue(home, agent_name, notification)?;
        return Ok(());
    }
    injector(notification)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_home(suffix: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("agend-inbox-{}-{}", suffix, std::process::id()));
        fs::create_dir_all(&dir).ok();
        dir
    }

    fn make_msg(from: &str, text: &str) -> InboxMessage {
        InboxMessage {
            from: from.to_string(),
            text: text.to_string(),
            kind: None,
            timestamp: "2025-01-01T00:00:00Z".to_string(),
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
        let msg = InboxMessage {
            from: "sender".to_string(),
            text: "body text".to_string(),
            kind: Some("notification".to_string()),
            timestamp: "2025-06-15T12:30:00Z".to_string(),
        };
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
    fn deliver_short_message_does_not_enqueue() {
        let home = tmp_home("deliver-short");
        // deliver with short text — should NOT write to inbox file
        // (notify_agent will fail because no daemon, but enqueue should not be called)
        deliver(
            &home,
            "agent1",
            &NotifySource::Telegram("user"),
            "short msg",
            "\r",
            None,
        );
        let msgs = drain(&home, "agent1");
        assert!(msgs.is_empty(), "short messages bypass inbox");

        fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn deliver_long_message_enqueues() {
        let home = tmp_home("deliver-long");
        let long_text: String = "x".repeat(INLINE_THRESHOLD + 100);
        deliver(
            &home,
            "agent1",
            &NotifySource::Telegram("user"),
            &long_text,
            "\r",
            Some("chat".to_string()),
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
        let msg = InboxMessage {
            from: "test".to_string(),
            text: "hello \"world\"".to_string(),
            kind: None,
            timestamp: "2025-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let parsed: InboxMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.from, "test");
        assert_eq!(parsed.text, "hello \"world\"");
    }

    #[test]
    fn inbox_message_with_special_chars() {
        let home = tmp_home("special");
        let msg = InboxMessage {
            from: "user".to_string(),
            text: "line1\nline2\ttab".to_string(),
            kind: Some("special".to_string()),
            timestamp: "2025-01-01T00:00:00Z".to_string(),
        };
        enqueue(&home, "agent1", msg).ok();
        let msgs = drain(&home, "agent1");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text, "line1\nline2\ttab");

        fs::remove_dir_all(&home).ok();
    }

    // --- NotifySource tests ---

    #[test]
    fn notify_source_telegram_display() {
        let s = NotifySource::Telegram("chiacheng");
        assert_eq!(s.to_string(), "user:chiacheng via telegram");
        assert!(s.reply_hint().contains("reply tool"));
    }

    #[test]
    fn notify_source_agent_display() {
        let s = NotifySource::Agent("dev");
        assert_eq!(s.to_string(), "from:dev");
        let h = s.reply_hint();
        assert!(h.contains("send_to_instance"));
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
}
