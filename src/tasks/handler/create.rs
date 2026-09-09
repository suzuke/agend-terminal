use serde_json::Value;
use std::path::Path;

use super::{parse_assignee_for_create, parse_due_at, read_task_record_at, record_to_task};

pub(super) fn handle_create(
    home: &Path,
    emitter: crate::task_events::InstanceName,
    args: &Value,
) -> Value {
    handle_create_with_id(home, emitter, args, None)
}

/// The durable Job run reserves its task id before the first side effect.
/// Private daemon entry: public task args cannot choose a task identity.
pub(crate) fn create_schedule_task(home: &Path, id: &str, args: &Value) -> Value {
    handle_create_with_id(
        home,
        crate::task_events::InstanceName::from("system:schedule_job"),
        args,
        Some(id),
    )
}

fn handle_create_with_id(
    home: &Path,
    emitter: crate::task_events::InstanceName,
    args: &Value,
    reserved_id: Option<&str>,
) -> Value {
    let title = match args["title"].as_str() {
        Some(t) => t,
        None => return serde_json::json!({"error": "missing 'title'"}),
    };
    // #2249 pre-work alignment gate: validation mirrors second_reviewer_reason
    // (comms_gates/dispatch.rs:104-109) — N>0 requires a non-empty reason.
    // 0 (default/absent) leaves plan_ack_required unset, so the in_progress
    // gate never fires — byte-identical to pre-#2249 behavior.
    let plan_ack_required = args["plan_ack_required"].as_u64().unwrap_or(0);
    if plan_ack_required > 0 {
        let reason = args["plan_ack_reason"].as_str().unwrap_or("");
        if reason.is_empty() {
            return serde_json::json!({
                "error": "plan_ack_required > 0 requires non-empty 'plan_ack_reason'"
            });
        }
    }
    // #3419: hold the governing decision's existing flock across fresh
    // resolution and the Created append. This closes the resolve→Created
    // supersession race; the guard is intentionally kept alive to function end.
    let _governing_decision_lock =
        match super::super::governance::acquire_creation_decision_lock(home, args) {
            Ok(lock) => lock,
            Err(error) => return error,
        };
    let authority = match super::super::governance::resolve_creation_authority(home, args) {
        Ok(authority) => authority,
        Err(error) => return error,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    static ID_SEQ: AtomicU64 = AtomicU64::new(0);
    let ts = chrono::Utc::now().format("%Y%m%d%H%M%S%6f");
    let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
    // CR-2026-06-14 (correctness): `tasks::handle` runs in every MCP server
    // process AND the daemon. ts (microseconds) + ID_SEQ is only PROCESS-unique,
    // so two processes minting in the same microsecond both produce `t-<ts>-0`
    // and `apply_created`'s `or_insert_with` silently drops the second Created at
    // replay. The pid disambiguates across processes. Format is now a THREE-
    // numeric-segment id `t-<ts>-<pid>-<seq>`; the sweep `Closes`-marker regex,
    // its strict validator, and the `has_task_id` probe accept both this and the
    // legacy two-segment form (see src/daemon/task_sweep.rs).
    let pid = std::process::id();
    let id = reserved_id
        .map(String::from)
        .unwrap_or_else(|| format!("t-{ts}-{pid}-{seq}"));
    let assignee = match parse_assignee_for_create(args) {
        Ok(a) => a,
        Err(e) => return e,
    };
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
        tags: args["tags"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        parent_id: args["parent_id"]
            .as_str()
            .map(|s| crate::task_events::TaskId(s.to_string())),
        governing_decision_id: authority.governing_decision_id.clone(),
        review_class: authority.review_class,
    };
    // #2117 P1: route create to the caller's current project board (or an
    // explicit `project` arg override). Single-project → DEFAULT → board == home
    // (byte-identical). Record the task→project mapping in the append-only index
    // so later done/update/claim/activity resolve the board in O(1).
    //
    // #2760: canonicalise the project id to its filesystem-safe SLUG — the project
    // id IS the slug (board_router doc: the board dir name equals the project id).
    // `resolve_current_project` already returns a slug, but a raw explicit `project`
    // arg (e.g. `orgA/projA`) is stored raw in the index while `board_root` slugs
    // the on-disk dir (`orgA_projA`) — so the STRICT router's index-vs-physical
    // consistency check reads them as a mismatch (Unreadable) and the parent-project
    // comparison compares slug-vs-raw. Slugging here makes the index entry, the
    // board dir, and every later strict route agree. Idempotent on an already-safe id.
    let project = crate::task_events::project_slug(
        &args["project"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| {
                super::super::board_router::resolve_current_project(home, emitter.as_str())
            }),
    );
    // #2117 P3a: `parent_id` is subtask COMPOSITION ("A is composed of B/C/D").
    // DP4 invariant — a subtask MUST live in its parent's project. Cross-project
    // composition breaks board isolation: `cascade_cancel_children` only replays
    // the PARENT's board, so a cross-project child would be silently orphaned when
    // the parent is cancelled. Enforce the invariant at the only write point that
    // can violate it — reject here, fail-closed, rather than detect it later.
    // Single-project → `resolve_task_project(parent) == project` always → never
    // fires → byte-identical. (`depends_on` is execution-order dependency, NOT
    // composition — cross-board references are allowed there per the epic and are
    // NOT guarded here.)
    if let Some(parent_id) = args["parent_id"].as_str() {
        // #2760: resolve the parent's board via the strict route. A route error
        // (NotFound / Unreadable / Ambiguous) fails closed — a subtask is never
        // created against a parent whose board cannot be uniquely proven.
        let parent_project = match super::super::load_routed(home, parent_id) {
            Ok(rt) => rt.board().project().to_string(),
            Err(e) => {
                return serde_json::json!({
                    "error": format!(
                        "cross-project parent_id rejected: parent {parent_id} could not be \
                         routed ({e}) — a subtask must live in its parent's uniquely-resolved \
                         project (board isolation, #2117 P3a / #2760)"
                    )
                });
            }
        };
        if parent_project != project {
            return serde_json::json!({
                "error": format!(
                    "cross-project parent_id rejected: parent {parent_id} resolves to project \
                     '{parent_project}' but this subtask targets project '{project}' — a subtask \
                     must live in its parent's project (board isolation, #2117 P3a)"
                )
            });
        }
    }
    let board = crate::task_events::board_root(home, &project);
    if let Some(predecessor_id) = args["supersedes"]
        .as_str()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        return super::super::supersession::create_with_supersession(
            home,
            &project,
            &emitter,
            &id,
            predecessor_id,
            event,
            args,
        );
    }
    // #2760 item 2: create under the per-id router lock. The id routing NOWHERE
    // (NotFound) is re-proved under the lock BEFORE the Created append + index
    // record, so two racing creates of the same id (or a create racing a mutation)
    // can never land a cross-board duplicate — which would make every later route
    // Ambiguous and un-mutatable. A fresh id is timestamp+pid+seq so this virtually
    // always holds; it fails closed if the id somehow already exists on a board or
    // its absence cannot be proven. The lock is held across the pure board-record
    // writes below (no dispatch side-effects, #1496 → no self-IPC under the flock).
    let _create_lock = match super::super::board_router::acquire_task_id_lock(home, &id) {
        Ok(g) => g,
        Err(e) => {
            return serde_json::json!({
                "error": format!("failed to acquire per-id lock for create of '{id}': {e}")
            })
        }
    };
    match super::super::load_routed(home, &id) {
        // The id is free on every board → safe to create.
        Err(super::super::TaskRouteError::NotFound) => {}
        Ok(_) => {
            return serde_json::json!({
                "error": format!("task id '{id}' already exists on a board"),
                "code": "duplicate_task_id",
            })
        }
        Err(e) => {
            return serde_json::json!({
                "error": format!("cannot prove task id '{id}' is unused: {e}"),
                "code": "task_route_unresolved",
            })
        }
    }
    match crate::task_events::append_at(&board, &emitter, event) {
        Ok(_) => {
            if args["project"].as_str().is_some() {
                crate::daemon::task_sweep::note_explicit_project(home, &project);
            }
            // #2249: seed the plan-ack gate's metadata right after Created.
            // No new event variant — composes onto the existing MetadataSet
            // event/action, matching #2249's "reuse existing mechanisms"
            // design. Absent when plan_ack_required == 0 (the common case),
            // so `handle_update`'s gate check (which reads this same key)
            // never fires — byte-identical to pre-#2249 for every task that
            // doesn't opt in.
            if plan_ack_required > 0 {
                let reason = args["plan_ack_reason"].as_str().unwrap_or("");
                let task_id_typed = crate::task_events::TaskId(id.clone());
                let _ = crate::task_events::append_at(
                    &board,
                    &emitter,
                    crate::task_events::TaskEvent::MetadataSet {
                        task_id: task_id_typed.clone(),
                        by: emitter.clone(),
                        key: "plan_ack_required".to_string(),
                        value: serde_json::json!(plan_ack_required),
                    },
                );
                let _ = crate::task_events::append_at(
                    &board,
                    &emitter,
                    crate::task_events::TaskEvent::MetadataSet {
                        task_id: task_id_typed,
                        by: emitter.clone(),
                        key: "plan_ack_reason".to_string(),
                        value: serde_json::json!(reason),
                    },
                );
            }
            // Preserve the legacy explicit invalid value for ungoverned tasks;
            // governed creation rejects it before the Created append. Valid
            // classes are already projected atomically from Created above.
            if let Some(rc) = authority.legacy_review_class_raw.as_deref() {
                let _ = crate::task_events::append_at(
                    &board,
                    &emitter,
                    crate::task_events::TaskEvent::MetadataSet {
                        task_id: crate::task_events::TaskId(id.clone()),
                        by: emitter.clone(),
                        key: "review_class".to_string(),
                        value: serde_json::json!(rc),
                    },
                );
            }
            let task = read_task_record_at(&board, &id).map(|r| record_to_task(&r));
            // #1496 Option 1: `task(action:create)` is a PURE board record
            // with ZERO dispatch side-effects — no inbox enqueue, no
            // dispatch_tracking, no PTY notify. Dispatch (notify + worktree
            // auto-bind) is solely `send(kind=task)`'s job; it auto-creates
            // the board row when `task_id` is empty (comms.rs), so the
            // single-step "create + dispatch" use case is fully preserved
            // via one `send(kind=task)` call.
            //
            // The prior auto-notify (#1238) was a second, inferior dispatch
            // path: a title-only, non-actionable wake carrying no task
            // description. It fired prematurely — pushing the assignee into
            // the busy state before the real, context-rich `send(kind=task)`
            // arrived — so that send hit the busy-gate and forced operators
            // to re-send with `force=true`. Removing it unifies dispatch on
            // one path and kills the race (see #1496 spike).
            serde_json::json!({
                "id": id,
                "event": "created",
                "task": task,
                "status": "created",
            })
        }
        Err(e) => serde_json::json!({"error": format!("event log append failed: {e}")}),
    }
}
