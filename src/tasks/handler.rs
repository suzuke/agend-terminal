use serde_json::Value;
use std::path::Path;

use super::acl::{can_mutate_record, instance_exists};
use super::orphan::build_health_response;
use super::{list_all, record_to_task, status_to_legacy_str, Task};

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
            // Description update.
            if let Some(desc) = args["description"].as_str() {
                pending_events.push(crate::task_events::TaskEvent::DescriptionUpdated {
                    task_id: crate::task_events::TaskId(id.clone()),
                    by: crate::task_events::InstanceName::from(caller.as_str()),
                    description: desc.to_string(),
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
    }
}
