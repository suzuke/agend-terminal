//! Task board — fleet-wide task tracking via JSON file.

mod acl;
mod handler;
mod orphan;
mod sweep;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests.rs"]
mod tests;

use serde::{Deserialize, Serialize};
use std::path::Path;

pub use handler::handle;
pub use orphan::{orphan_tasks_for_owner, reconcile_orphan_owners_with_live};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: String,   // open, claimed, done, blocked, cancelled
    pub priority: String, // low, normal, high, urgent
    pub assignee: Option<String>,
    /// When assignee is a team name, this holds the resolved orchestrator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routed_to: Option<String>,
    pub created_by: String,
    pub depends_on: Vec<String>,
    pub result: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<String>,
    /// Git branch the implementer should work on. Set by orchestrator at
    /// dispatch; reviewer uses this to scope `checkout_repo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// RFC3339 timestamp captured the first time `status`
    /// transitions to `in_progress` via `TaskEvent::InProgress`.
    /// Used by the daemon-side anti-stall scanner to compute
    /// elapsed time against `eta_secs`. `None` for tasks that
    /// never reached in_progress OR pre-existed Sprint 59 schema
    /// migration.
    ///
    /// #807 Item 3: renamed `dispatched_at` → `started_at`. The
    /// value is stamped on first InProgress (post-claim), not at
    /// `send()` dispatch — old name was misleading. `serde(alias)`
    /// preserves replay of legacy persisted JSON.
    #[serde(
        default,
        alias = "dispatched_at",
        skip_serializing_if = "Option::is_none"
    )]
    pub started_at: Option<String>,
    /// Sprint 59 Wave 1 PR-1 (#9 task stall watchdog) — operator-
    /// supplied estimate of seconds to completion. The anti-stall
    /// scanner emits a `task_stalled` inbox event when elapsed time
    /// since `last_progress_at` exceeds `eta_secs * 1.5`. `None`
    /// means "no stall detection for this task" — emit suppressed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<i64>,
    /// #870 — per-task opt-out for the daemon's
    /// `auto_release_on_verdict` flow. When `Some(false)`, the
    /// supervisor's `AutoReleaseTracker` skips this task even if a
    /// VERIFIED verdict has been enqueued (operator workflows that
    /// chain follow-up PRs on the same branch can disable the
    /// auto-release so the binding survives until they manually
    /// release). `None` (default) and `Some(true)` both enable
    /// auto-release. r0 NOTE: the operator-write surface (MCP /
    /// TaskEvent::Created) is deferred to a follow-up PR; the field
    /// reads correctly via `record_to_task` but cannot yet be set
    /// through any production code path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_release_on_verdict: Option<bool>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TaskStore {
    #[serde(default)]
    schema_version: u32,
    tasks: Vec<Task>,
}

impl crate::store::SchemaVersioned for TaskStore {
    const CURRENT: u32 = 1;
    fn version_mut(&mut self) -> &mut u32 {
        &mut self.schema_version
    }
}

fn store_path(home: &Path) -> std::path::PathBuf {
    crate::store::store_path(home, "tasks.json")
}

fn load(home: &Path) -> TaskStore {
    crate::store::load_versioned(
        &store_path(home),
        <TaskStore as crate::store::SchemaVersioned>::CURRENT,
    )
}

/// Evaluate dependency status for a single task.
/// Returns the effective status after considering depends_on:
/// - open + any dep not done → "blocked"
/// - blocked + all deps done → "open" (auto-unblock)
/// - claimed/done/cancelled → unchanged
///
/// Uses a visited set to prevent infinite loops on circular deps
/// (circular → treated as blocked).
pub fn evaluate_dependency_status(tasks: &[Task], task: &Task) -> String {
    if task.depends_on.is_empty()
        || matches!(task.status.as_str(), "claimed" | "done" | "cancelled")
    {
        return task.status.clone();
    }
    let all_deps_done = task.depends_on.iter().all(|dep_id| {
        tasks
            .iter()
            .find(|t| t.id == *dep_id)
            .map(|t| t.status == "done")
            .unwrap_or(false) // missing dep → not done → blocked
    });
    if all_deps_done {
        if task.status == "blocked" {
            "open".to_string()
        } else {
            task.status.clone()
        }
    } else {
        "blocked".to_string()
    }
}

/// PR3 — option (a) from m-42: in-memory derived dep eval. Computed at
/// list-time, **not** persisted as Blocked/Unblocked events. The event
/// log captures only explicit operator/agent transitions; dep-derived
/// status is a view-layer concern, not part of the canonical history.
fn apply_dependency_eval_in_memory(tasks: &mut [Task]) {
    let snapshot: Vec<Task> = tasks.to_vec();
    for task in tasks.iter_mut() {
        let effective = evaluate_dependency_status(&snapshot, task);
        if effective != task.status {
            task.status = effective;
        }
    }
}

