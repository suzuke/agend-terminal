use crate::channel::sink_registry::registry as ux_sink_registry;
use crate::channel::ux_event::{FleetEvent, UxEvent};
use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

use super::dispatch::RuntimeContext;
use super::{
    comms_gates::{
        enforce_send_invariants, record_triaged_if_present, validate_request_kind,
        validate_selector_exclusivity, validate_triaged,
    },
    err_needs_identity, is_ok_result,
};

// W2.2: delegate_task phase pipeline (resolve→validate→compose→lease→create→send→track).
#[path = "comms_delegate/mod.rs"]
mod comms_delegate;
pub(crate) use comms_delegate::handle_delegate_task;
// #6: re-export so ci/review_workspace_tests can drive bind rejection tests.
// #2782 slice 1: unconditional (not cfg(test)) — the `revoke_review_assignment`
// MCP tool dispatch adapter (`handlers::dispatch::dispatch_revoke_review_assignment`)
// calls `review_assignment::handle_revoke_review_assignment` in production too.
pub(crate) use comms_delegate::review_assignment;
// p0c_tests (cfg test child) pin `super::dispatch_should_skip_auto_bind`.
#[cfg(test)]
pub(super) use comms_delegate::dispatch_should_skip_auto_bind;

/// Sprint 30: unified `send` handler. Routes to existing handlers based on
/// `request_kind` or infers from args (targets/team → broadcast, task field
/// → delegate, summary → report, question → query, default → send_to).
pub(super) fn handle_unified_send(
    home: &Path,
    args: &Value,
    sender: &Option<Sender>,
    runtime: Option<&RuntimeContext>,
) -> Value {
    let mut args = args.clone();
    if let Some(err) = validate_selector_exclusivity(&args) {
        return err;
    }
    if let Some(err) = enforce_send_invariants(home, &args, sender) {
        return err;
    }
    if let Some(err) = validate_request_kind(&args) {
        return err;
    }
    // Broadcast mode: instances/team present (tags-only rejected above)
    if args.get("instances").is_some() || args.get("team").is_some() {
        return handle_broadcast(home, &args, sender, runtime);
    }

    fn lift_message(args: &mut Value, dst: &str) {
        if args.get(dst).is_none() {
            if let Some(msg) = args.get("message").cloned() {
                args[dst] = msg;
            }
        }
    }
    match args["request_kind"].as_str().unwrap_or("") {
        "task" => {
            lift_message(&mut args, "task");
            handle_delegate_task(home, &args, sender, runtime)
        }
        "report" => {
            lift_message(&mut args, "summary");
            handle_report_result(home, &args, sender, runtime)
        }
        "query" => {
            lift_message(&mut args, "question");
            handle_request_information(home, &args, sender, runtime)
        }
        _ => handle_send_to_instance(home, &args, "send", sender, runtime),
    }
}

pub(super) fn handle_send_to_instance(
    home: &Path,
    args: &Value,
    tool: &str,
    sender: &Option<Sender>,
    runtime: Option<&RuntimeContext>,
) -> Value {
    let Some(sender) = sender.as_ref() else {
        return err_needs_identity(tool);
    };
    let target = match super::require_instance(args) {
        Ok(t) => t,
        Err(e) => return e,
    };
    crate::validate_name_or_err!(target);
    if *sender == target {
        return json!({"error": "cannot send to self — use a different instance"});
    }
    let triaged = match validate_triaged(args) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let text = match args["message"].as_str() {
        Some(t) if !t.is_empty() => t,
        _ => return json!({"error": "missing or empty 'message'"}),
    };
    let kind = args["request_kind"]
        .as_str()
        .or_else(|| args["kind"].as_str());
    let parent_id = args["parent_id"].as_str();

    let req = send_request_from_args(sender.as_str(), target, text, kind, args);
    let result = if let Some(rt) = runtime {
        match crate::agent_ops::messaging::execute_send(home, &rt.registry, req) {
            crate::agent_ops::messaging::SendOutcome::Success { delivery_mode, .. } => {
                json!({"target": target, "delivery_mode": delivery_mode})
            }
            crate::agent_ops::messaging::SendOutcome::Error { error, .. } => {
                json!({"error": error})
            }
        }
    } else {
        crate::agent_ops::send_via_api_bridge(home, &req)
    };
    let mut result = result;
    if kind == Some("report") && parent_id.is_none() {
        attach_report_parent_warning(&mut result);
    }
    if is_ok_result(&result) {
        record_triaged_if_present(home, sender.as_str(), triaged);
    }
    result
}

