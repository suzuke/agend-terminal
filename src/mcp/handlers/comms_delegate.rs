//! W2.2: `handle_delegate_task` as an ordered phase pipeline.
//!
//! Stages (failure order preserved — a reject before lease never leases;
//! a send failure may still have leased/created a task, same as pre-split):
//!
//! 1. **resolve** — identity, instance/team target, self-dispatch reject
//! 2. **validate** — pre-send gates (`comms_gates::run_dispatch_pre_checks`)
//! 3. **compose** — message body + force_meta
//! 4. **lease** — optional `dispatch_auto_bind_lease` when `branch` set
//! 5. **create** — optional auto board task after all rejectable checks
//! 6. **send** — API SEND / inbox fallback via [`SendEnvelope`]
//! 7. **track** — dispatch_tracking + UX + `auto_created_task_id` on success
//!
//! Loaded as a child of `comms` so `file_size_invariant` keeps `comms.rs` under
//! the handler LOC cap while the choreography stays one ordered function.

use crate::channel::sink_registry::registry as ux_sink_registry;
use crate::channel::ux_event::{FleetEvent, UxEvent};
use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

use super::super::comms_gates::{self, DispatchPreChecks};
use super::super::dispatch_hook;
use super::super::send_envelope::SendEnvelope;
use super::super::{err_needs_identity, is_ok_result, require_instance};

/// Sprint 55 P0-C — true when the caller passed `bind: false`.
pub(in crate::mcp::handlers) fn dispatch_should_skip_auto_bind(args: &Value) -> bool {
    args["bind"].as_bool() == Some(false)
}

struct ResolvedDelegate<'a> {
    sender: &'a Sender,
    resolved_target: String,
    task: &'a str,
}

/// Phase 1 — identity, target resolution, self-dispatch reject, require `task`.
fn resolve_delegate<'a>(
    home: &Path,
    args: &'a Value,
    sender: &'a Option<Sender>,
) -> Result<ResolvedDelegate<'a>, Value> {
    let Some(sender) = sender.as_ref() else {
        return Err(err_needs_identity("delegate_task"));
    };
    let raw_target = require_instance(args)?;
    if let Err(e) = crate::agent::validate_name(raw_target) {
        return Err(json!({"error": e}));
    }
    // Sprint 46 P2: resolve target via InstanceId — replaces P1 name-lookup bandaid.
    let resolved_target = match crate::agent::resolve_instance(home, raw_target) {
        Ok((_id, name)) => name,
        Err(crate::agent::ResolveError::NotFound(_)) => {
            match crate::teams::resolve_team_orchestrator(home, raw_target) {
                Ok(Some(orch)) => orch,
                Ok(None) => raw_target.to_string(),
                Err(e) => return Err(json!({"error": e})),
            }
        }
    };
    let target = resolved_target.as_str();
    // M5: reject if team-orchestrator resolution collapsed target to sender.
    if *sender == target && raw_target != target {
        return Err(json!({"error": format!(
            "task target '{}' resolved to sender '{}' (team orchestrator loop) \
             — verify instance name does not collide with a team template name",
            raw_target, sender.as_str()
        )}));
    }
    // CR-2026-06-14 (resource-leak): reject plain self-dispatch BEFORE lease.
    if *sender == target {
        return Err(json!({"error": "cannot delegate task to self — use a different instance"}));
    }
    let task = match args["task"].as_str() {
        Some(t) => t,
        None => return Err(json!({"error": "missing 'task'"})),
    };
    Ok(ResolvedDelegate {
        sender,
        resolved_target,
        task,
    })
}

struct ComposedDelegate {
    msg: String,
    force_meta_json: Option<Value>,
    second_reviewer: bool,
    plan_ack_required: u64,
}

