//! Task board — fleet-wide task tracking via JSON file.

mod acl;
pub mod auto_close;
mod board_router;
mod handler;
pub mod lifecycle;
mod orphan;
mod sweep;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests.rs"]
mod tests;

use serde::{Deserialize, Serialize};
use std::path::Path;

pub use handler::handle;
pub use handler::register_subscriber as register_cascade_subscriber;
// #2117 P2: resolution helpers for the out-of-`tasks` callers — comms dispatch
// auto-create (target board) and the per-board task sweep (project id from a
// team's source_repo).
pub(crate) use board_router::{
    project_id_from_source_repo, resolve_target_project, resolve_task_project,
};
// #2127 Phase 1 / #2117 P3: shared per-board mutation ACL primitive, re-exported
// for callers outside the `tasks` module (the reclaim per-tick handler).
pub(crate) use acl::can_mutate_on_board;
pub use orphan::{
    cancel_tasks_for_owner, orphan_tasks_for_owner, reconcile_orphan_owners_with_live,
    release_inprogress_orphans_with_live,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: crate::task_events::TaskStatus,
    pub priority: crate::task_events::TaskPriority,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub metadata: std::collections::BTreeMap<String, serde_json::Value>,
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

/// #2117 Q2 cross-board dependency resolver. Resolves a `depends_on` id's
/// PERSISTED status, reaching beyond the local board when a dep isn't found
/// locally. Built once per eval pass with a per-board replay cache, so a board
/// with many cross-board deps replays each foreign board at most once (no
/// per-dep replay explosion). Single-project / same-board deps resolve from
/// `local` alone and never touch the filesystem → byte-identical to the
/// pre-#2117 path.
///
/// We only ever ask "is this dep Done". `Done` is a PERSISTED status (never
/// dep-derived), so a raw cross-board replay is authoritative — no need to
/// recursively dep-evaluate a foreign task (and no cross-board cycles).
struct DepResolver<'a> {
    home: &'a Path,
    /// The board the current pass's tasks live on. A dep that resolves back to
    /// this board is, by construction, already covered by `local` (absent here =
    /// absent there) → skip the redundant replay.
    local_board: &'a Path,
    local: std::collections::HashMap<String, crate::task_events::TaskStatus>,
    /// Memoized `resolve_task_project` per dep_id (its index-miss fallback is a
    /// full-board scan — resolve each distinct dep at most once per pass).
    proj_cache: std::collections::HashMap<String, String>,
    /// Lazily-replayed foreign boards: board path → {task_id → status}.
    board_cache: std::collections::HashMap<
        std::path::PathBuf,
        std::collections::HashMap<String, crate::task_events::TaskStatus>,
    >,
}

impl<'a> DepResolver<'a> {
    fn new(home: &'a Path, local_board: &'a Path, snapshot: &[Task]) -> Self {
        let local = snapshot.iter().map(|t| (t.id.clone(), t.status)).collect();
        Self {
            home,
            local_board,
            local,
            proj_cache: std::collections::HashMap::new(),
            board_cache: std::collections::HashMap::new(),
        }
    }

    /// Persisted status of `dep_id`, or `None` if it exists nowhere reachable
    /// (treated as not-done → blocking, matching the pre-#2117 missing-dep rule).
    fn status_of(&mut self, dep_id: &str) -> Option<crate::task_events::TaskStatus> {
        if let Some(s) = self.local.get(dep_id) {
            return Some(*s);
        }
        let project = self
            .proj_cache
            .entry(dep_id.to_string())
            .or_insert_with(|| board_router::resolve_task_project(self.home, dep_id))
            .clone();
        let board = crate::task_events::board_root(self.home, &project);
        if board.as_path() == self.local_board {
            // Resolves back to the local board → already covered by `local`
            // (and definitively absent, since the local check above missed).
            return None;
        }
        let map = self.board_cache.entry(board.clone()).or_insert_with(|| {
            crate::task_events::replay_at(&board)
                .map(|st| {
                    st.tasks
                        .iter()
                        .map(|(id, r)| (id.0.clone(), r.status))
                        .collect()
                })
                .unwrap_or_default()
        });
        map.get(dep_id).copied()
    }
}

/// Evaluate dependency status for a single task against a [`DepResolver`].
/// - open + any dep not done → "blocked"
/// - blocked + all deps done → "open" (auto-unblock)
/// - claimed/done/cancelled → unchanged
fn evaluate_with_resolver(
    resolver: &mut DepResolver,
    task: &Task,
) -> crate::task_events::TaskStatus {
    use crate::task_events::TaskStatus;
    if task.depends_on.is_empty()
        || matches!(
            task.status,
            TaskStatus::Claimed | TaskStatus::Done | TaskStatus::Cancelled
        )
    {
        return task.status;
    }
    let all_deps_done = task
        .depends_on
        .iter()
        .all(|dep_id| resolver.status_of(dep_id) == Some(TaskStatus::Done));
    if all_deps_done {
        if task.status == TaskStatus::Blocked {
            TaskStatus::Open
        } else {
            task.status
        }
    } else {
        TaskStatus::Blocked
    }
}