fn send_request_from_args(
    from: &str,
    target: &str,
    text: &str,
    kind: Option<&str>,
    args: &Value,
) -> crate::agent_ops::messaging::SendRequest {
    crate::agent_ops::messaging::SendRequest {
        from: from.to_string(),
        target: target.to_string(),
        text: text.to_string(),
        kind: kind.map(String::from),
        thread_id: args["thread_id"].as_str().map(String::from),
        parent_id: args["parent_id"].as_str().map(String::from),
        correlation_id: args["correlation_id"].as_str().map(String::from),
        reviewed_head: args["reviewed_head"].as_str().map(String::from),
        report_purpose: args["report_purpose"].as_str().map(String::from),
        code_review: args.get("code_review").filter(|v| !v.is_null()).cloned(),
        eta_minutes: args["eta_minutes"].as_u64(),
        reporting_cadence: args["reporting_cadence"].as_str().map(String::from),
        worktree_binding_required: args["worktree_binding_required"].as_bool(),
        expect_reply_within_secs: args["expect_reply_within_secs"].as_i64(),
        terminal: args["terminal"].as_bool(),
        no_report_expected: args["no_report_expected"].as_bool(),
        delivery_nonce: args["delivery_nonce"].as_str().map(String::from),
        task_id: args["task_id"].as_str().map(String::from),
        force_meta: args.get("force_meta").filter(|v| !v.is_null()).cloned(),
        provenance: args.get("provenance").filter(|v| !v.is_null()).cloned(),
        branch: args["branch"].as_str().map(String::from),
        broadcast_context: args
            .get("broadcast_context")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        priority: args["priority"].as_str().map(String::from),
    }
}

/// #2050 simplify PR-B (⑫): attach the "parent_id recommended" warning to a
/// result object (no-op when `result` isn't a JSON object). Only the shared
/// insert is deduped — each CALLER keeps its own guard (the send path gates on
/// `kind == Some("report") && parent_id.is_none()`, the report path on
/// `args["parent_id"].as_str().is_none()`), so behavior is byte-identical.
fn attach_report_parent_warning(result: &mut Value) {
    if let Some(obj) = result.as_object_mut() {
        obj.insert(
            "warning".to_string(),
            json!("parent_id recommended for report kind; will be required in future version"),
        );
    }
}

pub(super) fn handle_report_result(
    home: &Path,
    args: &Value,
    sender: &Option<Sender>,
    runtime: Option<&RuntimeContext>,
) -> Value {
    let Some(sender) = sender.as_ref() else {
        return err_needs_identity("report_result");
    };
    let target = match super::require_instance(args) {
        Ok(t) => t,
        Err(e) => return e,
    };
    crate::validate_name_or_err!(target);
    let summary = match args["summary"].as_str() {
        Some(s) => s,
        None => return json!({"error": "missing 'summary'"}),
    };
    let triaged = match validate_triaged(args) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let msg = comms_inbox::build_report_text(
        summary,
        args["correlation_id"].as_str(),
        args["artifacts"].as_str(),
    );
    let result = {
        let is_code_review = args["report_purpose"].as_str() == Some("code_review");

        let scan_body = super::comms_gates::report_scan_body(summary, args["artifacts"].as_str());
        if is_code_review {
            let Some(verdict) = super::comms_gates::detect_verdict(summary) else {
                return json!({"error": "code_review text must begin with VERIFIED, REJECTED, or UNVERIFIED"});
            };
            if let Err(e) = super::comms_gates::check_evidence_gate(&scan_body, verdict) {
                return json!({"error": e});
            }

            super::comms_gates::cross_check_and_log(
                home,
                sender.as_str(),
                summary,
                &scan_body,
                verdict,
            );
        }

        let req = send_request_from_args(sender.as_str(), target, &msg, Some("report"), args);
        if let Some(rt) = runtime {
            match crate::agent_ops::messaging::execute_send(home, &rt.registry, req) {
                crate::agent_ops::messaging::SendOutcome::Success { delivery_mode, .. } => {
                    json!({"target": target, "delivery_mode": delivery_mode})
                }
                crate::agent_ops::messaging::SendOutcome::Error { error, .. } => {
                    json!({"error": error})
                }
            }
        } else {
            crate::agent_ops::send_via_api_bridge(home, &req)
        }
    };
    let mut result = result;
    if args["parent_id"].as_str().is_none() {
        attach_report_parent_warning(&mut result);
    }
    if is_ok_result(&result) {
        let cid = args["correlation_id"].as_str();
        crate::dispatch_tracking::mark_completed(home, cid, sender.as_str());
        if let Some(cid) = cid.filter(|s| !s.is_empty()) {
            let settled = crate::inbox::ack_by_correlation(home, sender.as_str(), cid);
            if let Some(obj) = result.as_object_mut() {
                obj.insert("inbox_settled".to_string(), json!(settled));
            }
        }
        record_triaged_if_present(home, sender.as_str(), triaged);
        // for binding lifecycle. Sprint 54 candidates: explicit `task_done`
        // flag in report envelope, or lifecycle rework with auto
        // release_full on that explicit signal.
        let task_id = args["correlation_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        ux_sink_registry().emit(&UxEvent::Fleet(FleetEvent::ReportResult {
            from: sender.as_str().to_string(),
            to: target.to_string(),
            summary: summary.to_string(),
            task_id,
        }));
    }
    // task66: generic correlated reports no longer write assignment evidence.
    // The validated receipt retained by PR state is the sole evidence source.
    result
}

