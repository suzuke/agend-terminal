//! Event log — append-only audit trail for daemon events.
//!
//! Rotates at 10 MB to prevent unbounded growth.

use serde::Serialize;
use std::path::Path;

/// Maximum log file size before rotation (10 MB).
const MAX_LOG_SIZE: u64 = 10 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub struct Event {
    pub timestamp: String,
    pub kind: &'static str,
    pub instance: String,
    pub detail: String,
}

/// Append an event to the log file. Rotates when size exceeds MAX_LOG_SIZE.
pub fn log(home: &Path, kind: &'static str, instance: &str, detail: &str) {
    let log_path = home.join("event-log.jsonl");
    let event = Event {
        timestamp: chrono::Utc::now().to_rfc3339(),
        kind,
        instance: instance.to_string(),
        detail: detail.to_string(),
    };

    // Rotate if too large
    if let Ok(meta) = std::fs::metadata(&log_path) {
        if meta.len() > MAX_LOG_SIZE {
            let rotated = home.join("event-log.jsonl.1");
            let _ = std::fs::rename(&log_path, &rotated);
        }
    }

    if let Ok(json) = serde_json::to_string(&event) {
        use std::io::Write;
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            Ok(mut f) => {
                if let Err(e) = writeln!(f, "{json}") {
                    tracing::warn!(path = %log_path.display(), error = %e, "failed to write event log entry");
                }
            }
            Err(e) => {
                tracing::warn!(path = %log_path.display(), error = %e, "failed to open event log");
            }
        }
    }
}
