//! Task board — fleet-wide task tracking via JSON file.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

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

/// Check if an instance name is known (in fleet.yaml).
/// Returns true if fleet.yaml doesn't exist (no fleet = no restriction).
fn instance_exists(home: &Path, name: &str) -> bool {
    let fleet_path = crate::fleet::fleet_yaml_path(home);
    if !fleet_path.exists() {
        return true; // no fleet config = no restriction
    }
    crate::fleet::FleetConfig::load(&fleet_path)
        .map(|c| c.instances.contains_key(name))
        .unwrap_or(true) // parse error = permissive
}

/// Check if caller is allowed to mutate a task (assignee or orchestrator).
/// Unassigned tasks can be mutated by anyone.
///
/// Sprint 23 P0: promoted from `fn` to `pub fn` to mirror
/// `decisions::can_mutate_decision` (PR #220, Sprint 21 Phase 2 D1). Public
/// visibility lets external auditors / tests verify the predicate without
/// going through `mutate_versioned`. Race-free invocation requires calling
/// from inside `mutate_versioned`'s locked closure (existing internal
/// callers at the `done` / `update` arms already do this).
///
/// **TOCTOU caveat** (Sprint 23 P0 r2 M2 doc strengthening): external
/// callers using read-only checks for diagnostics or tests are fine; callers
/// wanting to **act on the result** MUST do so inside `mutate_versioned`'s
/// locked closure to avoid time-of-check-to-time-of-use race on the
/// `assignee` field. A separate process / thread can change `assignee`
/// between an out-of-lock predicate call and a follow-up mutation, voiding
/// the gate.
///
/// **PR3 cutover note** — kept as a `pub` for any external auditor /
/// test still importing it. New in-tree handle arms use
/// [`can_mutate_record`] which operates on the replay-derived
/// `TaskRecord`. Marked `#[allow(dead_code)]` because the in-tree
/// usages migrated.
#[allow(dead_code)]
pub fn can_mutate_task(home: &Path, caller: &str, task: &Task) -> bool {
    match &task.assignee {
        None => true,
        Some(assignee) => {
            if assignee == caller {
                return true;
            }
            // Check if caller is orchestrator of assignee's team
            if crate::teams::is_orchestrator_of(home, caller, assignee) {
                return true;
            }
            // Check if assignee is a team name and caller is its orchestrator
            if let Ok(Some(orch)) = crate::teams::resolve_team_orchestrator(home, assignee) {
                if orch == caller {
                    return true;
                }
            }
            false
        }
    }
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
fn record_to_task(r: &crate::task_events::TaskRecord) -> Task {
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
    }
}

fn status_to_legacy_str(s: crate::task_events::TaskStatus) -> &'static str {
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

fn parse_due_at(args: &Value) -> Option<String> {
    if let Some(due) = args["due_at"].as_str() {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(due) {
            return Some(dt.with_timezone(&chrono::Utc).to_rfc3339());
        }
    }
    if let Some(dur) = args["duration"].as_str() {
        if let Some(d) = parse_duration(dur) {
            return Some((chrono::Utc::now() + d).to_rfc3339());
        }
    }
    None
}

fn parse_duration(s: &str) -> Option<chrono::Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = num.parse().ok()?;
    match unit {
        "m" => Some(chrono::Duration::minutes(n)),
        "h" => Some(chrono::Duration::hours(n)),
        "d" => Some(chrono::Duration::days(n)),
        _ => None,
    }
}

/// Read a single task's current replay-derived record. Used by
/// `handle`'s mutation arms to validate `(prev_status, transition)`
/// before emitting an event.
fn read_task_record(home: &Path, id: &str) -> Option<crate::task_events::TaskRecord> {
    let state = crate::task_events::replay(home).ok()?;
    state
        .tasks
        .get(&crate::task_events::TaskId(id.to_string()))
        .cloned()
}

/// PR3 — predicate variant of [`can_mutate_task`] that operates on the
/// replay-derived record's `created_by` + `owner` fields. Behaviour
/// matches the legacy [`can_mutate_task`] surface (caller is owner OR
/// orchestrator-of-owner OR caller-is-orchestrator-and-owner-is-team).
///
/// **PR4 F2 absorbed (TOCTOU caveat, mirrors PR #235 r2 M2 doc on the
/// legacy `can_mutate_task`)**: the predicate reads from a `replay()`
/// snapshot taken **before** the read-out — there is no inherent lock on
/// the event log between this check and a follow-up `task_events::append`
/// emission. A separate process / thread can append a `Claimed` /
/// `OwnerAssigned` / `Released` event between an out-of-lock predicate
/// call and the caller's emission, voiding the gate. Production usage in
/// `handle`'s mutation arms accepts this small TOCTOU window: the
/// canonical authority is the event log itself, and conflicting emissions
/// resolve at replay time with the later seq winning. Auditors / tests
/// using this for diagnostic checks are fine.
/// System identities allowed to bypass normal ACL checks.
/// These are internal daemon modules that emit events on behalf of the system.
const SYSTEM_IDENTITIES: &[&str] = &[
    "system:auto_close",
    "system:auto_orphan",
    "system:branch_sweep",
    "system:overdue_sweep",
    "system:task_sweep",
];

/// Check if a caller is a recognized system identity.
pub fn is_system_identity(caller: &str) -> bool {
    SYSTEM_IDENTITIES.contains(&caller)
}

/// #808: clear ownership on tasks owned by a deleted instance so the
/// ACL gate (`can_mutate_record`) doesn't lock survivors out. Called
/// from `full_delete_instance` after fleet-yaml membership cleanup.
///
/// Replays the event log, enumerates tasks where `owner == owner_name`
/// AND status is still "live" (Open/Claimed/InProgress/Blocked), and
/// emits one `OwnerAssigned { owner: None }` per affected task via
/// `append_batch` so the entire orphan transition lands under one
/// fsync. Done/Cancelled tasks are skipped — their terminal state
/// already disables ACL writes, so re-orphaning them would only churn
/// the event log.
///
/// Concurrency: the caller (`full_delete_instance`) issues
/// `api::method::DELETE` BEFORE invoking this helper, so the doomed
/// instance is already dead and cannot claim new tasks mid-flight.
/// The TOCTOU window between `replay()` and `append_batch()` is
/// acceptable — a sweeper or operator race that lands later still
/// wins at replay (later seq overrides).
///
/// Returns the count of orphaned tasks on success (0 when nothing
/// matched), or an `Err` carrying the underlying replay / append
/// failure detail for the caller to surface into its audit chain.
pub fn orphan_tasks_for_owner(home: &Path, owner_name: &str) -> Result<usize, String> {
    use crate::task_events::{InstanceName, TaskEvent, TaskStatus};

    let state = crate::task_events::replay(home).map_err(|e| e.to_string())?;
    let affected: Vec<crate::task_events::TaskId> = state
        .tasks
        .values()
        .filter(|r| r.owner.as_ref().map(|o| o.0 == owner_name).unwrap_or(false))
        .filter(|r| {
            matches!(
                r.status,
                TaskStatus::Open
                    | TaskStatus::Claimed
                    | TaskStatus::InProgress
                    | TaskStatus::Blocked
            )
        })
        .map(|r| r.id.clone())
        .collect();
    if affected.is_empty() {
        return Ok(0);
    }
    let count = affected.len();
    let emitter = InstanceName::from("system:auto_orphan");
    let events: Vec<TaskEvent> = affected
        .into_iter()
        .map(|id| TaskEvent::OwnerAssigned {
            task_id: id,
            by: emitter.clone(),
            owner: None,
            routed_to: None,
        })
        .collect();
    crate::task_events::append_batch(home, &emitter, events)
        .map(|_| count)
        .map_err(|e| e.to_string())
}

fn can_mutate_record(home: &Path, caller: &str, record: &crate::task_events::TaskRecord) -> bool {
    // B1: system identities pass ACL via explicit allow-list
    if is_system_identity(caller) {
        return true;
    }
    match record.owner.as_ref() {
        None => true,
        Some(owner) => {
            let owner_str = owner.0.as_str();
            if owner_str == caller {
                return true;
            }
            if crate::teams::is_orchestrator_of(home, caller, owner_str) {
                return true;
            }
            if let Ok(Some(orch)) = crate::teams::resolve_team_orchestrator(home, owner_str) {
                if orch == caller {
                    return true;
                }
            }
            false
        }
    }
}