/// Convert a replay-derived [`crate::task_events::TaskRecord`] into the
/// public [`Task`] surface consumed by every reader (TUI render,
/// status_summary, MCP `task list`, etc.). The mapping is lossless for
/// fields tasks.json carried; v2 newtype wrappers unwrap to their inner
/// strings.
pub(super) fn record_to_task(r: &crate::task_events::TaskRecord) -> Task {
    Task {
        id: r.id.0.clone(),
        title: r.title.clone(),
        description: r.description.clone(),
        status: status_to_legacy_str(r.status).to_string(),
        priority: r.priority.clone(),
        assignee: r.owner.as_ref().map(|i| i.0.clone()),
        routed_to: r.routed_to.as_ref().map(|i| i.0.clone()),
        created_by: r.created_by.0.clone(),
        depends_on: r.depends_on.iter().map(|t| t.0.clone()).collect(),
        result: r.result.clone(),
        created_at: r.created_at.clone(),
        updated_at: r.updated_at.clone(),
        due_at: r.due_at.clone(),
        branch: r.branch.clone(),
        started_at: r.started_at.clone(),
        eta_secs: r.eta_secs,
        // #870 — TaskRecord does not yet carry this field; r0 always
        // returns `None` (= auto-release enabled). Future PR adds an
        // event variant + record field if/when an operator-write
        // surface lands.
        auto_release_on_verdict: None,
    }
}

pub(super) fn status_to_legacy_str(s: crate::task_events::TaskStatus) -> &'static str {
    use crate::task_events::TaskStatus;
    match s {
        TaskStatus::Open => "open",
        TaskStatus::Claimed => "claimed",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Verified => "verified",
        TaskStatus::Done => "done",
        TaskStatus::Cancelled => "cancelled",
        TaskStatus::Blocked => "blocked",
    }
}

pub fn load_by_id(home: &Path, task_id: &str) -> Option<Task> {
    handler::read_task_record(home, task_id).map(|r| record_to_task(&r))
}

/// Return all tasks as typed structs. **PR3 cutover** — sources state
/// from `task_events::replay()` instead of the legacy `tasks.json`.
/// Dep-derived blocking is computed in-memory at this call (option (a)
/// per m-42); explicit operator-emitted Blocked/Unblocked events are
/// honoured by replay's `apply()` as before.
pub fn list_all(home: &Path) -> Vec<Task> {
    let state = crate::task_events::replay(home).unwrap_or_default();
    let mut tasks: Vec<Task> = state.tasks.values().map(record_to_task).collect();
    apply_dependency_eval_in_memory(&mut tasks);
    tasks
}

/// Sweep overdue claimed tasks back to open by **emitting `Released`
/// events** (PR3 cutover from tasks.json mutation). `Released` is
/// distinct from `Reopened`: it clears the owner (claim is gone),
/// while `Reopened` preserves owner for done→open re-work.
///
/// Returns the IDs of tasks released. Errors emitting events are logged
/// but don't abort the sweep — the affected task simply stays Claimed
/// until the next maintenance pass retries.
pub fn sweep_overdue_claimed(home: &Path) -> Vec<String> {
    let now = chrono::Utc::now();
    let state = crate::task_events::replay(home).unwrap_or_default();
    let emitter = crate::task_events::InstanceName::from("system:overdue_sweep");
    let mut released = Vec::new();

    for (tid, record) in &state.tasks {
        if record.status != crate::task_events::TaskStatus::Claimed {
            continue;
        }
        let due = match &record.due_at {
            Some(d) => d,
            None => continue,
        };
        let due_utc = match chrono::DateTime::parse_from_rfc3339(due) {
            Ok(dt) => dt.with_timezone(&chrono::Utc),
            Err(_) => continue,
        };
        if now <= due_utc {
            continue;
        }
        let event = crate::task_events::TaskEvent::Released {
            task_id: tid.clone(),
            reason: format!("overdue claim past due_at={due}"),
        };
        match crate::task_events::append(home, &emitter, event) {
            Ok(_) => released.push(tid.0.clone()),
            Err(e) => {
                tracing::warn!(error = %e, task = %tid, "overdue sweep: append failed; will retry next pass");
            }
        }
    }
    released
}

/// Result of [`migrate_legacy_tasks_json_to_event_log`]. Reported back to
/// daemon startup logs so operators see how many legacy tasks crossed the
/// PR2 bridge into the canonical event log.
#[derive(Debug, Clone)]
pub struct MigrationReport {
    pub migrated: usize,
    pub skipped: usize,
}

