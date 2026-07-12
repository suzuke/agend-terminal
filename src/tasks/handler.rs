use serde_json::Value;
use std::path::Path;

use super::acl::{can_mutate_record, instance_exists};
use super::orphan::build_health_response;
use super::{record_to_task, status_to_legacy_str, Task};

fn parse_due_at(args: &Value) -> Option<String> {
    let due = args["due_at"].as_str()?;
    let dt = chrono::DateTime::parse_from_rfc3339(due).ok()?;
    Some(dt.with_timezone(&chrono::Utc).to_rfc3339())
}

/// Read a single task's current replay-derived record. Used by
/// `handle`'s mutation arms to validate `(prev_status, transition)`
/// before emitting an event.
pub(super) fn read_task_record(home: &Path, id: &str) -> Option<crate::task_events::TaskRecord> {
    // #2117 P1: home IS the default board root (`board_root(home, DEFAULT)`), so
    // this is byte-identical; routed callers use `read_task_record_at` with the
    // task's resolved board.
    read_task_record_at(home, id)
}

/// #2117 P1: board-root variant of [`read_task_record`].
pub(super) fn read_task_record_at(
    board: &Path,
    id: &str,
) -> Option<crate::task_events::TaskRecord> {
    let state = crate::task_events::replay_at(board).ok()?;
    state
        .tasks
        .get(&crate::task_events::TaskId(id.to_string()))
        .cloned()
}

pub fn handle(home: &Path, instance_name: &str, args: &Value) -> Value {
    let action = match args["action"].as_str() {
        Some(a) => a,
        None => return serde_json::json!({"error": "missing 'action'"}),
    };
    let emitter = crate::task_events::InstanceName::from(instance_name);

    match action {
        "create" => handle_create(home, emitter, args),
        "list" => handle_list(home, instance_name, args),
        "get" => handle_get(home, instance_name, args),
        "claim" => handle_claim(home, instance_name, emitter, args),
        "done" => handle_done(home, instance_name, emitter, args),
        "update" => handle_update(home, instance_name, emitter, args),
        "sweep" => handle_sweep(home, args),
        "health" => handle_health(home),
        "activity" => handle_activity(home, args),
        "metadata_set" => handle_metadata_set(home, instance_name, emitter, args),
        "metadata_get" => handle_metadata_get(home, args),
        "ack_plan" => handle_ack_plan(home, instance_name, emitter, args),
        _ => serde_json::json!({"error": format!("unknown action: {action}")}),
    }
}

