//! Team handlers: UPDATE_TEAM (Slice C1), CREATE_TEAM (Slice C2).

use super::HandlerCtx;
use crate::api::ApiEvent;
use serde_json::{json, Value};

pub(crate) fn handle_update_team(params: &Value, ctx: &HandlerCtx) -> Value {
    let team_name = match params["name"].as_str() {
        Some(n) => n.to_string(),
        None => return json!({"ok": false, "error": "missing name"}),
    };
    let before = crate::teams::get_members(ctx.home, &team_name);
    let result = crate::teams::update(ctx.home, params);
    let after = crate::teams::get_members(ctx.home, &team_name);
    let before_set: std::collections::HashSet<&String> = before.iter().collect();
    let after_set: std::collections::HashSet<&String> = after.iter().collect();
    let added: Vec<String> = after
        .iter()
        .filter(|m| !before_set.contains(m))
        .cloned()
        .collect();
    let removed: Vec<String> = before
        .iter()
        .filter(|m| !after_set.contains(m))
        .cloned()
        .collect();
    if let Some(n) = ctx.notifier {
        if !added.is_empty() || !removed.is_empty() {
            tracing::info!(team = %team_name, added = ?added, removed = ?removed, "UPDATE_TEAM emitting TeamMembersChanged");
            n.notify(ApiEvent::TeamMembersChanged {
                name: team_name.clone(),
                added,
                removed,
            });
        }
    }
    json!({"ok": true, "result": result})
}
