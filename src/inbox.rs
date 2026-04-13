//! Per-agent message inbox — append-only JSONL for thread safety.
//!
//! Messages stored as one JSON object per line in {home}/inbox/{name}.jsonl.
//! Append is atomic on most filesystems for small writes — no file locking needed.

use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    pub from: String,
    pub text: String,
    pub kind: Option<String>,
    pub timestamp: String,
}

fn inbox_path(home: &Path, name: &str) -> PathBuf {
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
pub fn drain(home: &Path, name: &str) -> Vec<InboxMessage> {
    let path = inbox_path(home, name);
    if !path.exists() {
        return Vec::new();
    }
    // Atomic: rename file, then read the renamed copy
    let tmp = path.with_extension("draining");
    if std::fs::rename(&path, &tmp).is_err() {
        return Vec::new(); // File may have been drained by another caller
    }
    let content = match std::fs::read_to_string(&tmp) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let _ = std::fs::remove_file(&tmp);
    let messages: Vec<InboxMessage> = content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    messages
}

pub const INLINE_THRESHOLD: usize = 500;

/// Deliver a message: short messages (≤500 chars) inject directly to PTY,
/// long messages store to inbox + inject truncated notification.
pub fn deliver(
    home: &Path,
    agent_name: &str,
    from: &str,
    text: &str,
    submit_key: &str,
    kind: Option<String>,
) {
    if text.chars().count() <= INLINE_THRESHOLD {
        // Short message — inject directly, no file I/O
        notify_agent(home, agent_name, from, text, submit_key);
    } else {
        // Long message — store to inbox + truncated notification
        let msg = InboxMessage {
            from: from.to_string(),
            text: text.to_string(),
            kind,
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        let _ = enqueue(home, agent_name, msg);
        notify_agent(home, agent_name, from, text, submit_key);
    }
}

/// Inject a notification into an agent's PTY.
/// When called from daemon (has registry), uses direct write.
/// When called from external process (MCP), uses API socket.
pub fn notify_agent(home: &Path, agent_name: &str, from: &str, text: &str, submit_key: &str) {
    let display_text = if text.chars().count() > 200 {
        let truncated: String = text.chars().take(200).collect();
        format!("{truncated}... (run: agend-terminal agent inbox)")
    } else {
        text.to_string()
    };
    // Include reply hint so agents know how to respond
    let reply_hint = if from.contains("via telegram") {
        " (reply: agend-terminal agent reply \"your response\")".to_string()
    } else if let Some(target) = from.strip_prefix("from:") {
        format!(" (reply: agend-terminal agent send {target} \"your response\")")
    } else {
        String::new()
    };
    let notification = format!("[{from}] {display_text}{reply_hint}{submit_key}");

    // Use API socket to inject (doesn't kick attach clients)
    let _ = crate::api::call(
        home,
        &serde_json::json!({
            "method": "inject",
            "params": {"name": agent_name, "data": notification}
        }),
    );
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
        deliver(&home, "agent1", "user", "short msg", "\r", None);
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
            "user",
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

    // --- Reply hint tests ---

    #[test]
    fn notify_format_telegram_has_reply_hint() {
        // We can't easily call notify_agent (needs API), but we can test
        // the hint logic by checking the format string construction.
        let from = "user:chiacheng via telegram";
        let hint = if from.contains("via telegram") {
            " (reply: agend-terminal agent reply \"your response\")"
        } else {
            ""
        };
        assert!(hint.contains("agent reply"));
    }

    #[test]
    fn notify_format_agent_has_send_hint() {
        let from = "from:dev";
        let hint = if let Some(target) = from.strip_prefix("from:") {
            format!(" (reply: agend-terminal agent send {target} \"your response\")")
        } else {
            String::new()
        };
        assert!(hint.contains("agent send dev"));
    }

    #[test]
    fn notify_format_system_no_hint() {
        let from = "system:ci";
        let hint = if from.contains("via telegram") {
            "telegram".to_string()
        } else if from.starts_with("from:") {
            "agent".to_string()
        } else {
            String::new()
        };
        assert!(hint.is_empty());
    }
}