pub fn handle(home: &Path, instance_name: &str, args: &Value) -> Value {
    let action = match args["action"].as_str() {
        Some(a) => a,
        None => return serde_json::json!({"error": "missing 'action'"}),
    };
    let emitter = crate::task_events::InstanceName::from(instance_name);

    match action {
        "create" => {
            let title = match args["title"].as_str() {
                Some(t) => t,
                None => return serde_json::json!({"error": "missing 'title'"}),
            };
            use std::sync::atomic::{AtomicU64, Ordering};
            static ID_SEQ: AtomicU64 = AtomicU64::new(0);
            let ts = chrono::Utc::now().format("%Y%m%d%H%M%S%6f");
            let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
            let id = format!("t-{ts}-{seq}");
            let assignee = args["assignee"].as_str().map(String::from);
            let routed_to = if let Some(ref name) = assignee {
                match crate::teams::resolve_team_orchestrator(home, name) {
                    Ok(Some(orch)) => Some(orch),
                    Ok(None) => None,
                    Err(e) => return serde_json::json!({"error": e}),
                }
            } else {
                None
            };
            let depends_on: Vec<String> = args["depends_on"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let event = crate::task_events::TaskEvent::Created {
                task_id: crate::task_events::TaskId(id.clone()),
                title: title.to_string(),
                description: args["description"].as_str().unwrap_or("").to_string(),
                priority: args["priority"].as_str().unwrap_or("normal").to_string(),
                owner: assignee
                    .as_ref()
                    .map(|s| crate::task_events::InstanceName(s.clone())),
                due_at: parse_due_at(args),
                depends_on: depends_on
                    .iter()
                    .map(|s| crate::task_events::TaskId(s.clone()))
                    .collect(),
                routed_to: routed_to
                    .as_ref()
                    .map(|s| crate::task_events::InstanceName(s.clone())),
                branch: args["branch"].as_str().map(String::from),
                // Sprint 55 P0-C: opt-out flag for daemon auto-bind on
                // dispatch. None = default auto-bind behavior preserved.
                bind: args["bind"].as_bool(),
                // Sprint 59 Wave 1 PR-1 (#9 task stall watchdog):
                // optional operator-supplied ETA in seconds. None
                // disables stall detection for the task.
                eta_secs: args["eta_secs"].as_i64(),
            };
            match crate::task_events::append(home, &emitter, event) {
                Ok(_) => {
                    // #807 Item 1: response shape consistency. `event`
                    // names the action verb; `task` carries the full
                    // Task object so callers can read lifecycle status
                    // (`task.status == "open"` after create, NOT the
                    // event name "created"). Legacy `status` field
                    // kept as back-compat alias.
                    let task = read_task_record(home, &id).map(|r| record_to_task(&r));
                    serde_json::json!({
                        "id": id,
                        "event": "created",
                        "task": task,
                        // #807 deprecated alias kept for back-compat — see task.status for lifecycle.
                        "status": "created",
                    })
                }
                Err(e) => serde_json::json!({"error": format!("event log append failed: {e}")}),
            }
        }
        "list" => {
            let filter_assignee = args["filter_assignee"].as_str();
            let filter_status = args["filter_status"].as_str();
            // #806: default trim to actionable statuses unless caller
            // opts in to history. `filtered_default=true` on the
            // response signals callers (audit / forensics) that the
            // trim fired so they can re-call with include_history=true.
            let include_history = args["include_history"].as_bool().unwrap_or(false);
            let limit = args["limit"].as_u64();
            let filtered_default = !include_history && filter_status.is_none();
            const ACTIONABLE: &[&str] = &["open", "claimed", "in_progress", "blocked"];
            let now = chrono::Utc::now();
            let done_ttl = chrono::Duration::days(14);
            let tasks = list_all(home);
            let mut filtered: Vec<Task> = tasks
                .iter()
                .filter(|t| filter_assignee.is_none_or(|a| t.assignee.as_deref() == Some(a)))
                .filter(|t| filter_status.is_none_or(|s| t.status == s))
                // #806 default-actionable-only filter — only fires
                // when neither include_history nor filter_status is
                // set. Preserves zero impact on filter_status callers.
                .filter(|t| {
                    include_history
                        || filter_status.is_some()
                        || ACTIONABLE.contains(&t.status.as_str())
                })
                .filter(|t| {
                    // 14d done-ttl preserved for include_history=true
                    // path (default trim already drops done entries).
                    if filter_status.is_some() || t.status != "done" {
                        return true;
                    }
                    chrono::DateTime::parse_from_rfc3339(&t.updated_at)
                        .map(|dt| {
                            now.signed_duration_since(dt.with_timezone(&chrono::Utc)) < done_ttl
                        })
                        .unwrap_or(true)
                })
                .cloned()
                .collect();
            // #806 `limit`: newest-first cap by `updated_at` desc.
            if let Some(n) = limit {
                filtered.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
                filtered.truncate(n as usize);
            }
            serde_json::json!({
                "tasks": filtered,
                "filtered_default": filtered_default,
            })
        }
        "claim" => {
            let id = match args["id"].as_str() {
                Some(i) => i.to_string(),
                None => return serde_json::json!({"error": "missing 'id'"}),
            };
            let iname = instance_name.to_string();
            if !instance_exists(home, &iname) {
                return serde_json::json!({"error": format!("instance '{iname}' not found in fleet.yaml")});
            }
            // PR3: dep-derived blocking is computed in-memory at list time
            // (not persisted). claim must respect that view, otherwise an
            // operator could claim a task whose deps are unsatisfied. Use
            // `list_all` (which applies the in-memory dep eval) instead of
            // raw replay() for the validation read.
            let tasks_view = list_all(home);
            let task_view = match tasks_view.iter().find(|t| t.id == id) {
                Some(t) => t,
                None => return serde_json::json!({"error": format!("task '{id}' not found")}),
            };
            let is_self_reclaim = task_view.status == "claimed"
                && task_view.assignee.as_deref() == Some(iname.as_str());
            if !is_self_reclaim && task_view.status != "open" {
                return serde_json::json!({
                    "error": format!(
                        "task '{id}' status is '{}', only 'open' tasks can be claimed",
                        task_view.status
                    )
                });
            }
            let event = crate::task_events::TaskEvent::Claimed {
                task_id: crate::task_events::TaskId(id.clone()),
                by: crate::task_events::InstanceName(iname.clone()),
            };
            match crate::task_events::append(home, &emitter, event) {
                Ok(_) => {
                    // #807 Item 1: see create arm note. claim's
                    // legacy `status` happens to match lifecycle
                    // ("claimed"), but the field is still the action
                    // event name semantically — kept as alias for
                    // shape consistency.
                    let task = read_task_record(home, &id).map(|r| record_to_task(&r));
                    serde_json::json!({
                        "id": id,
                        "event": "claimed",
                        "task": task,
                        "assignee": instance_name,
                        // #807 deprecated alias kept for back-compat — see task.status for lifecycle.
                        "status": "claimed",
                    })
                }
                Err(e) => serde_json::json!({"error": format!("event log append failed: {e}")}),
            }
        }
        "done" => {
            let id = match args["id"].as_str() {
                Some(i) => i.to_string(),
                None => return serde_json::json!({"error": "missing 'id'"}),
            };
            let result_text = args["result"].as_str().map(String::from);
            let caller = instance_name.to_string();
            let record = match read_task_record(home, &id) {
                Some(r) => r,
                None => return serde_json::json!({"error": format!("task '{id}' not found")}),
            };
            // #808: force flag bypasses the ACL gate for historical
            // ghost-owned cleanup. Validator mirrors comms.rs:200-218.
            let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
            let force_reason = args
                .get("force_reason")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if force && force_reason.is_empty() {
                return serde_json::json!({
                    "error": "force=true requires a non-empty 'force_reason'"
                });
            }
            if !force && !can_mutate_record(home, &caller, &record) {
                return serde_json::json!({
                    "error": format!(
                        "task '{id}' owned by '{}', caller '{caller}' not authorized",
                        record.owner.as_ref().map(|o| o.0.as_str()).unwrap_or("unassigned")
                    )
                });
            }
            if force {
                crate::event_log::log(
                    home,
                    "task_force_done",
                    &caller,
                    &format!(
                        "task={id} owner={} reason={force_reason}",
                        record
                            .owner
                            .as_ref()
                            .map(|o| o.0.as_str())
                            .unwrap_or("none")
                    ),
                );
            }
            let by = record
                .owner
                .as_ref()
                .map(|o| o.0.clone())
                .unwrap_or_else(|| caller.clone());
            // #808: when force is set, prefix the result with an
            // audit marker so the persisted event itself names the
            // caller + reason (event_log carries the same record for
            // cross-board audit).
            let result_text = if force {
                Some(format!(
                    "[forced by '{caller}': {force_reason}] {}",
                    result_text.unwrap_or_default()
                ))
            } else {
                result_text
            };
            let event = crate::task_events::TaskEvent::Done {
                task_id: crate::task_events::TaskId(id.clone()),
                by: crate::task_events::InstanceName(by),
                // B2: honor caller-provided done_source for audit trail
                source: args
                    .get("done_source")
                    .and_then(|v| {
                        serde_json::from_value::<crate::task_events::DoneSource>(v.clone()).ok()
                    })
                    .unwrap_or_else(|| crate::task_events::DoneSource::OperatorManual {
                        authored_at: chrono::Utc::now().to_rfc3339(),
                        result: result_text,
                    }),
            };
            match crate::task_events::append(home, &emitter, event) {
                Ok(_) => {
                    // #789: task-completion is a workflow boundary —
                    // clean any empty `init` commits the backend has
                    // accumulated in the agent's bound worktree since
                    // the last cleanup at `dispatch_auto_bind_lease`.
                    // Best-effort: failure is logged inside the helper
                    // but never blocks the done response (the task
                    // event already appended successfully — cleanup is
                    // a polish step, not load-bearing).
                    let owner = record
                        .owner
                        .as_ref()
                        .map(|o| o.0.clone())
                        .unwrap_or_else(|| caller.clone());
                    if let Some(wt) = crate::binding::read(home, &owner)
                        .and_then(|v| v["worktree"].as_str().map(std::path::PathBuf::from))
                    {
                        let _ =
                            crate::mcp::handlers::dispatch_hook::clean_empty_init_commits(&wt).ok();
                    }
                    // #807 Item 1: see create arm note.
                    let task = read_task_record(home, &id).map(|r| record_to_task(&r));
                    serde_json::json!({
                        "id": id,
                        "event": "done",
                        "task": task,
                        // #807 deprecated alias kept for back-compat — see task.status for lifecycle.
                        "status": "done",
                    })
                }
                Err(e) => serde_json::json!({"error": format!("event log append failed: {e}")}),
            }
        }
        "update" => {
            let id = match args["id"].as_str() {
                Some(i) => i.to_string(),
                None => return serde_json::json!({"error": "missing 'id'"}),
            };
            let new_status = args["status"].as_str().map(String::from);
            let new_priority = args["priority"].as_str();
            let new_assignee = args["assignee"].as_str().map(String::from);
            // Resolve team routing for new assignee (validates team exists / not degraded).
            let _new_routed_to = if let Some(ref name) = new_assignee {
                match crate::teams::resolve_team_orchestrator(home, name) {
                    Ok(orch) => orch,
                    Err(e) => return serde_json::json!({"error": e}),
                }
            } else {
                None
            };
            let caller = instance_name.to_string();
            let record = match read_task_record(home, &id) {
                Some(r) => r,
                None => return serde_json::json!({"error": format!("task '{id}' not found")}),
            };
            // #808: force flag bypasses the ACL gate for historical
            // ghost-owned cleanup. Validator mirrors comms.rs:200-218.
            let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
            let force_reason = args
                .get("force_reason")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if force && force_reason.is_empty() {
                return serde_json::json!({
                    "error": "force=true requires a non-empty 'force_reason'"
                });
            }
            if !force && !can_mutate_record(home, &caller, &record) {
                return serde_json::json!({
                    "error": format!(
                        "task '{id}' owned by '{}', caller '{caller}' not authorized",
                        record.owner.as_ref().map(|o| o.0.as_str()).unwrap_or("unassigned")
                    )
                });
            }
            if force {
                crate::event_log::log(
                    home,
                    "task_force_update",
                    &caller,
                    &format!(
                        "task={id} owner={} reason={force_reason}",
                        record
                            .owner
                            .as_ref()
                            .map(|o| o.0.as_str())
                            .unwrap_or("none")
                    ),
                );
            }
            // #808: when force is set, embed the caller + reason
            // directly in the emitted event's `reason` field so the
            // per-task replay trail also carries the audit (in
            // addition to the event_log entry above).
            let reason_text = |base: &str| -> String {
                if force {
                    format!("{base} [forced by '{caller}': {force_reason}]")
                } else {
                    base.to_string()
                }
            };
            // PR4 F1 — collect transitions into a Vec then emit via
            // single `append_batch` so updates are atomic at the F7 batch
            // level (all-or-nothing fsync window).
            let mut pending_events: Vec<crate::task_events::TaskEvent> = Vec::new();
            // PR3 — explicit status transition emits the canonical event.
            // Priority / assignee changes without status change have no
            // event variant in v2; the change is observable only through
            // tasks.json's archeology (deferred to a future metadata-event
            // PR if a use case surfaces). The MCP response still reports
            // "updated" so callers don't need to special-case.
            if let Some(ref s) = new_status {
                let prev_status = record.status;
                let event_for_transition: Option<crate::task_events::TaskEvent> =
                    match (prev_status, s.as_str()) {
                        (_, "claimed") => Some(crate::task_events::TaskEvent::Claimed {
                            task_id: crate::task_events::TaskId(id.clone()),
                            by: crate::task_events::InstanceName::from(
                                record
                                    .owner
                                    .as_ref()
                                    .map(|o| o.0.as_str())
                                    .unwrap_or(caller.as_str()),
                            ),
                        }),
                        (_, "in_progress") => Some(crate::task_events::TaskEvent::InProgress {
                            task_id: crate::task_events::TaskId(id.clone()),
                            by: crate::task_events::InstanceName::from(
                                record
                                    .owner
                                    .as_ref()
                                    .map(|o| o.0.as_str())
                                    .unwrap_or(caller.as_str()),
                            ),
                        }),
                        (_, "done") => {
                            // B2: allow caller-provided done_source for audit trail
                            let source = args
                                .get("done_source")
                                .and_then(|v| {
                                    serde_json::from_value::<crate::task_events::DoneSource>(
                                        v.clone(),
                                    )
                                    .ok()
                                })
                                .unwrap_or_else(|| {
                                    crate::task_events::DoneSource::OperatorManual {
                                        authored_at: chrono::Utc::now().to_rfc3339(),
                                        result: record.result.clone(),
                                    }
                                });
                            Some(crate::task_events::TaskEvent::Done {
                                task_id: crate::task_events::TaskId(id.clone()),
                                by: crate::task_events::InstanceName::from(
                                    record
                                        .owner
                                        .as_ref()
                                        .map(|o| o.0.as_str())
                                        .unwrap_or(caller.as_str()),
                                ),
                                source,
                            })
                        }
                        (_, "cancelled") => Some(crate::task_events::TaskEvent::Cancelled {
                            task_id: crate::task_events::TaskId(id.clone()),
                            by: crate::task_events::InstanceName::from(caller.as_str()),
                            reason: reason_text("operator update"),
                        }),
                        (_, "blocked") => Some(crate::task_events::TaskEvent::Blocked {
                            task_id: crate::task_events::TaskId(id.clone()),
                            reason: reason_text("operator update"),
                        }),
                        (crate::task_events::TaskStatus::Blocked, "open") => {
                            Some(crate::task_events::TaskEvent::Unblocked {
                                task_id: crate::task_events::TaskId(id.clone()),
                            })
                        }
                        // Claimed/InProgress → open: emit Released so owner
                        // is cleared (tasks.json bridge previously did this
                        // via direct mutation). For Done → Open, emit
                        // Reopened (preserves owner — the same person
                        // typically re-does the work).
                        (crate::task_events::TaskStatus::Claimed, "open")
                        | (crate::task_events::TaskStatus::InProgress, "open") => {
                            Some(crate::task_events::TaskEvent::Released {
                                task_id: crate::task_events::TaskId(id.clone()),
                                reason: reason_text("operator update (status → open)"),
                            })
                        }
                        (_, "open") => Some(crate::task_events::TaskEvent::Reopened {
                            task_id: crate::task_events::TaskId(id.clone()),
                            reason: reason_text("operator update"),
                            source_evidence: format!(
                                "status {} → open",
                                status_to_legacy_str(prev_status)
                            ),
                        }),
                        _ => None,
                    };
                // PR4 F1 (PR3 r1 reviewer-2 LOW) — collect events into
                // a Vec and emit via `append_batch` so all transitions
                // produced by a single update call land under one fsync.
                // F7 atomic-batch contract: either all land or none do
                // (a partial-write window can't surface to readers).
                if let Some(ev) = event_for_transition {
                    pending_events.push(ev);
                }
            }
            // Priority change without status transition: queue
            // PriorityChanged so replay reflects the new value.
            if let Some(p) = new_priority {
                pending_events.push(crate::task_events::TaskEvent::PriorityChanged {
                    task_id: crate::task_events::TaskId(id.clone()),
                    by: crate::task_events::InstanceName::from(caller.as_str()),
                    priority: p.to_string(),
                });
            }
            // Assignee change without status transition: queue
            // OwnerAssigned. Distinct from Claimed (status stays put).
            if let Some(ref new_owner) = new_assignee {
                let routed_to = match crate::teams::resolve_team_orchestrator(home, new_owner) {
                    Ok(orch) => orch,
                    Err(e) => return serde_json::json!({"error": e}),
                };
                pending_events.push(crate::task_events::TaskEvent::OwnerAssigned {
                    task_id: crate::task_events::TaskId(id.clone()),
                    by: crate::task_events::InstanceName::from(caller.as_str()),
                    owner: Some(crate::task_events::InstanceName(new_owner.clone())),
                    routed_to: routed_to
                        .as_ref()
                        .map(|s| crate::task_events::InstanceName(s.clone())),
                });
            }
            // F1: single atomic append_batch over all the update arm's
            // queued events. Either all land or none do.
            if !pending_events.is_empty() {
                if let Err(e) = crate::task_events::append_batch(home, &emitter, pending_events) {
                    return serde_json::json!({
                        "error": format!("event log append_batch failed: {e}")
                    });
                }
            }
            // #807 Item 1: see create arm note.
            let task = read_task_record(home, &id).map(|r| record_to_task(&r));
            serde_json::json!({
                "id": id,
                "event": "updated",
                "task": task,
                // #807 deprecated alias kept for back-compat — see task.status for lifecycle.
                "status": "updated",
            })
        }
        "sweep" => {
            // #806 manual board-hygiene sweep — distinct from the
            // daemon-ticked `task_sweep` (which auto-Dones tasks via
            // `Closes t-XXX-N` PR markers). This action is operator-
            // triggered, scans for 4 stale categories, returns a
            // dry-run plan, then applies on a confirm round-trip.
            let apply = args["apply"].as_bool().unwrap_or(false);
            let confirm_ids: std::collections::HashSet<String> = args["confirm_ids"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let audit_reason = args["audit_reason"].as_str().unwrap_or("");
            // Repo resolution: explicit arg → SweepConfig fallback →
            // None (shipped/superseded categories skipped without repo).
            let repo_owned: Option<String> = args["repo"]
                .as_str()
                .map(String::from)
                .or_else(|| crate::daemon::task_sweep::load_sweep_config_for_doctor(home).repo);
            let live_instances: std::collections::HashSet<String> = crate::api::call(
                home,
                &serde_json::json!({"method": crate::api::method::LIST}),
            )
            .ok()
            .and_then(|r| {
                r["result"]["agents"].as_array().map(|arr| {
                    arr.iter()
                        .filter_map(|a| a["name"].as_str().map(String::from))
                        .collect()
                })
            })
            .unwrap_or_default();
            let now = chrono::Utc::now();
            let pr_lookup: sweep_impl::PrLookup = &sweep_impl::gh_pr_lookup;
            let categories = sweep_impl::scan_categories(
                home,
                &live_instances,
                pr_lookup,
                repo_owned.as_deref(),
                now,
            );
            if !apply {
                return serde_json::json!({
                    "dry_run": true,
                    "categories": categories.as_json(),
                    "candidate_ids": categories.all_ids(),
                    "total_candidates": categories.total(),
                    "to_apply_hint": "task action=sweep apply=true confirm_ids=<subset> audit_reason=<...>",
                });
            }
            // Apply path — validate inputs + emit Cancelled batch.
            if confirm_ids.is_empty() {
                return serde_json::json!({
                    "error": "apply=true requires non-empty 'confirm_ids' (subset of candidate_ids from a prior dry-run)"
                });
            }
            if audit_reason.is_empty() {
                return serde_json::json!({
                    "error": "apply=true requires non-empty 'audit_reason' for the cross-board event log entry"
                });
            }
            let candidate_set: std::collections::HashSet<String> =
                categories.all_ids().into_iter().collect();
            let unknown: Vec<String> = confirm_ids.difference(&candidate_set).cloned().collect();
            if !unknown.is_empty() {
                return serde_json::json!({
                    "error": "confirm_ids contained entries not in current sweep candidates",
                    "unknown": unknown,
                    "hint": "re-run dry-run; candidates may have changed since last scan",
                });
            }
            let applied =
                sweep_impl::emit_cancelled_batch(home, &categories, &confirm_ids, audit_reason);
            match applied {
                Ok(count) => serde_json::json!({
                    "applied": count,
                    "audit_reason": audit_reason,
                }),
                Err(e) => serde_json::json!({"error": format!("sweep apply failed: {e}")}),
            }
        }
        _ => serde_json::json!({"error": format!("unknown action: {action}")}),
    }
}

/// #806 manual board-hygiene sweeper. Operator-triggered, scans the
/// task board for 4 categories of stale entries (shipped, superseded,
/// team_disbanded, validation_leftovers), and returns a dry-run plan
/// the operator confirms via a second call with `apply=true +
/// confirm_ids`. Distinct from the daemon-ticked `task_sweep` which
/// only handles `Closes t-XXX-N` PR-marker auto-Done.
///
/// `PrLookup` is injected as a function pointer so tests can swap
/// `gh_pr_lookup` (production shellout) for a deterministic stub.
mod sweep_impl {
    use super::list_all;
    use chrono::{DateTime, Duration, Utc};
    use std::collections::{HashMap, HashSet};
    use std::path::Path;

    /// State of a PR referenced by a task title/description.
    #[derive(Debug, Clone, PartialEq)]
    pub(super) enum PrState {
        /// PR was merged; carries the `mergedAt` timestamp.
        Merged { merged_at: String },
        /// PR was closed without merging — task is superseded.
        Closed,
        /// PR is still open — task may still be in flight.
        Open,
        /// PR doesn't exist or query failed — skip categorization.
        Unknown,
    }

    /// Function-pointer abstraction over `gh pr view`. Tests inject
    /// a stub closure to bypass the shell-out. Production uses
    /// `gh_pr_lookup` below.
    pub(super) type PrLookup<'a> = &'a dyn Fn(&str, u32) -> Result<PrState, String>;

    /// Production PR-state lookup — shells out to `gh pr view`.
    /// Mirrors the existing precedent at
    /// `src/mcp/handlers/sha_gate.rs::fetch_pr_head_sha`.
    pub(super) fn gh_pr_lookup(repo: &str, num: u32) -> Result<PrState, String> {
        let output = std::process::Command::new("gh")
            .args([
                "pr",
                "view",
                &num.to_string(),
                "--repo",
                repo,
                "--json",
                "state,mergedAt",
            ])
            .output()
            .map_err(|e| format!("gh pr view failed: {e}"))?;
        if !output.status.success() {
            // PR may not exist on this repo — treat as Unknown so
            // categorization skips rather than erroring out the whole
            // sweep over a stale PR reference.
            return Ok(PrState::Unknown);
        }
        let body = String::from_utf8_lossy(&output.stdout);
        let json: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| format!("gh json parse: {e}"))?;
        match json["state"].as_str() {
            Some("MERGED") => Ok(PrState::Merged {
                merged_at: json["mergedAt"].as_str().unwrap_or("unknown").to_string(),
            }),
            Some("CLOSED") => Ok(PrState::Closed),
            Some("OPEN") => Ok(PrState::Open),
            _ => Ok(PrState::Unknown),
        }
    }

    #[derive(Debug, Clone, serde::Serialize)]
    pub(super) struct Candidate {
        pub id: String,
        pub reason: String,
        pub owner: Option<String>,
        pub pr: Option<u32>,
    }

    #[derive(Debug, Default)]
    pub(super) struct Categories {
        pub shipped: Vec<Candidate>,
        pub superseded: Vec<Candidate>,
        pub team_disbanded: Vec<Candidate>,
        pub validation_leftovers: Vec<Candidate>,
    }

    impl Categories {
        pub fn all_ids(&self) -> Vec<String> {
            let mut v: Vec<String> = self
                .shipped
                .iter()
                .chain(self.superseded.iter())
                .chain(self.team_disbanded.iter())
                .chain(self.validation_leftovers.iter())
                .map(|c| c.id.clone())
                .collect();
            v.sort();
            v.dedup();
            v
        }

        pub fn total(&self) -> usize {
            self.all_ids().len()
        }

        pub fn as_json(&self) -> serde_json::Value {
            serde_json::json!({
                "shipped": self.shipped,
                "superseded": self.superseded,
                "team_disbanded": self.team_disbanded,
                "validation_leftovers": self.validation_leftovers,
            })
        }
    }

    /// Scan the task board and bucket non-terminal tasks into the 4
    /// hygiene categories. Tasks already in `done`/`cancelled`/
    /// `verified` are skipped — they're already cleaned up. Each task
    /// lands in at most one category (first match wins, order:
    /// validation_leftovers → team_disbanded → shipped/superseded).
    ///
    /// `now` is parameterized so tests can fast-forward age thresholds
    /// without forging event-log timestamps.
    pub(super) fn scan_categories(
        home: &Path,
        live_instances: &HashSet<String>,
        pr_lookup: PrLookup,
        repo: Option<&str>,
        now: DateTime<Utc>,
    ) -> Categories {
        let tasks = list_all(home);
        let mut cats = Categories::default();
        let mut pr_cache: HashMap<u32, PrState> = HashMap::new();
        for t in &tasks {
            if matches!(t.status.as_str(), "done" | "cancelled" | "verified") {
                continue;
            }
            let age = chrono::DateTime::parse_from_rfc3339(&t.updated_at)
                .ok()
                .map(|dt| now.signed_duration_since(dt.with_timezone(&Utc)));
            // (1) validation_leftovers — title prefix match + 1d stale.
            let title_lc = t.title.to_lowercase();
            let is_validation = title_lc.starts_with("val-")
                || title_lc.starts_with("canary-")
                || title_lc.starts_with("test/")
                || title_lc.starts_with("test_")
                || t.branch
                    .as_deref()
                    .map(|b| b.starts_with("test/"))
                    .unwrap_or(false);
            if is_validation {
                if let Some(a) = age {
                    if a > Duration::days(1) {
                        cats.validation_leftovers.push(Candidate {
                            id: t.id.clone(),
                            reason: format!(
                                "validation/canary title prefix, {}d stale",
                                a.num_days()
                            ),
                            owner: t.assignee.clone(),
                            pr: None,
                        });
                        continue;
                    }
                }
            }
            // (2) team_disbanded — owner not in live fleet + 30d stale.
            if let (Some(owner), Some(a)) = (t.assignee.as_ref(), age) {
                if !live_instances.contains(owner) && a > Duration::days(30) {
                    cats.team_disbanded.push(Candidate {
                        id: t.id.clone(),
                        reason: format!(
                            "owner '{owner}' not in live fleet, {}d stale",
                            a.num_days()
                        ),
                        owner: Some(owner.clone()),
                        pr: None,
                    });
                    continue;
                }
            }
            // (3) shipped / (4) superseded — extract PR ref + query.
            let Some(repo) = repo else { continue };
            let search_text = format!("{}\n{}", t.title, t.description);
            let Some(pr_num) = extract_pr_number(&search_text) else {
                continue;
            };
            let state = pr_cache
                .entry(pr_num)
                .or_insert_with(|| pr_lookup(repo, pr_num).unwrap_or(PrState::Unknown))
                .clone();
            match state {
                PrState::Merged { merged_at } => {
                    if let Some(a) = age {
                        if a > Duration::days(7) {
                            cats.shipped.push(Candidate {
                                id: t.id.clone(),
                                reason: format!(
                                    "PR #{pr_num} merged at {merged_at}, task {}d stale",
                                    a.num_days()
                                ),
                                owner: t.assignee.clone(),
                                pr: Some(pr_num),
                            });
                        }
                    }
                }
                PrState::Closed => {
                    cats.superseded.push(Candidate {
                        id: t.id.clone(),
                        reason: format!("PR #{pr_num} closed without merge"),
                        owner: t.assignee.clone(),
                        pr: Some(pr_num),
                    });
                }
                PrState::Open | PrState::Unknown => {}
            }
        }
        cats
    }

    /// Extract the first `PR #<digits>` (or `PR <digits>`) reference
    /// from a haystack. Strict `PR ` prefix avoids false positives on
    /// standalone `#NNN` issue references.
    fn extract_pr_number(text: &str) -> Option<u32> {
        static PR_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = PR_RE.get_or_init(|| regex::Regex::new(r"\bPR #?(\d+)\b").expect("pr regex"));
        re.captures(text)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<u32>().ok())
    }

    /// Apply phase — emit `Cancelled` events for the `confirm_ids`
    /// subset under the `system:task_sweep` identity (already in
    /// `SYSTEM_IDENTITIES` bypass list). Each Cancelled carries the
    /// audit_reason in its reason field; the event log records a
    /// `task_sweep_apply` line per cancelled task for cross-board
    /// audit.
    pub(super) fn emit_cancelled_batch(
        home: &Path,
        categories: &Categories,
        confirm_ids: &HashSet<String>,
        audit_reason: &str,
    ) -> Result<usize, String> {
        use crate::task_events::{InstanceName, TaskEvent, TaskId};
        let emitter = InstanceName::from("system:task_sweep");
        let mut events: Vec<TaskEvent> = Vec::new();
        let lookup_category = |id: &str| -> &'static str {
            if categories.shipped.iter().any(|c| c.id == id) {
                return "shipped";
            }
            if categories.superseded.iter().any(|c| c.id == id) {
                return "superseded";
            }
            if categories.team_disbanded.iter().any(|c| c.id == id) {
                return "team_disbanded";
            }
            "validation_leftovers"
        };
        for id in confirm_ids {
            let category = lookup_category(id);
            events.push(TaskEvent::Cancelled {
                task_id: TaskId(id.clone()),
                by: emitter.clone(),
                reason: format!("sweep:{category}: {audit_reason}"),
            });
            crate::event_log::log(
                home,
                "task_sweep_apply",
                "system:task_sweep",
                &format!("task={id} category={category} reason={audit_reason}"),
            );
        }
        let count = events.len();
        if count == 0 {
            return Ok(0);
        }
        crate::task_events::append_batch(home, &emitter, events)
            .map(|_| count)
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tmp_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-tasks-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Sprint 23 P0 r2 F2 helper — minimal Task with assignee for
    /// `can_mutate_task` predicate tests. Mirrors decisions.rs
    /// `make_test_decision` fixture pattern.
    fn make_test_task(assignee: Option<&str>) -> Task {
        Task {
            id: "t-test-fixture".into(),
            title: "fixture".into(),
            description: String::new(),
            status: "open".into(),
            priority: "normal".into(),
            assignee: assignee.map(String::from),
            routed_to: None,
            created_by: "test".into(),
            depends_on: Vec::new(),
            result: None,
            created_at: "2026-04-27T00:00:00Z".into(),
            updated_at: "2026-04-27T00:00:00Z".into(),
            due_at: None,
            branch: None,
            started_at: None,
            eta_secs: None,
        }
    }

    // Sprint 23 P0 r2 F2 (dev-reviewer-2 most-material): mirror
    // `decisions::can_mutate_decision` test coverage onto
    // `tasks::can_mutate_task`. Decisions added 5 dedicated unit tests
    // (PR #220 Sprint 21 Phase 2 D1); tasks shipped the predicate
    // pub-promotion in Sprint 23 P0 with zero direct unit coverage —
    // closed here. Behavioural mirror of the Phase 2 D1 operator-pitfall
    // gate.

    #[test]
    fn can_mutate_task_assignee_match() {
        let home = tmp_home("can_mutate_assignee");
        let task = make_test_task(Some("dev-impl-2"));
        assert!(can_mutate_task(&home, "dev-impl-2", &task));
        std::fs::remove_dir_all(&home).ok();
    }

    /// Sprint 54 fleet-yaml unification: teams now live in fleet.yaml's
    /// `teams:` block (was: separate `teams.json` runtime store).
    /// Helper writes the dev team directly there, bypassing `teams::create`
    /// validation paths so tests stay focused on `can_mutate_task` logic.
    fn write_dev_team_to_fleet(home: &std::path::Path) {
        std::fs::write(
            crate::fleet::fleet_yaml_path(home),
            "teams:\n  dev:\n    members: [dev-lead, dev-impl-2]\n    \
             orchestrator: dev-lead\n    created_at: \"2026-04-27T00:00:00Z\"\n",
        )
        .expect("write fleet.yaml");
    }

    #[test]
    fn can_mutate_task_orchestrator_of_assignee() {
        // dev-lead is the orchestrator of the "dev" team; "dev-impl-2"
        // belongs to that team. Cross-team orchestrator path → pass.
        let home = tmp_home("can_mutate_orchestrator");
        write_dev_team_to_fleet(&home);
        let task = make_test_task(Some("dev-impl-2"));
        assert!(
            can_mutate_task(&home, "dev-lead", &task),
            "orchestrator of assignee's team must be allowed to mutate"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn can_mutate_task_team_assignee_orchestrator() {
        // Task-specific path absent in decisions: assignee is a team
        // NAME (not an instance). The team's orchestrator must be
        // allowed to mutate even though their name doesn't match
        // `task.assignee`.
        let home = tmp_home("can_mutate_team_assignee");
        write_dev_team_to_fleet(&home);
        let task = make_test_task(Some("dev")); // assignee = team name
        assert!(
            can_mutate_task(&home, "dev-lead", &task),
            "team orchestrator must mutate task assigned to team name"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn can_mutate_task_unassigned_returns_true() {
        // Task-specific branch absent in decisions: unassigned tasks
        // (`assignee = None`) are open to mutation by anyone — the gate
        // returns `true` regardless of caller. Lock the contract so
        // future refactor doesn't accidentally tighten this.
        let home = tmp_home("can_mutate_unassigned");
        let task = make_test_task(None);
        assert!(can_mutate_task(&home, "anyone", &task));
        assert!(can_mutate_task(&home, "even-fleet-stranger", &task));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn can_mutate_task_string_compare_no_numeric_coerce() {
        // Operator-pitfall regression mirror (decisions test of same
        // name): caller string vs assignee. `Task.assignee` is
        // `Option<String>` (e.g. "dev-impl-1"); the gate compares
        // strings, never parses an int. Verify alphabetically-similar
        // but non-equal callers do NOT pass, and that numeric-suffixed
        // names compare verbatim.
        let home = tmp_home("can_mutate_string_compare");
        let task = make_test_task(Some("dev-impl-1"));
        // Exact string match — passes.
        assert!(can_mutate_task(&home, "dev-impl-1", &task));
        // Suffix mismatch — rejects (no int coerce to "1 == 1").
        assert!(!can_mutate_task(&home, "dev-impl-2", &task));
        // Bare numeric caller — rejects (would only "match" under int
        // coerce path, which we explicitly do not have).
        assert!(!can_mutate_task(&home, "1", &task));
        // Substring of assignee — rejects (no prefix-match path).
        assert!(!can_mutate_task(&home, "dev-impl", &task));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_create_list_claim_done() {
        let home = tmp_home("crud");
        let r = handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "create", "title": "Fix bug", "priority": "high"}),
        );
        assert_eq!(r["status"], "created");
        let id = r["id"].as_str().expect("id").to_string();

        let listed = handle(&home, "agent1", &serde_json::json!({"action": "list"}));
        assert_eq!(listed["tasks"].as_array().expect("arr").len(), 1);
        assert_eq!(listed["tasks"][0]["status"], "open");

        let claim = handle(
            &home,
            "agent2",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        assert_eq!(claim["status"], "claimed");
        assert_eq!(claim["assignee"], "agent2");

        let done = handle(
            &home,
            "agent2",
            &serde_json::json!({"action": "done", "id": id, "result": "fixed"}),
        );
        assert_eq!(done["status"], "done");

        let listed = handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "list", "filter_status": "done"}),
        );
        assert_eq!(listed["tasks"][0]["result"], "fixed");

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_claim_nonexistent() {
        let home = tmp_home("claim_nonexistent");
        let r = handle(
            &home,
            "a",
            &serde_json::json!({"action": "claim", "id": "nope"}),
        );
        assert!(r["error"].as_str().is_some());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_assign_to_team_routes_to_orchestrator() {
        let home = tmp_home("team_route");
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "devs", "members": ["lead", "worker"], "orchestrator": "lead"}),
        );
        let r = handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "fix bug", "assignee": "devs"}),
        );
        assert_eq!(r["status"], "created");
        let tasks = list_all(&home);
        let t = tasks.iter().find(|t| t.title == "fix bug").expect("task");
        assert_eq!(t.assignee.as_deref(), Some("devs"));
        assert_eq!(t.routed_to.as_deref(), Some("lead"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_assign_to_degraded_team_rejects() {
        let home = tmp_home("degraded_reject");
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "devs", "members": ["lead", "worker"], "orchestrator": "lead"}),
        );
        crate::teams::remove_member_from_all(&home, "lead");
        let r = handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "fix bug", "assignee": "devs"}),
        );
        assert!(
            r["error"].as_str().expect("err").contains("degraded"),
            "got: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_assign_to_agent_unchanged() {
        let home = tmp_home("agent_direct");
        let r = handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "fix bug", "assignee": "at-dev-2"}),
        );
        assert_eq!(r["status"], "created");
        let tasks = list_all(&home);
        let t = tasks.iter().find(|t| t.title == "fix bug").expect("task");
        assert_eq!(t.assignee.as_deref(), Some("at-dev-2"));
        assert!(
            t.routed_to.is_none(),
            "no routing for direct agent assignment"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn claim_clears_routed_to() {
        let home = tmp_home("claim_clears_rt");
        // Sprint 54 fleet-yaml unification: teams::create now writes
        // to fleet.yaml, so the claim path's instance_exists check fires
        // (previously fleet.yaml stayed absent → permissive). Pre-seed
        // instances so the claim by `worker` resolves to a fleet member.
        write_fleet_yaml(&home, &["lead", "worker"]);
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "devs", "members": ["lead", "worker"], "orchestrator": "lead"}),
        );
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "fix", "assignee": "devs"}),
        );
        let id = list_all(&home)[0].id.clone();
        assert_eq!(list_all(&home)[0].routed_to.as_deref(), Some("lead"));
        handle(
            &home,
            "worker",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        let t = &list_all(&home)[0];
        assert_eq!(t.assignee.as_deref(), Some("worker"));
        assert!(t.routed_to.is_none(), "claim should clear routed_to");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn update_assignee_re_resolves_routed_to() {
        let home = tmp_home("update_re_resolve");
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "alpha", "members": ["a1"], "orchestrator": "a1"}),
        );
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "beta", "members": ["b1"], "orchestrator": "b1"}),
        );
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "task", "assignee": "alpha"}),
        );
        let id = list_all(&home)[0].id.clone();
        assert_eq!(list_all(&home)[0].routed_to.as_deref(), Some("a1"));
        handle(
            &home,
            "a1",
            &serde_json::json!({"action": "update", "id": id, "assignee": "beta"}),
        );
        let t = &list_all(&home)[0];
        assert_eq!(t.assignee.as_deref(), Some("beta"));
        assert_eq!(t.routed_to.as_deref(), Some("b1"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_board_create_via_handle() {
        let home = tmp_home("board_create");
        let r = handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "new feature", "priority": "normal"}),
        );
        assert_eq!(r["status"], "created");
        let tasks = list_all(&home);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "new feature");
        assert_eq!(tasks[0].status, "open");
        assert_eq!(tasks[0].priority, "normal");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_board_move_status() {
        let home = tmp_home("board_move");
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "item", "priority": "low"}),
        );
        let tasks = list_all(&home);
        let id = &tasks[0].id;
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "update", "id": id, "priority": "normal"}),
        );
        let t = &list_all(&home)[0];
        assert_eq!(t.priority, "normal");
        assert_eq!(t.status, "open");
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "update", "id": id, "status": "claimed"}),
        );
        assert_eq!(list_all(&home)[0].status, "claimed");
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "update", "id": id, "status": "done"}),
        );
        assert_eq!(list_all(&home)[0].status, "done");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_board_assign_agent() {
        let home = tmp_home("board_assign");
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "fix bug"}),
        );
        let id = &list_all(&home)[0].id.clone();
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "update", "id": id, "assignee": "at-dev-2"}),
        );
        assert_eq!(list_all(&home)[0].assignee.as_deref(), Some("at-dev-2"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_board_cancel() {
        let home = tmp_home("board_cancel");
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "create", "title": "remove me"}),
        );
        let id = &list_all(&home)[0].id.clone();
        handle(
            &home,
            "user",
            &serde_json::json!({"action": "update", "id": id, "status": "cancelled"}),
        );
        assert_eq!(list_all(&home)[0].status, "cancelled");
        let all = list_all(&home);
        let columns = crate::render::task_board_columns(&all);
        let total: usize = columns.iter().map(|c| c.len()).sum();
        assert_eq!(total, 0, "cancelled task should not appear in any column");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn task_board_shift_d_marks_done() {
        // Test Shift+D (done action) from all 3 non-done columns
        for (label, setup) in [
            (
                "backlog",
                vec![(
                    "create",
                    r#"{"action":"create","title":"t","priority":"low"}"#,
                )],
            ),
            (
                "open",
                vec![(
                    "create",
                    r#"{"action":"create","title":"t","priority":"normal"}"#,
                )],
            ),
            (
                "in_progress",
                vec![
                    (
                        "create",
                        r#"{"action":"create","title":"t","priority":"normal"}"#,
                    ),
                    ("claim", r#"{"action":"claim","id":"__ID__"}"#),
                ],
            ),
        ] {
            let home = tmp_home(&format!("shift_d_{label}"));
            let mut id = String::new();
            for (_, json_str) in &setup {
                let json_str = json_str.replace("__ID__", &id);
                let v: serde_json::Value =
                    serde_json::from_str(&json_str).expect("test JSON literal");
                let r = handle(&home, "user", &v);
                if let Some(i) = r["id"].as_str() {
                    id = i.to_string();
                }
            }
            if id.is_empty() {
                id = list_all(&home)[0].id.clone();
            }
            let r = handle(
                &home,
                "user",
                &serde_json::json!({"action": "done", "id": id}),
            );
            assert_eq!(r["status"], "done", "failed for {label}");
            assert_eq!(list_all(&home)[0].status, "done", "failed for {label}");
            std::fs::remove_dir_all(&home).ok();
        }
    }

    #[test]
    fn test_concurrent_creates_unique_ids() {
        let home = tmp_home("concurrent_ids");
        let home_arc = std::sync::Arc::new(home.clone());
        let threads: Vec<_> = (0..20)
            .map(|i| {
                let h = home_arc.clone();
                std::thread::spawn(move || {
                    handle(
                        &h,
                        &format!("agent-{i}"),
                        &serde_json::json!({"action": "create", "title": format!("task-{i}")}),
                    )
                })
            })
            .collect();
        let ids: Vec<String> = threads
            .into_iter()
            .map(|h| {
                let r = h.join().expect("thread");
                assert_eq!(r["status"], "created");
                r["id"].as_str().expect("id").to_string()
            })
            .collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            20,
            "all 20 task IDs must be unique, got: {ids:?}"
        );
        let tasks = list_all(&home);
        assert_eq!(tasks.len(), 20);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_task_blocked_when_dep_not_done() {
        let home = tmp_home("dep-blocked");
        // Create dep task (stays open)
        let r1 = handle(
            &home,
            "u",
            &serde_json::json!({"action": "create", "title": "dep"}),
        );
        let dep_id = r1["id"].as_str().unwrap().to_string();
        // Create task depending on dep
        handle(
            &home,
            "u",
            &serde_json::json!({
                "action": "create", "title": "child", "depends_on": [dep_id]
            }),
        );
        // List triggers eval → child should be blocked
        let listed = handle(&home, "u", &serde_json::json!({"action": "list"}));
        let tasks = listed["tasks"].as_array().unwrap();
        let child = tasks.iter().find(|t| t["title"] == "child").unwrap();
        assert_eq!(
            child["status"], "blocked",
            "task with open dep must be blocked"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_task_auto_unblock_when_all_deps_done() {
        let home = tmp_home("dep-unblock");
        let r1 = handle(
            &home,
            "u",
            &serde_json::json!({"action": "create", "title": "dep"}),
        );
        let dep_id = r1["id"].as_str().unwrap().to_string();
        handle(
            &home,
            "u",
            &serde_json::json!({
                "action": "create", "title": "child", "depends_on": [dep_id]
            }),
        );
        // List → child blocked
        let listed = handle(&home, "u", &serde_json::json!({"action": "list"}));
        let child = listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["title"] == "child")
            .unwrap();
        assert_eq!(child["status"], "blocked");

        // Complete dep → done triggers re-eval → child auto-unblocks
        handle(
            &home,
            "u",
            &serde_json::json!({"action": "done", "id": dep_id, "result": "ok"}),
        );
        let listed = handle(&home, "u", &serde_json::json!({"action": "list"}));
        let child = listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["title"] == "child")
            .unwrap();
        assert_eq!(
            child["status"], "open",
            "child must auto-unblock when dep is done"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// PR3 — depends_on is set in the Created event and immutable via
    /// the MCP surface (no "depends_on update" event variant). Circular
    /// deps can still arise from migration of a circular legacy
    /// `tasks.json` or from forward-references in Created events
    /// (Task A's `depends_on=[B]` written before Task B exists).
    /// This test exercises the latter path via direct event_log writes.
    #[test]
    fn test_circular_dep_no_infinite_loop() {
        let home = tmp_home("dep-circular");
        let inst = crate::task_events::InstanceName::from("u");
        // Hand-craft two Created events with cross-references.
        crate::task_events::append(
            &home,
            &inst,
            crate::task_events::TaskEvent::Created {
                task_id: crate::task_events::TaskId("t-A".into()),
                title: "A".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                branch: None,
                depends_on: vec![crate::task_events::TaskId("t-B".into())],
                routed_to: None,
                bind: None,
                eta_secs: None,
            },
        )
        .unwrap();
        crate::task_events::append(
            &home,
            &inst,
            crate::task_events::TaskEvent::Created {
                task_id: crate::task_events::TaskId("t-B".into()),
                title: "B".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: None,
                due_at: None,
                branch: None,
                depends_on: vec![crate::task_events::TaskId("t-A".into())],
                routed_to: None,
                bind: None,
                eta_secs: None,
            },
        )
        .unwrap();
        // List must not hang — both should be blocked (neither is done).
        let listed = handle(&home, "u", &serde_json::json!({"action": "list"}));
        let tasks = listed["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2, "must return without infinite loop");
        for t in tasks {
            assert_eq!(
                t["status"], "blocked",
                "circular dep tasks must be blocked: {}",
                t["title"]
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_task_create_accepts_due_at_iso() {
        let home = tmp_home("due-at-iso");
        let future = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        let result = handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "create", "title": "timed", "due_at": future}),
        );
        assert_eq!(result["status"], "created");
        let listed = handle(&home, "agent1", &serde_json::json!({"action": "list"}));
        let task = &listed["tasks"][0];
        assert!(task["due_at"].is_string(), "due_at must be set");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_overdue_claimed_task_unclaimed_by_sweep() {
        let home = tmp_home("overdue-sweep");
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let r = handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "create", "title": "overdue", "due_at": past}),
        );
        let id = r["id"].as_str().unwrap().to_string();
        handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        let unclaimed = sweep_overdue_claimed(&home);
        assert_eq!(unclaimed, vec![id.clone()]);
        let listed = handle(&home, "agent1", &serde_json::json!({"action": "list"}));
        let task = &listed["tasks"][0];
        assert_eq!(task["status"], "open");
        assert!(task["assignee"].is_null());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_not_yet_due_not_touched() {
        let home = tmp_home("not-due");
        let future = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        let r = handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "create", "title": "future", "due_at": future}),
        );
        let id = r["id"].as_str().unwrap().to_string();
        handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        let unclaimed = sweep_overdue_claimed(&home);
        assert!(unclaimed.is_empty(), "future task must not be unclaimed");
        let listed = handle(&home, "agent1", &serde_json::json!({"action": "list"}));
        assert_eq!(listed["tasks"][0]["status"], "claimed");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_done_task_ignored_by_sweep() {
        let home = tmp_home("done-ignore");
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let r = handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "create", "title": "done-overdue", "due_at": past}),
        );
        let id = r["id"].as_str().unwrap().to_string();
        handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "done", "id": id, "result": "finished"}),
        );
        let unclaimed = sweep_overdue_claimed(&home);
        assert!(unclaimed.is_empty(), "done task must not be unclaimed");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_task_create_accepts_duration_30m() {
        let home = tmp_home("dur-30m");
        let before = chrono::Utc::now();
        let result = handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "create", "title": "timed", "duration": "30m"}),
        );
        assert_eq!(result["status"], "created");
        let listed = handle(&home, "agent1", &serde_json::json!({"action": "list"}));
        let due_str = listed["tasks"][0]["due_at"].as_str().expect("due_at set");
        let due = chrono::DateTime::parse_from_rfc3339(due_str)
            .expect("valid rfc3339")
            .with_timezone(&chrono::Utc);
        let expected = before + chrono::Duration::minutes(30);
        let diff = (due - expected).num_seconds().abs();
        assert!(diff < 5, "due_at should be ~now+30m, diff={diff}s");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_task_create_duration_variants() {
        let home = tmp_home("dur-variants");
        let now = chrono::Utc::now();
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": "1h", "duration": "1h"}),
        );
        let listed = handle(&home, "a", &serde_json::json!({"action": "list"}));
        let due =
            chrono::DateTime::parse_from_rfc3339(listed["tasks"][0]["due_at"].as_str().unwrap())
                .unwrap()
                .with_timezone(&chrono::Utc);
        assert!((due - now).num_minutes() >= 59);
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": "2d", "duration": "2d"}),
        );
        let listed = handle(&home, "a", &serde_json::json!({"action": "list"}));
        let due =
            chrono::DateTime::parse_from_rfc3339(listed["tasks"][1]["due_at"].as_str().unwrap())
                .unwrap()
                .with_timezone(&chrono::Utc);
        assert!((due - now).num_hours() >= 47);
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": "bad", "duration": "xyz"}),
        );
        let listed = handle(&home, "a", &serde_json::json!({"action": "list"}));
        assert!(
            listed["tasks"][2]["due_at"].is_null(),
            "invalid duration → no due_at"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_daemon_maintenance_unclaims_overdue_and_logs_event() {
        let home = tmp_home("daemon-maint");
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let r = handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "create", "title": "overdue-maint", "due_at": past}),
        );
        let id = r["id"].as_str().unwrap().to_string();
        handle(
            &home,
            "agent1",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        crate::daemon::run_task_maintenance(&home);
        let listed = handle(&home, "agent1", &serde_json::json!({"action": "list"}));
        assert_eq!(listed["tasks"][0]["status"], "open");
        assert!(listed["tasks"][0]["assignee"].is_null());
        let log_content = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            log_content.contains("task_overdue_unclaimed"),
            "event_log must contain task_overdue_unclaimed entry"
        );
        assert!(
            log_content.contains(&id),
            "event_log must reference the task id"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Mutation integrity tests ──

    fn write_fleet_yaml(home: &std::path::Path, instances: &[&str]) {
        let entries: Vec<String> = instances
            .iter()
            .map(|n| format!("  {n}:\n    backend: claude"))
            .collect();
        let yaml = format!("instances:\n{}", entries.join("\n"));
        std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).ok();
    }

    #[test]
    fn test_claim_unknown_instance_rejected() {
        let home = tmp_home("claim-unknown");
        write_fleet_yaml(&home, &["known-agent"]);
        let r = handle(
            &home,
            "known-agent",
            &serde_json::json!({"action": "create", "title": "t"}),
        );
        let id = r["id"].as_str().unwrap();
        // Unknown instance tries to claim
        let r = handle(
            &home,
            "phantom",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        assert!(
            r["error"].as_str().unwrap().contains("not found in fleet"),
            "got: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_claim_already_claimed_by_other_rejected() {
        let home = tmp_home("claim-stolen");
        write_fleet_yaml(&home, &["agent-a", "agent-b"]);
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "t"}),
        );
        let id = r["id"].as_str().unwrap();
        handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        // agent-b tries to steal
        let r = handle(
            &home,
            "agent-b",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        assert!(
            r["error"].as_str().unwrap().contains("only 'open'"),
            "claimed task must not be claimable by others: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_claim_self_reclaim_ok() {
        let home = tmp_home("claim-reclaim");
        write_fleet_yaml(&home, &["agent-a"]);
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "t"}),
        );
        let id = r["id"].as_str().unwrap();
        handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        // Re-claim own task → ok
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        assert_eq!(r["status"], "claimed", "self re-claim must succeed");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_done_non_assignee_rejected() {
        let home = tmp_home("done-non-assignee");
        write_fleet_yaml(&home, &["agent-a", "agent-b"]);
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "t"}),
        );
        let id = r["id"].as_str().unwrap();
        handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        // agent-b tries to mark done
        let r = handle(
            &home,
            "agent-b",
            &serde_json::json!({"action": "done", "id": id}),
        );
        assert!(
            r["error"].as_str().unwrap().contains("not authorized"),
            "got: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_done_assignee_ok() {
        let home = tmp_home("done-assignee");
        write_fleet_yaml(&home, &["agent-a"]);
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "t"}),
        );
        let id = r["id"].as_str().unwrap();
        handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "done", "id": id, "result": "ok"}),
        );
        assert_eq!(r["status"], "done");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_done_orchestrator_ok() {
        let home = tmp_home("done-orch");
        write_fleet_yaml(&home, &["lead", "worker"]);
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "dev", "members": ["lead", "worker"], "orchestrator": "lead"}),
        );
        let r = handle(
            &home,
            "lead",
            &serde_json::json!({"action": "create", "title": "t", "assignee": "worker"}),
        );
        let id = r["id"].as_str().unwrap();
        handle(
            &home,
            "worker",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        // Orchestrator marks done on behalf
        let r = handle(
            &home,
            "lead",
            &serde_json::json!({"action": "done", "id": id, "result": "merged"}),
        );
        assert_eq!(
            r["status"], "done",
            "orchestrator must be able to mark done"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_update_non_owner_rejected() {
        let home = tmp_home("update-non-owner");
        write_fleet_yaml(&home, &["agent-a", "agent-b"]);
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "t"}),
        );
        let id = r["id"].as_str().unwrap();
        handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        // agent-b tries to change priority
        let r = handle(
            &home,
            "agent-b",
            &serde_json::json!({"action": "update", "id": id, "priority": "urgent"}),
        );
        assert!(
            r["error"].as_str().unwrap().contains("not authorized"),
            "got: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_update_orchestrator_ok() {
        let home = tmp_home("update-orch");
        write_fleet_yaml(&home, &["lead", "worker"]);
        crate::teams::create(
            &home,
            &serde_json::json!({"name": "dev", "members": ["lead", "worker"], "orchestrator": "lead"}),
        );
        let r = handle(
            &home,
            "lead",
            &serde_json::json!({"action": "create", "title": "t", "assignee": "worker"}),
        );
        let id = r["id"].as_str().unwrap();
        handle(
            &home,
            "worker",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        let r = handle(
            &home,
            "lead",
            &serde_json::json!({"action": "update", "id": id, "priority": "urgent"}),
        );
        assert_eq!(
            r["status"], "updated",
            "orchestrator must be able to update"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_update_release_claim_ok() {
        let home = tmp_home("update-release");
        write_fleet_yaml(&home, &["agent-a"]);
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "t"}),
        );
        let id = r["id"].as_str().unwrap();
        handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        // Release claim by setting status=open
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "update", "id": id, "status": "open"}),
        );
        assert_eq!(r["status"], "updated");
        let tasks = list_all(&home);
        let t = tasks.iter().find(|t| t.id == id).unwrap();
        assert_eq!(t.status, "open");
        assert!(t.assignee.is_none(), "release must clear assignee");
        assert!(t.routed_to.is_none(), "release must clear routed_to");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_claim_blocked_task_rejected() {
        let home = tmp_home("claim-blocked");
        write_fleet_yaml(&home, &["agent-a"]);
        // Create dep (stays open) + child that depends on it (auto-blocked)
        let r1 = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "dep"}),
        );
        let dep_id = r1["id"].as_str().unwrap().to_string();
        let r2 = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "child", "depends_on": [dep_id]}),
        );
        let child_id = r2["id"].as_str().unwrap();
        // List triggers dep eval → child becomes blocked
        handle(&home, "agent-a", &serde_json::json!({"action": "list"}));
        // Try to claim blocked task
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "claim", "id": child_id}),
        );
        assert!(
            r["error"].as_str().unwrap().contains("only 'open'"),
            "blocked task must not be claimable: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_update_non_owner_on_open_assigned_rejected() {
        let home = tmp_home("update-non-owner-assigned");
        write_fleet_yaml(&home, &["agent-a", "agent-b"]);
        // Create task assigned to agent-a (but not claimed yet → status open)
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "t", "assignee": "agent-a"}),
        );
        let id = r["id"].as_str().unwrap();
        // agent-b tries to change priority on agent-a's assigned task
        let r = handle(
            &home,
            "agent-b",
            &serde_json::json!({"action": "update", "id": id, "priority": "urgent"}),
        );
        assert!(
            r["error"].as_str().unwrap().contains("not authorized"),
            "non-owner must not update assigned task: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // --- Sprint 8 PR-M: Done TTL filter ---
    // PR3 cutover note — backdating `updated_at` requires writing
    // hand-crafted envelopes with old timestamps directly to
    // `task_events.jsonl` (the public `task_events::append` always
    // stamps `Utc::now()`). The TTL filter logic itself is exercised by
    // every other test that creates Done tasks; this specific 14-day
    // boundary check is gated behind a future invariant test that uses
    // direct envelope writes.

    #[test]
    fn test_list_done_filter_returns_all() {
        let home = tmp_home("done-ttl-all");
        let r1 = handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": "old"}),
        );
        let id1 = r1["id"].as_str().unwrap().to_string();
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "claim", "id": id1}),
        );
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "done", "id": id1, "result": "ok"}),
        );

        let r2 = handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": "new"}),
        );
        let id2 = r2["id"].as_str().unwrap().to_string();
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "claim", "id": id2}),
        );
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "done", "id": id2, "result": "ok"}),
        );

        let _ = crate::store::mutate_versioned(
            &crate::store::store_path(&home, "tasks.json"),
            |store: &mut TaskStore| {
                if let Some(t) = store.tasks.iter_mut().find(|t| t.id == id1) {
                    t.updated_at = (chrono::Utc::now() - chrono::Duration::days(15)).to_rfc3339();
                }
                Ok(())
            },
        );

        // Explicit filter_status=done returns ALL done tasks regardless of age
        let listed = handle(
            &home,
            "a",
            &serde_json::json!({"action": "list", "filter_status": "done"}),
        );
        let tasks = listed["tasks"].as_array().unwrap();
        assert_eq!(
            tasks.len(),
            2,
            "filter_status=done must return all done tasks"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_list_non_done_always_returns() {
        let home = tmp_home("done-ttl-nondone");
        // Create open + claimed tasks — they should always appear
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": "open task"}),
        );
        let r2 = handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": "claimed task"}),
        );
        let id2 = r2["id"].as_str().unwrap().to_string();
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "claim", "id": id2}),
        );

        let listed = handle(&home, "a", &serde_json::json!({"action": "list"}));
        let tasks = listed["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2, "non-done tasks must always appear");
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Sprint 24 P0 PR2 — bridge-phase migration tests ─────────────

    /// Migration helper imports every legacy `tasks.json` task into the
    /// event log: open / claimed / done / cancelled / blocked all reach
    /// the same final status under replay() that they had on disk.
    #[test]
    fn migration_imports_legacy_tasks_to_event_log() {
        let home = tmp_home("migration_import");
        // Seed a legacy tasks.json with one task per status branch.
        crate::store::mutate_versioned(&store_path(&home), |store: &mut TaskStore| {
            for (id, status, assignee) in [
                ("t-mig-open", "open", None),
                ("t-mig-claimed", "claimed", Some("agent-a")),
                ("t-mig-in-prog", "in_progress", Some("agent-b")),
                ("t-mig-done", "done", Some("agent-c")),
                ("t-mig-cancelled", "cancelled", None),
                ("t-mig-blocked", "blocked", None),
            ] {
                store.tasks.push(Task {
                    id: id.into(),
                    title: format!("title {id}"),
                    description: "legacy".into(),
                    status: status.into(),
                    priority: "normal".into(),
                    assignee: assignee.map(String::from),
                    routed_to: None,
                    created_by: "operator".into(),
                    depends_on: Vec::new(),
                    result: if status == "done" {
                        Some("completed".into())
                    } else {
                        None
                    },
                    created_at: chrono::Utc::now().to_rfc3339(),
                    updated_at: chrono::Utc::now().to_rfc3339(),
                    due_at: None,
                    branch: None,
                    started_at: None,
                    eta_secs: None,
                });
            }
            Ok(())
        })
        .unwrap();

        let report = migrate_legacy_tasks_json_to_event_log(&home).unwrap();
        assert_eq!(report.migrated, 6, "all 6 legacy tasks imported");
        assert_eq!(report.skipped, 0);

        // Replay should observe 6 tasks at their pre-migration statuses.
        let state = crate::task_events::replay(&home).unwrap();
        assert_eq!(state.tasks.len(), 6);
        let lookup = |id: &str| {
            state
                .tasks
                .get(&crate::task_events::TaskId::from(id))
                .map(|t| t.status)
                .unwrap()
        };
        assert_eq!(lookup("t-mig-open"), crate::task_events::TaskStatus::Open);
        assert_eq!(
            lookup("t-mig-claimed"),
            crate::task_events::TaskStatus::Claimed
        );
        assert_eq!(
            lookup("t-mig-in-prog"),
            crate::task_events::TaskStatus::InProgress
        );
        assert_eq!(lookup("t-mig-done"), crate::task_events::TaskStatus::Done);
        assert_eq!(
            lookup("t-mig-cancelled"),
            crate::task_events::TaskStatus::Cancelled
        );
        assert_eq!(
            lookup("t-mig-blocked"),
            crate::task_events::TaskStatus::Blocked
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Migration is idempotent: re-running on already-migrated state is a
    /// no-op (every task_id already in the event log → skipped).
    #[test]
    fn migration_idempotent_on_second_run() {
        let home = tmp_home("migration_idempotent");
        crate::store::mutate_versioned(&store_path(&home), |store: &mut TaskStore| {
            store.tasks.push(Task {
                id: "t-idp-1".into(),
                title: "idem".into(),
                description: String::new(),
                status: "open".into(),
                priority: "normal".into(),
                assignee: None,
                routed_to: None,
                created_by: "op".into(),
                depends_on: Vec::new(),
                result: None,
                created_at: chrono::Utc::now().to_rfc3339(),
                updated_at: chrono::Utc::now().to_rfc3339(),
                due_at: None,
                branch: None,
                started_at: None,
                eta_secs: None,
            });
            Ok(())
        })
        .unwrap();

        let first = migrate_legacy_tasks_json_to_event_log(&home).unwrap();
        assert_eq!(first.migrated, 1);
        assert_eq!(first.skipped, 0);

        // PR4 — after a successful first migration the live `tasks.json`
        // is archived to `tasks.json.legacy_pre_v2`. The second run sees
        // an empty input store and reports zero work.
        let second = migrate_legacy_tasks_json_to_event_log(&home).unwrap();
        assert_eq!(
            second.migrated, 0,
            "second run finds no live tasks.json → no new emit"
        );
        assert_eq!(second.skipped, 0);
        // Archive sidecar should exist for archeology.
        assert!(
            home.join("tasks.json.legacy_pre_v2").exists(),
            "PR4 retire: live tasks.json archived to .legacy_pre_v2"
        );
        assert!(
            !home.join("tasks.json").exists(),
            "PR4 retire: live tasks.json removed from daemon's read path"
        );

        // Replay confirms exactly one TaskRecord (no duplicate Created
        // event accumulated across the two migration runs).
        let state = crate::task_events::replay(&home).unwrap();
        assert_eq!(state.tasks.len(), 1);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_task_create_with_branch() {
        let home = tmp_home("branch");
        let result = handle(
            &home,
            "dev-lead",
            &serde_json::json!({"action": "create", "title": "Fix bug", "branch": "sprint-28-fix"}),
        );
        assert!(
            result.get("error").is_none(),
            "create must succeed: {result}"
        );
        let tasks = list_all(&home);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].branch.as_deref(), Some("sprint-28-fix"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_task_create_without_branch_defaults_none() {
        let home = tmp_home("no-branch");
        let result = handle(
            &home,
            "dev-lead",
            &serde_json::json!({"action": "create", "title": "Fix bug"}),
        );
        assert!(result.get("error").is_none());
        let tasks = list_all(&home);
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].branch.is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    // --- M4 r1-fix: ACL allow-list + state guard tests ---

    #[test]
    fn system_identity_in_allow_list() {
        assert!(is_system_identity("system:auto_close"));
        assert!(is_system_identity("system:overdue_sweep"));
    }

    #[test]
    fn non_system_identity_rejected() {
        assert!(!is_system_identity("random_agent"));
        assert!(!is_system_identity("system")); // bare "system" not in list
    }

    #[test]
    fn done_action_honors_done_source_override() {
        let home = tmp_home("done_source");
        // Create a task, claim it, then done with custom done_source
        let created = handle(
            &home,
            "dev",
            &serde_json::json!({"action": "create", "title": "test task"}),
        );
        let id = created["id"].as_str().expect("task id");
        handle(
            &home,
            "dev",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        let done = handle(
            &home,
            "dev",
            &serde_json::json!({
                "action": "done",
                "id": id,
                "done_source": {
                    "via": "AutoCloseOnPrMerge",
                    "branch": "feat/test",
                    "merged_at": "2026-05-01T00:00:00Z"
                }
            }),
        );
        assert_eq!(done["status"], "done", "done should succeed: {done}");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn system_identity_can_mutate_others_task() {
        let home = tmp_home("sys_acl");
        // Create task owned by "dev"
        let created = handle(
            &home,
            "dev",
            &serde_json::json!({"action": "create", "title": "dev task"}),
        );
        let id = created["id"].as_str().expect("task id");
        handle(
            &home,
            "dev",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        // system:auto_close should be able to close dev's task
        let done = handle(
            &home,
            "system:auto_close",
            &serde_json::json!({"action": "done", "id": id}),
        );
        assert_eq!(
            done["status"], "done",
            "system identity should bypass ACL: {done}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ----------------------------------------------------------------------
    // #789 anchor (§3.10 red→green) — task action=done triggers cleanup of
    // empty init commits accumulated post-bind. Pre-C3: tasks::handle("done")
    // does NOT invoke `clean_empty_init_commits`, so the backend-style
    // empty inits ride along to push. Post-C3: handle("done") cleans up at
    // the task-completion workflow boundary.
    //
    // Cross-platform: no `#[cfg(unix)]` per #785/#786 precedent + reviewer C6.
    // ----------------------------------------------------------------------

    #[test]
    #[cfg(unix)]
    fn task_done_cleans_post_lease_empty_init_commits() {
        // Setup: bind agent to a temp worktree that has accumulated 3
        // empty `init` commits between origin/main and HEAD (simulating
        // backend session-checkpoint heartbeat bursts post-lease).
        // Asserting that `task action=done` cleans them at the workflow
        // boundary.
        let home = tmp_home("789-task-done-cleanup");
        let worktree = home.join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        let bypass = ("AGEND_GIT_BYPASS", "1");

        // git init + initial commit (origin/main reference)
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&worktree)
            .env(bypass.0, bypass.1)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ])
            .current_dir(&worktree)
            .env(bypass.0, bypass.1)
            .output()
            .unwrap();
        // Snapshot HEAD into refs/remotes/origin/main so the cleanup's
        // `git log origin/main..HEAD` range resolves locally.
        let initial_sha = String::from_utf8(
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&worktree)
                .env(bypass.0, bypass.1)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        std::process::Command::new("git")
            .args(["update-ref", "refs/remotes/origin/main", &initial_sha])
            .current_dir(&worktree)
            .env(bypass.0, bypass.1)
            .output()
            .unwrap();

        // Accumulate 3 empty `init` commits on HEAD (post-lease pollution).
        for _ in 0..3 {
            std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "--allow-empty",
                    "-m",
                    "init",
                ])
                .current_dir(&worktree)
                .env(bypass.0, bypass.1)
                .output()
                .unwrap();
        }

        // Write binding so tasks::handle("done") can locate the worktree.
        let runtime = crate::paths::runtime_dir(&home).join("dev");
        std::fs::create_dir_all(&runtime).ok();
        std::fs::write(
            runtime.join("binding.json"),
            serde_json::to_string(&serde_json::json!({
                "version": 1,
                "agent": "dev",
                "task_id": "T-1",
                "branch": "feat/p789",
                "worktree": worktree.display().to_string(),
                "source_repo": worktree.display().to_string(),
                "issued_at": "2026-01-01T00:00:00Z",
            }))
            .unwrap(),
        )
        .unwrap();

        // Confirm 3 empty inits sit between origin/main..HEAD pre-call.
        let pre_count = String::from_utf8(
            std::process::Command::new("git")
                .args(["log", "origin/main..HEAD", "--format=%H"])
                .current_dir(&worktree)
                .env(bypass.0, bypass.1)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .lines()
        .count();
        assert_eq!(pre_count, 3, "fixture must have 3 empty inits pre-call");

        // Task lifecycle: create + claim + done.
        let created = handle(
            &home,
            "dev",
            &serde_json::json!({"action": "create", "title": "p789 anchor"}),
        );
        let id = created["id"].as_str().expect("task id");
        handle(
            &home,
            "dev",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        let done = handle(
            &home,
            "dev",
            &serde_json::json!({"action": "done", "id": id}),
        );
        assert_eq!(done["status"], "done", "task done must succeed: {done}");

        // §3.10 anchor assertion: post-done, the empty init commits are
        // cleaned (HEAD back to origin/main).
        let post_count = String::from_utf8(
            std::process::Command::new("git")
                .args(["log", "origin/main..HEAD", "--format=%H"])
                .current_dir(&worktree)
                .env(bypass.0, bypass.1)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .lines()
        .count();
        assert_eq!(
            post_count, 0,
            "task action=done MUST trigger clean_empty_init_commits on bound worktree \
             (pre-#789: handler does not call cleanup → 3 commits remain → test fails)"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    // ── #808 ghost-owner ACL deadlock: auto-orphan + force flag tests ──

    #[test]
    fn test_ghost_owner_update_without_force_errors_with_acl() {
        // Baseline regression: today operator cannot cancel a task
        // whose owner is no longer in the fleet (ghost-owner ACL
        // deadlock the issue describes). The #808 force flag must NOT
        // change this behavior when force=false / absent — the ACL
        // gate stays load-bearing so accidental cancels still require
        // an explicit force opt-in.
        let home = tmp_home("ghost_acl_baseline");
        write_fleet_yaml(&home, &["operator"]);
        let r = handle(
            &home,
            "operator",
            &serde_json::json!({
                "action": "create",
                "title": "stuck",
                "assignee": "ghost-instance",
            }),
        );
        let id = r["id"].as_str().expect("id").to_string();
        let r = handle(
            &home,
            "operator",
            &serde_json::json!({
                "action": "update",
                "id": id,
                "status": "cancelled",
            }),
        );
        assert!(
            r["error"]
                .as_str()
                .map(|e| e.contains("not authorized"))
                .unwrap_or(false),
            "ghost-owned task cancel without force must surface ACL error, got: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_force_update_true_without_force_reason_rejected() {
        // Force-flag contract (mirrors comms.rs:200-218): force=true
        // without a non-empty force_reason must surface a validator
        // error, NOT fall through to ACL or succeed silently. The
        // grammar match is "force_reason" (the validator message names
        // the missing field) — pre-fix the handler ignores force and
        // returns the regular ACL error, so this test is RED until C2
        // ships.
        let home = tmp_home("force_no_reason");
        write_fleet_yaml(&home, &["operator"]);
        let r = handle(
            &home,
            "operator",
            &serde_json::json!({
                "action": "create",
                "title": "stuck",
                "assignee": "ghost-instance",
            }),
        );
        let id = r["id"].as_str().expect("id").to_string();
        let r = handle(
            &home,
            "operator",
            &serde_json::json!({
                "action": "update",
                "id": id,
                "status": "cancelled",
                "force": true,
            }),
        );
        assert!(
            r["error"]
                .as_str()
                .map(|e| e.contains("force_reason"))
                .unwrap_or(false),
            "force=true without force_reason must surface validator error, got: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_force_update_with_reason_cancels_ghost_and_logs_audit() {
        // GREEN test 1: force=true + non-empty force_reason on
        // `action=update status=cancelled` bypasses the ACL gate AND
        // pushes a `task_force_update` entry into event-log.jsonl
        // (cross-board audit) AND embeds the force_reason into the
        // per-task event's reason field (per-task replay audit).
        let home = tmp_home("force_cancel_ghost");
        write_fleet_yaml(&home, &["operator"]);
        let r = handle(
            &home,
            "operator",
            &serde_json::json!({
                "action": "create",
                "title": "stuck-by-ghost",
                "assignee": "ghost-instance",
            }),
        );
        let id = r["id"].as_str().expect("id").to_string();
        let r = handle(
            &home,
            "operator",
            &serde_json::json!({
                "action": "update",
                "id": id,
                "status": "cancelled",
                "force": true,
                "force_reason": "post-#808 board hygiene 2026-05-15",
            }),
        );
        assert_eq!(
            r["status"], "updated",
            "force=true + force_reason must succeed despite ghost owner, got: {r}"
        );
        let tasks = list_all(&home);
        let t = tasks
            .iter()
            .find(|t| t.id == id)
            .expect("task still present");
        assert_eq!(t.status, "cancelled", "task must be cancelled");
        // Cross-board audit: event-log.jsonl records the force action.
        let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            log.contains("task_force_update"),
            "event-log.jsonl must record task_force_update entry, got: {log}"
        );
        assert!(
            log.contains("post-#808 board hygiene"),
            "event-log.jsonl must capture the force_reason, got: {log}"
        );
        // Per-task replay audit: the Cancelled event's reason field
        // carries the forced marker.
        let task_log = std::fs::read_to_string(home.join("task_events.jsonl")).unwrap_or_default();
        assert!(
            task_log.contains("forced by 'operator'"),
            "Cancelled event reason must carry forced marker, got: {task_log}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_force_done_with_reason_succeeds_on_done_arm() {
        // BONUS test (mandatory per dispatch spec): force flag must
        // also gate the `action=done` arm — operators sometimes close
        // ghost-owned tasks as Done rather than Cancelled when the
        // work was effectively completed before the owner disbanded.
        let home = tmp_home("force_done_ghost");
        write_fleet_yaml(&home, &["operator"]);
        let r = handle(
            &home,
            "operator",
            &serde_json::json!({
                "action": "create",
                "title": "ghost-done",
                "assignee": "ghost-instance",
            }),
        );
        let id = r["id"].as_str().expect("id").to_string();
        // Without force, the done arm rejects (mirrors RED1 behavior).
        let r_blocked = handle(
            &home,
            "operator",
            &serde_json::json!({"action": "done", "id": id}),
        );
        assert!(
            r_blocked["error"]
                .as_str()
                .map(|e| e.contains("not authorized"))
                .unwrap_or(false),
            "done arm without force must reject ghost-owned task, got: {r_blocked}"
        );
        // With force + reason, the done arm proceeds and event-log
        // records the cross-board audit (task_force_done).
        let r = handle(
            &home,
            "operator",
            &serde_json::json!({
                "action": "done",
                "id": id,
                "result": "completed before owner disband",
                "force": true,
                "force_reason": "post-#808 done-arm hygiene",
            }),
        );
        assert_eq!(
            r["status"], "done",
            "force=true done must succeed, got: {r}"
        );
        let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            log.contains("task_force_done"),
            "event-log.jsonl must record task_force_done entry, got: {log}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #806 list default filter (Part A) — RED test + scaffolding ──

    #[test]
    fn test_list_default_returns_only_actionable_statuses() {
        // #806 RED test 1: pre-fix `task action=list` with no filter
        // returns every status including `done` / `cancelled`,
        // producing the 504KB / 3847-line dump the issue documents.
        // Post-fix: default trim → {open,claimed,in_progress,blocked}.
        // A `filtered_default` flag on the response signals callers
        // that the trim fired so audit/forensic consumers can re-call
        // with `include_history=true` if needed.
        let home = tmp_home("list_default_actionable");
        write_fleet_yaml(&home, &["agent-a"]);
        let create_with_status = |title: &str, target_status: &str| {
            let r = handle(
                &home,
                "agent-a",
                &serde_json::json!({"action": "create", "title": title, "assignee": "agent-a"}),
            );
            let id = r["id"].as_str().expect("id").to_string();
            if target_status != "open" {
                let r = handle(
                    &home,
                    "agent-a",
                    &serde_json::json!({"action": "update", "id": id, "status": target_status}),
                );
                assert_eq!(r["status"], "updated", "seed transition failed: {r}");
            }
            id
        };
        let open_id = create_with_status("open task", "open");
        let in_progress_id = create_with_status("active task", "in_progress");
        let cancelled_id = create_with_status("stale cancel", "cancelled");
        let done_id = create_with_status("stale done", "done");

        // include_history=true must surface everything (existing audit
        // consumers opt in this way).
        let r_history = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "list", "include_history": true}),
        );
        let history_ids: std::collections::HashSet<_> = r_history["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .filter_map(|t| t["id"].as_str().map(String::from))
            .collect();
        for id in [&open_id, &in_progress_id, &cancelled_id, &done_id] {
            assert!(
                history_ids.contains(id),
                "include_history=true must surface {id}, got {history_ids:?}"
            );
        }
        assert_eq!(
            r_history["filtered_default"], false,
            "include_history=true must report filtered_default=false"
        );

        // Default (no filter) — only actionable statuses.
        let r = handle(&home, "agent-a", &serde_json::json!({"action": "list"}));
        let tasks = r["tasks"].as_array().expect("tasks");
        let ids: std::collections::HashSet<_> = tasks
            .iter()
            .filter_map(|t| t["id"].as_str().map(String::from))
            .collect();
        assert!(
            ids.contains(&open_id),
            "open task must surface in default list, got {ids:?}"
        );
        assert!(
            ids.contains(&in_progress_id),
            "in_progress task must surface in default list, got {ids:?}"
        );
        assert!(
            !ids.contains(&cancelled_id),
            "cancelled task must NOT surface in default list, got {ids:?}"
        );
        assert!(
            !ids.contains(&done_id),
            "done task must NOT surface in default list, got {ids:?}"
        );
        assert_eq!(
            r["filtered_default"], true,
            "default list must report filtered_default=true"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_list_explicit_filter_status_overrides_default() {
        // GREEN 1b: an explicit `filter_status=cancelled` must still
        // return cancelled entries; the default trim only fires when
        // neither filter_status nor include_history is supplied.
        let home = tmp_home("list_explicit_filter");
        write_fleet_yaml(&home, &["a"]);
        let r = handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": "to-cancel", "assignee": "a"}),
        );
        let id = r["id"].as_str().expect("id").to_string();
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "update", "id": id, "status": "cancelled"}),
        );
        let r = handle(
            &home,
            "a",
            &serde_json::json!({"action": "list", "filter_status": "cancelled"}),
        );
        let tasks = r["tasks"].as_array().expect("tasks");
        assert!(
            tasks.iter().any(|t| t["id"] == id),
            "explicit filter_status=cancelled must surface cancelled task: {tasks:?}"
        );
        assert!(
            tasks.iter().all(|t| t["status"] == "cancelled"),
            "filter_status=cancelled must restrict to cancelled only"
        );
        assert_eq!(
            r["filtered_default"], false,
            "explicit filter_status must report filtered_default=false"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_list_limit_truncates_newest_first() {
        // GREEN 1c: `limit=N` caps response newest-first by updated_at
        // for caller-visible pagination without cursors.
        let home = tmp_home("list_limit");
        write_fleet_yaml(&home, &["a"]);
        let mut ids = Vec::new();
        for title in ["oldest", "middle", "newest"] {
            let r = handle(
                &home,
                "a",
                &serde_json::json!({"action": "create", "title": title, "assignee": "a"}),
            );
            ids.push(r["id"].as_str().expect("id").to_string());
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let r = handle(
            &home,
            "a",
            &serde_json::json!({"action": "list", "limit": 2}),
        );
        let tasks = r["tasks"].as_array().expect("tasks");
        assert_eq!(
            tasks.len(),
            2,
            "limit=2 must cap response, got {}",
            tasks.len()
        );
        let returned: std::collections::HashSet<_> = tasks
            .iter()
            .filter_map(|t| t["id"].as_str().map(String::from))
            .collect();
        assert!(
            returned.contains(&ids[2]),
            "newest task (idx 2) must be in newest-first cap"
        );
        assert!(
            returned.contains(&ids[1]),
            "second-newest (idx 1) must be in newest-first cap"
        );
        assert!(
            !returned.contains(&ids[0]),
            "oldest task (idx 0) must NOT be in newest-first cap"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #806 sweeper (Part B) — dry-run + apply tests ──

    /// Stub PR lookup for sweep tests — bypasses `gh pr view`
    /// shellout. Maps a static table of (repo, num) → PrState.
    fn stub_pr_lookup(repo: &str, num: u32) -> Result<super::sweep_impl::PrState, String> {
        match (repo, num) {
            ("test/repo", 999) => Ok(super::sweep_impl::PrState::Merged {
                merged_at: "2026-04-01T00:00:00Z".to_string(),
            }),
            ("test/repo", 998) => Ok(super::sweep_impl::PrState::Closed),
            _ => Ok(super::sweep_impl::PrState::Unknown),
        }
    }

    #[test]
    fn test_sweep_scan_identifies_team_disbanded_category() {
        // GREEN 2: scan_categories puts tasks owned by instances NOT
        // in live_instances AND aged > 30d into the team_disbanded
        // bucket. Fast-forward `now` 60 days into the future so the
        // freshly-created task crosses the 30d threshold without
        // needing event-log timestamp forgery.
        let home = tmp_home("sweep_disband");
        write_fleet_yaml(&home, &["alive"]);
        let r = handle(
            &home,
            "alive",
            &serde_json::json!({"action": "create", "title": "old work", "assignee": "ghost"}),
        );
        let task_id = r["id"].as_str().expect("id").to_string();
        let live: std::collections::HashSet<String> = ["alive".to_string()].into_iter().collect();
        let now = chrono::Utc::now() + chrono::Duration::days(60);
        let cats = super::sweep_impl::scan_categories(&home, &live, &stub_pr_lookup, None, now);
        assert_eq!(
            cats.team_disbanded.len(),
            1,
            "exactly one team_disbanded candidate expected, got {cats:?}"
        );
        assert_eq!(cats.team_disbanded[0].id, task_id);
        assert_eq!(cats.team_disbanded[0].owner.as_deref(), Some("ghost"));
        // No PR ref → other categories empty.
        assert!(cats.shipped.is_empty());
        assert!(cats.superseded.is_empty());
        assert!(cats.validation_leftovers.is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_sweep_scan_identifies_shipped_via_pr_lookup_stub() {
        // GREEN 2a: a task whose title carries `PR #999` and whose
        // stubbed PR state is Merged lands in the shipped bucket
        // when aged > 7d. The PrLookup function-pointer abstraction
        // lets the test bypass the production `gh pr view` shellout.
        let home = tmp_home("sweep_shipped");
        write_fleet_yaml(&home, &["alive"]);
        let r = handle(
            &home,
            "alive",
            &serde_json::json!({
                "action": "create",
                "title": "shipped via PR #999",
                "assignee": "alive",
            }),
        );
        let task_id = r["id"].as_str().expect("id").to_string();
        let live: std::collections::HashSet<String> = ["alive".to_string()].into_iter().collect();
        // 14 days forward — past the 7d shipped threshold but under
        // the 30d team_disbanded threshold (which wouldn't fire
        // anyway because owner is alive).
        let now = chrono::Utc::now() + chrono::Duration::days(14);
        let cats = super::sweep_impl::scan_categories(
            &home,
            &live,
            &stub_pr_lookup,
            Some("test/repo"),
            now,
        );
        assert_eq!(
            cats.shipped.len(),
            1,
            "exactly one shipped candidate expected, got {cats:?}"
        );
        assert_eq!(cats.shipped[0].id, task_id);
        assert_eq!(cats.shipped[0].pr, Some(999));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_sweep_handler_dry_run_returns_categorized_plan() {
        // GREEN 2b: full handler path returns dry_run=true with
        // categories shaped + candidate_ids list. Uses
        // team_disbanded category which doesn't require gh shellout.
        let home = tmp_home("sweep_handler_dryrun");
        write_fleet_yaml(&home, &["alive"]);
        handle(
            &home,
            "alive",
            &serde_json::json!({"action": "create", "title": "stuck", "assignee": "ghost"}),
        );
        // Forge updated_at via direct event-log append of an
        // OwnerAssigned older than the 30d threshold — bypass would
        // be cleaner, but for handler test the live `now` of the
        // task is recent. We invoke sweep but expect zero candidates
        // (handler uses real now, task fresh). Still validates the
        // dry-run shape contract.
        let r = handle(&home, "alive", &serde_json::json!({"action": "sweep"}));
        assert_eq!(r["dry_run"], true, "dry-run response shape: {r}");
        assert!(r["categories"].is_object(), "categories must be object");
        assert!(
            r["categories"]["team_disbanded"].is_array(),
            "team_disbanded slot present"
        );
        assert!(
            r["categories"]["shipped"].is_array(),
            "shipped slot present"
        );
        assert!(r["candidate_ids"].is_array(), "candidate_ids array present");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_sweep_apply_without_confirm_ids_rejected() {
        // GREEN 3a: apply=true without confirm_ids must surface
        // a validator error (NOT a silent no-op cancel of every
        // candidate — explicit subset is the double-opt-in guard).
        let home = tmp_home("sweep_no_confirm");
        write_fleet_yaml(&home, &["a"]);
        let r = handle(
            &home,
            "a",
            &serde_json::json!({"action": "sweep", "apply": true}),
        );
        assert!(
            r["error"]
                .as_str()
                .map(|e| e.contains("confirm_ids"))
                .unwrap_or(false),
            "apply=true without confirm_ids must reject: {r}"
        );
        // Also: apply=true with confirm_ids but no audit_reason.
        let r = handle(
            &home,
            "a",
            &serde_json::json!({
                "action": "sweep",
                "apply": true,
                "confirm_ids": ["t-nonexistent"],
            }),
        );
        assert!(
            r["error"]
                .as_str()
                .map(|e| e.contains("audit_reason"))
                .unwrap_or(false),
            "apply=true without audit_reason must reject: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_sweep_apply_emits_cancelled_and_logs_audit() {
        // GREEN 3: apply path with explicit (categories, confirm_ids)
        // emits Cancelled events under system:task_sweep + writes
        // task_sweep_apply lines to event-log.jsonl. The sweep_impl
        // helper is exercised directly because the handler's
        // `categories` rebuild uses real-time `now` and we want a
        // deterministic candidate set.
        let home = tmp_home("sweep_apply");
        write_fleet_yaml(&home, &["alive"]);
        let r = handle(
            &home,
            "alive",
            &serde_json::json!({"action": "create", "title": "ghost task", "assignee": "ghost"}),
        );
        let task_id = r["id"].as_str().expect("id").to_string();
        let live: std::collections::HashSet<String> = ["alive".to_string()].into_iter().collect();
        let now = chrono::Utc::now() + chrono::Duration::days(60);
        let cats = super::sweep_impl::scan_categories(&home, &live, &stub_pr_lookup, None, now);
        assert_eq!(cats.team_disbanded.len(), 1);
        let confirm: std::collections::HashSet<String> = [task_id.clone()].into_iter().collect();
        let count = super::sweep_impl::emit_cancelled_batch(
            &home,
            &cats,
            &confirm,
            "post-#806 sweep test fixture",
        )
        .expect("emit_cancelled_batch");
        assert_eq!(count, 1, "exactly one Cancelled must be emitted");
        let listed = list_all(&home);
        let after = listed.iter().find(|t| t.id == task_id).expect("task");
        assert_eq!(
            after.status, "cancelled",
            "swept task must transition to cancelled"
        );
        let log = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
        assert!(
            log.contains("task_sweep_apply"),
            "event-log.jsonl must record task_sweep_apply entry, got: {log}"
        );
        assert!(
            log.contains("post-#806 sweep test fixture"),
            "event-log.jsonl must carry the audit_reason"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #807 response shape consistency bundle — RED tests ──

    #[test]
    fn test_action_responses_carry_event_and_task_fields_with_lifecycle_status() {
        // #807 Item 1 RED: create/claim/update/done responses today
        // overload `status` with action-event names ("created" /
        // "updated") that don't match the task's lifecycle status
        // (which is "open" / unchanged). Post-fix: every action
        // response gains an `event` field (the action verb) AND a
        // `task` field (full Task object with the correct lifecycle
        // `status`). The legacy `status` field stays as a back-compat
        // alias per the dispatch spec (NOT removed).
        let home = tmp_home("action_response_shape");
        write_fleet_yaml(&home, &["agent-a"]);

        // create
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "create", "title": "t1", "assignee": "agent-a"}),
        );
        let id = r["id"].as_str().expect("id").to_string();
        assert_eq!(
            r["event"], "created",
            "create must carry event=created: {r}"
        );
        assert_eq!(
            r["task"]["status"], "open",
            "create task object must carry lifecycle status=open: {r}"
        );
        assert_eq!(
            r["status"], "created",
            "back-compat status alias preserved (status=event for create): {r}"
        );

        // claim
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "claim", "id": id}),
        );
        assert_eq!(r["event"], "claimed", "claim must carry event=claimed: {r}");
        assert_eq!(
            r["task"]["status"], "claimed",
            "claim task object must carry lifecycle status=claimed: {r}"
        );

        // update → in_progress
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
        );
        assert_eq!(
            r["event"], "updated",
            "update must carry event=updated: {r}"
        );
        assert_eq!(
            r["task"]["status"], "in_progress",
            "update task object must carry lifecycle status=in_progress (NOT 'updated'): {r}"
        );

        // done
        let r = handle(
            &home,
            "agent-a",
            &serde_json::json!({"action": "done", "id": id, "result": "shipped"}),
        );
        assert_eq!(r["event"], "done", "done must carry event=done: {r}");
        assert_eq!(
            r["task"]["status"], "done",
            "done task object must carry lifecycle status=done: {r}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_task_started_at_replaces_dispatched_at_in_serialized_output() {
        // #807 Item 3 RED: today the Task struct field is named
        // `dispatched_at` but the value is stamped on first transition
        // to InProgress — so the operator's "when was this dispatched
        // from send()?" reading is misleading. Rename to `started_at`
        // (matches `claimed_at` naming, mental model honest). serde
        // alias `dispatched_at` preserves replay of old persisted logs.
        let home = tmp_home("started_at_rename");
        write_fleet_yaml(&home, &["a"]);
        let r = handle(
            &home,
            "a",
            &serde_json::json!({"action": "create", "title": "t", "assignee": "a"}),
        );
        let id = r["id"].as_str().expect("id").to_string();
        handle(
            &home,
            "a",
            &serde_json::json!({"action": "update", "id": id, "status": "in_progress"}),
        );
        let listed = handle(&home, "a", &serde_json::json!({"action": "list"}));
        let task = listed["tasks"][0].as_object().expect("task object in list");
        assert!(
            task.contains_key("started_at"),
            "task must serialize `started_at` (not `dispatched_at`), got keys: {:?}",
            task.keys().collect::<Vec<_>>()
        );
        assert!(
            task["started_at"].is_string(),
            "started_at must be an RFC3339 string, got: {:?}",
            task["started_at"]
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_task_deserialize_dispatched_at_alias_preserves_back_compat() {
        // #807 Item 3: persisted JSON carrying the old `dispatched_at`
        // field name MUST still deserialize into the renamed
        // `started_at` field via `#[serde(alias = "dispatched_at")]`.
        // Locks the contract that pre-rename task_events.jsonl files
        // and external test fixtures keep working post-migration.
        let raw_old = serde_json::json!({
            "id": "t-test-back-compat",
            "title": "old",
            "description": "",
            "status": "open",
            "priority": "normal",
            "assignee": null,
            "created_by": "test",
            "depends_on": [],
            "result": null,
            "created_at": "2026-04-01T00:00:00Z",
            "updated_at": "2026-04-01T00:00:00Z",
            "dispatched_at": "2026-04-02T00:00:00Z",
        });
        let t: Task = serde_json::from_value(raw_old).expect("deserialize old shape");
        assert_eq!(
            t.started_at.as_deref(),
            Some("2026-04-02T00:00:00Z"),
            "serde alias must map legacy `dispatched_at` → new `started_at`"
        );
    }
}
