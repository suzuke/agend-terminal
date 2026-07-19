//! #2454 Slice 13: neutral typed CREATE_TEAM service.
//!
//! Shared owner for team creation logic — both the API handler
//! (`api::handlers::team`) and the MCP handler (`mcp::handlers::task`)
//! route through `create()` via thin adapters. The interface is typed
//! (no `HandlerCtx`, no raw `serde_json::Value` parameters).

use serde_json::{json, Value};
use std::path::Path;

/// Typed request for team creation — parsed from raw JSON by each
/// adapter (API and MCP) before calling `create()`.
pub(crate) struct CreateTeamRequest {
    pub name: String,
    pub per_member_backends: Vec<String>,
    pub existing_members: Vec<String>,
    pub topic_binding_mode: Option<String>,
    pub orchestrator: Option<String>,
    pub description: Option<String>,
    pub repository_path: Option<String>,
    pub accept_from: Vec<String>,
}

/// #1964 Bug 1: plan `count` member names for `team` as `<team>-N`.
pub(crate) fn plan_member_names(
    fleet: &crate::fleet::FleetConfig,
    team: &str,
    count: usize,
    taken: impl Fn(&str) -> bool,
) -> Vec<String> {
    let prefix = format!("{team}-");
    let mut next_n: u64 = fleet
        .instances
        .keys()
        .filter_map(|k| k.strip_prefix(&prefix)?.parse::<u64>().ok())
        .max()
        .map_or(1, |m| m + 1);
    let mut names = Vec::with_capacity(count);
    while names.len() < count {
        let candidate = format!("{team}-{next_n}");
        next_n += 1;
        if fleet.instances.contains_key(&candidate) || taken(&candidate) {
            tracing::info!(
                team,
                member = %candidate,
                "CREATE_TEAM: name taken — advancing to the next number (#1964)"
            );
            continue;
        }
        names.push(candidate);
    }
    names
}

/// #991 PR-B: build each planned member's fleet.yaml entry.
pub(crate) fn build_member_entries(
    planned: &[(String, String, std::path::PathBuf)],
    topic_binding_mode: Option<&str>,
) -> Vec<(String, crate::fleet::InstanceYamlEntry)> {
    planned
        .iter()
        .map(|(name, be, wd)| {
            (
                name.clone(),
                crate::fleet::InstanceYamlEntry {
                    backend: Some(be.clone()),
                    working_directory: Some(wd.display().to_string()),
                    topic_binding_mode: topic_binding_mode.map(String::from),
                    ..Default::default()
                },
            )
        })
        .collect()
}

