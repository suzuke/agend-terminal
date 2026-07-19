use crate::channel::sink_registry::registry as ux_sink_registry;
use crate::channel::ux_event::{FleetEvent, UxEvent};
use crate::identity::Sender;
use crate::mcp::handlers::dispatch::RuntimeContext;
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

/// #2454 Slice 13: thin MCP adapter — delegates to `team_ops::create`
/// via the in-process RuntimeContext when available, or returns a
/// structured transport failure when the runtime is absent.
pub(super) fn handle_create_team(
    home: &Path,
    args: &Value,
    runtime: Option<&super::dispatch::RuntimeContext>,
) -> Value {
    let Some(rt) = runtime else {
        return json!({
            "error": "runtime unavailable: team creation requires an in-process runtime"
        });
    };
    let name = match args["name"].as_str() {
        Some(n) => n.to_string(),
        None => return json!({"error": "missing 'name'"}),
    };
    let existing_members: Vec<String> = args["members"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let accept_from: Vec<String> = args["accept_from"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    crate::team_ops::create(
        home,
        crate::team_ops::CreateTeamRequest {
            name,
            per_member_backends: Vec::new(),
            existing_members,
            topic_binding_mode: None,
            orchestrator: args["orchestrator"].as_str().map(String::from),
            description: args["description"].as_str().map(String::from),
            repository_path: args["repository_path"].as_str().map(String::from),
            project_id: args["project_id"].as_str().map(String::from),
            accept_from,
        },
        &rt.registry,
        rt.notifier.as_deref(),
    )
}

pub(super) fn handle_delete_team(home: &Path, args: &Value) -> Value {
    crate::teams::delete(home, args)
}

pub(super) fn handle_list_teams(home: &Path) -> Value {
    crate::teams::list(home)
}

pub(super) fn handle_update_team(
    home: &Path,
    args: &Value,
    runtime: Option<&RuntimeContext>,
) -> Value {
    let team_name = args["name"].as_str().unwrap_or("");
    let outcome = crate::teams::update_with_diff(home, args);
    if outcome.result.get("error").is_none() {
        if let Some(notifier) = runtime.and_then(|runtime| runtime.notifier.as_ref()) {
            if !outcome.added.is_empty() || !outcome.removed.is_empty() {
                notifier.notify(crate::api::ApiEvent::TeamMembersChanged {
                    name: team_name.to_string(),
                    added: outcome.added.clone(),
                    removed: outcome.removed.clone(),
                });
            }
        }
    }
    outcome.result
}
