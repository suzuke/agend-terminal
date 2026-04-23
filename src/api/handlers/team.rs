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
    // Snapshot the pre-mutation roster so the TUI event carries the
    // *effective* diff (noop adds like re-adding an existing member
    // must not trigger a pane move).
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

#[allow(clippy::too_many_lines)]
pub(crate) fn handle_create_team(params: &Value, ctx: &HandlerCtx) -> Value {
    let team_name = match params["name"].as_str() {
        Some(n) => n,
        None => return json!({"ok": false, "error": "missing name"}),
    };
    // `backends: [..]` — per-member backend (heterogeneous team).
    // Falls back to repeating `backend` `count` times when absent.
    let per_member_backends: Vec<String> = if let Some(arr) = params["backends"].as_array() {
        arr.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    } else {
        let count = params["count"].as_u64().unwrap_or(0) as usize;
        let backend = params["backend"].as_str().unwrap_or("claude").to_string();
        vec![backend; count]
    };
    let count = per_member_backends.len();
    tracing::info!(
        team = team_name,
        count,
        backends = ?per_member_backends,
        "CREATE_TEAM begin"
    );

    let mut spawned: Vec<(String, String)> = Vec::new(); // (name, backend)
    let mut failed: Vec<String> = Vec::new();
    let size = crossterm::terminal::size().unwrap_or((120, 40));
    for (i, backend) in per_member_backends.iter().enumerate() {
        let inst_name = format!("{team_name}-{}", i + 1);
        // Dedup: see SPAWN handler note. Re-creating a team with an
        // existing name would otherwise overwrite the registry entry
        // and orphan the previous tab's PTY subscription.
        if crate::agent::lock_registry(ctx.registry).contains_key(&inst_name) {
            tracing::warn!(team = team_name, member = %inst_name, "CREATE_TEAM skip: name already exists");
            failed.push(format!("{inst_name}: agent already exists"));
            continue;
        }
        let work_dir = ctx.home.join("workspace").join(&inst_name);
        match crate::api::spawn_one(
            ctx.home,
            ctx.registry,
            &inst_name,
            backend,
            &[],
            crate::backend::SpawnMode::Fresh,
            &work_dir,
            size,
        ) {
            Ok(()) => {
                tracing::info!(team = team_name, member = %inst_name, backend = %backend, "CREATE_TEAM spawn ok");
                spawned.push((inst_name, backend.clone()));
            }
            Err(e) => {
                tracing::warn!(team = team_name, member = %inst_name, backend = %backend, error = %e, "CREATE_TEAM spawn failed");
                failed.push(format!("{inst_name}: {e}"));
            }
        }
    }
    tracing::info!(
        team = team_name,
        spawned = spawned.len(),
        failed = failed.len(),
        "CREATE_TEAM spawn phase done"
    );
    if count > 0 && spawned.is_empty() {
        return json!({"ok": false, "error": format!("all {} spawns failed: {}", count, failed.join("; "))});
    }

    let existing: Vec<String> = params["members"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let spawned_names: Vec<String> = spawned.iter().map(|(n, _)| n.clone()).collect();
    let all_members: Vec<String> = existing
        .into_iter()
        .chain(spawned_names.iter().cloned())
        .collect();

    if !spawned.is_empty() {
        let entries: Vec<(String, crate::fleet::InstanceYamlEntry)> = spawned
            .iter()
            .map(|(name, be)| {
                (
                    name.clone(),
                    crate::fleet::InstanceYamlEntry {
                        backend: Some(be.clone()),
                        working_directory: Some(
                            ctx.home.join("workspace").join(name).display().to_string(),
                        ),
                        role: None,
                    },
                )
            })
            .collect();
        let refs: Vec<(&str, &crate::fleet::InstanceYamlEntry)> =
            entries.iter().map(|(n, e)| (n.as_str(), e)).collect();
        if let Err(e) = crate::fleet::add_instances_to_yaml(ctx.home, &refs) {
            tracing::warn!(error = %e, "failed to persist team to fleet.yaml");
        }
    }

    let team_params =
        json!({"name": team_name, "members": all_members, "description": params["description"]});
    let result = crate::teams::create(ctx.home, &team_params);

    if let Some(n) = ctx.notifier {
        if !all_members.is_empty() {
            tracing::info!(team = team_name, members = ?all_members, "CREATE_TEAM emitting TeamCreated");
            n.notify(ApiEvent::TeamCreated {
                name: team_name.to_string(),
                members: all_members.clone(),
            });
        }
    }
    let mut resp = json!({"ok": true, "result": result, "spawned": &spawned_names});
    if !failed.is_empty() {
        resp["failed"] = json!(failed);
    }
    resp
}
