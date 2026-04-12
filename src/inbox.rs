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
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
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

const INLINE_THRESHOLD: usize = 500;

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
pub fn notify_agent(
    home: &Path,
    agent_name: &str,
    from: &str,
    text: &str,
    submit_key: &str,
) {
    let display_text = if text.chars().count() > 200 {
        let truncated: String = text.chars().take(200).collect();
        format!("{truncated}... (use inbox tool for full message)")
    } else {
        text.to_string()
    };
    let notification = format!("[{from}] {display_text}{submit_key}");

    // Use API socket to inject (doesn't kick attach clients)
    let _ = crate::api::call(
        home,
        &serde_json::json!({
            "method": "inject",
            "params": {"name": agent_name, "data": notification}
        }),
    );
}
