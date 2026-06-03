//! Dispatch tracking — monitors delegated tasks for timeout/stuck detection.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Warn threshold: dispatcher gets notified if no report_result after this.
pub const DISPATCH_WARN_MINUTES: i64 = 15;
/// Ask threshold: daemon sends query to assignee after this.
pub const DISPATCH_ASK_MINUTES: i64 = 30;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DispatchEntry {
    pub task_id: Option<String>,
    pub from: String,
    pub to: String,
    /// Sprint 46 P3: sender's InstanceId for audit trail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_id: Option<String>,
    /// Sprint 46 P3: target's InstanceId for audit trail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_id: Option<String>,
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
    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |store: &mut DispatchStore| {
            store.entries.push(entry);
            Ok(())
        }),
        "dispatch_track"
    );
}

/// Mark a dispatch as completed (matched by task_id or to-instance).
pub fn mark_completed(home: &Path, correlation_id: Option<&str>, _to: &str) {
    let cid = match correlation_id {
        Some(c) if !c.is_empty() => c,
        _ => return, // No correlation_id → can't match, let sweep continue tracking
    };
    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |store: &mut DispatchStore| {
            for entry in store.entries.iter_mut() {
                if entry.status == "completed" {
                    continue;
                }
                if entry.task_id.as_deref() == Some(cid) {
                    entry.status = "completed".to_string();
                }
            }
            Ok(())
        }),
        "dispatch_mark_completed"
    );
}

/// Sweep for stuck dispatches. Returns (warn_list, ask_list).
pub fn sweep_stuck(home: &Path) -> (Vec<DispatchEntry>, Vec<DispatchEntry>) {
    let now = chrono::Utc::now();
    let mut warns = Vec::new();
    let mut asks = Vec::new();

    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |store: &mut DispatchStore| {
            for entry in store.entries.iter_mut() {
                // Skip BOTH terminal states — mirrors `sweep_orphans`' skip set.
                // Without skipping `orphaned`, an entry past DISPATCH_ORPHAN_HOURS
                // flip-flops forever: `sweep_orphans` overwrites its `asked` status
                // with `orphaned`, then this sweep sees `orphaned != "asked"` and
                // re-asks (re-marking `asked`), which `sweep_orphans` flips back —
                // re-firing a "dispatch stuck check" every maintenance tick until
                // the 30-day GC. `orphaned` = already given up after 24h; never nag
                // again. (#1488 noted this re-fire but only fixed deleted-instance
                // entries via `cleanup_for_instance`; this fixes still-existing
                // targets whose orphaned entries otherwise nag for ~30 days.)
                if entry.status == "completed" || entry.status == "orphaned" {
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
        }),
        "dispatch_sweep_stuck"
    );
    (warns, asks)
}

/// Hours after which an uncompleted dispatch is considered orphaned.
pub const DISPATCH_ORPHAN_HOURS: i64 = 24;

/// Sweep for orphaned dispatches (>24h uncompleted). Marks them as "orphaned"
/// and returns the list for event logging.
pub fn sweep_orphans(home: &Path) -> Vec<DispatchEntry> {
    let now = chrono::Utc::now();
    let mut orphans = Vec::new();

    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |store: &mut DispatchStore| {
            for entry in store.entries.iter_mut() {
                if entry.status == "completed" || entry.status == "orphaned" {
                    continue;
                }
                let delegated = match chrono::DateTime::parse_from_rfc3339(&entry.delegated_at) {
                    Ok(dt) => dt.with_timezone(&chrono::Utc),
                    Err(_) => continue,
                };
                let age_hours = now.signed_duration_since(delegated).num_hours();
                if age_hours >= DISPATCH_ORPHAN_HOURS {
                    entry.status = "orphaned".to_string();
                    orphans.push(entry.clone());
                }
            }
            Ok(())
        }),
        "dispatch_sweep_orphans"
    );
    orphans
}

