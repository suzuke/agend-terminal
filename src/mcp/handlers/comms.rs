use crate::agent_ops::{list_agents, send_to};
use crate::channel::sink_registry::registry as ux_sink_registry;
use crate::channel::ux_event::{FleetEvent, UxEvent};
use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

use super::send_envelope::SendEnvelope;
use super::{
    comms_gates::{
        enforce_send_invariants, record_triaged_if_present, validate_request_kind, validate_triaged,
    },
    err_needs_identity, is_ok_result,
};

// W2.2: delegate_task phase pipeline (resolve→validate→compose→lease→create→send→track).
#[path = "comms_delegate/mod.rs"]
mod comms_delegate;
pub(crate) use comms_delegate::handle_delegate_task;
// p0c_tests (cfg test child) pin `super::dispatch_should_skip_auto_bind`.
#[cfg(test)]
pub(super) use comms_delegate::dispatch_should_skip_auto_bind;

/// Sprint 30: unified `send` handler. Routes to existing handlers based on
/// `request_kind` or infers from args (targets/team → broadcast, task field
/// → delegate, summary → report, question → query, default → send_to).
pub(super) fn handle_unified_send(home: &Path, args: &Value, sender: &Option<Sender>) -> Value {
    let mut args = args.clone();
    if let Some(err) = enforce_send_invariants(home, &args, sender) {
        return err;
    }
    if let Some(err) = validate_request_kind(&args) {
        return err;
    }
    // Broadcast mode: targets/team/tags present
    if args.get("instances").is_some() || args.get("team").is_some() || args.get("tags").is_some() {
        return handle_broadcast(home, &args, sender);
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
            handle_delegate_task(home, &args, sender)
        }
        "report" => {
            lift_message(&mut args, "summary");
            handle_report_result(home, &args, sender)
        }
        "query" => {
            lift_message(&mut args, "question");
            handle_request_information(home, &args, sender)
        }
        _ => handle_send_to_instance(home, &args, "send", sender),
    }
}

pub(super) fn handle_send_to_instance(
    home: &Path,
    args: &Value,
    tool: &str,
    sender: &Option<Sender>,
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
    // #2537/#2524 P6: validated up front so a malformed `triaged` rejects
    // before a message goes out.
    let triaged = match validate_triaged(args) {
        Ok(t) => t,
        Err(e) => return e,
    };
    // #1602: `message` only — the legacy `text` alias is dropped (reply's
    // text→message rename removed the last schema that declared `text`, and the
    // dispatch validator now rejects a message-less `send` before this handler
    // runs anyway). `send`/`reply`/`schedule` are all `message` now.
    let text = match args["message"].as_str() {
        Some(t) if !t.is_empty() => t,
        _ => return json!({"error": "missing or empty 'message'"}),
    };
    let kind = args["request_kind"]
        .as_str()
        .or_else(|| args["kind"].as_str());
    let thread_id = args["thread_id"].as_str();
    let parent_id = args["parent_id"].as_str();

    // #1024 (closes #1002 ROOT 2): `reviewed_head` MUST be forwarded; the
    // SendEnvelope projection carries it (+ the rest of the directive set) into
    // BOTH `params` and the fallback, so they cannot drift (smells#2).
    let env = SendEnvelope {
        from: sender.as_str().to_string(),
        target: target.to_string(),
        text: text.to_string(),
        // MED-5: carry `kind` (else a fallback `kind=query` lands kind=None and
        // `inbox clear` swallows it) — now via the shared envelope.
        kind: kind.map(String::from),
        thread_id: thread_id.map(String::from),
        parent_id: parent_id.map(String::from),
        ..SendEnvelope::directives_from_args(args)
    };
    let result = match crate::api::call(
        home,
        &json!({
            "request_id": uuid::Uuid::new_v4().to_string(),
            "method": crate::api::method::SEND,
            "params": env.to_send_params(),
        }),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            let dm = resp["delivery_mode"].as_str().unwrap_or("pty");
            json!({"target": target, "delivery_mode": dm})
        }
        Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("send failed")}),
        Err(e) => {
            // Centralised fallback (Sprint 40 T-7 B4). #1024/#1833 fix: the
            // fallback now carries the SAME directive set as `params` (shared
            // SendEnvelope projection) — it previously dropped reviewed_head +
            // eta/cadence/worktree_binding, a latent verdict-correlation
            // gap when a verdict send hit the API-down path.
            let mut resolved_thread = thread_id.map(String::from);
            let resolved_parent = parent_id.map(String::from);
            if resolved_thread.is_none() {
                if let Some(ref pid) = resolved_parent {
                    if let Some(parent_msg) = crate::inbox::find_message(home, pid) {
                        resolved_thread = parent_msg.thread_id.or_else(|| parent_msg.id.clone());
                    }
                }
            }
            let mut fb = env.clone();
            fb.thread_id = resolved_thread;
            fb.parent_id = resolved_parent;
            let msg = fb.to_inbox_message();
            crate::agent_ops::fallback_deliver(home, sender.as_str(), target, text, msg, &e)
        }
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