/// #2037 (3): `id` is canonical, `task_id` accepted as alias — `send` calls it
/// `task_id`, so the most common cross-tool slip is forgiving. Error messages
/// name both.
fn id_arg(args: &Value) -> Option<&str> {
    args["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| args["task_id"].as_str().filter(|s| !s.is_empty()))
}

fn handle_create(home: &Path, emitter: crate::task_events::InstanceName, args: &Value) -> Value {
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
    let id = format!("t-{ts}-{pid}-{seq}");
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
    };
    // #2117 P1: route create to the caller's current project board (or an
    // explicit `project` arg override). Single-project → DEFAULT → board == home
    // (byte-identical). Record the task→project mapping in the append-only index
    // so later done/update/claim/activity resolve the board in O(1).
    let project = args["project"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| super::board_router::resolve_current_project(home, emitter.as_str()));
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
        let parent_project = super::board_router::resolve_task_project(home, parent_id);
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
    match crate::task_events::append_at(&board, &emitter, event) {
        Ok(_) => {
            let _ = super::board_router::record_task_project(home, &id, &project);
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

fn handle_list(home: &Path, caller: &str, args: &Value) -> Value {
    // #2037 ①: accept the natural names as aliases of the filter_* params —
    // `status`/`assignee` are create/update params elsewhere, but on the
    // `list` action they can only mean filtering (three real mis-calls in
    // one day by the heaviest caller). filter_* stays canonical.
    let filter_assignee = args["filter_assignee"]
        .as_str()
        .or_else(|| args["assignee"].as_str());
    let filter_status = args["filter_status"]
        .as_str()
        .or_else(|| args["status"].as_str());
    let filter_tag = args["filter_tag"].as_str().or_else(|| args["tag"].as_str());
    // #806: default trim to actionable statuses unless caller
    // opts in to history. `filtered_default=true` on the
    // response signals callers (audit / forensics) that the
    // trim fired so they can re-call with include_history=true.
    let include_history = args["include_history"].as_bool().unwrap_or(false);
    let limit = args["limit"].as_u64();
    let filtered_default = !include_history && filter_status.is_none();
    const ACTIONABLE: &[&str] = &[
        "backlog",
        "open",
        "claimed",
        "in_progress",
        "in_review",
        "blocked",
    ];
    let now = chrono::Utc::now();
    let done_ttl = chrono::Duration::days(14);
    // #2117 P1: choose the source board(s).
    //   - `project=all` / `scope=fleet` → aggregate EVERY board (cross-board
    //     view; each task tagged with its project id in the response).
    //   - explicit `project=<id>` → that one board.
    //   - default → the caller's current project board.
    // Single-project resolves to DEFAULT → `board_root == home` → the source is
    // exactly `list_all(home)` and the response is byte-identical.
    let fleet_scope =
        args["project"].as_str() == Some("all") || args["scope"].as_str() == Some("fleet");
    let mut project_of: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let tasks: Vec<Task> = if fleet_scope {
        let mut all = Vec::new();
        for (project, ts) in super::board_router::list_all_boards(home) {
            for t in &ts {
                project_of.insert(t.id.clone(), project.clone());
            }
            all.extend(ts);
        }
        all
    } else {
        let project = args["project"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| super::board_router::resolve_current_project(home, caller));
        super::list_all_at(home, &crate::task_events::board_root(home, &project))
    };
    let mut filtered: Vec<Task> = tasks
        .iter()
        .filter(|t| filter_assignee.is_none_or(|a| t.assignee.as_deref() == Some(a)))
        .filter(|t| filter_status.is_none_or(|s| t.status.to_string() == s))
        .filter(|t| filter_tag.is_none_or(|tag| t.tags.iter().any(|tt| tt == tag)))
        // #806 default-actionable-only filter — only fires
        // when neither include_history nor filter_status is
        // set. Preserves zero impact on filter_status callers.
        .filter(|t| {
            include_history
                || filter_status.is_some()
                || ACTIONABLE.contains(&t.status.to_string().as_str())
        })
        .filter(|t| {
            // 14d done-ttl preserved for include_history=true
            // path (default trim already drops done entries).
            if filter_status.is_some() || t.status != crate::task_events::TaskStatus::Done {
                return true;
            }
            chrono::DateTime::parse_from_rfc3339(&t.updated_at)
                .map(|dt| now.signed_duration_since(dt.with_timezone(&chrono::Utc)) < done_ttl)
                .unwrap_or(true)
        })
        .cloned()
        .collect();
    // #806 `limit`: newest-first cap by `updated_at` desc.
    if let Some(n) = limit {
        filtered.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        filtered.truncate(n as usize);
    }
    // #2117 P1: the default/single-project response is byte-identical; the
    // fleet aggregate additionally tags each task with its project id.
    // #2475: terse-by-default. Routine `task list` is the heaviest MCP read
    // (one session hit the 60KB result cap). The bloat is the free-text
    // `description`/`result` fields; truncate them by default and let callers
    // opt into the full text with `verbose: true` (or fetch one task's detail).
    let verbose = args["verbose"].as_bool().unwrap_or(false);
    // #2475 Phase 2: opt-in column projection. `fields:"minimal"` keeps only the
    // routing-relevant columns (id/title/status/assignee/priority) so a lead can
    // poll the board for state without pulling every structured field into
    // context. Default ("full") is unchanged — additive, non-breaking shape.
    let minimal = args["fields"].as_str() == Some("minimal");
    if fleet_scope {
        let mut tagged: Vec<Value> = filtered
            .iter()
            .map(|t| {
                let mut v = serde_json::to_value(t).unwrap_or(Value::Null);
                if let (Some(obj), Some(p)) = (v.as_object_mut(), project_of.get(&t.id)) {
                    obj.insert("project".to_string(), Value::String(p.clone()));
                }
                v
            })
            .collect();
        if !verbose {
            for v in &mut tagged {
                terse_task_value(v);
            }
        }
        if minimal {
            for v in &mut tagged {
                minimal_task_value(v);
            }
        }
        return serde_json::json!({
            "tasks": tagged,
            "filtered_default": filtered_default,
            "scope": "fleet",
            "terse": !verbose,
            "fields": if minimal { "minimal" } else { "full" },
        });
    }
    let mut tasks: Vec<Value> = filtered
        .iter()
        .map(|t| serde_json::to_value(t).unwrap_or(Value::Null))
        .collect();
    if !verbose {
        for v in &mut tasks {
            terse_task_value(v);
        }
    }
    if minimal {
        for v in &mut tasks {
            minimal_task_value(v);
        }
    }
    serde_json::json!({
        "tasks": tasks,
        "filtered_default": filtered_default,
        "terse": !verbose,
        "fields": if minimal { "minimal" } else { "full" },
    })
}

/// #2475: trim the heavy free-text fields of a serialized task for the
/// terse-by-default `list` response. Structured fields (id/title/status/
/// assignee/priority/timestamps/…) are kept intact, so the shape still
/// deserializes; only `description` and `result` are length-capped with a
/// marker that points at `verbose: true` for the full text.
fn terse_task_value(v: &mut Value) {
    const CAP: usize = 200;
    let Some(obj) = v.as_object_mut() else { return };
    for key in ["description", "result"] {
        if let Some(Value::String(s)) = obj.get_mut(key) {
            if s.chars().count() > CAP {
                let kept: String = s.chars().take(CAP).collect();
                let dropped = s.chars().count() - CAP;
                *s = format!("{kept}… (+{dropped} chars; verbose=true for full)");
            }
        }
    }
}

/// #2475 Phase 2: project a serialized task down to the minimal routing columns
/// for `fields:"minimal"` list calls. Whitelists id/title/status/assignee/
/// priority (plus the fleet-scope `project` tag when present) and drops every
/// other field. Opt-in only — the default list shape is unchanged, so existing
/// callers that deserialize full `Task` rows are unaffected.
fn minimal_task_value(v: &mut Value) {
    const KEEP: &[&str] = &["id", "title", "status", "assignee", "priority", "project"];
    let Some(obj) = v.as_object_mut() else { return };
    obj.retain(|k, _| KEEP.contains(&k.as_str()));
}

/// #2475 Phase 2: single-task detail fetch. The terse-by-default `list` caps
/// `description`/`result`; `get` is the companion that returns ONE task's FULL
/// record by id, so a caller can pull the full text of just the task it cares
/// about instead of re-listing the whole board with `verbose:true`. Resolves the
/// task on the caller's current project board first, then falls back to scanning
/// every board (the id may live elsewhere). Returns `{ "task": {...} }` or an
/// `{ "error": ... }` when the id is absent.
fn handle_get(home: &Path, caller: &str, args: &Value) -> Value {
    let Some(id) = id_arg(args).map(str::to_string) else {
        return serde_json::json!({"error": "missing 'id' (or 'task_id')"});
    };
    // Primary board: explicit `project`, else the caller's current project.
    let project = args["project"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| super::board_router::resolve_current_project(home, caller));
    let board = crate::task_events::board_root(home, &project);
    let mut found = super::list_all_at(home, &board)
        .into_iter()
        .find(|t| t.id == id);
    let mut found_project = found.as_ref().map(|_| project);
    // Cross-board fallback: the id may live on another project's board.
    if found.is_none() {
        for (p, ts) in super::board_router::list_all_boards(home) {
            if let Some(t) = ts.into_iter().find(|t| t.id == id) {
                found_project = Some(p);
                found = Some(t);
                break;
            }
        }
    }
    match found {
        Some(t) => {
            let mut v = serde_json::to_value(&t).unwrap_or(Value::Null);
            if let (Some(obj), Some(p)) = (v.as_object_mut(), found_project) {
                obj.insert("project".to_string(), Value::String(p));
            }
            serde_json::json!({ "task": v })
        }
        None => serde_json::json!({"error": format!("task not found: {id}")}),
    }
}

/// #2117 P3a (FM5 / board isolation): per-board mutation authority. A task
/// mutation resolves its board from the task_id, so a caller can name a task that
/// lives on ANOTHER project's board. Deny unless the caller acts in that board's
/// project — `super::acl::can_mutate_on_board` (system identities bypass; a hard
/// fleet read failure fail-closes). `force` — the audited operator override, the
/// SAME axis as the owner-ACL `can_mutate_record` — bypasses it on the paths that
/// carry it. Single-project → caller project == task board project (both DEFAULT)
/// → allow → byte-identical (no new denial). Returns `Some(error)` when denied.
fn cross_board_denied(home: &Path, caller: &str, id: &str, force: bool) -> Option<Value> {
    if force {
        return None;
    }
    let board_project = super::board_router::resolve_task_project(home, id);
    if super::acl::can_mutate_on_board(home, caller, &board_project) {
        return None;
    }
    Some(serde_json::json!({
        "error": format!(
            "cross-board mutation denied: task '{id}' lives on the '{board_project}' project \
             board but caller '{caller}' acts in a different project (board isolation, #2117 P3a)"
        )
    }))
}

fn handle_claim(
    home: &Path,
    instance_name: &str,
    emitter: crate::task_events::InstanceName,
    args: &Value,
) -> Value {
    let id = match id_arg(args) {
        Some(i) => i.to_string(),
        None => return serde_json::json!({"error": "missing 'id' (alias: task_id)"}),
    };
    let iname = instance_name.to_string();
    if !instance_exists(home, &iname) {
        return serde_json::json!({"error": format!("instance '{iname}' not found in fleet.yaml")});
    }
    // #2117 P3a: board-isolation gate — a caller may only claim tasks on its own
    // project's board (claim has no `force`/owner-ACL — an open task is claimable
    // by anyone, but only within the board). Single-project → allow.
    if let Some(deny) = cross_board_denied(home, &iname, &id, false) {
        return deny;
    }
    // #t-21: validate + append in ONE critical section to close the
    // claim race. Pre-fix, the claimable check ran here (against a
    // replay snapshot) and the append happened afterwards under a
    // separate lock that did NOT re-validate — so two agents racing
    // the same Open task both passed the check and both appended a
    // Claimed event. `append_checked` re-runs the precondition under
    // the append lock against a FRESH replay, so exactly one wins.
    //
    // PR3: dep-derived blocking is computed in-memory at list time
    // (not persisted), so the precondition rebuilds the SAME dep-aware
    // view `list_all` produces (replay → record_to_task → dep eval)
    // rather than reading raw status — an operator must not claim a
    // task whose deps are unsatisfied.
    let claim_id = id.clone();
    let by = iname.clone();
    let event = crate::task_events::TaskEvent::Claimed {
        task_id: crate::task_events::TaskId(id.clone()),
        by: crate::task_events::InstanceName(iname.clone()),
    };
    // #2117 P1: operate on the task's resolved board (default → home).
    let board = super::board_router::board_for_task(home, &id);
    let result = crate::task_events::append_checked_at(&board, &emitter, event, |state| {
        let mut tasks: Vec<Task> = state.tasks.values().map(record_to_task).collect();
        // #2117 Q2: cross-board dep eval — a claim gate on a task whose
        // `depends_on` points at another project's board must read that board's
        // status, not just this one's. Pass home + the task's board.
        super::apply_dependency_eval_in_memory(&mut tasks, home, &board);
        let tv = tasks
            .iter()
            .find(|t| t.id == claim_id)
            .ok_or_else(|| format!("task '{claim_id}' not found"))?;
        let is_self_reclaim = tv.status == crate::task_events::TaskStatus::Claimed
            && tv.assignee.as_deref() == Some(by.as_str());
        if !is_self_reclaim && tv.status != crate::task_events::TaskStatus::Open {
            return Err(format!(
                "task '{claim_id}' status is '{}', only 'open' tasks can be claimed",
                tv.status
            ));
        }
        Ok(())
    });
    match result {
        Ok(Ok(_)) => {
            // #807 Item 1: see create arm note. claim's
            // legacy `status` happens to match lifecycle
            // ("claimed"), but the field is still the action
            // event name semantically — kept as alias for
            // shape consistency.
            let task = read_task_record_at(&board, &id).map(|r| record_to_task(&r));
            serde_json::json!({
                "id": id,
                "event": "claimed",
                "task": task,
                "assignee": instance_name,
                // #807 deprecated alias kept for back-compat — see task.status for lifecycle.
                "status": "claimed",
            })
        }
        // Lost the race / precondition no longer holds — no event written.
        Ok(Err(reason)) => serde_json::json!({"error": reason}),
        Err(e) => serde_json::json!({"error": format!("event log append failed: {e}")}),
    }
}

/// CR-2026-06-14 (security): clamp a caller-supplied `done_source` to the only
/// provenance an *untrusted* caller may attest — `OperatorManual`. The forensic
/// variants (`PrMerged` / `LegacyBackfill` / `AutoCloseOnPrMerge` /
/// `ReportAutoClose`) record what the daemon OBSERVED of GitHub state; an agent
/// forging one through the MCP `done`/`update` surface would poison the audit
/// trail's "this is what the daemon actually saw" guarantee.
///
/// The trust boundary is the CALLER IDENTITY, not a blanket downgrade: the
/// recognized system identities (`system:auto_close` etc. — the daemon
/// branch-merge / sweep paths) legitimately set forensic provenance and route
/// through `handle()` (e.g. `status_summary::auto_close_task_on_branch_merge`
/// closes with `AutoCloseOnPrMerge` as `system:auto_close`). So forensic is
/// honored from a system identity and downgraded from everyone else (agents AND
/// the human `operator`, who closes with `OperatorManual`). A forensic value
/// from a non-system caller (or an unparseable value) falls back to a
/// freshly-stamped `OperatorManual` — the done still succeeds, the forged
/// provenance is silently downgraded rather than surfaced as an error.
fn caller_attestable_done_source(
    caller: &str,
    done_source_arg: Option<&Value>,
    fallback_result: Option<String>,
) -> crate::task_events::DoneSource {
    use crate::task_events::DoneSource;
    match done_source_arg.and_then(|v| serde_json::from_value::<DoneSource>(v.clone()).ok()) {
        // OperatorManual is attestable by any caller.
        Some(src @ DoneSource::OperatorManual { .. }) => src,
        // Forensic provenance is trusted only from a recognized system identity.
        Some(src) if super::acl::is_system_identity(caller) => src,
        _ => DoneSource::OperatorManual {
            authored_at: chrono::Utc::now().to_rfc3339(),
            result: fallback_result,
        },
    }
}

fn handle_done(
    home: &Path,
    instance_name: &str,
    emitter: crate::task_events::InstanceName,
    args: &Value,
) -> Value {
    let id = match id_arg(args) {
        Some(i) => i.to_string(),
        None => return serde_json::json!({"error": "missing 'id' (alias: task_id)"}),
    };
    let result_text = args["result"].as_str().map(String::from);
    let caller = instance_name.to_string();
    // #2117 P1: operate on the task's resolved board (default → home).
    let board = super::board_router::board_for_task(home, &id);
    let record = match read_task_record_at(&board, &id) {
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
    // #2117 P3a: board-isolation gate (outer boundary, before the owner-ACL).
    if let Some(deny) = cross_board_denied(home, &caller, &id, force) {
        return deny;
    }
    if !force && !can_mutate_record(home, &caller, &record) {
        return serde_json::json!({
            "error": format!(
                "task '{id}' owned by '{}', caller '{caller}' not authorized",
                record.owner.as_ref().map(|o| o.0.as_str()).unwrap_or("unassigned")
            )
        });
    }
    // #1265: transition enforcement for done action.
    if !record
        .status
        .can_transition_to(crate::task_events::TaskStatus::Done)
    {
        return serde_json::json!({
            "error": format!(
                "illegal transition: {} → done (task {})",
                status_to_legacy_str(record.status),
                id
            ),
            "code": "illegal_transition",
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
        // CR-2026-06-14 (security): forensic done_source is daemon-only; a caller
        // may only attest OperatorManual. See `caller_attestable_done_source`.
        source: caller_attestable_done_source(&caller, args.get("done_source"), result_text),
    };
    // #1868: re-validate the →Done transition UNDER the append lock against FRESH
    // committed state. The `can_transition_to` check above is a fast-reject; a
    // concurrent writer (daemon `sweep_overdue_claimed` / `auto_close`, or a peer
    // update) could have moved the task between that read and this append, and
    // replay's `apply_done` does NOT re-guard transitions — so this precondition
    // is the authoritative gate (mirrors `handle_claim`'s `append_checked`).
    let done_id = id.clone();
    match crate::task_events::append_checked_at(&board, &emitter, event, |state| {
        let tv = state
            .tasks
            .values()
            .map(record_to_task)
            .find(|t| t.id == done_id)
            .ok_or_else(|| format!("task '{done_id}' not found"))?;
        if !tv
            .status
            .can_transition_to(crate::task_events::TaskStatus::Done)
        {
            return Err(format!(
                "illegal transition: {} → done (task {done_id})",
                status_to_legacy_str(tv.status)
            ));
        }
        Ok(())
    }) {
        Ok(Ok(_)) => {
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
            if let Some(binding) = crate::binding::read(home, &owner) {
                if let Some(wt) = binding["worktree"].as_str().map(std::path::PathBuf::from) {
                    let _ = crate::mcp::handlers::dispatch_hook::clean_empty_init_commits(&wt).ok();
                }
                // t-worktree-leak (PR-1): task-done is one of the 3 release
                // events. Enqueue a release-invariant recompute — if the
                // branch has no open PR and all its tasks are done, the
                // sweeper releases the worktree (covers tasks that never
                // produce a PR: RCA / design / spike). An open PR holds the
                // release until it terminates. (repo="" → sweeper derives it.)
                if let Some(branch) = binding["branch"].as_str() {
                    crate::daemon::auto_release::enqueue_release_recompute(
                        home,
                        "",
                        branch,
                        "task_done",
                    );
                }
            }
            // #1018/#78445-2 (d): a task reaching Done is terminal — clear BOTH
            // obligation stores (dispatch_idle sidecar + dispatch_tracking rows) via
            // the shared seam so no watchdog nags about the board-confirmed-done work.
            super::task_terminal_cleanup(home, &id);
            // #807 Item 1: see create arm note.
            let task = read_task_record_at(&board, &id).map(|r| record_to_task(&r));
            serde_json::json!({
                "id": id,
                "event": "done",
                "task": task,
                // #807 deprecated alias kept for back-compat — see task.status for lifecycle.
                "status": "done",
            })
        }
        Ok(Err(reason)) => serde_json::json!({"error": reason, "code": "illegal_transition"}),
        Err(e) => serde_json::json!({"error": format!("event log append failed: {e}")}),
    }
}

/// CR-2026-06-14 (:231) — the in-lock precondition for `handle_update`'s atomic
/// event batch. Runs under the append lock against FRESH committed `state`, so
/// it is the AUTHORITATIVE gate the out-of-lock ACL/transition checks only
/// fast-reject. Fails closed when, since the out-of-lock read:
/// - the task vanished;
/// - ownership drifted so `caller` is no longer authorized (`force` bypasses,
///   mirroring the out-of-lock gate at :633) — this covers BOTH status and
///   non-status updates (the latter had no in-lock guard at all before);
/// - the status transition became illegal (the prior #1868 guard); or
/// - the owner moved out from under a Claimed/InProgress/Done event whose `by`
///   was baked from the now-stale owner (attribution would be wrong).
///
/// Pure decision logic over the supplied `state` (no `api::call` — #1629-safe to
/// run under the lock) and a directly-testable seam.
#[allow(clippy::too_many_arguments)]
fn update_batch_precondition(
    state: &crate::task_events::TaskBoardState,
    home: &Path,
    caller: &str,
    upd_id: &str,
    force: bool,
    target_status: Option<crate::task_events::TaskStatus>,
    stale_owner: &Option<crate::task_events::InstanceName>,
) -> Result<(), String> {
    let fresh = state
        .tasks
        .get(&crate::task_events::TaskId(upd_id.to_string()))
        .ok_or_else(|| format!("task '{upd_id}' not found"))?;
    // (1) Ownership ACL re-check against fresh state — same `can_mutate_record`
    //     + force semantics as the out-of-lock gate (:633).
    if !force && !can_mutate_record(home, caller, fresh) {
        return Err(format!(
            "task '{upd_id}' ownership changed since authorization; \
             caller '{caller}' no longer authorized (retry)"
        ));
    }
    // (2) Status-transition legality (#1868), keyed on the fresh record.
    if let Some(target) = target_status {
        if !fresh.status.can_transition_to(target) {
            return Err(format!(
                "illegal transition: {} → {} (task {upd_id})",
                status_to_legacy_str(fresh.status),
                status_to_legacy_str(target)
            ));
        }
    }
    // (3) `by`-identity drift: Claimed/InProgress/Done bake `by` from the
    //     out-of-lock owner. If the owner moved, that attribution is stale →
    //     reject (we don't rebuild events under the lock).
    if matches!(
        target_status,
        Some(
            crate::task_events::TaskStatus::Claimed
                | crate::task_events::TaskStatus::InProgress
                | crate::task_events::TaskStatus::Done
        )
    ) && fresh.owner != *stale_owner
    {
        return Err(format!(
            "task '{upd_id}' owner changed since read; event attribution would be stale (retry)"
        ));
    }
    Ok(())
}

fn handle_update(
    home: &Path,
    instance_name: &str,
    emitter: crate::task_events::InstanceName,
    args: &Value,
) -> Value {
    let id = match id_arg(args) {
        Some(i) => i.to_string(),
        None => return serde_json::json!({"error": "missing 'id' (alias: task_id)"}),
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
    // #2117 P1: operate on the task's resolved board (default → home).
    let board = super::board_router::board_for_task(home, &id);
    let record = match read_task_record_at(&board, &id) {
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
    // #2117 P3a: board-isolation gate (outer boundary, before the owner-ACL).
    if let Some(deny) = cross_board_denied(home, &caller, &id, force) {
        return deny;
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
    // F3: `depends_on` is set at `Created` and is immutable via `update` — there
    // is no post-create event that carries it, by design (circular-dependency
    // containment; see the contract in `tasks/tests.rs`). Reject it explicitly
    // instead of the historical silent no-op that falsely reported "updated".
    if args.get("depends_on").is_some_and(|v| !v.is_null()) {
        return serde_json::json!({
            "error": format!(
                "'depends_on' is set at creation and is immutable via update (task {id})"
            ),
            "code": "immutable_field",
        });
    }
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
        // #1265: transition enforcement — reject illegal status changes.
        if let Some(target) = crate::task_events::TaskStatus::from_str(s) {
            if !prev_status.can_transition_to(target) {
                return serde_json::json!({
                    "error": format!(
                        "illegal transition: {} → {} (task {})",
                        crate::tasks::status_to_legacy_str(prev_status),
                        s,
                        id
                    ),
                    "code": "illegal_transition",
                });
            }
        }
        // #2249 pre-work alignment gate: the ONE live chokepoint for the
        // claimed→in_progress transition (verified during the #2524 P2a
        // spike — every other TaskEvent::InProgress emission site is
        // test-only or the one-time legacy-migration replay). Absent/0
        // `plan_ack_required` (the default) skips this entirely —
        // byte-identical to pre-#2249 for every task that doesn't opt in.
        if s.as_str() == "in_progress" {
            let required = record
                .metadata
                .get("plan_ack_required")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            if required > 0 {
                let acked = record
                    .metadata
                    .get("plan_acks")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len() as u64)
                    .unwrap_or(0);
                if acked < required {
                    return serde_json::json!({
                        "error": format!(
                            "plan-ack pending: {acked}/{required} acks (task {id})"
                        ),
                        "code": "plan_ack_pending",
                        "required": required,
                        "acked": acked,
                    });
                }
            }
        }
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
                    // CR-2026-06-14 (security): forensic done_source is daemon-only.
                    let source = caller_attestable_done_source(
                        &caller,
                        args.get("done_source"),
                        record.result.clone(),
                    );
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
                    source_evidence: format!("status {} → open", status_to_legacy_str(prev_status)),
                }),
                (_, "backlog") => Some(crate::task_events::TaskEvent::MovedToBacklog {
                    task_id: crate::task_events::TaskId(id.clone()),
                }),
                (_, "in_review") => Some(crate::task_events::TaskEvent::MovedToReview {
                    task_id: crate::task_events::TaskId(id.clone()),
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
    // Description update.
    if let Some(desc) = args["description"].as_str() {
        pending_events.push(crate::task_events::TaskEvent::DescriptionUpdated {
            task_id: crate::task_events::TaskId(id.clone()),
            by: crate::task_events::InstanceName::from(caller.as_str()),
            description: desc.to_string(),
        });
    }
    if let Some(new_tags) = args["tags"].as_array() {
        let tags: Vec<String> = new_tags
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        pending_events.push(crate::task_events::TaskEvent::TagsSet {
            task_id: crate::task_events::TaskId(id.clone()),
            tags,
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
    // F2: result backfill via the additive `ResultSet` event. Emitted only when
    // the supplied value actually differs from the current record — an equal
    // value produces no event, so the response below reports an honest
    // `unchanged` rather than a false `updated`. Rides the same atomic
    // `append_batch` + ownership ACL as every other update event (no separate
    // authority surface). Allowed on any state so an owner/orchestrator can
    // backfill the outcome on an already-done task (the witnessed case).
    if let Some(new_result) = args.get("result").and_then(|v| v.as_str()) {
        if record.result.as_deref() != Some(new_result) {
            pending_events.push(crate::task_events::TaskEvent::ResultSet {
                task_id: crate::task_events::TaskId(id.clone()),
                by: crate::task_events::InstanceName::from(caller.as_str()),
                result: new_result.to_string(),
            });
        }
    }
    // F1: single atomic append_batch over all the update arm's
    // queued events. Either all land or none do.
    let had_events = !pending_events.is_empty();
    if had_events {
        // #1868: re-validate the status transition UNDER the append lock against
        // FRESH committed state (mirrors `handle_claim`/`handle_done`). The
        // out-of-lock `can_transition_to` check above is a fast-reject; a
        // concurrent writer could have moved the task since, and replay does not
        // re-guard transitions. Only a status change is gated — priority/desc/
        // tags/owner events are last-write-wins metadata.
        let upd_id = id.clone();
        let target_status = new_status
            .as_deref()
            .and_then(crate::task_events::TaskStatus::from_str);
        // CR-2026-06-14 (:231): the ownership ACL (`can_mutate_record` above) and
        // the `by` field baked into Claimed/InProgress/Done events were BOTH
        // computed from the OUT-OF-LOCK `record` read. Re-validate under the
        // append lock against FRESH committed state so a concurrent owner change
        // can neither (a) slip an unauthorized write past the now-stale ACL, nor
        // (b) commit an event whose `by` attributes the transition to a former
        // owner. Fail closed on drift — the caller retries against the new state.
        // The pre-built events are kept (no in-lock rebuild); the drift check
        // guarantees their `by` is still correct at commit time.
        let stale_owner = record.owner.clone();
        let checked = crate::task_events::append_batch_checked_at(
            &board,
            &emitter,
            pending_events,
            |state| {
                update_batch_precondition(
                    state,
                    home,
                    &caller,
                    &upd_id,
                    force,
                    target_status,
                    &stale_owner,
                )
            },
        );
        match checked {
            Ok(Ok(_)) => {}
            Ok(Err(reason)) => {
                // Preserve the legacy `illegal_transition` code for the #1868
                // transition guard; the new in-lock ACL/owner-drift rejections
                // (:231) are a distinct, retryable precondition failure.
                let code = if reason.starts_with("illegal transition") {
                    "illegal_transition"
                } else {
                    "precondition_failed"
                };
                return serde_json::json!({"error": reason, "code": code});
            }
            Err(e) => {
                return serde_json::json!({"error": format!("event log append_batch failed: {e}")});
            }
        }
        // #1018 (B): mirror the `done` arm's cleanup hook for
        // the `update` arm's done/cancelled transitions.
        // Done-via-update + cancelled-via-update both close
        // the task and should clear pending sidecars.
        if let Some(ref s) = new_status {
            if matches!(s.as_str(), "done" | "cancelled") {
                // #1018/#78445-2 (d): terminal via update — shared cleanup of both
                // obligation stores.
                super::task_terminal_cleanup(home, &id);
            }
            if s == "cancelled" {
                cascade_cancel_children(home, &board, &id, &emitter);
            }
        }
        // #1916: a reassign (OwnerAssigned with a new owner) must retarget the
        // dispatch-idle sidecar to the new owner (+ reset its idle clock), else the
        // watchdog keeps nudging the FORMER owner about a task they no longer hold.
        // Mirrors the done/cancelled cleanup hook above; runs only after the append
        // committed. (The orphan path — OwnerAssigned with owner=None — calls the
        // same helper with None in orphan_tasks_for_owner, which clears the sidecar.)
        if let Some(ref new_owner) = new_assignee {
            let _ =
                crate::daemon::dispatch_idle::reassign_pending_for_task(home, &id, Some(new_owner));
            // #1923 G1/G3: re-point the SAME task's ci-watch handoff (next_after_ci)
            // + dispatch-tracking `to` to the new owner — sibling calls at the same
            // OwnerAssigned emit site, so a reassigned review hands off / is tracked
            // against the reviewer who now owns the task, not the stale original.
            let _ = crate::daemon::ci_watch::reassign_next_after_ci(home, &id, Some(new_owner));
            crate::dispatch_tracking::reassign_to(home, &id, Some(new_owner));
        }
    }
    // #807 Item 1: see create arm note.
    let task = read_task_record_at(&board, &id).map(|r| record_to_task(&r));
    if had_events {
        serde_json::json!({
            "id": id,
            "event": "updated",
            "task": task,
            // #807 deprecated alias kept for back-compat — see task.status for lifecycle.
            "status": "updated",
        })
    } else if [
        "status",
        "priority",
        "assignee",
        "description",
        "tags",
        "result",
    ]
    .iter()
    .any(|k| args.get(*k).is_some_and(|v| !v.is_null()))
    {
        // A recognized field was supplied but equalled the current value (only
        // `result` can reach here — status/priority/description/tags/owner always
        // emit an event). Honest idempotent no-op instead of a false "updated".
        serde_json::json!({
            "id": id,
            "event": "unchanged",
            "task": task,
            "status": "unchanged",
        })
    } else {
        // No supported mutable field supplied (`depends_on` was already rejected
        // above). Fail loud rather than the historical false "updated".
        serde_json::json!({
            "error": format!(
                "no updatable field supplied for task {id} \
                 (supported: status, priority, assignee, description, tags, result)"
            ),
            "code": "no_op",
        })
    }
}

fn handle_sweep(home: &Path, args: &Value) -> Value {
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
    let repo_owned: Option<String> = args["repository"]
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
    let pr_lookup: super::sweep::PrLookup = &super::sweep::gh_pr_lookup;
    let issue_lookup: super::sweep::IssueLookup = &super::sweep::gh_issue_lookup;
    let categories = super::sweep::scan_categories(
        home,
        &live_instances,
        pr_lookup,
        issue_lookup,
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
    let applied = super::sweep::emit_cancelled_batch(home, &categories, &confirm_ids, audit_reason);
    match applied {
        Ok(count) => serde_json::json!({
            "applied": count,
            "audit_reason": audit_reason,
        }),
        Err(e) => serde_json::json!({"error": format!("sweep apply failed: {e}")}),
    }
}

fn handle_health(home: &Path) -> Value {
    // #830 one-shot board hygiene snapshot. Operator self-serve
    // diagnosis: "is the board clean?" + recommended next
    // actions surfaced as a structured `recommendations` array.
    let live = crate::runtime::list_live_agents(home);
    let fleet_instances: std::collections::HashSet<String> =
        crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
            .ok()
            .map(|c| c.instances.keys().cloned().collect())
            .unwrap_or_default();
    // #2117 completeness: aggregate across EVERY project board (not just the
    // DEFAULT board) so `task action=health` surfaces per-project board load.
    // Single-project byte-identical (see `board_router::replay_all_boards`).
    let state = match super::board_router::replay_all_boards(home) {
        Ok(s) => s,
        Err(e) => {
            return serde_json::json!({
                "error": format!("task_events replay failed: {e}"),
                "code": "replay_failed",
            });
        }
    };
    build_health_response(&state, live.as_ref(), &fleet_instances)
}

fn handle_activity(home: &Path, args: &Value) -> Value {
    let task_id = match id_arg(args) {
        Some(id) => id,
        None => return serde_json::json!({"error": "missing 'id' (alias: task_id)"}),
    };
    activity_timeline(home, task_id)
}

fn handle_metadata_set(
    home: &Path,
    instance_name: &str,
    emitter: crate::task_events::InstanceName,
    args: &Value,
) -> Value {
    let id = match id_arg(args) {
        Some(i) => i,
        None => return serde_json::json!({"error": "missing 'id' (alias: task_id)"}),
    };
    let key = match args["metadata_key"].as_str() {
        Some(k) => k,
        None => return serde_json::json!({"error": "missing 'metadata_key'"}),
    };
    let value = if let Some(v) = args.get("metadata_value") {
        if v.is_null() {
            return serde_json::json!({"error": "missing 'metadata_value'"});
        }
        v.clone()
    } else {
        return serde_json::json!({"error": "missing 'metadata_value'"});
    };
    // AUDIT2-005: bound the value so a runaway blob can't bloat the append-only
    // event log (every list/get replay deserializes it). Owner-ACL-scoped, so
    // this is a belt-and-braces guard against an accidental huge value.
    const MAX_METADATA_VALUE_BYTES: usize = 64 * 1024;
    let value_len = serde_json::to_string(&value).map(|s| s.len()).unwrap_or(0);
    if value_len > MAX_METADATA_VALUE_BYTES {
        return serde_json::json!({
            "error": format!(
                "metadata_value too large ({value_len} bytes > {MAX_METADATA_VALUE_BYTES} byte cap)"
            ),
            "code": "metadata_value_too_large"
        });
    }
    // #2117 P1: operate on the task's resolved board (default → home).
    let board = super::board_router::board_for_task(home, id);
    let record = match read_task_record_at(&board, id) {
        Some(r) => r,
        None => return serde_json::json!({"error": format!("task not found: {id}")}),
    };
    // #2117 P3a: board-isolation gate (no force on metadata_set — mirror its
    // unconditional owner-ACL below).
    if let Some(deny) = cross_board_denied(home, instance_name, id, false) {
        return deny;
    }
    if !can_mutate_record(home, instance_name, &record) {
        return serde_json::json!({"error": "permission denied: caller is not owner/creator"});
    }
    let event = crate::task_events::TaskEvent::MetadataSet {
        task_id: crate::task_events::TaskId(id.to_string()),
        by: emitter.clone(),
        key: key.to_string(),
        value,
    };
    match crate::task_events::append_at(&board, &emitter, event) {
        Ok(_) => {
            let task = read_task_record_at(&board, id).map(|r| record_to_task(&r));
            serde_json::json!({"id": id, "event": "metadata_set", "task": task})
        }
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

/// #2249 pre-work alignment gate: idempotently record `instance_name`'s ack of
/// the task's `plan` metadata. The assignee may never ack their own plan (the
/// whole point is an outside check); plan_ack_required's threshold is read
/// separately by `handle_update`'s `(_, "in_progress")` gate — this action
/// only maintains the ack list, it never blocks a transition itself.
fn handle_ack_plan(
    home: &Path,
    instance_name: &str,
    emitter: crate::task_events::InstanceName,
    args: &Value,
) -> Value {
    let id = match id_arg(args) {
        Some(i) => i,
        None => return serde_json::json!({"error": "missing 'id' (alias: task_id)"}),
    };
    let board = super::board_router::board_for_task(home, id);
    let record = match read_task_record_at(&board, id) {
        Some(r) => r,
        None => return serde_json::json!({"error": format!("task not found: {id}")}),
    };
    if let Some(deny) = cross_board_denied(home, instance_name, id, false) {
        return deny;
    }
    if record.owner.as_ref().map(|o| o.0.as_str()) == Some(instance_name) {
        return serde_json::json!({
            "error": "the task's assignee cannot ack their own plan",
            "code": "self_ack_forbidden",
        });
    }
    if !record.metadata.contains_key("plan") {
        return serde_json::json!({
            "error": format!(
                "task '{id}' has no plan set yet — set one via \
                 task action=metadata_set metadata_key=plan first"
            ),
            "code": "plan_not_set",
        });
    }
    let mut acks: Vec<String> = record
        .metadata
        .get("plan_acks")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if acks.iter().any(|a| a == instance_name) {
        // Idempotent: already acked by this caller — no-op success, no new event.
        return serde_json::json!({
            "id": id, "event": "ack_plan", "acked": acks.len(), "already_acked": true,
        });
    }
    acks.push(instance_name.to_string());
    let acked_count = acks.len();
    let event = crate::task_events::TaskEvent::MetadataSet {
        task_id: crate::task_events::TaskId(id.to_string()),
        by: emitter.clone(),
        key: "plan_acks".to_string(),
        value: serde_json::json!(acks),
    };
    match crate::task_events::append_at(&board, &emitter, event) {
        Ok(_) => serde_json::json!({
            "id": id, "event": "ack_plan", "acked": acked_count, "already_acked": false,
        }),
        Err(e) => serde_json::json!({"error": format!("{e}")}),
    }
}

fn handle_metadata_get(home: &Path, args: &Value) -> Value {
    let id = match id_arg(args) {
        Some(i) => i,
        None => return serde_json::json!({"error": "missing 'id' (alias: task_id)"}),
    };
    let board = super::board_router::board_for_task(home, id);
    match read_task_record_at(&board, id) {
        Some(r) => {
            serde_json::json!({"id": id, "metadata": r.metadata})
        }
        None => serde_json::json!({"error": format!("task not found: {id}")}),
    }
}

/// #1147: Build a chronological activity timeline for a task.
fn activity_timeline(home: &Path, task_id: &str) -> Value {
    // #2117 P1: read the task's resolved board (default → home).
    let board = super::board_router::board_for_task(home, task_id);
    let envelopes = match crate::task_events::envelopes_for_task_at(&board, task_id) {
        Ok(e) => e,
        Err(e) => return serde_json::json!({"error": format!("failed to read task events: {e}")}),
    };

    let events: Vec<Value> = envelopes
        .iter()
        .map(|env| {
            let (event_type, actor, summary) = summarize_event(env);
            serde_json::json!({
                "timestamp": env.timestamp,
                "actor": actor,
                "event_type": event_type,
                "summary": summary,
            })
        })
        .collect();

    serde_json::json!({
        "task_id": task_id,
        "events": events,
        "count": events.len(),
    })
}

fn summarize_event(env: &crate::task_events::TaskEventEnvelope) -> (&str, String, String) {
    use crate::task_events::TaskEvent;
    let actor = env.instance.0.clone();
    match &env.event {
        TaskEvent::Created {
            title,
            branch,
            owner,
            ..
        } => {
            let assignee = owner.as_ref().map(|o| o.0.as_str()).unwrap_or("unassigned");
            let br = branch.as_deref().unwrap_or("");
            let summary = if br.is_empty() {
                format!("created task: {title} (assignee: {assignee})")
            } else {
                format!("created task: {title} (assignee: {assignee}, branch: {br})")
            };
            ("created", actor, summary)
        }
        TaskEvent::Claimed { by, .. } => ("claimed", by.0.clone(), "claimed task".to_string()),
        TaskEvent::InProgress { by, .. } => {
            ("in_progress", by.0.clone(), "started work".to_string())
        }
        TaskEvent::Verified { by_reviewer, .. } => {
            ("verified", by_reviewer.0.clone(), "verified".to_string())
        }
        TaskEvent::Done { source, .. } => {
            let detail = match source {
                crate::task_events::DoneSource::PrMerged { pr_id, .. } => {
                    format!("done (PR {} merged)", pr_id)
                }
                crate::task_events::DoneSource::OperatorManual { result, .. } => {
                    format!(
                        "done{}",
                        result
                            .as_ref()
                            .map(|r| format!(": {r}"))
                            .unwrap_or_default()
                    )
                }
                _ => "done".to_string(),
            };
            ("done", actor, detail)
        }
        TaskEvent::Cancelled { by, reason, .. } => {
            ("cancelled", by.0.clone(), format!("cancelled: {reason}"))
        }
        TaskEvent::Blocked { reason, .. } => ("blocked", actor, format!("blocked: {reason}")),
        TaskEvent::Unblocked { .. } => ("unblocked", actor, "unblocked".to_string()),
        TaskEvent::Reopened { reason, .. } => ("reopened", actor, format!("reopened: {reason}")),
        TaskEvent::Released { reason, .. } => {
            ("released", actor, format!("released claim: {reason}"))
        }
        TaskEvent::Linked { pr_id, .. } => ("linked", actor, format!("linked PR {pr_id}")),
        TaskEvent::TaskCloseProposed { .. } => (
            "close_proposed",
            actor,
            "close proposed by sweep".to_string(),
        ),
        TaskEvent::OwnerAssigned { owner, .. } => {
            let o = owner.as_ref().map(|n| n.0.as_str()).unwrap_or("none");
            ("owner_assigned", actor, format!("assigned to {o}"))
        }
        TaskEvent::PriorityChanged { priority, .. } => {
            ("priority_changed", actor, format!("priority → {priority}"))
        }
        TaskEvent::DescriptionUpdated { .. } => (
            "description_updated",
            actor,
            "description updated".to_string(),
        ),
        TaskEvent::TagsSet { tags, .. } => ("tags_set", actor, format!("tags → {tags:?}")),
        TaskEvent::ResultSet { by, .. } => ("result_set", by.0.clone(), "result set".to_string()),
        TaskEvent::MetadataSet { key, value, by, .. } => (
            "metadata_set",
            by.0.clone(),
            format!("metadata[{key}] = {value}"),
        ),
        TaskEvent::MovedToBacklog { .. } => ("moved_to_backlog", actor, "→ backlog".to_string()),
        TaskEvent::MovedToReview { .. } => ("moved_to_review", actor, "→ in_review".to_string()),
        TaskEvent::BranchLinked { branch, by, .. } => {
            ("branch_linked", by.0.clone(), format!("branch → {branch}"))
        }
    }
}

fn cascade_cancel_children(
    home: &Path,
    board: &Path,
    parent_id: &str,
    emitter: &crate::task_events::InstanceName,
) {
    // #2117 P1: a parent's children live on the parent's board — replay + cancel
    // there. `home` is kept only for the cross-instance cascade NOTIFICATION
    // (route_cascade_cancel), which is fleet-global. Single-project → board == home.
    let Ok(state) = crate::task_events::replay_at(board) else {
        return;
    };
    let parent_tid = crate::task_events::TaskId(parent_id.to_string());
    let mut cancel_events = Vec::new();
    let mut cancelled_ids = Vec::new();
    let mut notify_ids = Vec::new();
    for (child_id, child) in &state.tasks {
        if child.parent_id.as_ref() != Some(&parent_tid) {
            continue;
        }
        match child.status {
            crate::task_events::TaskStatus::Open | crate::task_events::TaskStatus::Claimed => {
                cancel_events.push(crate::task_events::TaskEvent::Cancelled {
                    task_id: child_id.clone(),
                    by: emitter.clone(),
                    reason: format!("cascade: parent {parent_id} cancelled"),
                });
                cancelled_ids.push(child_id.0.clone());
            }
            crate::task_events::TaskStatus::InProgress => {
                notify_ids.push((child_id.clone(), child.owner.clone()));
            }
            _ => {}
        }
    }
    if !cancel_events.is_empty()
        && crate::task_events::append_batch_at(board, emitter, cancel_events).is_ok()
    {
        // #78445-2 (d): each cascaded child-cancel is terminal — clear BOTH obligation
        // stores for EACH child (plural). task_id-scoped, so non-child rows are
        // untouched. (This path previously cleared NEITHER.)
        for cid in &cancelled_ids {
            super::task_terminal_cleanup(home, cid);
        }
    }
    for (child_id, owner) in notify_ids {
        if let Some(ref owner_name) = owner {
            route_cascade_cancel(home, &owner_name.0, parent_id, &child_id.0);
        }
    }
}

/// #event-bus pattern #7 (Option A): gate-ON → emit `CascadeCancelNotify` (the
/// subscriber delivers via `deliver_cascade_cancel`); gate-OFF (prod default) →
/// the legacy direct `deliver_cascade_cancel`. No double-delivery, no gate-off
/// regression.
fn route_cascade_cancel(home: &Path, owner: &str, parent_id: &str, child_id: &str) {
    // #event-bus Step 2 (legacy-zero): the bus is the sole delivery path.
    crate::daemon::event_bus::global().emit(
        home,
        crate::daemon::event_bus::EventKind::CascadeCancelNotify {
            owner: owner.to_string(),
            parent_id: parent_id.to_string(),
            child_id: child_id.to_string(),
        },
    );
}

/// Shared deliver: enqueue the parent-cancelled notify to the child's owner.
/// Called by BOTH the legacy path AND the event-bus subscriber, so the two are
/// byte-identical by construction.
fn deliver_cascade_cancel(home: &Path, owner: &str, parent_id: &str, child_id: &str) {
    let msg = crate::inbox::message::InboxMessage {
        text: format!(
            "[parent-cancelled] Parent task {parent_id} was cancelled. \
             Your in-progress subtask {child_id} may need attention."
        ),
        kind: Some("parent_cancelled".to_string()),
        ..Default::default()
    };
    persist_or_log!(
        crate::inbox::storage::enqueue(home, owner, msg),
        "cascade_cancel_notify",
        owner
    );
}

/// #event-bus pattern #7 subscriber: re-deliver a `CascadeCancelNotify` event
/// via the shared `deliver_cascade_cancel`.
fn handle_event(event: &crate::daemon::event_bus::Event) -> bool {
    if let crate::daemon::event_bus::EventKind::CascadeCancelNotify {
        owner,
        parent_id,
        child_id,
    } = &event.kind
    {
        deliver_cascade_cancel(&event.home, owner, parent_id, child_id);
        true
    } else {
        false
    }
}

/// Register the cascade-cancel subscriber once at daemon startup (`run_core`).
/// Home-agnostic — the home travels on each event.
pub fn register_subscriber() {
    crate::daemon::event_bus::global().subscribe(handle_event);
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod review_repro_tasks;
