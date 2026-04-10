//! Per-agent message inbox — file-based for thread safety.
//!
//! Messages stored as JSON in {home}/inbox/{name}.json.
//! File-based ensures sync core + MCP server + Telegram adapter can all access.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAX_INBOX: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    pub from: String,
    pub text: String,
    pub kind: Option<String>,
    pub timestamp: String,
}

fn inbox_path(home: &Path, name: &str) -> PathBuf {
    home.join("inbox").join(format!("{name}.json"))
}

/// Enqueue a message in an agent's inbox.
pub fn enqueue(home: &Path, name: &str, msg: InboxMessage) -> anyhow::Result<()> {
    let path = inbox_path(home, name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut messages = read_all(home, name);
    if messages.len() >= MAX_INBOX {
        messages.remove(0); // Drop oldest
    }
    messages.push(msg);

    let json = serde_json::to_string_pretty(&messages)?;
    std::fs::write(&path, json)?;
    Ok(())
}

/// Drain all messages from an agent's inbox (returns + clears).
pub fn drain(home: &Path, name: &str) -> Vec<InboxMessage> {
    let messages = read_all(home, name);
    if !messages.is_empty() {
        let path = inbox_path(home, name);
        let _ = std::fs::write(&path, "[]");
    }
    messages
}

/// Read all messages without draining.
fn read_all(home: &Path, name: &str) -> Vec<InboxMessage> {
    let path = inbox_path(home, name);
    if !path.exists() {
        return Vec::new();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Inject a notification into an agent's PTY via TUI socket.
/// Short messages injected directly, long ones truncated with inbox hint.
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

    let sock = crate::daemon::agent_socket_path(home, agent_name);
    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&sock) {
        let _ = crate::framing::write_frame(&mut stream, notification.as_bytes());
    }
}
