use serde_json::Value;
use std::path::Path;

use super::acl::{can_mutate_record, instance_exists};
use super::orphan::build_health_response;
use super::{list_all, record_to_task, status_to_legacy_str, Task};

fn parse_due_at(args: &Value) -> Option<String> {
    let due = args["due_at"].as_str()?;
    let dt = chrono::DateTime::parse_from_rfc3339(due).ok()?;
    Some(dt.with_timezone(&chrono::Utc).to_rfc3339())
}

/// Read a single task's current replay-derived record. Used by
/// `handle`'s mutation arms to validate `(prev_status, transition)`
/// before emitting an event.
pub(super) fn read_task_record(home: &Path, id: &str) -> Option<crate::task_events::TaskRecord> {
    let state = crate::task_events::replay(home).ok()?;
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
            match crate::task_events::append(home, &emitter, event) {
                Ok(_) => {
                    let task = read_task_record(home, &id).map(|r| record_to_task(&r));
                    let dispatch_target = routed_to.as_ref().or(assignee.as_ref());
                    if let Some(target) = dispatch_target {
                        if target != instance_name {
                            let branch_str = args["branch"]
                                .as_str()
                                .map(|b| format!("\nBranch: {b}"))
                                .unwrap_or_default();
                            let msg_text =
                                format!("[delegate_task] {title}{branch_str} (task id: {id})");
                            let inbox_msg = crate::inbox::InboxMessage {
                                from: format!("from:{instance_name}"),
                                text: msg_text.clone(),
                                kind: Some("task".to_string()),
                                task_id: Some(id.clone()),
                                correlation_id: Some(id.clone()),
                                timestamp: chrono::Utc::now().to_rfc3339(),
                                ..Default::default()
                            };
                            let _ = crate::inbox::storage::enqueue(home, target, inbox_msg);
                            crate::dispatch_tracking::track_dispatch(
                                home,
                                crate::dispatch_tracking::DispatchEntry {
                                    task_id: Some(id.clone()),
                                    from: instance_name.to_string(),
                                    to: target.to_string(),
                                    from_id: None,
                                    to_id: None,
                                    delegated_at: chrono::Utc::now().to_rfc3339(),
                                    status: "pending".to_string(),
                                },
                            );
                            let source = crate::inbox::NotifySource::Agent(instance_name);
                            crate::inbox::notify::notify_agent(home, target, &source, &msg_text);
                        }
                    }
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
        "list" => {
            let filter_assignee = args["filter_assignee"].as_str();
            let filter_status = args["filter_status"].as_str();
            let filter_tag = args["filter_tag"].as_str();
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
            let tasks = list_all(home);
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
            let is_self_reclaim = task_view.status == crate::task_events::TaskStatus::Claimed
                && task_view.assignee.as_deref() == Some(iname.as_str());
            if !is_self_reclaim && task_view.status != crate::task_events::TaskStatus::Open {
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
                    // #1018 (B): eager cleanup of pending dispatch
                    // sidecars whose correlation_id matches this
                    // closed task. Prevents the watchdog from firing
                    // `dispatch_idle_threshold_exceeded` later for
                    // work the task board already confirmed done.
                    let _ = crate::daemon::dispatch_idle::cleanup_pending_for_task_id(home, &id);
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
            // F1: single atomic append_batch over all the update arm's
            // queued events. Either all land or none do.
            if !pending_events.is_empty() {
                if let Err(e) = crate::task_events::append_batch(home, &emitter, pending_events) {
                    return serde_json::json!({
                        "error": format!("event log append_batch failed: {e}")
                    });
                }
                // #1018 (B): mirror the `done` arm's cleanup hook for
                // the `update` arm's done/cancelled transitions.
                // Done-via-update + cancelled-via-update both close
                // the task and should clear pending sidecars.
                if let Some(ref s) = new_status {
                    if matches!(s.as_str(), "done" | "cancelled") {
                        let _ =
                            crate::daemon::dispatch_idle::cleanup_pending_for_task_id(home, &id);
                    }
                    if s == "cancelled" {
                        cascade_cancel_children(home, &id, &emitter);
                    }
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
            let categories = super::sweep::scan_categories(
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
                super::sweep::emit_cancelled_batch(home, &categories, &confirm_ids, audit_reason);
            match applied {
                Ok(count) => serde_json::json!({
                    "applied": count,
                    "audit_reason": audit_reason,
                }),
                Err(e) => serde_json::json!({"error": format!("sweep apply failed: {e}")}),
            }
        }
        "health" => {
            // #830 one-shot board hygiene snapshot. Operator self-serve
            // diagnosis: "is the board clean?" + recommended next
            // actions surfaced as a structured `recommendations` array.
            let live = crate::runtime::list_live_agents(home);
            let fleet_instances: std::collections::HashSet<String> =
                crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                    .ok()
                    .map(|c| c.instances.keys().cloned().collect())
                    .unwrap_or_default();
            let state = match crate::task_events::replay(home) {
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
        "activity" => {
            let task_id = match args["id"].as_str().filter(|s| !s.is_empty()) {
                Some(id) => id,
                None => return serde_json::json!({"error": "missing 'id'"}),
            };
            activity_timeline(home, task_id)
        }
        "metadata_set" => {
            let id = match args["id"].as_str() {
                Some(i) => i,
                None => return serde_json::json!({"error": "missing 'id'"}),
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
            let record = match read_task_record(home, id) {
                Some(r) => r,
                None => return serde_json::json!({"error": format!("task not found: {id}")}),
            };
            if !can_mutate_record(home, instance_name, &record) {
                return serde_json::json!({"error": "permission denied: caller is not owner/creator"});
            }
            let event = crate::task_events::TaskEvent::MetadataSet {
                task_id: crate::task_events::TaskId(id.to_string()),
                by: emitter.clone(),
                key: key.to_string(),
                value,
            };
            match crate::task_events::append(home, &emitter, event) {
                Ok(_) => {
                    let task = read_task_record(home, id).map(|r| record_to_task(&r));
                    serde_json::json!({"id": id, "event": "metadata_set", "task": task})
                }
                Err(e) => serde_json::json!({"error": format!("{e}")}),
            }
        }
        "metadata_get" => {
            let id = match args["id"].as_str() {
                Some(i) => i,
                None => return serde_json::json!({"error": "missing 'id'"}),
            };
            match read_task_record(home, id) {
                Some(r) => {
                    serde_json::json!({"id": id, "metadata": r.metadata})
                }
                None => serde_json::json!({"error": format!("task not found: {id}")}),
            }
        }
        _ => serde_json::json!({"error": format!("unknown action: {action}")}),
    }
}

/// #1147: Build a chronological activity timeline for a task.
fn activity_timeline(home: &Path, task_id: &str) -> Value {
    let envelopes = match crate::task_events::envelopes_for_task(home, task_id) {
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
        TaskEvent::MetadataSet { key, value, by, .. } => (
            "metadata_set",
            by.0.clone(),
            format!("metadata[{key}] = {value}"),
        ),
        TaskEvent::MovedToBacklog { .. } => ("moved_to_backlog", actor, "→ backlog".to_string()),
        TaskEvent::MovedToReview { .. } => ("moved_to_review", actor, "→ in_review".to_string()),
    }
}

fn cascade_cancel_children(
    home: &Path,
    parent_id: &str,
    emitter: &crate::task_events::InstanceName,
) {
    let Ok(state) = crate::task_events::replay(home) else {
        return;
    };
    let parent_tid = crate::task_events::TaskId(parent_id.to_string());
    let mut cancel_events = Vec::new();
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
            }
            crate::task_events::TaskStatus::InProgress => {
                notify_ids.push((child_id.clone(), child.owner.clone()));
            }
            _ => {}
        }
    }
    if !cancel_events.is_empty() {
        let _ = crate::task_events::append_batch(home, emitter, cancel_events);
    }
    for (child_id, owner) in notify_ids {
        if let Some(ref owner_name) = owner {
            let msg = crate::inbox::message::InboxMessage {
                text: format!(
                    "[parent-cancelled] Parent task {parent_id} was cancelled. \
                     Your in-progress subtask {} may need attention.",
                    child_id.0
                ),
                kind: Some("parent_cancelled".to_string()),
                ..Default::default()
            };
            let _ = crate::inbox::storage::enqueue(home, &owner_name.0, msg);
        }
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
            "agend-metadata-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn create_task(home: &std::path::Path, task_id: &str) {
        let args = serde_json::json!({
            "action": "create",
            "title": "test task",
        });
        let emitter = crate::task_events::InstanceName::from("test:operator");
        let tid = crate::task_events::TaskId(task_id.into());
        crate::task_events::append(
            home,
            &emitter,
            crate::task_events::TaskEvent::Created {
                task_id: tid,
                title: "test task".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: Some(crate::task_events::InstanceName::from("dev-agent")),
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: None,
            },
        )
        .expect("create task");
        let _ = args;
    }

    #[test]
    fn metadata_set_writes_and_reads() {
        let home = tmp_home("set_read");
        create_task(&home, "t-meta-001");

        let result = handle(
            &home,
            "dev-agent",
            &serde_json::json!({
                "action": "metadata_set",
                "id": "t-meta-001",
                "metadata_key": "pr_url",
                "metadata_value": "https://github.com/test/repo/pull/42"
            }),
        );
        assert_eq!(result["event"], "metadata_set");
        assert!(result["error"].is_null(), "unexpected error: {result}");

        let get_result = handle(
            &home,
            "dev-agent",
            &serde_json::json!({
                "action": "metadata_get",
                "id": "t-meta-001",
            }),
        );
        assert_eq!(
            get_result["metadata"]["pr_url"],
            "https://github.com/test/repo/pull/42"
        );
    }

    #[test]
    fn metadata_set_overwrites_existing_key() {
        let home = tmp_home("overwrite");
        create_task(&home, "t-meta-002");

        handle(
            &home,
            "dev-agent",
            &serde_json::json!({
                "action": "metadata_set",
                "id": "t-meta-002",
                "metadata_key": "commit_sha",
                "metadata_value": "abc123"
            }),
        );
        handle(
            &home,
            "dev-agent",
            &serde_json::json!({
                "action": "metadata_set",
                "id": "t-meta-002",
                "metadata_key": "commit_sha",
                "metadata_value": "def456"
            }),
        );

        let result = handle(
            &home,
            "dev-agent",
            &serde_json::json!({
                "action": "metadata_get",
                "id": "t-meta-002",
            }),
        );
        assert_eq!(result["metadata"]["commit_sha"], "def456");
    }

    #[test]
    fn metadata_supports_non_string_values() {
        let home = tmp_home("non_string");
        create_task(&home, "t-meta-003");

        handle(
            &home,
            "dev-agent",
            &serde_json::json!({
                "action": "metadata_set",
                "id": "t-meta-003",
                "metadata_key": "retry_count",
                "metadata_value": 3
            }),
        );

        let result = handle(
            &home,
            "dev-agent",
            &serde_json::json!({
                "action": "metadata_get",
                "id": "t-meta-003",
            }),
        );
        assert_eq!(result["metadata"]["retry_count"], 3);
    }

    #[test]
    fn metadata_get_empty_on_new_task() {
        let home = tmp_home("empty_meta");
        create_task(&home, "t-meta-004");

        let result = handle(
            &home,
            "dev-agent",
            &serde_json::json!({
                "action": "metadata_get",
                "id": "t-meta-004",
            }),
        );
        assert!(result["error"].is_null());
        assert_eq!(result["metadata"], serde_json::json!({}));
    }

    #[test]
    fn metadata_set_missing_key_returns_error() {
        let home = tmp_home("missing_key");
        create_task(&home, "t-meta-005");

        let result = handle(
            &home,
            "dev-agent",
            &serde_json::json!({
                "action": "metadata_set",
                "id": "t-meta-005",
                "metadata_value": "some_value"
            }),
        );
        assert!(result["error"].as_str().unwrap().contains("metadata_key"));
    }

    #[test]
    fn metadata_set_missing_value_returns_error() {
        let home = tmp_home("missing_val");
        create_task(&home, "t-meta-006");

        let result = handle(
            &home,
            "dev-agent",
            &serde_json::json!({
                "action": "metadata_set",
                "id": "t-meta-006",
                "metadata_key": "some_key"
            }),
        );
        assert!(result["error"].as_str().unwrap().contains("metadata_value"));
    }

    #[test]
    fn metadata_appears_in_list() {
        let home = tmp_home("list_meta");
        create_task(&home, "t-meta-007");

        handle(
            &home,
            "dev-agent",
            &serde_json::json!({
                "action": "metadata_set",
                "id": "t-meta-007",
                "metadata_key": "pr_url",
                "metadata_value": "https://example.com/pr/1"
            }),
        );

        let list = handle(&home, "dev-agent", &serde_json::json!({"action": "list"}));
        let tasks = list["tasks"].as_array().unwrap();
        let task = tasks.iter().find(|t| t["id"] == "t-meta-007").unwrap();
        assert_eq!(task["metadata"]["pr_url"], "https://example.com/pr/1");
    }

    #[test]
    fn metadata_get_nonexistent_task_returns_error() {
        let home = tmp_home("nonexistent");

        let result = handle(
            &home,
            "dev-agent",
            &serde_json::json!({
                "action": "metadata_get",
                "id": "t-meta-999",
            }),
        );
        assert!(result["error"].as_str().unwrap().contains("not found"));
    }

    fn drain_inbox(home: &std::path::Path, agent: &str) -> Vec<crate::inbox::InboxMessage> {
        crate::inbox::storage::drain(home, agent)
    }

    #[test]
    fn create_with_assignee_sends_task_to_inbox() {
        let home = tmp_home("assign_inbox");
        let result = handle(
            &home,
            "lead-agent",
            &serde_json::json!({
                "action": "create",
                "title": "implement feature X",
                "assignee": "dev-agent",
                "branch": "fix/feature-x",
            }),
        );
        assert_eq!(result["event"], "created");
        let task_id = result["id"].as_str().unwrap();

        let msgs = drain_inbox(&home, "dev-agent");
        assert!(!msgs.is_empty(), "assignee should receive inbox message");
        let msg = &msgs[0];
        assert_eq!(msg.kind.as_deref(), Some("task"));
        assert_eq!(msg.task_id.as_deref(), Some(task_id));
        assert!(msg.text.contains("implement feature X"));
        assert!(msg.text.contains("fix/feature-x"));
    }

    #[test]
    fn create_without_assignee_sends_no_message() {
        let home = tmp_home("no_assign");
        let result = handle(
            &home,
            "lead-agent",
            &serde_json::json!({
                "action": "create",
                "title": "unassigned task",
            }),
        );
        assert_eq!(result["event"], "created");

        let msgs = drain_inbox(&home, "lead-agent");
        assert!(msgs.is_empty(), "no inbox message without assignee");
    }

    #[test]
    fn create_self_assign_sends_no_message() {
        let home = tmp_home("self_assign");
        let result = handle(
            &home,
            "dev-agent",
            &serde_json::json!({
                "action": "create",
                "title": "self-assigned task",
                "assignee": "dev-agent",
            }),
        );
        assert_eq!(result["event"], "created");

        let msgs = drain_inbox(&home, "dev-agent");
        assert!(msgs.is_empty(), "self-assign should not send inbox message");
    }

    #[test]
    fn create_with_assignee_task_status_is_open() {
        let home = tmp_home("status_open");
        let result = handle(
            &home,
            "lead-agent",
            &serde_json::json!({
                "action": "create",
                "title": "test task",
                "assignee": "dev-agent",
            }),
        );
        let task = &result["task"];
        assert_eq!(task["status"], "open");
        assert_eq!(task["assignee"], "dev-agent");
    }

    #[test]
    fn create_with_assignee_correlation_id_matches_task_id() {
        let home = tmp_home("corr_id");
        let result = handle(
            &home,
            "lead-agent",
            &serde_json::json!({
                "action": "create",
                "title": "correlated task",
                "assignee": "dev-agent",
            }),
        );
        let task_id = result["id"].as_str().unwrap();

        let msgs = drain_inbox(&home, "dev-agent");
        let msg = &msgs[0];
        assert_eq!(
            msg.correlation_id.as_deref(),
            Some(task_id),
            "correlation_id should match task_id for auto-close"
        );
    }

    fn write_fleet_yaml_with_team(home: &std::path::Path, team: &str, orchestrator: &str) {
        let yaml = format!(
            "teams:\n  {team}:\n    orchestrator: {orchestrator}\n    members:\n      - dev-a\n      - dev-b\n"
        );
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();
    }

    #[test]
    fn create_with_team_assignee_routes_to_orchestrator() {
        let home = tmp_home("team_route");
        write_fleet_yaml_with_team(&home, "my-team", "team-lead");

        let result = handle(
            &home,
            "operator",
            &serde_json::json!({
                "action": "create",
                "title": "team task",
                "assignee": "my-team",
            }),
        );
        assert_eq!(result["event"], "created");

        let orch_msgs = drain_inbox(&home, "team-lead");
        assert!(
            !orch_msgs.is_empty(),
            "orchestrator should receive inbox message when team is assignee"
        );
        assert_eq!(orch_msgs[0].kind.as_deref(), Some("task"));
        assert!(orch_msgs[0].text.contains("team task"));

        let team_msgs = drain_inbox(&home, "my-team");
        assert!(
            team_msgs.is_empty(),
            "raw team name should NOT receive inbox message"
        );
    }

    #[test]
    fn create_with_assignee_tracks_dispatch() {
        let home = tmp_home("dispatch_track");
        let result = handle(
            &home,
            "lead-agent",
            &serde_json::json!({
                "action": "create",
                "title": "tracked dispatch task",
                "assignee": "dev-agent",
            }),
        );
        assert_eq!(result["event"], "created");
        let task_id = result["id"].as_str().unwrap();

        let store: serde_json::Value =
            crate::store::load(&crate::store::store_path(&home, "dispatch_tracking.json"));
        let entries = store["entries"].as_array().expect("entries array");
        assert_eq!(entries.len(), 1, "one dispatch entry expected");
        assert_eq!(entries[0]["task_id"], task_id);
        assert_eq!(entries[0]["from"], "lead-agent");
        assert_eq!(entries[0]["to"], "dev-agent");
        assert_eq!(entries[0]["status"], "pending");
    }

    #[test]
    fn create_self_assign_no_dispatch_tracking() {
        let home = tmp_home("dispatch_self");
        handle(
            &home,
            "dev-agent",
            &serde_json::json!({
                "action": "create",
                "title": "self task",
                "assignee": "dev-agent",
            }),
        );

        let path = crate::store::store_path(&home, "dispatch_tracking.json");
        assert!(
            !path.exists(),
            "self-assign should not create dispatch tracking entry"
        );
    }

    #[test]
    fn create_without_assignee_no_dispatch_tracking() {
        let home = tmp_home("dispatch_none");
        handle(
            &home,
            "lead-agent",
            &serde_json::json!({
                "action": "create",
                "title": "unassigned",
            }),
        );

        let path = crate::store::store_path(&home, "dispatch_tracking.json");
        assert!(
            !path.exists(),
            "unassigned task should not create dispatch tracking entry"
        );
    }
}