pub(super) fn handle_report_result(home: &Path, args: &Value, sender: &Option<Sender>) -> Value {
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
    // #2537/#2524 P6: same pre-send gate as handle_send_to_instance.
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

        // UX-only evidence checks are scoped by the caller's typed purpose. The
        // API sink validates again and is the sole trust boundary; reviewed_head
        // is display-only and never drives an MCP SHA/PR lookup.
        let scan_body = super::comms_gates::report_scan_body(summary, args["artifacts"].as_str());
        if is_code_review {
            let Some(verdict) = super::comms_gates::detect_verdict(summary) else {
                return json!({"error": "code_review text must begin with VERIFIED, REJECTED, or UNVERIFIED"});
            };
            if let Err(e) = super::comms_gates::check_evidence_gate(&scan_body, verdict) {
                return json!({"error": e});
            }

            // #1666 Phase B (WARN-first): cross-check the checkable evidence and
            // LOG (never reject) — measures the false-positive rate. See the fn.
            super::comms_gates::cross_check_and_log(
                home,
                sender.as_str(),
                summary,
                &scan_body,
                verdict,
            );
        }

        // smells#2: report carries {correlation_id, reviewed_head, terminal}
        // (a reply needs no dispatch directives); the shared SendEnvelope reads
        // the directive set from args (the rest stay None → emitted as inert null
        // keys, read as absent) and projects to BOTH params and the fallback.
        let env = SendEnvelope {
            from: sender.as_str().to_string(),
            target: target.to_string(),
            text: msg.clone(),
            kind: Some("report".to_string()),
            thread_id: args["thread_id"].as_str().map(String::from),
            parent_id: args["parent_id"].as_str().map(String::from),
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
                // Centralised fallback (Sprint 40 T-7 B4) — shared SendEnvelope
                // projection keeps the fallback aligned with params.
                let inbox_msg = env.to_inbox_message();
                crate::agent_ops::fallback_deliver(
                    home,
                    sender.as_str(),
                    target,
                    &msg,
                    inbox_msg,
                    &e,
                )
            }
        }
    };
    // Add warning for report kind without parent_id
    let mut result = result;
    if args["parent_id"].as_str().is_none() {
        attach_report_parent_warning(&mut result);
    }
    if is_ok_result(&result) {
        // Mark dispatch as completed so timeout sweep doesn't false-warn.
        let cid = args["correlation_id"].as_str();
        crate::dispatch_tracking::mark_completed(home, cid, sender.as_str());
        // #35896-11 ⑤ (Q2 vet): any kind=report+correlation auto-settles the
        // SENDER's own delivering dispatch row (was gated on ack_inbox=true);
        // sender-scoped via ack_by_correlation (#2647 isolation), ack_inbox now a
        // no-op. Q2 over-settle tradeoff: docs/DESIGN-notify-noise-unified.md.
        if let Some(cid) = cid.filter(|s| !s.is_empty()) {
            let settled = crate::inbox::ack_by_correlation(home, sender.as_str(), cid);
            if let Some(obj) = result.as_object_mut() {
                obj.insert("inbox_settled".to_string(), json!(settled));
            }
        }
        // #2537/#2524 P6 PR-1: best-effort — a ledger write failure doesn't
        // fail the report (it already landed).
        record_triaged_if_present(home, sender.as_str(), triaged);
        // Sprint 53 P0-Y: do NOT auto-unbind here. Every kind=report reply
        // (progress update OR final done) landed in this handler, so the
        // prior auto-unbind tore down the binding on the first progress
        // update — Phase 1 trailer stopped firing on subsequent commits,
        // P0-X release_worktree refused ("no binding"), Phase 4 GC saw
        // zero candidates (unbind doesn't write released_at; only
        // release()/release_full() do), and orphan worktrees accumulated.
        // `release_worktree` MCP tool is now the single source of truth
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
    send_to(home, sender, target, &msg, "query", None)
}

pub(super) fn handle_broadcast(home: &Path, args: &Value, sender: &Option<Sender>) -> Value {
    let Some(sender) = sender.as_ref() else {
        return err_needs_identity("broadcast");
    };
    // Also validated in `handle_unified_send` before routing here — repeated
    // so a direct call to this handler (bypassing the unified dispatch) is
    // still covered.
    if let Some(err) = validate_request_kind(args) {
        return err;
    }
    let message = match args["message"].as_str() {
        Some(m) => m,
        None => return json!({"error": "missing 'message'"}),
    };
    let team_name = args["team"].as_str().map(String::from);
    let targets: Vec<String> = if let Some(team) = team_name.as_deref() {
        crate::teams::get_members(home, team)
    } else if let Some(t) = args["instances"].as_array() {
        t.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    } else {
        list_agents()
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
        let result = send_to(home, sender, target, message, kind, Some(&broadcast_ctx));
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
