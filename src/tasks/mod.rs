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

// #2760 RED (frozen-plan d-…-7): strict-routing contract for `load_routed`.
// Proven-failing against the checkpoint stub; the GREEN strict-resolution body
// turns them green. `#[path]` (mirroring the `tests` submodule above) marks this a
// cfg(test) module file so the task-events anti-bypass invariant recognizes it as
// test-only — the Unreadable RED deliberately makes a board's event log a directory.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "routing_red_2760.rs"]
mod routing_red_2760;

use serde::{Deserialize, Serialize};
use std::path::Path;

/// #78445-2 (d): the SINGLE terminal-cleanup seam for a task reaching a terminal
/// state (done / cancelled) via ANY writer — interactive done/update, auto-close,
/// the merged-PR sweep, the batch-cancel sweep, and the cascade child-cancel.
/// Clears BOTH obligation stores so no watchdog nags about closed work:
/// - the dispatch_idle sidecar (#1018 `cleanup_pending_for_task_id`), and
/// - the dispatch_tracking rows that drive the stuck-dispatch sweep
///   (`mark_completed`, matched by task_id → a co-dispatcher's OTHER task rows
///   survive).
///
/// Centralized (reviewer4 #2679): before this, only 3 of the 6 terminal writers
/// cleared even the sidecar and none cleared dispatch_tracking. Routing every
/// writer through one call means a new terminal path wires ONE line and cannot
/// silently leak either store.
pub(crate) fn task_terminal_cleanup(home: &Path, task_id: &str) {
    let _ = crate::daemon::dispatch_idle::cleanup_pending_for_task_id(home, task_id);
    crate::dispatch_tracking::remove_all_for_task(home, task_id);
}

pub use handler::handle;
pub(crate) use handler::handle_with_live_instances;
pub use handler::register_subscriber as register_cascade_subscriber;
// #2117 P2: resolution helpers for the out-of-`tasks` callers — comms dispatch
// auto-create (target board) and the per-board task sweep (project id from a
// team's source_repo).
// #2760: `board_for_task` / `resolve_task_project` (the LENIENT default-fallback
// seams) are no longer re-exported — every per-id authority path routes through
// the strict `load_routed` / `caller_can_mutate_task` above instead.
pub(crate) use board_router::{
    list_all_boards, list_all_strict, project_id_from_source_repo, resolve_target_project,
};
// #2760: the per-board mutation ACL is no longer re-exported for external callers
// (reclaim now routes through the strict `caller_can_mutate_task` above). It stays
// tasks-internal — `caller_can_mutate_task` and the handler ACL use `acl::` directly.
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
    /// #2760: memoized STRICT route per dep_id → the dep's authoritative board, or
    /// `None` when the route fails closed (NotFound/Unreadable/Ambiguous). Resolve
    /// each distinct dep at most once per pass (its index-miss path is a full scan).
    proj_cache: std::collections::HashMap<String, Option<std::path::PathBuf>>,
    /// Lazily-replayed foreign boards: board path → {task_id → status}.
    board_cache: std::collections::HashMap<
        std::path::PathBuf,
        std::collections::HashMap<String, crate::task_events::TaskStatus>,
    >,
}

