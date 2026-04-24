//! Dispatch tracking — monitors delegated tasks for timeout/stuck detection.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Warn threshold: dispatcher gets notified if no report_result after this.
pub const DISPATCH_WARN_MINUTES: i64 = 15;
/// Ask threshold: daemon sends query to assignee after this.
pub const DISPATCH_ASK_MINUTES: i64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DispatchEntry {
    pub task_id: Option<String>,
    pub from: String,
    pub to: String,
    pub delegated_at: String,
    pub status: String, // "pending" | "completed" | "warned" | "asked"
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DispatchStore {
    #[serde(default)]
    schema_version: u32,
    entries: Vec<DispatchEntry>,
}

impl crate::store::SchemaVersioned for DispatchStore {
    const CURRENT: u32 = 1;
    fn version_mut(&mut self) -> &mut u32 {
        &mut self.schema_version
    }
}

fn store_path(home: &Path) -> std::path::PathBuf {
    crate::store::store_path(home, "dispatch_tracking.json")
}

/// Record a new delegation.
pub fn track_dispatch(home: &Path, entry: DispatchEntry) {
    let _ = crate::store::mutate_versioned(&store_path(home), |store: &mut DispatchStore| {
        store.entries.push(entry);
        Ok(())
    });
}

/// Mark a dispatch as completed (matched by task_id or to-instance).
pub fn mark_completed(home: &Path, correlation_id: Option<&str>, _to: &str) {
    let cid = match correlation_id {
        Some(c) if !c.is_empty() => c,
        _ => return, // No correlation_id → can't match, let sweep continue tracking
    };
    let _ = crate::store::mutate_versioned(&store_path(home), |store: &mut DispatchStore| {
        for entry in store.entries.iter_mut() {
            if entry.status == "completed" {
                continue;
            }
            if entry.task_id.as_deref() == Some(cid) {
                entry.status = "completed".to_string();
            }
        }
        Ok(())
    });
}

/// Sweep for stuck dispatches. Returns (warn_list, ask_list).
pub fn sweep_stuck(home: &Path) -> (Vec<DispatchEntry>, Vec<DispatchEntry>) {
    let now = chrono::Utc::now();
    let mut warns = Vec::new();
    let mut asks = Vec::new();

    let _ = crate::store::mutate_versioned(&store_path(home), |store: &mut DispatchStore| {
        for entry in store.entries.iter_mut() {
            if entry.status == "completed" {
                continue;
            }
            let delegated = match chrono::DateTime::parse_from_rfc3339(&entry.delegated_at) {
                Ok(dt) => dt.with_timezone(&chrono::Utc),
                Err(_) => continue,
            };
            let age_min = now.signed_duration_since(delegated).num_minutes();

            if age_min >= DISPATCH_ASK_MINUTES && entry.status != "asked" {
                entry.status = "asked".to_string();
                asks.push(entry.clone());
            } else if age_min >= DISPATCH_WARN_MINUTES && entry.status == "pending" {
                entry.status = "warned".to_string();
                warns.push(entry.clone());
            }
        }
        Ok(())
    });
    (warns, asks)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-dispatch-test-{}-{}",
            tag,
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn test_dispatch_warn_after_15min() {
        let home = tmp_home("warn-15");
        let past = (chrono::Utc::now() - chrono::Duration::minutes(16)).to_rfc3339();
        track_dispatch(
            &home,
            DispatchEntry {
                task_id: Some("t-test".into()),
                from: "lead".into(),
                to: "impl".into(),
                delegated_at: past,
                status: "pending".into(),
            },
        );
        let (warns, asks) = sweep_stuck(&home);
        assert_eq!(warns.len(), 1, "16min old dispatch must warn");
        assert!(asks.is_empty());
        // Verify event_log integration
        crate::event_log::log(&home, "dispatch_stuck_warn", &warns[0].to, "test");
        let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(log.contains("dispatch_stuck_warn"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_dispatch_ask_after_30min() {
        let home = tmp_home("ask-30");
        let past = (chrono::Utc::now() - chrono::Duration::minutes(31)).to_rfc3339();
        track_dispatch(
            &home,
            DispatchEntry {
                task_id: Some("t-test".into()),
                from: "lead".into(),
                to: "reviewer".into(),
                delegated_at: past,
                status: "pending".into(),
            },
        );
        let (warns, asks) = sweep_stuck(&home);
        // 31min → jumps straight to ask (skips warn since ask threshold met)
        assert!(
            warns.is_empty(),
            "31min should go straight to ask, not warn"
        );
        assert_eq!(asks.len(), 1, "31min old dispatch must ask");
        assert_eq!(asks[0].to, "reviewer");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_report_result_marks_dispatch_completed() {
        let home = tmp_home("mark-complete");
        track_dispatch(
            &home,
            DispatchEntry {
                task_id: Some("t-123".into()),
                from: "lead".into(),
                to: "impl".into(),
                delegated_at: (chrono::Utc::now() - chrono::Duration::minutes(20)).to_rfc3339(),
                status: "pending".into(),
            },
        );
        // Mark completed (simulates report_result handler calling this)
        mark_completed(&home, Some("t-123"), "impl");
        // Sweep should find nothing — entry is completed
        let (warns, asks) = sweep_stuck(&home);
        assert!(warns.is_empty(), "completed dispatch must not warn");
        assert!(asks.is_empty(), "completed dispatch must not ask");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_dispatch_ask_sends_query_to_inbox() {
        let home = tmp_home("ask-inbox");
        let past = (chrono::Utc::now() - chrono::Duration::minutes(31)).to_rfc3339();
        track_dispatch(
            &home,
            DispatchEntry {
                task_id: Some("t-stuck".into()),
                from: "lead".into(),
                to: "reviewer".into(),
                delegated_at: past,
                status: "pending".into(),
            },
        );
        // Run the daemon maintenance path
        crate::daemon::run_task_maintenance(&home);
        // Verify reviewer got a query in inbox
        let msgs = crate::inbox::drain(&home, "reviewer");
        assert!(
            msgs.iter().any(|m| m.text.contains("dispatch stuck check")),
            "reviewer must receive stuck query in inbox: {:?}",
            msgs.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