/// #1488: drop every dispatch entry that involves `instance` as either the
/// dispatcher (`from`) or the target (`to`). When an instance is deleted, its
/// in-flight dispatches can never complete, so without cleanup they linger as
/// noise (the empirical ~81 "dispatch stuck check" messages). `sweep_stuck` now
/// skips `orphaned` (the flip-flop fix), so such entries stop re-asking after
/// 24h — but removal is still preferred for a deleted instance: the entry can
/// never complete and carries no re-target value, so freeing the row beats
/// leaving it for the 30-day GC. Returns the number removed.
pub fn cleanup_for_instance(home: &Path, instance: &str) -> usize {
    let mut removed = 0usize;
    persist_or_log!(
        crate::store::mutate_versioned(&store_path(home), |store: &mut DispatchStore| {
            let before = store.entries.len();
            store
                .entries
                .retain(|e| e.from != instance && e.to != instance);
            removed = before - store.entries.len();
            Ok(())
        }),
        "dispatch_cleanup_for_instance"
    );
    if removed > 0 {
        tracing::info!(
            %instance,
            count = removed,
            "#1488: removed dispatch_tracking entries for deleted instance"
        );
    }
    removed
}

/// #1488: distinct, still-active (`status != "completed"`) dispatch target
/// names. The boot orphan sweep uses this to find entries whose `to` instance
/// no longer exists, then reuses [`cleanup_for_instance`] to remove them —
/// sharing the exact delete-path logic instead of duplicating it.
pub fn active_target_names(home: &Path) -> Vec<String> {
    let store: DispatchStore = crate::store::load_versioned(
        &store_path(home),
        <DispatchStore as crate::store::SchemaVersioned>::CURRENT,
    );
    let mut names: Vec<String> = store
        .entries
        .iter()
        .filter(|e| e.status != "completed" && !e.to.is_empty())
        .map(|e| e.to.clone())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// M3: Remove terminal entries (completed/orphaned) older than 30 days.
/// Prevents unbounded growth of dispatch_tracking.json.
pub fn gc_old_entries(home: &Path) {
    const RETENTION_DAYS: i64 = 30;
    let now = chrono::Utc::now();
    // best-effort (#1647): unlike the sibling track/sweep/cleanup writes above,
    // a dropped GC pass is harmless — it only delays pruning already-terminal
    // rows, and the next maintenance tick retries. Intentionally not logged.
    let _ = crate::store::mutate_versioned(&store_path(home), |store: &mut DispatchStore| {
        store.entries.retain(|entry| {
            if entry.status != "completed" && entry.status != "orphaned" {
                return true; // keep active entries
            }
            let delegated = match chrono::DateTime::parse_from_rfc3339(&entry.delegated_at) {
                Ok(dt) => dt.with_timezone(&chrono::Utc),
                Err(_) => return false, // unparseable → drop
            };
            now.signed_duration_since(delegated).num_days() < RETENTION_DAYS
        });
        Ok(())
    });
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
                from_id: None,
                to_id: None,
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
                from_id: None,
                to_id: None,
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

    /// Flip-flop fix: an `orphaned` entry (already given up on after 24h) must
    /// NOT be re-asked by `sweep_stuck`. Before the fix, `sweep_orphans`
    /// overwrote `asked` → `orphaned`, then `sweep_stuck` saw `orphaned != "asked"`
    /// and re-asked, which `sweep_orphans` flipped back — re-firing a "dispatch
    /// stuck check" every maintenance tick for ~30 days until GC. The same-age
    /// `pending` entry MUST still ask: the fix skips only the terminal `orphaned`
    /// state, it does not suppress normal nagging.
    #[test]
    fn orphaned_entry_is_not_re_asked() {
        let home = tmp_home("orphaned-no-reask");
        let old = (chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
        track_dispatch(
            &home,
            DispatchEntry {
                task_id: Some("t-orphaned".into()),
                from: "lead".into(),
                to: "fixup-dev-2".into(),
                from_id: None,
                to_id: None,
                delegated_at: old.clone(),
                status: "orphaned".into(),
            },
        );
        track_dispatch(
            &home,
            DispatchEntry {
                task_id: Some("t-pending".into()),
                from: "lead".into(),
                to: "fixup-dev-2".into(),
                from_id: None,
                to_id: None,
                delegated_at: old,
                status: "pending".into(),
            },
        );
        let (_warns, asks) = sweep_stuck(&home);
        assert_eq!(
            asks.len(),
            1,
            "only the pending entry asks; the orphaned one is skipped"
        );
        assert_eq!(asks[0].task_id.as_deref(), Some("t-pending"));
        assert!(
            !asks
                .iter()
                .any(|a| a.task_id.as_deref() == Some("t-orphaned")),
            "an orphaned entry must never be re-asked (flip-flop fix)"
        );
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
                from_id: None,
                to_id: None,
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
                from_id: None,
                to_id: None,
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

    #[test]
    fn test_dispatch_orphan_after_24h() {
        let home = tmp_home("orphan");
        let old_ts = (chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
        track_dispatch(
            &home,
            DispatchEntry {
                task_id: Some("t-orphan".into()),
                from: "lead".into(),
                to: "worker".into(),
                from_id: None,
                to_id: None,
                delegated_at: old_ts,
                status: "pending".into(),
            },
        );
        let orphans = sweep_orphans(&home);
        assert_eq!(orphans.len(), 1, "25h old dispatch must be orphaned");
        assert_eq!(orphans[0].task_id.as_deref(), Some("t-orphan"));
        assert_eq!(orphans[0].status, "orphaned");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_gc_removes_old_terminal_entries() {
        let home = tmp_home("gc_old");
        // Add a completed entry from 31 days ago
        track_dispatch(
            &home,
            DispatchEntry {
                task_id: Some("t-old".into()),
                from: "lead".into(),
                to: "dev".into(),
                from_id: None,
                to_id: None,
                delegated_at: (chrono::Utc::now() - chrono::Duration::days(31)).to_rfc3339(),
                status: "completed".into(),
            },
        );
        // Add a recent completed entry (should survive)
        track_dispatch(
            &home,
            DispatchEntry {
                task_id: Some("t-recent".into()),
                from: "lead".into(),
                to: "dev".into(),
                from_id: None,
                to_id: None,
                delegated_at: chrono::Utc::now().to_rfc3339(),
                status: "completed".into(),
            },
        );
        gc_old_entries(&home);
        let store: serde_json::Value =
            crate::store::load(&crate::store::store_path(&home, "dispatch_tracking.json"));
        let entries = store["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), 1, "old entry should be removed: {entries:?}");
        assert_eq!(entries[0]["task_id"], "t-recent");
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #1488 cascade: drop dispatch entries for a deleted instance ──

    fn entry(from: &str, to: &str) -> DispatchEntry {
        DispatchEntry {
            task_id: Some(format!("t-{from}-{to}")),
            from: from.into(),
            to: to.into(),
            from_id: None,
            to_id: None,
            delegated_at: chrono::Utc::now().to_rfc3339(),
            status: "pending".into(),
        }
    }

    #[test]
    fn cleanup_for_instance_removes_from_and_to_keeps_unrelated() {
        let home = tmp_home("cleanup-inst");
        track_dispatch(&home, entry("lead", "doomed")); // to == doomed
        track_dispatch(&home, entry("doomed", "dev")); // from == doomed
        track_dispatch(&home, entry("lead", "dev")); // unrelated
        let removed = cleanup_for_instance(&home, "doomed");
        assert_eq!(removed, 2, "both from== and to==doomed entries removed");
        let store: serde_json::Value =
            crate::store::load(&crate::store::store_path(&home, "dispatch_tracking.json"));
        let entries = store["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "unrelated entry must survive");
        assert_eq!(entries[0]["to"], "dev");
        assert_eq!(entries[0]["from"], "lead");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn active_target_names_returns_distinct_non_completed() {
        let home = tmp_home("active-targets");
        track_dispatch(&home, entry("lead", "dev"));
        track_dispatch(&home, entry("lead", "dev")); // dup target
        let mut done = entry("lead", "reviewer");
        done.status = "completed".into();
        track_dispatch(&home, done);
        let names = active_target_names(&home);
        assert_eq!(
            names,
            vec!["dev"],
            "distinct, completed excluded: {names:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