// AUDIT2-014: test-only seam letting a regression test simulate a concurrent
// writer landing in the narrow TOCTOU window between `DepResolver::status_of`
// reading a foreign board's dep status and the local claim's commit (both
// happen under only the LOCAL board's lock — the foreign board is read
// lock-free). Armed via `set_after_foreign_dep_read_hook_for_test`; fires at
// most once (`take`), so uninstrumented tests are unaffected.
#[cfg(test)]
thread_local! {
    static AFTER_FOREIGN_DEP_READ_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
pub(crate) fn set_after_foreign_dep_read_hook_for_test(f: impl FnOnce() + 'static) {
    AFTER_FOREIGN_DEP_READ_HOOK.with(|h| *h.borrow_mut() = Some(Box::new(f)));
}

#[cfg(test)]
fn fire_after_foreign_dep_read_hook_for_test() {
    let hook = AFTER_FOREIGN_DEP_READ_HOOK.with(|h| h.borrow_mut().take());
    if let Some(f) = hook {
        f();
    }
}

// AUDIT2-014 (codex-reviewer, PR #2521): test-only seam for the B' detective's
// OWN scan→append TOCTOU — simulates a task legitimately advancing (e.g.
// Claimed → InProgress) between `reconcile_stale_cross_board_claims`'s stale
// scan and its checked commit. Fires at most once.
#[cfg(test)]
thread_local! {
    static BEFORE_CROSS_BOARD_RELEASE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
pub(crate) fn set_before_cross_board_release_hook_for_test(f: impl FnOnce() + 'static) {
    BEFORE_CROSS_BOARD_RELEASE_HOOK.with(|h| *h.borrow_mut() = Some(Box::new(f)));
}

#[cfg(test)]
fn fire_before_cross_board_release_hook_for_test() {
    let hook = BEFORE_CROSS_BOARD_RELEASE_HOOK.with(|h| h.borrow_mut().take());
    if let Some(f) = hook {
        f();
    }
}

// #2760 R2 (root+independent REJECT of a542517b): test-only seam for the per-id
// authority-mutation TOCTOU. Fires in the OUT-OF-LOCK window right before a mutation
// (`ack_plan` / `metadata_set`) takes its per-id + board locks, so a test can
// deterministically land a CONCURRENT mutation on the same id and prove the
// under-lock recompute (ack UNION / fresh authorization) is race-safe. The injected
// mutation completes fully (acquires+releases its own locks) BEFORE the instrumented
// caller locks, so there is no deadlock; fires at most once (`take`).
#[cfg(test)]
thread_local! {
    static BEFORE_MUTATION_COMMIT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
pub(crate) fn set_before_mutation_commit_hook_for_test(f: impl FnOnce() + 'static) {
    BEFORE_MUTATION_COMMIT_HOOK.with(|h| *h.borrow_mut() = Some(Box::new(f)));
}

#[cfg(test)]
pub(super) fn fire_before_mutation_commit_hook_for_test() {
    let hook = BEFORE_MUTATION_COMMIT_HOOK.with(|h| h.borrow_mut().take());
    if let Some(f) = hook {
        f();
    }
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
        // #2760: resolve the dep's board via the STRICT route (cached per dep_id).
        // A route error → `None` → the dep is not-reachable → not-Done → blocking
        // (the conservative pre-#2117 missing-dep rule); never a silent DEFAULT read.
        let board = self
            .proj_cache
            .entry(dep_id.to_string())
            .or_insert_with(|| {
                board_router::route_task(self.home, dep_id)
                    .map(|(_, board, _)| board)
                    .ok()
            })
            .clone();
        let board = board?;
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
        let status = map.get(dep_id).copied();
        // AUDIT2-014 regression seam: this is the exact TOCTOU window — a
        // foreign-board write landing right here (after we've read `status`
        // but before the caller's local claim commits) races the claim.
        // No-op unless a test arms the hook.
        #[cfg(test)]
        fire_after_foreign_dep_read_hook_for_test();
        status
    }
}

/// Evaluate the VIEW status of a single task against a [`DepResolver`] (decision
/// d-20260713172801677583-63). Dependency-derived blocking is view-only and applies
/// to RAW `Open` ONLY:
/// - raw Open + any dep not Done → "blocked" (view; the raw replay stays Open)
/// - raw Open + all deps Done (or no deps) → "open"
/// - EVERY other raw status (Blocked / InProgress / InReview / Verified / Backlog /
///   Claimed / Done / Cancelled) → UNCHANGED
///
/// So an EXPLICIT/persisted Blocked (operator, usage-limit, legacy migration) never
/// auto-unblocks when its deps finish, and non-Open statuses are never projected to
/// Blocked — get/list agree with the update response and the raw replay (no
/// split-brain). Dep-derived Blocked is never persisted (see
/// [`apply_dependency_eval_in_memory`]).
fn evaluate_with_resolver(
    resolver: &mut DepResolver,
    task: &Task,
) -> crate::task_events::TaskStatus {
    use crate::task_events::TaskStatus;
    // Only RAW Open is view-derived; everything else passes through unchanged.
    if task.status != TaskStatus::Open || task.depends_on.is_empty() {
        return task.status;
    }
    let all_deps_done = task
        .depends_on
        .iter()
        .all(|dep_id| resolver.status_of(dep_id) == Some(TaskStatus::Done));
    if all_deps_done {
        TaskStatus::Open
    } else {
        TaskStatus::Blocked
    }
}

/// PR3 — option (a) from m-42: in-memory derived dep eval. Computed at
/// list-time, **not** persisted as Blocked/Unblocked events. The event
/// log captures only explicit operator/agent transitions; dep-derived
/// status is a view-layer concern, not part of the canonical history.
/// This persistence contract is pinned by
/// `tests::cross_board_dep_derived_block_is_in_memory_not_persisted_2117_q2`
/// (a `replay_at` of a dep-blocked task's board still yields its un-derived
/// persisted status). Keeping the block in-memory is also what lets the
/// cross-board resolver stay acyclic: `Done` is the only persisted status it
/// reads, so a foreign-board replay is authoritative without recursive eval.
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

// ── #2760: strict routed task authority ────────────────────────────────
//
// The removed `load_by_id` seam read ONLY the default board
// (`read_task_record_at(home, id)`, where `home` IS the default board root), so a
// task on a per-project board was invisible to it — the t-…-35 live failure (a
// project-board task with `review_class=single` dispatched as
// `review_class_unspecified`). `load_routed` is its fail-closed replacement: it
// resolves the ONE board that authoritatively holds the id and NEVER silently
// falls back to the default board on a miss.

/// #2760: an opaque board identity carried by a [`RoutedTask`]. The inner path
/// and its constructor are PRIVATE to the `tasks` module (`pub(in crate::tasks)`),
/// so an external consumer receives a [`RoutedTask`] — a [`Task`] view — and can
/// never obtain a raw board `Path` to write to the wrong board. The high-level
/// `tasks` write operations (branch-link, usage-limit) that DO need the board
/// take it back through this opaque handle, not as a bare path.
#[derive(Debug, Clone)]
pub struct BoardRoot {
    /// The resolved project id (a read-only board LABEL — used by the per-board
    /// mutation ACL). NOT a filesystem path: exposing it via [`RoutedTask`] cannot
    /// let an external module write to a board.
    project: String,
    path: std::path::PathBuf,
}

impl BoardRoot {
    pub(in crate::tasks) fn new(project: String, path: std::path::PathBuf) -> Self {
        Self { project, path }
    }
    /// The board's project id (the ACL axis). `tasks`-private.
    pub(in crate::tasks) fn project(&self) -> &str {
        &self.project
    }
    /// The board's on-disk root. `tasks`-private so no external module can obtain a
    /// raw board `Path` to write to the wrong board (#2760 point 3/7).
    pub(in crate::tasks) fn path(&self) -> &Path {
        &self.path
    }
}

/// #2760 item 3: an opaque board identity for write-time route revalidation. Two
/// routes are the "same board" iff their `BoardKey`s are equal. It wraps the
/// project id (the board membership axis) but is `tasks`-private and compared only
/// by value — an external module can never mint one to spoof a route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardKey(String);

/// #2760 item 3: a fingerprint of a resolved strict route — the board PLUS the
/// task's incarnation (`created_at`). A mutation captures the fingerprint at route
/// time and [`RoutedTask::with_revalidated_board`] re-resolves the route under the
/// per-id lock and asserts it STILL fingerprints identically before appending. The
/// `created_at` component defends against id reuse (a task deleted and a NEW task
/// created with the same id would carry a different `created_at`, so a stale
/// mutation cannot be replayed against the fresh incarnation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteFingerprint {
    board: BoardKey,
    created_at: String,
}

impl RouteFingerprint {
    fn of(project: &str, record: &crate::task_events::TaskRecord) -> Self {
        Self {
            board: BoardKey(project.to_string()),
            created_at: record.created_at.clone(),
        }
    }
}

/// #2760: a task resolved through the strict router — the public [`Task`] view
/// PLUS the opaque board it authoritatively lives on. External consumers read
/// `.task`; the board identity is `tasks`-private, so no caller outside this
/// module can mutate the resolved task on some other board.
#[derive(Debug, Clone)]
pub struct RoutedTask {
    pub task: Task,
    /// The raw replay record for the routed task — carries fields the [`Task`]
    /// view omits (e.g. `block_reason`) that a write-owner (usage-limit) needs for
    /// an idempotency pre-check on the SAME single route (no re-load).
    record: crate::task_events::TaskRecord,
    board: BoardRoot,
    /// #2760 item 3: the route fingerprint captured at resolve time —
    /// [`RoutedTask::with_revalidated_board`] re-resolves under the per-id lock and
    /// refuses the write unless the fresh route fingerprints identically.
    fingerprint: RouteFingerprint,
}

impl RoutedTask {
    /// The opaque board this task was routed to — `tasks`-private, so only the
    /// high-level task operations (which own branch-link / usage-limit writes)
    /// can reach it.
    pub(in crate::tasks) fn board(&self) -> &BoardRoot {
        &self.board
    }

    /// The raw replay record (fields not projected into the [`Task`] view, e.g.
    /// `block_reason`). Read-only snapshot from the strict route.
    pub(crate) fn record(&self) -> &crate::task_events::TaskRecord {
        &self.record
    }

    /// #2760 items 2+3: run `write` under the per-task-ID router lock (OUTER),
    /// after RE-RESOLVING the strict route inside the lock and asserting it still
    /// fingerprints identically to this route (same board + incarnation). `write`
    /// receives the REVALIDATED board root and performs the actual append (which
    /// takes the board writer lock, INNER). The per-id lock guarantees no
    /// concurrent authority mutation on this id interleaves between the
    /// revalidation and the append; the fingerprint guarantees the board / task
    /// incarnation the caller decided against has not changed under it.
    ///
    /// A route error or fingerprint mismatch → `Err(TaskRouteError)` and `write`
    /// NEVER runs (no side effect) — every mutation consumer maps that to its
    /// fail-closed policy (deny, no task events). Any cascade/cleanup/notify the
    /// caller performs MUST run AFTER this returns (the per-id flock is dropped by
    /// then), never inside `write` — self-IPC under the flock is refused (#1629).
    pub(in crate::tasks) fn with_revalidated_board<T>(
        &self,
        home: &Path,
        write: impl FnOnce(&Path) -> T,
    ) -> Result<T, TaskRouteError> {
        let _id_lock = board_router::acquire_task_id_lock(home, &self.task.id).map_err(|e| {
            TaskRouteError::Unreadable {
                path: home.to_path_buf(),
                cause: format!(
                    "acquire per-task-id router lock for '{}': {e}",
                    self.task.id
                ),
            }
        })?;
        // Re-resolve the strict route UNDER the lock — the authoritative
        // revalidation. Any route failure (NotFound / Unreadable / Ambiguous) fails
        // the mutation closed.
        let (project, board, record) = board_router::route_task(home, &self.task.id)?;
        if RouteFingerprint::of(&project, &record) != self.fingerprint {
            return Err(TaskRouteError::Unreadable {
                path: board,
                cause: format!(
                    "route revalidation mismatch for '{}': board/incarnation changed under the \
                     per-id lock since it was resolved",
                    self.task.id
                ),
            });
        }
        Ok(write(&board))
    }

    /// #2760 R2: like [`RoutedTask::with_revalidated_board`] but the append EVENTS
    /// are COMPUTED from the FRESH under-lock replay ([`crate::task_events::append_batch_computed_at`]).
    /// A route-only revalidation (board/incarnation) is NOT enough for a mutation
    /// whose authority or payload depends on current task CONTENT — ownership,
    /// status, metadata, the ack set. This runs the caller's `compute` under the
    /// board writer lock (INNER; the per-id lock is OUTER), so it re-evaluates
    /// authorization AND builds the events (e.g. an idempotent `plan_acks` UNION,
    /// or a governance-policy re-check) against committed state that no concurrent
    /// writer can change between the decision and the write — closing the
    /// authorization/predicate TOCTOU. `compute` must be pure decision + event
    /// construction (no `api::call` under the flocks, #1629).
    pub(in crate::tasks) fn with_revalidated_computed<F>(
        &self,
        home: &Path,
        emitter: &crate::task_events::InstanceName,
        compute: F,
    ) -> Result<anyhow::Result<Result<Vec<u64>, String>>, TaskRouteError>
    where
        F: FnOnce(
            &crate::task_events::TaskBoardState,
        ) -> Result<Vec<crate::task_events::TaskEvent>, String>,
    {
        self.with_revalidated_board(home, |board| {
            crate::task_events::append_batch_computed_at(board, emitter, compute)
        })
    }
}

/// #2760: strict per-board mutation authorization for an EXTERNAL caller (the
/// reclaim per-tick handler). Resolves the task's authoritative board via the
/// strict route (fail-closed) and applies the per-board mutation ACL. `Err` on any
/// route failure so the caller DENIES (never mutates a task it cannot uniquely
/// route). Keeps the board identity `tasks`-private — the caller learns only the
/// yes/no authorization.
pub(crate) fn caller_can_mutate_task(
    home: &Path,
    caller: &str,
    task_id: &str,
) -> Result<bool, TaskRouteError> {
    let routed = load_routed(home, task_id)?;
    Ok(acl::can_mutate_on_board(
        home,
        caller,
        routed.board().project(),
    ))
}

// ── #2760 item 4: narrow task-bound authority ops for EXTERNAL modules ──────
//
// External modules (the daemon supervisor's usage-limit control, the per-tick
// reclaim handler) must NEVER obtain a raw board `Path` or a generic board
// append. They call these narrow, task-bound operations instead: the strict
// route, the per-id lock, the write-time revalidation, and the board append all
// happen INSIDE the `tasks` module — the caller supplies only domain intent
// (which task, what identity guard, what reason) and learns an outcome.

/// #2760 item 4: the identity a usage-limit block/recover is authorised against —
/// the task id plus the owner (`source`), the linked `branch`, and the
/// `episode_id` (the caller's `notification_id`). The board-side revalidation
/// asserts the routed task still carries this owner+branch (and, for recovery,
/// this episode) before mutating; a generation change → [`ApplyOutcome::Stale`].
/// (Binding-generation freshness is the CALLER's concern — it holds the binding
/// lock and checks it before calling — so it is deliberately NOT in this guard.)
#[derive(Debug, Clone)]
pub struct UsageLimitGuard {
    pub task_id: String,
    pub source: String,
    pub branch: String,
    pub episode_id: String,
}

/// #2760 item 4: the outcome of a narrow usage-limit op. `Applied` = the event(s)
/// committed; `AlreadyApplied` = the desired state already holds (idempotent
/// no-op); `Stale` = the routed task no longer matches the guard (owner/branch/
/// status/episode changed, or the route no longer resolves uniquely) → nothing
/// written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied,
    AlreadyApplied,
    Stale,
}

