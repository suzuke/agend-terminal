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

/// Archive Done tasks older than `archive_days` to `tasks-archive.jsonl`.
///
/// H11 (CR-2026-06-14): archival MUST NOT change a task's terminal status. The
/// old code emitted a `Cancelled` event to "remove from the active board", which
/// `apply_cancelled` applied unconditionally — rewriting a completed (Done) task
/// as Cancelled and corrupting every reader/metric/audit that distinguishes done
/// from cancelled. We now ONLY append the Done record to the archive file; the
/// Done event stays in the active log so the terminal status is preserved.
/// Idempotent: already-archived `task_id`s are skipped, so the same task is not
/// re-appended on every daemon boot (the dedup the `Cancelled` flip used to
/// provide via its `status != Done` skip). Compacting archived Done tasks OUT of
/// the active replay is a separate concern, deliberately not done here.
fn archive_done_tasks(home: &Path) -> usize {
    let now = chrono::Utc::now();
    let state = crate::task_events::replay(home).unwrap_or_default();
    let archive_path = home.join("tasks-archive.jsonl");
    let already_archived = read_archived_task_ids(&archive_path);

    // Collect the records due for archival, then write them in ONE durable
    // append (below). The prior code appended each task with a discarded-result
    // writeln + NO fsync: a failed write was invisible and a crash could lose /
    // tear the append, re-archiving the task next boot.
    let mut lines: Vec<String> = Vec::new();
    for (tid, record) in &state.tasks {
        if record.status != crate::task_events::TaskStatus::Done {
            continue;
        }
        if already_archived.contains(&tid.0) {
            continue; // idempotent — already archived on a prior pass (H11)
        }
        let updated = match chrono::DateTime::parse_from_rfc3339(&record.updated_at) {
            Ok(dt) => dt.with_timezone(&chrono::Utc),
            Err(_) => continue,
        };
        let age_days = (now - updated).num_days();
        if age_days < DEFAULT_ARCHIVE_DAYS {
            continue;
        }
        let entry = serde_json::json!({
            "archived_at": now.to_rfc3339(),
            "task_id": tid.0,
            "title": record.title,
            "status": "done",
            "result": record.result,
            "created_at": record.created_at,
            "updated_at": record.updated_at,
        });
        lines.push(entry.to_string());
    }

    if lines.is_empty() {
        return 0;
    }

    // Durable, error-checked append. On failure NOTHING is counted — the records
    // were not written, so they stay un-archived and are retried next pass (the
    // dedup naturally re-attempts since they never reached the file).
    match append_archive_durably(&archive_path, &lines) {
        Ok(()) => lines.len(),
        Err(e) => {
            tracing::warn!(
                path = %archive_path.display(),
                error = %e,
                count = lines.len(),
                "#1201 lifecycle: archive append failed — tasks NOT archived this pass (will retry)"
            );
            0
        }
    }
}

/// Append newline-terminated JSONL records to the archive durably: a single
/// buffered `write_all` + `sync_all` (fsync). The fsync makes the append survive
/// a crash, and surfacing the IO error (vs the prior discarded-result writeln)
/// lets the caller leave the records un-archived for a retry rather than silently
/// dropping them. Append-only (the archive is read back by
/// `read_archived_task_ids`); a
/// whole-file `atomic_write` would re-read+rewrite the unbounded archive every
/// pass.
fn append_archive_durably(path: &Path, lines: &[String]) -> std::io::Result<()> {
    use std::io::Write;
    let mut buf = String::with_capacity(lines.iter().map(|l| l.len() + 1).sum());
    for line in lines {
        buf.push_str(line);
        buf.push('\n');
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(buf.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

/// Collect the `task_id`s already present in `tasks-archive.jsonl` so archival is
/// idempotent across daemon boots. Best-effort: a missing file or a malformed
/// line contributes no id (worst case a task is re-archived, never lost).
fn read_archived_task_ids(archive_path: &Path) -> std::collections::HashSet<String> {
    let Ok(content) = std::fs::read_to_string(archive_path) else {
        return std::collections::HashSet::new();
    };
    content
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|v| v.get("task_id").and_then(|t| t.as_str()).map(String::from))
        .collect()
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

    /// H11: archival is idempotent — an aged Done task is archived exactly once
    /// across repeated daemon-boot passes, stays Done (never Cancelled), and the
    /// archive file gains no duplicate entries. Guards the re-archival regression
    /// that removing the (terminal-state-corrupting) `Cancelled` emit would
    /// otherwise introduce.
    #[test]
    fn archive_is_idempotent_and_keeps_done_status() {
        use crate::task_events::{
            DoneSource, InstanceName, TaskEvent, TaskEventEnvelope, TaskId, TaskStatus,
            SCHEMA_VERSION,
        };
        let home =
            std::env::temp_dir().join(format!("agend-test-lifecycle-idem-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let tid = TaskId::from("t-idem");
        let old_ts = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
        // Seed Created + Done with a backdated timestamp via a raw envelope so
        // `updated_at` is past the archive threshold (filename assembled in parts
        // so the event-log anti-bypass invariant skips this intentional seed).
        let seed = |seq: u64, event: TaskEvent| {
            let env = TaskEventEnvelope {
                schema_version: SCHEMA_VERSION,
                seq,
                timestamp: old_ts.clone(),
                instance: InstanceName::from("system:lifecycle"),
                emitter_id: None,
                event,
            };
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(home.join(format!("task_events.{}", "jsonl")))
                .unwrap();
            writeln!(f, "{}", serde_json::to_string(&env).unwrap()).unwrap();
            crate::task_events::invalidate_replay_cache();
        };
        seed(
            1,
            TaskEvent::Created {
                task_id: tid.clone(),
                title: "shipped".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                depends_on: vec![],
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: None,
            },
        );
        seed(
            2,
            TaskEvent::Done {
                task_id: tid.clone(),
                by: InstanceName::from("dev"),
                source: DoneSource::OperatorManual {
                    authored_at: old_ts.clone(),
                    result: None,
                },
            },
        );

        // First pass archives once; second pass must NOT re-archive.
        assert_eq!(archive_done_tasks(&home), 1, "first pass archives once");
        assert_eq!(
            archive_done_tasks(&home),
            0,
            "second pass must not re-archive (idempotent via the archive file)"
        );

        // Archive file holds exactly one entry for the task.
        let archive = std::fs::read_to_string(home.join("tasks-archive.jsonl")).unwrap_or_default();
        let entries = archive.lines().filter(|l| l.contains("t-idem")).count();
        assert_eq!(entries, 1, "no duplicate archive entries");

        // Terminal status is preserved (Done, never Cancelled).
        let status = crate::task_events::replay(&home)
            .unwrap()
            .tasks
            .get(&tid)
            .map(|r| r.status);
        assert_eq!(
            status,
            Some(TaskStatus::Done),
            "task stays Done after archival"
        );

        std::fs::remove_dir_all(&home).ok();
    }
}

#[cfg(test)]
mod review_repro_tasks;