/// PR3 — option (a) from m-42: in-memory derived dep eval. Computed at
/// list-time, **not** persisted as Blocked/Unblocked events. The event
/// log captures only explicit operator/agent transitions; dep-derived
/// status is a view-layer concern, not part of the canonical history.
///
/// #2117 Q2: `home` + `board` enable cross-board dep resolution — a dep on
/// another project's board is read from that board (cached), so a satisfied
/// cross-board dependency auto-unblocks and an unsatisfied one blocks. `board`
/// is the board these `tasks` were replayed from. Single-project → every dep is
/// local → byte-identical.
fn apply_dependency_eval_in_memory(tasks: &mut [Task], home: &Path, board: &Path) {
    let snapshot: Vec<Task> = tasks.to_vec();
    let mut resolver = DepResolver::new(home, board, &snapshot);
    for task in tasks.iter_mut() {
        let effective = evaluate_with_resolver(&mut resolver, task);
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
        status: r.status,
        priority: serde_json::from_value(serde_json::Value::String(r.priority.clone()))
            .unwrap_or_default(),
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
        tags: r.tags.clone(),
        parent_id: r.parent_id.as_ref().map(|t| t.0.clone()),
        metadata: r.metadata.clone(),
    }
}

pub(super) fn status_to_legacy_str(s: crate::task_events::TaskStatus) -> &'static str {
    use crate::task_events::TaskStatus;
    match s {
        TaskStatus::Backlog => "backlog",
        TaskStatus::Open => "open",
        TaskStatus::Claimed => "claimed",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::InReview => "in_review",
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
    list_all_at(home, home)
}

/// #2117 P1: list a specific board's tasks (board-root replay + in-memory dep
/// eval). `list_all(home)` is exactly `list_all_at(home, home)` — byte-identical,
/// since `home` is the default board root (`board_root(home, DEFAULT)`).
///
/// #2117 Q2: `home` is threaded so the in-memory dep eval can resolve a task's
/// `depends_on` across project boards (a dep on another board is read from that
/// board, cached per pass). For single-project deployments every dep is on this
/// same board → no cross-board read → byte-identical.
pub(crate) fn list_all_at(home: &Path, board: &Path) -> Vec<Task> {
    let state = crate::task_events::replay_at(board).unwrap_or_default();
    let mut tasks: Vec<Task> = state.tasks.values().map(record_to_task).collect();
    apply_dependency_eval_in_memory(&mut tasks, home, board);
    tasks
}

/// #1942: link a git `branch` to an existing task by emitting a `BranchLinked`
/// event. Called from the dispatch path (`send kind=task` carrying `branch=`)
/// so a separately-created task — which starts with `branch: None` — gains the
/// task↔branch link that `auto_close_merged_tasks` needs to auto-close the task
/// when the branch merges (the #1942 lead-merges gap).
///
/// Returns `Ok(true)` if a `BranchLinked` event was emitted, `Ok(false)` if it
/// was a no-op (task absent, empty branch, or the branch is already linked —
/// idempotent so a re-dispatch of the same branch doesn't churn the log).
pub fn link_branch_to_task(home: &Path, task_id: &str, branch: &str) -> anyhow::Result<bool> {
    if branch.is_empty() || !task_id.starts_with("t-") {
        return Ok(false);
    }
    let state = crate::task_events::replay(home).unwrap_or_default();
    let tid = crate::task_events::TaskId(task_id.to_string());
    let Some(record) = state.tasks.get(&tid) else {
        return Ok(false);
    };
    if record.branch.as_deref() == Some(branch) {
        return Ok(false);
    }
    let emitter = crate::task_events::InstanceName::from("system:branch-link");
    crate::task_events::append_batch(
        home,
        &emitter,
        vec![crate::task_events::TaskEvent::BranchLinked {
            task_id: tid,
            by: emitter.clone(),
            branch: branch.to_string(),
        }],
    )?;
    tracing::info!(task_id, branch, "linked branch to task (#1942)");
    Ok(true)
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
            priority: t.priority.to_string(),
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
            tags: Vec::new(),
            parent_id: None,
        });
        // Emit the minimum status-transition events to bring the task to
        // its current legacy status. The replay-derived view post-PR3
        // cutover sees the same final state as the legacy tasks.json.
        match t.status {
            crate::task_events::TaskStatus::Claimed => {
                if let Some(by) = &t.assignee {
                    events.push(crate::task_events::TaskEvent::Claimed {
                        task_id: tid.clone(),
                        by: crate::task_events::InstanceName(by.clone()),
                    });
                }
            }
            crate::task_events::TaskStatus::InProgress => {
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
            crate::task_events::TaskStatus::Done => {
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
            crate::task_events::TaskStatus::Cancelled => {
                events.push(crate::task_events::TaskEvent::Cancelled {
                    task_id: tid.clone(),
                    by: crate::task_events::InstanceName(t.created_by.clone()),
                    reason: "migrated from legacy tasks.json (status was cancelled)".to_string(),
                });
            }
            crate::task_events::TaskStatus::Blocked => {
                events.push(crate::task_events::TaskEvent::Blocked {
                    task_id: tid.clone(),
                    reason: "migrated from legacy tasks.json (status was blocked)".to_string(),
                });
            }
            // Open or other statuses: Created already left the task at Open.
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
