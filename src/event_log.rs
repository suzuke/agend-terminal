//! Event log — append-only audit trail for daemon events.

use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct Event {
    pub timestamp: String,
    pub kind: &'static str,
    pub instance: String,
    pub detail: String,
}

/// Append an event to the log file.
pub fn log(home: &Path, kind: &'static str, instance: &str, detail: &str) {
    let log_path = home.join("event-log.jsonl");
    let event = Event {
        timestamp: chrono::Utc::now().to_rfc3339(),
        kind,
        instance: instance.to_string(),
        detail: detail.to_string(),
    };
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