/// Neutral typed CREATE_TEAM entry point.
///
/// Both API and MCP adapters call this after parsing their transport-specific
/// input into a [`CreateTeamRequest`]. The `registry` is the live agent
/// registry (for spawn + UUID resolution); `notifier` emits API lifecycle
/// events (TeamCreated / TeamMembersChanged).
#[allow(clippy::too_many_lines)]
pub(crate) fn create(
    home: &Path,
    request: CreateTeamRequest,
    registry: &crate::agent::AgentRegistry,
    notifier: Option<&dyn crate::api::ApiNotifier>,
) -> Value {
    let team_name = &request.name;
    let count = request.per_member_backends.len();
    tracing::info!(
        team = %team_name,
        count,
        backends = ?request.per_member_backends,
        topic_binding_mode = ?request.topic_binding_mode,
        "CREATE_TEAM begin"
    );

    let fleet_snapshot =
        crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).unwrap_or_default();
    let team_already_exists = fleet_snapshot.teams.contains_key(team_name.as_str());

    // Phase 1 — plan every member's fleet.yaml entry
    let names = plan_member_names(&fleet_snapshot, team_name, count, |c| {
        crate::fleet::resolve_uuid(home, c)
            .is_some_and(|id| crate::agent::lock_registry(registry).contains_key(&id))
    });
    let mut planned: Vec<(String, String, std::path::PathBuf)> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for (inst_name, backend) in names.into_iter().zip(request.per_member_backends.iter()) {
        let work_dir = crate::paths::workspace_dir(home).join(&inst_name);
        planned.push((inst_name, backend.clone(), work_dir));
    }

    if !planned.is_empty() {
        let entries = build_member_entries(&planned, request.topic_binding_mode.as_deref());
        let refs: Vec<(&str, &crate::fleet::InstanceYamlEntry)> =
            entries.iter().map(|(n, e)| (n.as_str(), e)).collect();
        if let Err(e) = crate::fleet::add_instances_to_yaml(home, &refs) {
            tracing::warn!(error = %e, "failed to persist team to fleet.yaml");
        }
    }

    // Phase 2 — generate instructions and spawn each planned member.
    let mut spawned: Vec<(String, String)> = Vec::new();
    let size = crossterm::terminal::size().unwrap_or((120, 40));
    for (inst_name, backend, work_dir) in &planned {
        if let Err(e) =
            crate::agent_ops::spawn::prepare_instructions(home, inst_name, backend, work_dir, None)
        {
            tracing::error!(agent = %inst_name, error = %e,
                "team spawn: provisioning refused — skipping member");
            continue;
        }
        let resolved = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
            .ok()
            .and_then(|f| f.resolve_instance(inst_name));
        let resolved_env = resolved.as_ref().map(|r| r.env.clone());
        let mut member_args = resolved
            .as_ref()
            .map(|r| r.args.clone())
            .unwrap_or_default();
        let declared_backend = resolved
            .as_ref()
            .map(|r| r.backend.clone())
            .unwrap_or_else(|| crate::backend::Backend::parse_str(backend));
        if let Some(model) = resolved.as_ref().and_then(|r| r.model.as_deref()) {
            crate::backend::Backend::push_model_arg(&mut member_args, &declared_backend, model);
        }
        match crate::agent_ops::spawn_one(
            home,
            registry,
            inst_name,
            backend,
            &member_args,
            crate::backend::SpawnMode::Fresh,
            work_dir,
            size,
            resolved_env.as_ref(),
            Some(&declared_backend),
        ) {
            Ok(_) => {
                tracing::info!(team = %team_name, member = %inst_name, backend = %backend, "CREATE_TEAM spawn ok");
                if let Some(mode) = request.topic_binding_mode.as_deref() {
                    tracing::info!(
                        team = %team_name,
                        member = %inst_name,
                        topic_binding_mode = mode,
                        "CREATE_TEAM: skipping topic creation (opted out)"
                    );
                } else {
                    match crate::channel::ensure_topic_for(inst_name) {
                        crate::channel::TopicOutcome::Created(_)
                        | crate::channel::TopicOutcome::NoChannel => {}
                        crate::channel::TopicOutcome::Failed(err) => {
                            tracing::warn!(
                                team = %team_name,
                                member = %inst_name,
                                error = %err,
                                "CREATE_TEAM: channel exists but create_topic failed; \
                                 member spawn proceeds without topic"
                            );
                        }
                    }
                }
                spawned.push((inst_name.clone(), backend.clone()));
            }
            Err(e) => {
                tracing::warn!(team = %team_name, member = %inst_name, backend = %backend, error = %e, "CREATE_TEAM spawn failed");
                failed.push(format!("{inst_name}: {e}"));
            }
        }
    }
    tracing::info!(
        team = %team_name,
        spawned = spawned.len(),
        failed = failed.len(),
        "CREATE_TEAM spawn phase done"
    );
    if count > 0 && spawned.is_empty() {
        return json!({"ok": false, "error": format!("all {} spawns failed: {}", count, failed.join("; "))});
    }

    let spawned_names: Vec<String> = spawned.iter().map(|(n, _)| n.clone()).collect();
    let all_members: Vec<String> = request
        .existing_members
        .into_iter()
        .chain(spawned_names.iter().cloned())
        .collect();

    // Roster write — teams::create for new team, teams::update for existing
    let mut team_params = json!({
        "name": team_name,
        "members": all_members,
    });
    if let Some(desc) = &request.description {
        team_params["description"] = json!(desc);
    }
    if let Some(ref orch) = request.orchestrator {
        team_params["orchestrator"] = json!(orch);
    }
    if let Some(ref repo) = request.repository_path {
        team_params["repository_path"] = json!(repo);
    }
    if !request.accept_from.is_empty() {
        team_params["accept_from"] = json!(request.accept_from);
    }

    let result = if team_already_exists {
        tracing::info!(
            team = %team_name,
            adding = ?all_members,
            "CREATE_TEAM: team exists — extending roster (#1964)"
        );
        let mut update_params = json!({
            "name": team_name,
            "add": all_members,
        });
        if let Some(ref orch) = request.orchestrator {
            update_params["orchestrator"] = json!(orch);
        }
        if let Some(ref repo) = request.repository_path {
            update_params["repository_path"] = json!(repo);
        }
        crate::teams::update(home, &update_params)
    } else {
        crate::teams::create(home, &team_params)
    };

    if let Some(err) = result.get("error").and_then(|e| e.as_str()) {
        tracing::warn!(team = %team_name, error = %err, "CREATE_TEAM roster write failed — spawned members are NOT on the team roster");
        return json!({
            "ok": false,
            "error": format!("members spawned but roster write failed: {err}"),
            "spawned": &spawned_names,
        });
    }

    if let Some(n) = notifier {
        if team_already_exists {
            if !spawned_names.is_empty() {
                tracing::info!(team = %team_name, added = ?spawned_names, "CREATE_TEAM emitting TeamMembersChanged (extend)");
                n.notify(crate::api::ApiEvent::TeamMembersChanged {
                    name: team_name.to_string(),
                    added: spawned_names.clone(),
                    removed: Vec::new(),
                });
            }
        } else if !all_members.is_empty() {
            tracing::info!(team = %team_name, members = ?all_members, "CREATE_TEAM emitting TeamCreated");
            n.notify(crate::api::ApiEvent::TeamCreated {
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