/// True iff the routed record still bears the guard's owner + branch and one of
/// `statuses`. The board-state counterpart of the supervisor's old
/// `task_matches_key`, now owned by `tasks` (the board authority).
fn task_matches_guard(
    record: &crate::task_events::TaskRecord,
    guard: &UsageLimitGuard,
    statuses: &[crate::task_events::TaskStatus],
) -> bool {
    record.owner.as_ref().map(|o| o.as_str()) == Some(guard.source.as_str())
        && record.branch.as_deref() == Some(guard.branch.as_str())
        && statuses.contains(&record.status)
}

/// #2760 item 4: block a task for a usage-limit episode on ITS authoritative board
/// (never the default board). Strict-resolves the route, short-circuits
/// idempotently if the task is already `Blocked` for this episode, then appends a
/// `Blocked` event under the per-id lock with write-time revalidation and a
/// commit-time guard (still `Claimed`/`InProgress`, still this owner+branch).
///
/// `reason` is the fully-formed block-reason payload the caller built (it embeds
/// the `episode_id`, so the idempotency check can recognise it). A route error /
/// revalidation mismatch / commit-guard failure → `Stale` (no event); only a real
/// board IO/replay failure returns `Err`.
pub fn apply_usage_limit_block(
    home: &Path,
    guard: &UsageLimitGuard,
    reason: String,
) -> anyhow::Result<ApplyOutcome> {
    let routed = match load_routed(home, &guard.task_id) {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!(task = %guard.task_id, route_err = %e,
                "#2760 usage-limit block: task route unresolved → Stale (no default-board write)");
            return Ok(ApplyOutcome::Stale);
        }
    };
    // Idempotent: already blocked for THIS episode (its id is embedded in the
    // committed block_reason) → nothing to do.
    if routed.record().status == crate::task_events::TaskStatus::Blocked
        && routed
            .record()
            .block_reason
            .as_deref()
            .is_some_and(|r| r.contains(&guard.episode_id))
    {
        return Ok(ApplyOutcome::AlreadyApplied);
    }
    let tid = crate::task_events::TaskId(guard.task_id.clone());
    let emitter = crate::task_events::InstanceName("system:usage-limit".into());
    let guard_for_check = guard.clone();
    let check_tid = tid.clone();
    let revalidated = routed.with_revalidated_board(home, |board| {
        crate::task_events::append_batch_checked_at(
            board,
            &emitter,
            vec![crate::task_events::TaskEvent::Blocked {
                task_id: tid.clone(),
                reason,
            }],
            move |fresh| {
                let record = fresh
                    .tasks
                    .get(&check_tid)
                    .ok_or_else(|| "task disappeared before usage-limit block".to_string())?;
                task_matches_guard(
                    record,
                    &guard_for_check,
                    &[
                        crate::task_events::TaskStatus::Claimed,
                        crate::task_events::TaskStatus::InProgress,
                    ],
                )
                .then_some(())
                .ok_or_else(|| "task generation changed before usage-limit block".to_string())
            },
        )
    });
    match revalidated {
        Ok(Ok(Ok(_))) => Ok(ApplyOutcome::Applied),
        // Commit-guard rejected (generation changed) → Stale, no event.
        Ok(Ok(Err(_))) => Ok(ApplyOutcome::Stale),
        // Real board IO / replay failure.
        Ok(Err(e)) => Err(e),
        // Route revalidation refused under the per-id lock → Stale.
        Err(route_err) => {
            tracing::warn!(task = %guard.task_id, %route_err,
                "#2760 usage-limit block: route revalidation failed → Stale");
            Ok(ApplyOutcome::Stale)
        }
    }
}

