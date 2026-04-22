use crate::agent_ops;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const COMPOSE_IDLE_TIMEOUT: Duration = Duration::from_secs(3);
const COMPOSE_METADATA_KEY: &str = "last_input_epoch_ms";

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
}