pub(super) fn handle_request_information(
    home: &Path,
    args: &Value,
    sender: &Option<Sender>,
    runtime: Option<&RuntimeContext>,
) -> Value {
    let Some(sender) = sender.as_ref() else {
        return err_needs_identity("request_information");
    };
    let target = match super::require_instance(args) {
        Ok(t) => t,
        Err(e) => return e,
    };
    crate::validate_name_or_err!(target);
    let question = match args["question"].as_str() {
        Some(q) => q,
        None => return json!({"error": "missing 'question'"}),
    };
    let mut msg = format!("[request_information] {question}");
    if let Some(ctx) = args["context"].as_str() {
        msg.push_str(&format!("\n\nContext: {ctx}"));
    }
    let req = send_request_from_args(sender.as_str(), target, &msg, Some("query"), args);
    if let Some(rt) = runtime {
        match crate::agent_ops::messaging::execute_send(home, &rt.registry, req) {
            crate::agent_ops::messaging::SendOutcome::Success { delivery_mode, .. } => {
                json!({"target": target, "delivery_mode": delivery_mode})
            }
            crate::agent_ops::messaging::SendOutcome::Error { error, .. } => {
                json!({"error": error})
            }
        }
    } else {
        crate::agent_ops::send_via_api_bridge(home, &req)
    }
}

pub(super) fn handle_broadcast(
    home: &Path,
    args: &Value,
    sender: &Option<Sender>,
    runtime: Option<&RuntimeContext>,
) -> Value {
    let Some(sender) = sender.as_ref() else {
        return err_needs_identity("broadcast");
    };
    if let Some(err) = validate_request_kind(args) {
        return err;
    }
    let message = match args["message"].as_str() {
        Some(m) => m,
        None => return json!({"error": "missing 'message'"}),
    };
    if let Some(err) = validate_selector_exclusivity(args) {
        return err;
    }
    let team_name = args["team"].as_str().map(String::from);
    let targets: Vec<String> = if let Some(team) = team_name.as_deref() {
        crate::teams::get_members(home, team)
    } else if let Some(t) = args["instances"].as_array() {
        t.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    } else {
        return json!({"error": "no valid recipient selector — specify instances or team", "code": "missing_selector"});
    };
    let targets: Vec<String> = targets
        .into_iter()
        .filter(|t| *sender != t.as_str())
        .collect();
    let kind = args["request_kind"].as_str().unwrap_or("update");
    let broadcast_ctx = crate::inbox::BroadcastContext {
        team: team_name,
        targets: targets.clone(),
        count: targets.len(),
    };
    let mut sent = Vec::new();
    let mut failed: Vec<Value> = Vec::new();
    for target in &targets {
        let mut req = send_request_from_args(sender.as_str(), target, message, Some(kind), args);
        req.broadcast_context = Some(broadcast_ctx.clone());
        let result = if let Some(rt) = runtime {
            match crate::agent_ops::messaging::execute_send(home, &rt.registry, req) {
                crate::agent_ops::messaging::SendOutcome::Success { delivery_mode, .. } => {
                    json!({"target": target, "delivery_mode": delivery_mode})
                }
                crate::agent_ops::messaging::SendOutcome::Error { error, .. } => {
                    json!({"error": error})
                }
            }
        } else {
            crate::agent_ops::send_via_api_bridge(home, &req)
        };
        if result.get("error").is_some() {
            failed.push(json!({"target": target, "error": result["error"]}));
        } else {
            sent.push(target.clone());
        }
    }
    if !sent.is_empty() {
        ux_sink_registry().emit(&UxEvent::Fleet(FleetEvent::Broadcast {
            from: sender.as_str().to_string(),
            recipients: sent.clone(),
            summary: message.to_string(),
        }));
    }
    let mut resp = json!({"sent_to": sent, "count": sent.len()});
    if !failed.is_empty() {
        resp["failed"] = json!(failed);
    }
    resp
}