/// #2760 item 4: recover (unblock → back to InProgress) a usage-limit-blocked task
/// on its authoritative board. Idempotent if the task is already `InProgress` for
/// this owner+branch (the crash window where the Unblocked+InProgress append
/// committed but the episode-state persist did not). Otherwise appends
/// `[Unblocked, InProgress]` under the per-id lock with revalidation and a
/// commit-time guard (still `Blocked` for THIS episode, still this owner+branch).
pub fn recover_usage_limit_block(
    home: &Path,
    guard: &UsageLimitGuard,
) -> anyhow::Result<ApplyOutcome> {
    let routed = match load_routed(home, &guard.task_id) {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!(task = %guard.task_id, route_err = %e,
                "#2760 usage-limit recovery: task route unresolved → Stale");
            return Ok(ApplyOutcome::Stale);
        }
    };
    // Crash-window idempotency: the atomic Unblocked+InProgress already committed
    // but persisting the recovered episode state did not.
    if task_matches_guard(
        routed.record(),
        guard,
        &[crate::task_events::TaskStatus::InProgress],
    ) {
        return Ok(ApplyOutcome::AlreadyApplied);
    }
    let tid = crate::task_events::TaskId(guard.task_id.clone());
    let owner = crate::task_events::InstanceName(guard.source.clone());
    let emitter = crate::task_events::InstanceName("system:usage-limit".into());
    let guard_for_check = guard.clone();
    let check_tid = tid.clone();
    let revalidated = routed.with_revalidated_board(home, |board| {
        crate::task_events::append_batch_checked_at(
            board,
            &emitter,
            vec![
                crate::task_events::TaskEvent::Unblocked {
                    task_id: tid.clone(),
                },
                crate::task_events::TaskEvent::InProgress {
                    task_id: tid.clone(),
                    by: owner,
                },
            ],
            move |fresh| {
                let record = fresh
                    .tasks
                    .get(&check_tid)
                    .ok_or_else(|| "task disappeared before usage-limit recovery".to_string())?;
                if task_matches_guard(
                    record,
                    &guard_for_check,
                    &[crate::task_events::TaskStatus::Blocked],
                ) && record
                    .block_reason
                    .as_deref()
                    .is_some_and(|r| r.contains(&guard_for_check.episode_id))
                {
                    Ok(())
                } else {
                    Err("task generation changed before usage-limit recovery".to_string())
                }
            },
        )
    });
    match revalidated {
        Ok(Ok(Ok(_))) => Ok(ApplyOutcome::Applied),
        Ok(Ok(Err(_))) => Ok(ApplyOutcome::Stale),
        Ok(Err(e)) => Err(e),
        Err(route_err) => {
            tracing::warn!(task = %guard.task_id, %route_err,
                "#2760 usage-limit recovery: route revalidation failed → Stale");
            Ok(ApplyOutcome::Stale)
        }
    }
}