/// Phase 3 — build inject message + force_meta from pre-check scalars.
fn compose_delegate_message(
    task: &str,
    args: &Value,
    checks: &DispatchPreChecks,
) -> ComposedDelegate {
    let force = checks.force;
    let force_reason = checks.force_reason.as_deref();
    let mut msg = format!("[delegate_task] {task}");
    if force {
        if let Some(r) = force_reason {
            msg.push_str(&format!("\n\n⚠️ FORCED (reason: {r})"));
        }
    }
    if let Some(criteria) = args["success_criteria"].as_str() {
        msg.push_str(&format!("\n\nSuccess criteria: {criteria}"));
    }
    if let Some(ctx) = args["context"].as_str() {
        msg.push_str(&format!("\n\nContext: {ctx}"));
    }
    if let Some(branch) = args["branch"].as_str() {
        msg.push_str(&format!("\n\nBranch: {branch}"));
    }
    let force_meta_json = if force {
        Some(json!({
            "forced": true,
            "reason": force_reason.unwrap_or(""),
            "forced_at": chrono::Utc::now().to_rfc3339()
        }))
    } else {
        None
    };
    ComposedDelegate {
        msg,
        force_meta_json,
        second_reviewer: checks.second_reviewer,
        plan_ack_required: checks.plan_ack_required,
    }
}

/// Phase 4 — optional auto-bind lease (rejectable).
fn maybe_auto_bind_lease(
    home: &Path,
    args: &Value,
    target: &str,
    second_reviewer: bool,
) -> Result<(), Value> {
    let Some(branch) = args["branch"].as_str() else {
        return Ok(());
    };
    let task_id_val = args["task_id"].as_str().unwrap_or("");
    if dispatch_should_skip_auto_bind(args) {
        tracing::info!(
            %target, %branch, task_id = %task_id_val,
            "dispatch_auto_bind_lease skipped (bind: false)"
        );
        return Ok(());
    }
    let next_after_ci =
        crate::daemon::ci_watch::watch_state::normalize_next_after_ci(&args["next_after_ci"]);
    dispatch_hook::dispatch_auto_bind_lease_with_source_and_chain(
        home,
        target,
        task_id_val,
        branch,
        args["repository"].as_str(),
        None,
        &next_after_ci,
        if second_reviewer { Some("dual") } else { None },
        true,
    )
    .map(|_| ())
    .map_err(|e| json!({"ok": false, "error": format!("dispatch rejected: {e}")}))
}

/// Phase 5 — optional auto-create board task after rejectable checks.
fn maybe_auto_create_task(
    home: &Path,
    args: &Value,
    sender: &Sender,
    target: &str,
    plan_ack_required: u64,
) -> (Option<String>, Option<String>) {
    if !args["task_id"].as_str().unwrap_or("").is_empty() || *sender == target {
        return (args["task_id"].as_str().map(String::from), None);
    }
    let auto_title = args["message"]
        .as_str()
        .or_else(|| args["task"].as_str())
        .unwrap_or("(untitled dispatch)")
        .chars()
        .take(80)
        .collect::<String>();
    let target_project = crate::tasks::resolve_target_project(home, target);
    let create_args = json!({
        "action": "create",
        "title": auto_title,
        "assignee": target,
        "branch": args["branch"].as_str(),
        "priority": "normal",
        "project": target_project,
        "plan_ack_required": plan_ack_required,
        "plan_ack_reason": args["plan_ack_reason"].as_str(),
    });
    let task_result = crate::tasks::handle(home, sender.as_str(), &create_args);
    match task_result["id"].as_str() {
        Some(id) => {
            crate::daemon::task_progress::touch(
                home,
                id,
                crate::daemon::task_progress::ProgressSource::Broadcast,
            );
            (Some(id.to_string()), Some(id.to_string()))
        }
        None => (None, None),
    }
}