pub(super) fn handle_inbox(home: &Path, instance_name: &str) -> Value {
    let messages = crate::inbox::drain(home, instance_name);
    if !messages.is_empty() {
        let meta_path = crate::agent_ops::metadata_path_resolved(home, instance_name);
        let mut processed_msg_ids: Vec<String> = Vec::new();
        if let Some(meta) = std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|c| serde_json::from_str::<Value>(&c).ok())
        {
            if let Some(arr) = meta["pending_pickup_ids"].as_array() {
                use crate::channel::binding::BindingRef;
                use crate::channel::event::MsgRef;
                use crate::channel::ux_event::UxEvent;
                for entry in arr {
                    let kind_str = entry["kind"].as_str().unwrap_or("");
                    let msg_id = entry["msg_id"].as_str().unwrap_or("");
                    let kind: &'static str = match kind_str {
                        "telegram" => "telegram",
                        "discord" => "discord",
                        "slack" => "slack",
                        _ => continue,
                    };
                    if msg_id.is_empty() {
                        continue;
                    }
                    processed_msg_ids.push(msg_id.to_string());
                    let origin_msg = MsgRef {
                        binding: BindingRef::new(kind, Some(instance_name.to_string()), ()),
                        id: msg_id.to_string(),
                    };
                    crate::channel::sink_registry::registry().emit(&UxEvent::AgentPickedUp {
                        origin_msg,
                        agent: instance_name.to_string(),
                    });
                }
            }
        }
        if !processed_msg_ids.is_empty() {
            // CR-2026-06-14 (concurrency): filter the processed pickup ids out of
            // `pending_pickup_ids` under the metadata flock (atomic read-modify-
            // write). The prior code re-read the file UNLOCKED, filtered, then
            // wrote the remainder via save_metadata — a pickup id appended between
            // the unlocked re-read and the write was clobbered (lost update).
            // Filtering inside the locked closure reads the CURRENT on-disk set,
            // so a concurrently-appended id is preserved.
            crate::agent_ops::update_metadata(
                home,
                instance_name,
                "pending_pickup_ids",
                |current| {
                    let remaining: Vec<Value> = current
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|e| {
                            !processed_msg_ids
                                .contains(&e["msg_id"].as_str().unwrap_or("").to_string())
                        })
                        .collect();
                    json!(remaining)
                },
            );
        }
    }
    json!({"messages": messages})
}

// #1286: inbox describe handlers extracted to stay under file_size_invariant.
#[path = "comms_inbox.rs"]
mod comms_inbox;
#[cfg(test)]
pub(crate) use comms_inbox::build_report_text;
pub(super) use comms_inbox::{
    handle_describe_message, handle_describe_thread, handle_inbox_ack, handle_inbox_clear,
};

// Sprint 55 P0-C — helper tests in sibling file (file_size_invariant ceiling).
// `dispatch_should_skip_auto_bind` is re-exported from `comms_delegate` above.
#[cfg(test)]
#[path = "comms_p0c_tests.rs"]
mod p0c_tests;