/// #2760 item 2 (reclaim BUG fix): emit a `Released` event for a reclaimed task on
/// ITS authoritative board, under the per-id lock with revalidation. The pre-#2760
/// reclaim handler appended `Released` to the DEFAULT board unconditionally, so a
/// project-board task's release was written where its own board's replay never saw
/// it (the task stayed Claimed forever). The commit-time guard only releases a task
/// still `Claimed`/`InProgress`. Returns `Ok(true)` iff the `Released` committed.
pub fn release_reclaimed_task(home: &Path, task_id: &str, reason: String) -> anyhow::Result<bool> {
    let routed = match load_routed(home, task_id) {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!(task = task_id, route_err = %e,
                "#2760 reclaim release: task route unresolved → skip (no default-board write)");
            return Ok(false);
        }
    };
    let tid = crate::task_events::TaskId(task_id.to_string());
    let emitter = crate::task_events::InstanceName::from("system:reclaim_usage_limit");
    let check_tid = tid.clone();
    let revalidated = routed.with_revalidated_board(home, |board| {
        crate::task_events::append_checked_at(
            board,
            &emitter,
            crate::task_events::TaskEvent::Released {
                task_id: tid.clone(),
                reason,
            },
            move |fresh| {
                let record = fresh
                    .tasks
                    .get(&check_tid)
                    .ok_or_else(|| "task disappeared before reclaim release".to_string())?;
                matches!(
                    record.status,
                    crate::task_events::TaskStatus::Claimed
                        | crate::task_events::TaskStatus::InProgress
                )
                .then_some(())
                .ok_or_else(|| {
                    "task no longer claimed/in-progress before reclaim release".to_string()
                })
            },
        )
    });
    match revalidated {
        Ok(Ok(Ok(_))) => Ok(true),
        Ok(Ok(Err(_))) => Ok(false),
        Ok(Err(e)) => Err(e),
        Err(route_err) => {
            tracing::warn!(task = task_id, %route_err,
                "#2760 reclaim release: route revalidation failed → skip");
            Ok(false)
        }
    }
}

