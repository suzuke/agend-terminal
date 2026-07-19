//! Messaging handler: SEND.
//!
//! Thin adapter: rejects flat review-field smuggling, projects raw JSON
//! params into a typed `SendRequest`, delegates to the neutral
//! `agent_ops::messaging::execute_send` service, and maps `SendOutcome`
//! back to the API JSON envelope. All business validation (identity,
//! target existence, policy gates, delivery) lives in the neutral service.

use super::HandlerCtx;
#[cfg(test)]
use crate::agent;
use crate::agent_ops::messaging::{self, SendOutcome, SendRequest};
use serde_json::{json, Value};

fn reject_flat_review_smuggling(params: &Value) -> Option<Value> {
    if !crate::review_receipt::has_flat_review_smuggling_fields(params) {
        return None;
    }
    Some(
        json!({"ok": false, "error": "code-review request fields must be inside the typed code_review object"}),
    )
}

pub(crate) fn handle_send(params: &Value, ctx: &HandlerCtx) -> Value {
    if let Some(e) = reject_flat_review_smuggling(params) {
        return e;
    }
    let request = api_params_to_send_request(params);
    match messaging::execute_send(ctx.home, ctx.registry, request) {
        SendOutcome::Success {
            delivery_mode,
            branch_checked_out,
            auto_task_id,
        } => {
            let mut resp = json!({"ok": true, "delivery_mode": delivery_mode});
            if let Some(branch) = branch_checked_out {
                resp["branch_checked_out"] = json!(branch);
            }
            if let Some(ref tid) = auto_task_id {
                resp["task_id"] = json!(tid);
            }
            resp
        }
        SendOutcome::Error {
            error, code, hint, ..
        } => {
            let mut resp = json!({"ok": false, "error": error});
            if let Some(c) = code {
                resp["code"] = json!(c);
            }
            if let Some(h) = hint {
                resp["hint"] = json!(h);
            }
            resp
        }
    }
}

fn api_params_to_send_request(params: &Value) -> SendRequest {
    SendRequest {
        from: params["from"].as_str().unwrap_or("").to_string(),
        target: params["target"].as_str().unwrap_or("").to_string(),
        text: params["text"].as_str().unwrap_or("").to_string(),
        kind: params["kind"].as_str().map(String::from),
        thread_id: params["thread_id"].as_str().map(String::from),
        parent_id: params["parent_id"].as_str().map(String::from),
        correlation_id: params["correlation_id"].as_str().map(String::from),
        reviewed_head: params["reviewed_head"].as_str().map(String::from),
        report_purpose: params["report_purpose"].as_str().map(String::from),
        code_review: params.get("code_review").filter(|v| !v.is_null()).cloned(),
        eta_minutes: params["eta_minutes"].as_u64(),
        reporting_cadence: params["reporting_cadence"].as_str().map(String::from),
        worktree_binding_required: params["worktree_binding_required"].as_bool(),
        expect_reply_within_secs: params["expect_reply_within_secs"].as_i64(),
        terminal: params["terminal"].as_bool(),
        no_report_expected: params["no_report_expected"].as_bool(),
        delivery_nonce: params["delivery_nonce"].as_str().map(String::from),
        task_id: params["task_id"].as_str().map(String::from),
        force_meta: params.get("force_meta").filter(|v| !v.is_null()).cloned(),
        provenance: params.get("provenance").filter(|v| !v.is_null()).cloned(),
        branch: params["branch"].as_str().map(String::from),
        broadcast_context: params
            .get("broadcast_context")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        priority: params["priority"].as_str().map(String::from),
    }
}

// ── Legacy test helpers ──

#[cfg(test)]
pub(crate) use crate::agent_ops::messaging::process_verdicts;

#[cfg(test)]
pub(crate) fn track_dispatch(
    home: &std::path::Path,
    params: &Value,
    from: &str,
    target: &str,
    msg: &crate::inbox::InboxMessage,
) {
    let request = api_params_to_send_request(params);
    messaging::track_dispatch(home, &request, from, target, msg);
}

#[cfg(test)]
fn bridge_verdict_to_review_task(
    home: &std::path::Path,
    reporter: &str,
    msg: &crate::inbox::InboxMessage,
) {
    let Some(receipt) = msg.validated_code_review.as_ref() else {
        return;
    };
    let summary = receipt.summary();
    let task_id = &summary.task_id;
    let _ = crate::daemon::dispatch_idle::mark_resolved(home, task_id, reporter);
    if matches!(
        summary.verdict,
        crate::review_receipt::ReviewVerdict::Verified
    ) {
        let _ = crate::tasks::auto_close::auto_close_on_report(
            home, "report", task_id, reporter, &msg.text, true,
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