/// Sprint 24 P0 PR2 — bridge-phase migration. Walks the legacy
/// `tasks.json` and emits canonical `TaskEvent`s into `task_events.jsonl`
/// for every task whose `task_id` doesn't already appear in the event
/// log. Idempotent: re-running on already-migrated state is a no-op.
///
/// Runs synchronously at daemon startup before the first MCP tool call
/// so operators never observe a "list empty during migration" race.
///
/// **Bridge phase note**: PR2 keeps `tasks.json` writes (via dual-write
/// in [`handle`]). PR3 retires `tasks.json` entirely; this migration is
/// the one-shot that ensures replay() at PR3 cutover sees every legacy
/// task. Re-running after PR3 cutover (when `tasks.json` is gone) is a
/// no-op via the empty-store branch.
pub fn migrate_legacy_tasks_json_to_event_log(home: &Path) -> anyhow::Result<MigrationReport> {
    let store = load(home);
    if store.tasks.is_empty() {
        return Ok(MigrationReport {
            migrated: 0,
            skipped: 0,
        });
    }
    let state = crate::task_events::replay(home).unwrap_or_default();
    let migrator = crate::task_events::InstanceName::from("system:legacy_migration");
    let mut events: Vec<crate::task_events::TaskEvent> = Vec::new();
    let mut migrated = 0usize;
    let mut skipped = 0usize;

    for t in &store.tasks {
        let tid = crate::task_events::TaskId(t.id.clone());
        if state.tasks.contains_key(&tid) {
            // Already in event log — idempotent skip.
            skipped += 1;
            continue;
        }
        events.push(crate::task_events::TaskEvent::Created {
            task_id: tid.clone(),
            title: t.title.clone(),
            description: t.description.clone(),
            priority: t.priority.clone(),
            owner: t
                .assignee
                .as_ref()
                .map(|s| crate::task_events::InstanceName(s.clone())),
            due_at: t.due_at.clone(),
            depends_on: t
                .depends_on
                .iter()
                .map(|s| crate::task_events::TaskId(s.clone()))
                .collect(),
            routed_to: t
                .routed_to
                .as_ref()
                .map(|s| crate::task_events::InstanceName(s.clone())),
            branch: t.branch.clone(),
            // Sprint 59 Wave 1 PR-1: legacy migration has no eta value;
            // default to None — disables stall detection on migrated
            // tasks (pre-watchdog tasks weren't created with an ETA).
            eta_secs: None,
            // Sprint 55 P0-C: legacy migration has no bind value; default
            // to None which preserves current auto-bind on subsequent
            // dispatches referencing this task_id.
            bind: None,
        });
        // Emit the minimum status-transition events to bring the task to
        // its current legacy status. The replay-derived view post-PR3
        // cutover sees the same final state as the legacy tasks.json.
        match t.status.as_str() {
            "claimed" => {
                if let Some(by) = &t.assignee {
                    events.push(crate::task_events::TaskEvent::Claimed {
                        task_id: tid.clone(),
                        by: crate::task_events::InstanceName(by.clone()),
                    });
                }
            }
            "in_progress" => {
                if let Some(by) = &t.assignee {
                    events.push(crate::task_events::TaskEvent::Claimed {
                        task_id: tid.clone(),
                        by: crate::task_events::InstanceName(by.clone()),
                    });
                    events.push(crate::task_events::TaskEvent::InProgress {
                        task_id: tid.clone(),
                        by: crate::task_events::InstanceName(by.clone()),
                    });
                }
            }
            "done" => {
                let by = t
                    .assignee
                    .as_deref()
                    .unwrap_or(t.created_by.as_str())
                    .to_string();
                events.push(crate::task_events::TaskEvent::Done {
                    task_id: tid.clone(),
                    by: crate::task_events::InstanceName(by),
                    source: crate::task_events::DoneSource::OperatorManual {
                        authored_at: t.updated_at.clone(),
                        result: t.result.clone(),
                    },
                });
            }
            "cancelled" => {
                events.push(crate::task_events::TaskEvent::Cancelled {
                    task_id: tid.clone(),
                    by: crate::task_events::InstanceName(t.created_by.clone()),
                    reason: "migrated from legacy tasks.json (status was cancelled)".to_string(),
                });
            }
            "blocked" => {
                events.push(crate::task_events::TaskEvent::Blocked {
                    task_id: tid.clone(),
                    reason: "migrated from legacy tasks.json (status was blocked)".to_string(),
                });
            }
            // "open" or unknown: Created already left the task at Open.
            _ => {}
        }
        migrated += 1;
    }

    if !events.is_empty() {
        crate::task_events::append_batch(home, &migrator, events)?;
    }
    // PR4 — retire tasks.json: rename the live file to a `.legacy_pre_v2`
    // sidecar so the operator can archeologically inspect the
    // pre-migration state, but the daemon's read path (now via
    // `task_events::replay()`) no longer touches it. Idempotent: if the
    // file is already absent the rename silently no-ops. Only triggers
    // when the migration found legacy data — otherwise leaving the empty
    // file in place avoids surprising the operator with sudden file
    // disappearance.
    if migrated > 0 {
        let live = store_path(home);
        if live.exists() {
            let archive = live.with_extension("json.legacy_pre_v2");
            if let Err(e) = std::fs::rename(&live, &archive) {
                tracing::warn!(
                    error = %e,
                    "task_events: post-migration rename of tasks.json failed; legacy file remains in place"
                );
            } else {
                tracing::info!(
                    archive = %archive.display(),
                    "task_events: legacy tasks.json archived to .legacy_pre_v2"
                );
            }
        }
    }
    Ok(MigrationReport { migrated, skipped })
}