/// #2760: why a strict route failed. There is deliberately NO variant that means
/// "fell back to the default board" — a route either names the ONE authoritative
/// board or fails closed here. Consumers map these to their own fail-closed
/// policy (deny / keep-obligation / keep-reserved-live); they must never treat an
/// error as "assume default".
// #2760 checkpoint: consumers migrate onto this in the GREEN step; until then
// the strict variants have no non-test constructor in the bin build.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum TaskRouteError {
    /// The id is present on NO readable board — a definitive absence (every board
    /// enumerated and replayed cleanly, none held the id).
    NotFound,
    /// A board's index or event log could not be read/parsed, so uniqueness could
    /// not be proven. Fails closed rather than guessing a board.
    Unreadable {
        path: std::path::PathBuf,
        cause: String,
    },
    /// The id resolves to more than one board (a duplicate id across boards, or
    /// conflicting distinct index entries) — there is no single authority.
    Ambiguous {
        candidates: Vec<String>,
        cause: String,
    },
}

impl std::fmt::Display for TaskRouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskRouteError::NotFound => write!(f, "task not found on any board"),
            TaskRouteError::Unreadable { path, cause } => {
                write!(f, "task route unreadable ({}): {cause}", path.display())
            }
            TaskRouteError::Ambiguous { candidates, cause } => {
                write!(f, "task route ambiguous across {candidates:?}: {cause}")
            }
        }
    }
}

impl std::error::Error for TaskRouteError {}

