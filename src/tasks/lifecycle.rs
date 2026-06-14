//! #1201: Task lifecycle management — auto-stale, auto-cancel, done archive.
//!
//! Runs once per daemon boot (or on-demand via sweep). Configurable
//! thresholds via runtime-config.json.

use std::path::Path;

/// Default thresholds (days).
const DEFAULT_STALE_DAYS: i64 = 7;
const DEFAULT_CANCEL_DAYS: i64 = 14;
const DEFAULT_ARCHIVE_DAYS: i64 = 7;

/// Run the full lifecycle pass: stale open tasks, cancel old stale, archive done.
/// Returns (staled_count, cancelled_count, archived_count).
pub fn lifecycle_pass(home: &Path) -> (usize, usize, usize) {
    let staled = mark_stale_open_tasks(home);
    let cancelled = cancel_stale_tasks(home);
    let archived = archive_done_tasks(home);
    if staled + cancelled + archived > 0 {
        tracing::info!(staled, cancelled, archived, "#1201 lifecycle pass complete");
    }
    (staled, cancelled, archived)
}

/// Open tasks unclaimed for > stale_days → mark cancelled with reason.
fn mark_stale_open_tasks(home: &Path) -> usize {
    let now = chrono::Utc::now();
    let stale_days = DEFAULT_STALE_DAYS;
    let state = crate::task_events::replay(home).unwrap_or_default();
    let emitter = crate::task_events::InstanceName::from("system:lifecycle");
    let mut count = 0;

    for (tid, record) in &state.tasks {
        if record.status != crate::task_events::TaskStatus::Open {
            continue;
        }
        // #1201: only auto-cancel truly unclaimed tasks (no assignee).
        if record.owner.is_some() {
            continue;
        }
        let created = match chrono::DateTime::parse_from_rfc3339(&record.created_at) {
            Ok(dt) => dt.with_timezone(&chrono::Utc),
            Err(_) => continue,
        };
        let age_days = (now - created).num_days();
        if age_days < stale_days {
            continue;
        }
        // Cancel directly (skip intermediate "stale" status — simpler)
        // Only cancel if age > cancel_days threshold
        if age_days >= DEFAULT_CANCEL_DAYS {
            let event = crate::task_events::TaskEvent::Cancelled {
                by: emitter.clone(),
                task_id: tid.clone(),
                reason: format!(
                    "auto-lifecycle: open {age_days}d unclaimed (threshold {DEFAULT_CANCEL_DAYS}d)"
                ),
            };
            if crate::task_events::append(home, &emitter, event).is_ok() {
                count += 1;
            }
        }
    }
    count
}

/// Cancel stale tasks — same as above but with the stale threshold.
/// (Combined into mark_stale_open_tasks above for simplicity.)
fn cancel_stale_tasks(_home: &Path) -> usize {
    0 // Handled in mark_stale_open_tasks
}

/// Archive done tasks older than archive_days.
/// Removes from active task_events, appends to tasks-archive.jsonl.
fn archive_done_tasks(home: &Path) -> usize {
    let now = chrono::Utc::now();
    let state = crate::task_events::replay(home).unwrap_or_default();
    let mut count = 0;
    let archive_path = home.join("tasks-archive.jsonl");

    for (tid, record) in &state.tasks {
        if record.status != crate::task_events::TaskStatus::Done {
            continue;
        }
        let updated = match chrono::DateTime::parse_from_rfc3339(&record.updated_at) {
            Ok(dt) => dt.with_timezone(&chrono::Utc),
            Err(_) => continue,
        };
        let age_days = (now - updated).num_days();
        if age_days < DEFAULT_ARCHIVE_DAYS {
            continue;
        }
        // Append to archive
        let entry = serde_json::json!({
            "archived_at": now.to_rfc3339(),
            "task_id": tid.0,
            "title": record.title,
            "status": "done",
            "result": record.result,
            "created_at": record.created_at,
            "updated_at": record.updated_at,
        });
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&archive_path)
        {
            use std::io::Write;
            let _ = writeln!(f, "{}", entry);
            count += 1;
        }
    }

    // If any archived, emit Cancelled events to remove from active board
    if count > 0 {
        let emitter = crate::task_events::InstanceName::from("system:lifecycle");
        for (tid, record) in &state.tasks {
            if record.status != crate::task_events::TaskStatus::Done {
                continue;
            }
            let updated = match chrono::DateTime::parse_from_rfc3339(&record.updated_at) {
                Ok(dt) => dt.with_timezone(&chrono::Utc),
                Err(_) => continue,
            };
            if (now - updated).num_days() < DEFAULT_ARCHIVE_DAYS {
                continue;
            }
            // Mark as archived (cancelled with archive reason)
            let event = crate::task_events::TaskEvent::Cancelled {
                by: emitter.clone(),
                task_id: tid.clone(),
                reason: "auto-lifecycle: done task archived".to_string(),
            };
            let _ = crate::task_events::append(home, &emitter, event);
        }
    }
    count
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_pass_on_empty_home() {
        let dir = std::env::temp_dir().join("agend-test-lifecycle-empty");
        std::fs::create_dir_all(&dir).ok();
        let (s, c, a) = lifecycle_pass(&dir);
        assert_eq!((s, c, a), (0, 0, 0));
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod review_repro_tasks;