/// Phase 6 — SEND via API with envelope fallback.
fn deliver_delegate(
    home: &Path,
    sender: &Sender,
    target: &str,
    msg: &str,
    task: &str,
    task_id_str: Option<&str>,
    force_meta_json: Option<Value>,
    args: &Value,
) -> Value {
    let env = SendEnvelope {
        from: sender.as_str().to_string(),
        target: target.to_string(),
        text: msg.to_string(),
        kind: Some("task".to_string()),
        thread_id: args["thread_id"].as_str().map(String::from),
        parent_id: args["parent_id"].as_str().map(String::from),
        task_id: task_id_str.map(String::from),
        force_meta: force_meta_json,
        provenance: Some(json!({ "from": sender.as_str(), "task": task })),
        branch: args["branch"].as_str().map(String::from),
        ..SendEnvelope::directives_from_args(args)
    };
    match crate::api::call(
        home,
        &json!({
            "request_id": uuid::Uuid::new_v4().to_string(),
            "method": crate::api::method::SEND,
            "params": env.to_send_params(),
        }),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => json!({"target": target}),
        Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("send failed")}),
        Err(e) => {
            let inbox_msg = env.to_inbox_message();
            crate::agent_ops::fallback_deliver(home, sender.as_str(), target, msg, inbox_msg, &e)
        }
    }
}

/// Phase 7 — post-success tracking / UX / auto_created_task_id.
fn track_delegate_success(
    home: &Path,
    args: &Value,
    sender: &Sender,
    target: &str,
    task: &str,
    task_id_str: Option<&str>,
    auto_created_task_id: Option<String>,
    mut result: Value,
) -> Value {
    if is_ok_result(&result) {
        let task_id = task_id_str.map(str::to_string);
        let status = if args["no_report_expected"].as_bool().unwrap_or(false) {
            "no_report_expected"
        } else {
            "pending"
        };
        crate::dispatch_tracking::track_dispatch(
            home,
            crate::dispatch_tracking::DispatchEntry {
                task_id: task_id.clone(),
                from: sender.as_str().to_string(),
                to: target.to_string(),
                from_id: crate::agent::resolve_instance(home, sender.as_str())
                    .ok()
                    .map(|(id, _)| id.full()),
                to_id: crate::agent::resolve_instance(home, target)
                    .ok()
                    .map(|(id, _)| id.full()),
                delegated_at: chrono::Utc::now().to_rfc3339(),
                status: status.to_string(),
            },
        );
        if let Some(branch) = args["branch"].as_str() {
            tracing::info!(
                target = %target,
                branch = %branch,
                task_id = ?task_id_str,
                "delegate_task branch hint — implementer should work on this branch"
            );
        }
        ux_sink_registry().emit(&UxEvent::Fleet(FleetEvent::DelegateTask {
            from: sender.as_str().to_string(),
            to: target.to_string(),
            summary: task.to_string(),
            task_id,
        }));
    }
    if let Some(tid) = auto_created_task_id {
        if let Some(obj) = result.as_object_mut() {
            obj.insert("auto_created_task_id".into(), json!(tid));
        }
    }
    result
}

/// Ordered choreography for MCP `delegate_task` / unified send kind=task.
pub(crate) fn handle_delegate_task(home: &Path, args: &Value, sender: &Option<Sender>) -> Value {
    let resolved = match resolve_delegate(home, args, sender) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let target = resolved.resolved_target.as_str();
    let sender = resolved.sender;
    let task = resolved.task;

    // Phase 2 — pre-send gates (busy / branch-dedup / enrich / second-reviewer / …)
    let checks = match comms_gates::run_dispatch_pre_checks(home, sender, args, target, task) {
        Ok(c) => c,
        Err(rejection) => return rejection,
    };

    let composed = compose_delegate_message(task, args, &checks);
    if let Err(e) = maybe_auto_bind_lease(home, args, target, composed.second_reviewer) {
        return e;
    }

    let (effective_task_id, auto_created_task_id) =
        maybe_auto_create_task(home, args, sender, target, composed.plan_ack_required);
    let task_id_str = effective_task_id.as_deref();
    let mut msg = composed.msg;
    if let Some(tid) = task_id_str {
        msg.push_str(&format!(" (task id: {tid})"));
    }

    let result = deliver_delegate(
        home,
        sender,
        target,
        &msg,
        task,
        task_id_str,
        composed.force_meta_json,
        args,
    );
    track_delegate_success(
        home,
        args,
        sender,
        target,
        task,
        task_id_str,
        auto_created_task_id,
        result,
    )
}