/// #2760 STRICT per-id task router — the fail-closed replacement for the
/// default-board [`load_by_id`] seam. Resolves the ONE board that authoritatively
/// holds `task_id` and returns its [`Task`] view + opaque board, or a typed
/// [`TaskRouteError`] that NEVER means "assume the default board".
///
/// Resolves the ONE board that authoritatively holds `task_id` (frozen-plan
/// point 2: indexed route replay-verified, else a checked scan of the default +
/// every project board — exactly-one hit = success, zero = NotFound, >1 =
/// Ambiguous, any replay/index failure = Unreadable) and returns its [`Task`] view
/// and its opaque board. A [`TaskRouteError`] NEVER means "assume the default
/// board": consumers map it to their own fail-closed policy.
pub(crate) fn load_routed(home: &Path, task_id: &str) -> Result<RoutedTask, TaskRouteError> {
    let (project, board, record) = board_router::route_task(home, task_id)?;
    let fingerprint = RouteFingerprint::of(&project, &record);
    Ok(RoutedTask {
        task: record_to_task(&record),
        record,
        board: BoardRoot::new(project, board),
        fingerprint,
    })
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
    // #2760: resolve the ONE authoritative board via the strict route and append
    // BranchLinked THERE under a checked precondition — never a silent default-board
    // write (the pre-#2760 body read `replay(home)`, invisible to project boards).
    // A route error is surfaced (logged) and writes nothing.
    let routed = match load_routed(home, task_id) {
        Ok(rt) => rt,
        Err(TaskRouteError::NotFound) => return Ok(false),
        Err(e) => {
            tracing::warn!(
                task_id, branch, route_err = %e,
                "#2760 branch-link skipped: task route unresolved (no default-board write)"
            );
            return Ok(false);
        }
    };
    if routed.task.branch.as_deref() == Some(branch) {
        return Ok(false);
    }
    let tid = crate::task_events::TaskId(task_id.to_string());
    let emitter = crate::task_events::InstanceName::from("system:branch-link");
    let branch_owned = branch.to_string();
    let closure_tid = tid.clone();
    // #2760 items 2+3: append under the per-id router lock with write-time route
    // revalidation. A route change under the lock (board/incarnation) fails closed
    // → no branch-link, no side effect (never a wrong-board write).
    let revalidated = routed.with_revalidated_board(home, |board| {
        crate::task_events::append_batch_checked_at(
            board,
            &emitter,
            vec![crate::task_events::TaskEvent::BranchLinked {
                task_id: tid,
                by: emitter.clone(),
                branch: branch_owned.clone(),
            }],
            move |fresh| match fresh.tasks.get(&closure_tid) {
                None => Err(format!(
                    "task '{}' disappeared before branch-link",
                    closure_tid.0
                )),
                // A concurrent link raced the same branch in → idempotent no-op.
                Some(record) if record.branch.as_deref() == Some(branch_owned.as_str()) => {
                    Err("branch already linked".to_string())
                }
                Some(_) => Ok(()),
            },
        )
    });
    match revalidated {
        // Committed the BranchLinked event.
        Ok(Ok(Ok(_))) => {
            tracing::info!(
                task_id,
                branch,
                "linked branch to task (#1942, #2760 routed)"
            );
            Ok(true)
        }
        // Precondition failed at commit (raced re-link / task gone) → no-op.
        Ok(Ok(Err(_))) => Ok(false),
        // Board IO / replay error inside the checked append.
        Ok(Err(e)) => Err(e),
        // Route revalidation refused under the lock (board/incarnation changed, or
        // the id no longer routes uniquely) → no write.
        Err(route_err) => {
            tracing::warn!(
                task_id, branch, %route_err,
                "#2760 branch-link skipped: route revalidation failed under the per-id lock"
            );
            Ok(false)
        }
    }
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

/// Result of [`reconcile_stale_cross_board_claims`].
// #2549: dead_code allowed — the per-tick `CrossBoardDepDetectiveHandler`
// wrapper that called this was retired (d-20260703021554626467-13), but this
// AUDIT2-014 reconcile backstop is standalone task-board machinery, not
// handler-specific glue, so it's kept (test-covered) rather than deleted.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct CrossBoardReconcileReport {
    /// Claimed-but-not-yet-InProgress tasks released back to Open because a
    /// cross-board dependency was found not-Done. Self-heals the AUDIT2-014
    /// claim-time TOCTOU (see [`DepResolver::status_of`]).
    pub released: Vec<String>,
    /// InProgress tasks whose cross-board dependency is not-Done. Reported
    /// only — work already underway is never auto-released.
    pub flagged_in_progress: Vec<String>,
}

