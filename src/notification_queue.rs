use crate::agent_ops;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const COMPOSE_IDLE_TIMEOUT: Duration = Duration::from_secs(3);
const COMPOSE_METADATA_KEY: &str = "last_input_epoch_ms";
/// Sprint 54 P2-3: epoch-ms timestamp of the most recent submit-key
/// keystroke (e.g. `\r` for claude). Distinct from
/// `COMPOSE_METADATA_KEY` which records ANY input keystroke. Used by
/// the daemon supervisor to detect "typed but not submitted" state.
const SUBMIT_METADATA_KEY: &str = "last_submit_epoch_ms";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedNotification {
    pub text: String,
    pub timestamp: String,
}

fn queue_path(home: &Path, agent_name: &str) -> PathBuf {
    home.join("notification-queue")
        .join(format!("{agent_name}.jsonl"))
}

fn draining_path(home: &Path, agent_name: &str) -> PathBuf {
    queue_path(home, agent_name).with_extension("draining")
}

pub fn record_input_activity(home: &Path, agent_name: &str) {
    agent_ops::save_metadata(
        home,
        agent_name,
        COMPOSE_METADATA_KEY,
        json!(chrono::Utc::now().timestamp_millis()),
    );
}

/// Sprint 54 P2-3: record a submit-key keystroke (e.g. claude `\r`).
/// Caller (`app::write_to_focused`) is responsible for the backend
/// allowlist + submit-key match — this helper only persists the
/// timestamp. The daemon supervisor tick reads it via
/// `last_submit_at_ms` and compares against `last_input_at_ms` for
/// the typed-but-not-submitted detection.
pub fn record_submit_activity(home: &Path, agent_name: &str) {
    agent_ops::save_metadata(
        home,
        agent_name,
        SUBMIT_METADATA_KEY,
        json!(chrono::Utc::now().timestamp_millis()),
    );
}

/// Sprint 54 P2-3: read the last input/submit timestamps. Returns
/// `(typed_ms, submit_ms)` tuple; either component is `0` when missing
/// (legacy data, agent never typed, or non-submit-detected backend).
/// Used by the daemon supervisor tick for typed-but-not-submitted
/// detection — keeps the read inline-cheap (single file read, single
/// JSON parse) so per-tick overhead stays bounded.
pub fn read_input_submit_timestamps(home: &Path, agent_name: &str) -> (i64, i64) {
    let meta_path = home.join("metadata").join(format!("{agent_name}.json"));
    let Ok(content) = std::fs::read_to_string(meta_path) else {
        return (0, 0);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return (0, 0);
    };
    let typed_ms = value[COMPOSE_METADATA_KEY].as_i64().unwrap_or(0);
    let submit_ms = value[SUBMIT_METADATA_KEY].as_i64().unwrap_or(0);
    (typed_ms, submit_ms)
}

pub fn is_composing(home: &Path, agent_name: &str) -> bool {
    let meta_path = home.join("metadata").join(format!("{agent_name}.json"));
    let meta = match std::fs::read_to_string(meta_path) {
        Ok(content) => content,
        Err(_) => return false,
    };
    let value: serde_json::Value = match serde_json::from_str(&meta) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let Some(last_input_ms) = value[COMPOSE_METADATA_KEY].as_i64() else {
        return false;
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    now_ms.saturating_sub(last_input_ms) < COMPOSE_IDLE_TIMEOUT.as_millis() as i64
}

pub fn enqueue(home: &Path, agent_name: &str, text: &str) -> anyhow::Result<()> {
    let path = queue_path(home, agent_name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let msg = QueuedNotification {
        text: text.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{}", serde_json::to_string(&msg)?)?;
    Ok(())
}

pub fn pending_count(home: &Path, agent_name: &str) -> usize {
    let mut count = 0;
    for path in [
        queue_path(home, agent_name),
        draining_path(home, agent_name),
    ] {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        count += content.lines().count();
    }
    count
}

pub fn drain(home: &Path, agent_name: &str) -> Vec<QueuedNotification> {
    let path = queue_path(home, agent_name);
    let tmp = draining_path(home, agent_name);
    if tmp.exists() {
        return read_drain_file(&tmp);
    }
    if !path.exists() {
        return Vec::new();
    }
    if std::fs::rename(&path, &tmp).is_err() {
        return Vec::new();
    }
    read_drain_file(&tmp)
}

pub fn requeue_all(home: &Path, agent_name: &str, notifications: &[QueuedNotification]) {
    for notification in notifications {
        let _ = enqueue(home, agent_name, &notification.text);
    }
}

fn read_drain_file(path: &Path) -> Vec<QueuedNotification> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let notifications = content
        .lines()
        .filter_map(|line| serde_json::from_str::<QueuedNotification>(line).ok())
        .collect::<Vec<_>>();
    let _ = std::fs::remove_file(path);
    notifications
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-notification-queue-{}-{}",
            suffix,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn pending_count_tracks_enqueued_notifications() {
        let home = tmp_home("count");
        enqueue(&home, "agent1", "a").expect("enqueue a");
        enqueue(&home, "agent1", "b").expect("enqueue b");
        assert_eq!(pending_count(&home, "agent1"), 2);
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn drain_roundtrip() {
        let home = tmp_home("drain");
        enqueue(&home, "agent1", "a").expect("enqueue a");
        enqueue(&home, "agent1", "b").expect("enqueue b");
        let drained = drain(&home, "agent1");
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].text, "a");
        assert_eq!(pending_count(&home, "agent1"), 0);
        std::fs::remove_dir_all(home).ok();
    }

    /// Sprint 54 P2-3: round-trip both timestamps; ensure
    /// `read_input_submit_timestamps` returns paired values and
    /// `record_submit_activity` writes a value strictly newer than the
    /// preceding `record_input_activity` call.
    #[test]
    fn record_and_read_input_submit_timestamps_round_trip() {
        let home = tmp_home("ts_round_trip");
        // Fresh agent → both 0.
        let (typed0, submit0) = read_input_submit_timestamps(&home, "agent1");
        assert_eq!((typed0, submit0), (0, 0));
        record_input_activity(&home, "agent1");
        std::thread::sleep(Duration::from_millis(2));
        record_submit_activity(&home, "agent1");
        let (typed1, submit1) = read_input_submit_timestamps(&home, "agent1");
        assert!(typed1 > 0, "typed timestamp must be set after record");
        assert!(submit1 > 0, "submit timestamp must be set after record");
        assert!(
            submit1 >= typed1,
            "submit (called second) must be ≥ typed (called first), got typed={typed1} submit={submit1}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    /// Sprint 54 P2-3: typed-only (no submit) must read as
    /// `submit_ms == 0`. This is the daemon-supervisor's signal for
    /// "user typed but never pressed Enter" — it MUST distinguish
    /// from "user typed AND submitted" otherwise the dedup logic
    /// degrades to never firing.
    #[test]
    fn typed_only_leaves_submit_zero() {
        let home = tmp_home("typed_only");
        record_input_activity(&home, "agent1");
        let (typed, submit) = read_input_submit_timestamps(&home, "agent1");
        assert!(typed > 0);
        assert_eq!(
            submit, 0,
            "submit must stay 0 until record_submit_activity is called"
        );
        std::fs::remove_dir_all(home).ok();
    }
}
