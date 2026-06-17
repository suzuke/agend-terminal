use crate::channel::sink_registry::registry as ux_sink_registry;
use crate::channel::ux_event::{FleetEvent, UxEvent};
use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

pub(super) fn handle_post_decision(
    home: &Path,
    args: &Value,
    instance_name: &str,
    sender: &Option<Sender>,
) -> Value {
    let result = crate::decisions::post(home, instance_name, args);
    if let (Some(id), Some(title), Some(sender)) = (
        result.get("id").and_then(|v| v.as_str()),
        args["title"].as_str(),
        sender.as_ref(),
    ) {
        ux_sink_registry().emit(&UxEvent::Fleet(FleetEvent::PostDecision {
            by: sender.as_str().to_string(),
            title: title.to_string(),
            decision_id: id.to_string(),
        }));
    }
    result
}

pub(super) fn handle_list_decisions(home: &Path, args: &Value) -> Value {
    crate::decisions::list(home, args)
}

pub(super) fn handle_update_decision(home: &Path, args: &Value, instance_name: &str) -> Value {
    crate::decisions::update(home, instance_name, args)
}

/// #2305: record an operator's answer to a pending decision, then notify the
/// decision author (e.g. the lead who posted the question) so they unblock. The
/// answerer is the calling identity (the TUI overlay passes `"operator"`; an
/// agent recording on the operator's behalf is attributed by its own name).
pub(super) fn handle_answer_decision(
    home: &Path,
    args: &Value,
    instance_name: &str,
    sender: &Option<Sender>,
) -> Value {
    let answerer = sender
        .as_ref()
        .map(|s| s.as_str().to_string())
        .unwrap_or_else(|| instance_name.to_string());
    let result = crate::decisions::answer(home, &answerer, args);

    // On success the result carries the decision author — notify them via the
    // inbox with an idle-hint wake so a blocked author resumes. Skipped on error
    // (no "author" field) and best-effort (a notify failure never fails the
    // answer write). NOT under any registry lock here (handler context), so the
    // #1492 self-IPC guard in enqueue_with_idle_hint is satisfied.
    if let (Some(author), Some(id)) = (
        result.get("author").and_then(|v| v.as_str()),
        result.get("id").and_then(|v| v.as_str()),
    ) {
        let ans = result.get("answer").and_then(|v| v.as_str()).unwrap_or("");
        let body = format!("[decision-answered] {id}: {ans}\n(answered by {answerer})");
        if let Err(e) = crate::inbox::notify_system(
            home,
            author,
            "system:decision",
            "update",
            body,
            Some(id),
            None,
        ) {
            tracing::debug!(author, %e, "#2305: decision-answered author notify failed");
        }
    }
    result
}

pub(super) fn handle_task(home: &Path, args: &Value, instance_name: &str) -> Value {
    crate::tasks::handle(home, instance_name, args)
}

pub(super) fn handle_task_sweep_config(home: &Path, args: &Value) -> Value {
    crate::daemon::task_sweep::handle_task_sweep_config(home, args)
}

pub(super) fn handle_create_team(home: &Path, args: &Value) -> Value {
    match crate::api::call(
        home,
        &json!({"method": crate::api::method::CREATE_TEAM, "params": args}),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            resp.get("result").cloned().unwrap_or_default()
        }
        Ok(resp) => {
            json!({"error": resp["error"].as_str().unwrap_or("create_team failed")})
        }
        Err(_) => crate::teams::create(home, args),
    }
}

pub(super) fn handle_delete_team(home: &Path, args: &Value) -> Value {
    crate::teams::delete(home, args)
}

pub(super) fn handle_list_teams(home: &Path) -> Value {
    crate::teams::list(home)
}

pub(super) fn handle_update_team(home: &Path, args: &Value) -> Value {
    match crate::api::call(
        home,
        &json!({"method": crate::api::method::UPDATE_TEAM, "params": args}),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => resp["result"].clone(),
        Ok(resp) => {
            json!({"error": resp["error"].as_str().unwrap_or("update_team failed")})
        }
        Err(_) => crate::teams::update(home, args),
    }
}