/// AUDIT2-014 (B', daemon detective backstop, multi-board only): the claim
/// precondition validates a cross-board `depends_on` via a lock-free replay of
/// the FOREIGN board (`DepResolver::status_of`), while the local `Claimed`
/// commit lands under only the LOCAL board's lock. A foreign-board write
/// landing in that window (dep reopened/cancelled) lets a claim commit against
/// a dependency that is, by commit time, no longer Done — and because
/// `evaluate_with_resolver` short-circuits already-`Claimed` tasks, list-time
/// dep eval never revisits it (permanently stuck).
///
/// This is the periodic backstop that revisits what the claim-time check
/// cannot atomically guarantee across boards: every board's Claimed/InProgress
/// tasks that have at least one cross-board dependency are re-evaluated
/// against CURRENT foreign-board state.
/// - Claimed (not yet InProgress) + dep not Done → emit `Released` (clears
///   owner, back to Open — list-time eval then shows Blocked again until the
///   dep is Done, exactly like a task that was never wrongly claimed).
/// - InProgress + dep not Done → flagged only; work already underway is never
///   yanked out from under an agent.
///
/// Same-board deps are untouched (a same-board claim precondition already
/// validates atomically under one lock — no TOCTOU, no reconciliation needed;
/// touching them here would be a scope-creeping behavior change, not a fix).
#[allow(dead_code)] // #2549: see CrossBoardReconcileReport's dead_code note above
pub fn reconcile_stale_cross_board_claims(home: &Path) -> CrossBoardReconcileReport {
    use crate::task_events::{InstanceName, TaskEvent, TaskId, TaskStatus};
    let mut report = CrossBoardReconcileReport::default();
    let emitter = InstanceName::from("system:cross_board_dep_detective");

    // #2760 R1: an unenumerable boards/ dir → skip this reconcile pass entirely
    // (conservative: never release a claim we cannot fully survey).
    let projects = match board_router::enumerate_projects(home) {
        Ok(p) => p,
        Err(_) => return report,
    };
    for project in projects {
        let board = crate::task_events::board_root(home, &project);
        // RAW persisted state (`replay_at`, no in-memory dep derivation) — the
        // derived view (`list_all_at`/`list_all_boards`) would relabel an
        // InProgress task with an unsatisfied dep to `Blocked` before we ever
        // see it, hiding it from both the Claimed and InProgress arms below.
        let state = match crate::task_events::replay_at(&board) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let snapshot: Vec<Task> = state.tasks.values().map(record_to_task).collect();
        let candidates: Vec<&Task> = snapshot
            .iter()
            .filter(|t| {
                !t.depends_on.is_empty()
                    && matches!(t.status, TaskStatus::Claimed | TaskStatus::InProgress)
            })
            .collect();
        if candidates.is_empty() {
            continue;
        }
        let mut resolver = DepResolver::new(home, &board, &snapshot);
        for task in candidates {
            // #2760: classify cross-board via the STRICT route. A dep that cannot
            // be uniquely routed → treated as NOT cross-board (`unwrap_or(false)`) →
            // the detective skips it (never releases a claim on an unprovable dep).
            let has_cross_board_dep = task.depends_on.iter().any(|d| {
                board_router::route_task(home, d)
                    .map(|(dep_project, _, _)| dep_project != project)
                    .unwrap_or(false)
            });
            if !has_cross_board_dep {
                continue;
            }
            let all_done = task
                .depends_on
                .iter()
                .all(|d| resolver.status_of(d) == Some(TaskStatus::Done));
            if all_done {
                continue;
            }
            if task.status == TaskStatus::InProgress {
                report.flagged_in_progress.push(task.id.clone());
                tracing::warn!(
                    task_id = %task.id,
                    "AUDIT2-014: in-progress task's cross-board dependency is not Done — flagged, not released"
                );
                continue;
            }
            // codex-reviewer (PR #2521): a bare `append_at` here would commit the
            // release against this stale, unlocked `snapshot` — the exact scan→
            // append TOCTOU this whole audit is about, just relocated. Between the
            // scan above and this write, the task can legitimately transition
            // (Claimed → InProgress/Done) via a normal, properly-locked claim/
            // update — and an unconditional Released would then yank that
            // in-progress or completed work back to Open/ownerless. `append_checked_at`
            // re-validates against a FRESH replay under the board's lock: only if
            // the task is STILL Claimed with a STILL-unsatisfied cross-board dep
            // does the write land; any other outcome (now InProgress/Done/gone,
            // or the dep resolved) is a silent no-op here (a fresh flag/skip, not
            // an error).
            let task_id = task.id.clone();
            let project_for_check = project.clone();
            let event = TaskEvent::Released {
                task_id: TaskId(task_id.clone()),
                reason: "AUDIT2-014: cross-board dependency no longer Done (claim raced a foreign-board write)".to_string(),
            };
            // Regression seam for the detective's OWN scan→append TOCTOU
            // (codex-reviewer, PR #2521): fires right before the checked commit,
            // exactly where a test can simulate the task legitimately advancing
            // (e.g. Claimed → InProgress) between the stale scan above and this
            // write. No-op unless a test arms it.
            #[cfg(test)]
            fire_before_cross_board_release_hook_for_test();
            let outcome =
                crate::task_events::append_checked_at(&board, &emitter, event, |fresh_state| {
                    let tid = TaskId(task_id.clone());
                    let record = fresh_state
                        .tasks
                        .get(&tid)
                        .ok_or_else(|| format!("task '{task_id}' no longer present"))?;
                    if record.status != TaskStatus::Claimed {
                        return Err(format!(
                            "task '{task_id}' is now '{}', no longer Claimed",
                            record.status
                        ));
                    }
                    let fresh_snapshot: Vec<Task> =
                        fresh_state.tasks.values().map(record_to_task).collect();
                    let mut fresh_resolver = DepResolver::new(home, &board, &fresh_snapshot);
                    let still_cross_board = record.depends_on.iter().any(|d| {
                        board_router::route_task(home, &d.0)
                            .map(|(dep_project, _, _)| dep_project != project_for_check)
                            .unwrap_or(false)
                    });
                    if !still_cross_board {
                        return Err(format!("task '{task_id}' no longer has a cross-board dep"));
                    }
                    let still_not_done = record
                        .depends_on
                        .iter()
                        .any(|d| fresh_resolver.status_of(&d.0) != Some(TaskStatus::Done));
                    if !still_not_done {
                        return Err(format!("task '{task_id}' cross-board dep is now Done"));
                    }
                    Ok(())
                });
            match outcome {
                Ok(Ok(_)) => report.released.push(task_id),
                Ok(Err(reason)) => {
                    tracing::debug!(
                        task_id = %task_id,
                        reason = %reason,
                        "AUDIT2-014 detective: release precondition failed at commit time \
                         (task changed between scan and commit) — skipped, no event written"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, task_id = %task_id, "AUDIT2-014 detective: release append failed; will retry next pass");
                }
            }
        }
    }
    report
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
///
/// **#2117 P3 Gap1 — no single→multi-project backfill (operator decision (a),
/// 2026-06-18)**: legacy tasks migrated here carry no `project_id`, so they land
/// on the DEFAULT board (`board_root(home, DEFAULT)`) and STAY there. When a
/// single-repo deployment later adopts per-project boards (P2 #2125), existing
/// default-board tasks are NOT retroactively re-bucketed — only tasks created
/// after #2125 are per-project-stamped. This asymmetry is the ACCEPTED semantics,
/// not a gap to close: legacy tasks predate `project_id` and carry no signal to
/// auto-bucket, and `board_router::resolve_task_project`'s full-board-scan
/// fallback keeps cross-board lookups correct regardless. An operator who wants a
/// legacy task on a specific project board moves it explicitly. See #2117 P3.
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
