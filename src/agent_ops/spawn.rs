//! Transport-neutral SPAWN resolution, provisioning, and execution.

use crate::agent::{self, AgentRegistry, ExternalRegistry};
use crate::api::ApiNotifier;
use crate::backend::{Backend, SpawnMode};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Typed wire-independent inputs shared by API and MCP adapters.
pub struct SpawnParams<'a> {
    pub name: &'a str,
    pub backend: Option<&'a str>,
    pub args: Option<&'a str>,
    pub model: Option<&'a str>,
    pub model_tier: Option<&'a str>,
    pub working_directory: Option<&'a Path>,
    pub env: Option<&'a HashMap<String, String>>,
    pub mode: SpawnMode,
    pub explicit_role: Option<&'a str>,
    pub self_kick_on_ready: bool,
    pub topic_binding: &'a str,
    pub layout: &'a str,
    pub spawner: Option<&'a str>,
    pub target_pane: Option<&'a str>,
}

/// Fully resolved SPAWN request. All precedence decisions happen once here.
pub struct SpawnRequest {
    pub name: String,
    pub backend: String,
    pub args: Vec<String>,
    pub declared_backend: Backend,
    pub working_directory: PathBuf,
    pub env: Option<HashMap<String, String>>,
    pub mode: SpawnMode,
    pub explicit_role: Option<String>,
    pub self_kick_on_ready: bool,
    pub topic_binding: String,
    pub layout: String,
    pub spawner: Option<String>,
    pub target_pane: Option<String>,
}

/// Runtime-owned state required by the neutral SPAWN service.
pub struct SpawnContext<'a> {
    pub home: &'a Path,
    pub registry: &'a AgentRegistry,
    pub externals: &'a ExternalRegistry,
    pub notifier: Option<&'a Arc<dyn ApiNotifier>>,
}

#[derive(Debug)]
pub struct SpawnOutcome {
    pub topic_id: Option<String>,
}

/// Resolve command, argv, model, declared backend, environment, and wire
/// metadata with the same precedence as the direct API SPAWN handler.
pub fn resolve_spawn_request(home: &Path, params: &SpawnParams<'_>) -> SpawnRequest {
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();
    let fleet_resolved = fleet.as_ref().and_then(|f| f.resolve_instance(params.name));
    let command = params
        .backend
        .map(|s| Backend::parse_str(s).command_string())
        .or_else(|| {
            fleet
                .as_ref()
                .and_then(|f| f.defaults.backend.as_ref())
                .map(|b| b.preset().command.to_string())
        })
        .unwrap_or_else(|| "claude".to_string());
    let mut args = params
        .args
        .map(|s| s.split_whitespace().map(String::from).collect())
        .or_else(|| fleet_resolved.as_ref().map(|r| r.args.clone()))
        .unwrap_or_default();

    let params_backend = params.backend.map(Backend::parse_str);
    let declared_backend = params_backend
        .clone()
        .filter(|b| !matches!(b, Backend::Raw(_)))
        .or_else(|| fleet_resolved.as_ref().map(|r| r.backend.clone()))
        .or_else(|| fleet.as_ref().and_then(|f| f.defaults.backend.clone()))
        .or(params_backend)
        .unwrap_or(Backend::ClaudeCode);
    let tier_model = params
        .model_tier
        .filter(|m| !m.is_empty())
        .and_then(|tier| fleet.as_ref().and_then(|f| f.model_tiers.get(tier)))
        .map(String::as_str);
    if let Some(model) = params
        .model
        .filter(|m| !m.is_empty())
        .or(tier_model)
        .or_else(|| fleet_resolved.as_ref().and_then(|r| r.model.as_deref()))
    {
        Backend::push_model_arg(&mut args, &declared_backend, model);
    }

    SpawnRequest {
        name: params.name.to_string(),
        backend: command,
        args,
        declared_backend,
        working_directory: params
            .working_directory
            .map(Path::to_path_buf)
            .unwrap_or_else(|| crate::paths::workspace_dir(home).join(params.name)),
        env: params
            .env
            .cloned()
            .or_else(|| fleet_resolved.as_ref().map(|r| r.env.clone())),
        mode: params.mode,
        explicit_role: params.explicit_role.map(str::to_string),
        self_kick_on_ready: params.self_kick_on_ready,
        topic_binding: params.topic_binding.to_string(),
        layout: params.layout.to_string(),
        spawner: params.spawner.map(str::to_string),
        target_pane: params.target_pane.map(str::to_string),
    }
}

/// Execute the shared SPAWN behavior. Caller-specific fleet persistence,
/// rollback, and restart teardown remain outside this service.
pub fn spawn_instance(
    context: &SpawnContext<'_>,
    request: &SpawnRequest,
) -> Result<SpawnOutcome, String> {
    agent::validate_name(&request.name).map_err(|e| e.to_string())?;
    if agent::lock_external(context.externals).contains_key(&request.name) {
        return Err(format!(
            "agent '{}' already exists (external)",
            request.name
        ));
    }
    if crate::fleet::resolve_uuid(context.home, &request.name)
        .is_some_and(|id| agent::lock_registry(context.registry).contains_key(&id))
    {
        return Err(format!("agent '{}' already exists", request.name));
    }
    let work_dir = crate::api::validate_working_directory(&request.working_directory, context.home)
        .map_err(|e| e.to_string())?;
    if let Some(collider) =
        crate::fleet::persist::workspace_identity_collision(context.home, &request.name, &work_dir)
    {
        return Err(format!(
            "workspace identity collision: '{}' would share the working directory with existing instance '{}' ({})",
            request.name,
            collider,
            work_dir.display()
        ));
    }
    let behavior_command = match &request.declared_backend {
        Backend::Shell | Backend::Raw(_) => request.backend.clone(),
        _ => request.declared_backend.command_string(),
    };
    prepare_instructions(
        context.home,
        &request.name,
        &behavior_command,
        &work_dir,
        request.explicit_role.as_deref(),
    )
    .map_err(|e| format!("provisioning refused: {e}"))?;

    let _mode = super::spawn_one(
        context.home,
        context.registry,
        &request.name,
        &request.backend,
        &request.args,
        request.mode,
        &work_dir,
        crossterm::terminal::size().unwrap_or((120, 40)),
        request.env.as_ref(),
        Some(&request.declared_backend),
    )
    .map_err(|e| e.to_string())?;

    if request.self_kick_on_ready {
        if let Some(id) = crate::fleet::resolve_uuid(context.home, &request.name) {
            let ready_timeout = request
                .declared_backend
                .preset()
                .ready_timeout_secs
                .saturating_add(15);
            agent::spawn_self_kick_bootstrap(
                std::sync::Arc::clone(context.registry),
                id,
                request.name.clone(),
                std::time::Duration::from_secs(ready_timeout),
                None,
            );
        } else {
            tracing::warn!(
                agent = %request.name,
                "self_kick_on_ready set but instance UUID unresolved — skipping self-kick"
            );
        }
    }

    let topic_id = if matches!(request.topic_binding.as_str(), "skip" | "deferred") {
        None
    } else {
        match crate::channel::ensure_topic_for(&request.name) {
            crate::channel::TopicOutcome::Created(tid) => Some(tid),
            crate::channel::TopicOutcome::NoChannel => None,
            crate::channel::TopicOutcome::Failed(err) => {
                tracing::warn!(agent = %request.name, error = %err, "SPAWN topic creation failed");
                None
            }
        }
    };
    if let Some(notifier) = context.notifier {
        notifier.notify(crate::api::ApiEvent::InstanceCreated {
            name: request.name.clone(),
            layout: crate::api::LayoutHint::parse(&request.layout),
            spawner: request.spawner.clone(),
            target_pane: request.target_pane.clone(),
        });
    }
    Ok(SpawnOutcome { topic_id })
}

/// Write the instruction files and MCP config before a backend is spawned.
pub(crate) fn prepare_instructions(
    home: &Path,
    name: &str,
    command: &str,
    work_dir: &Path,
    explicit_role: Option<&str>,
) -> Result<(), String> {
    let fleet_path = crate::fleet::fleet_yaml_path(home);
    let team_record = crate::teams::find_team_for(home, name);
    let team_ctx = team_record
        .as_ref()
        .map(|t| crate::instructions::TeamContext {
            name: t.name.as_str(),
            orchestrator: t.orchestrator.as_deref(),
            members: t.members.as_slice(),
        });
    match crate::fleet::FleetConfig::load(&fleet_path) {
        Ok(fleet) => {
            std::fs::create_dir_all(work_dir).map_err(|e| {
                format!(
                    "prepare_instructions: create {} failed: {e}",
                    work_dir.display()
                )
            })?;
            let peers: Vec<(String, Option<String>)> = fleet
                .instances
                .iter()
                .map(|(n, c)| (n.clone(), c.role.clone()))
                .collect();
            let role = explicit_role
                .map(str::to_string)
                .or_else(|| fleet.instances.get(name).and_then(|c| c.role.clone()));
            let fleet_dir = fleet_path.parent().unwrap_or(home);
            let extra_instr = crate::instructions::resolve_extra_from_path(
                fleet
                    .instances
                    .get(name)
                    .and_then(|c| c.instructions.as_deref())
                    .or(fleet.defaults.instructions.as_deref()),
                fleet_dir,
            );
            let ctx = crate::instructions::AgentContext {
                name,
                role: role.as_deref(),
                fleet_peers: &peers,
                team: team_ctx.as_ref(),
                extra_instructions: extra_instr.as_deref(),
            };
            crate::instructions::generate_with_context(work_dir, command, Some(&ctx))
        }
        Err(e) => match std::fs::metadata(&fleet_path) {
            Err(io_err) if io_err.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(work_dir).map_err(|e2| {
                    format!(
                        "prepare_instructions: create {} failed: {e2}",
                        work_dir.display()
                    )
                })?;
                let ctx = crate::instructions::AgentContext {
                    name,
                    role: explicit_role,
                    fleet_peers: &[],
                    team: team_ctx.as_ref(),
                    extra_instructions: None,
                };
                crate::instructions::generate_with_context(work_dir, command, Some(&ctx))
            }
            _ => Err(format!(
                "fleet.yaml unreadable/malformed — refusing provisioning: {e}"
            )),
        },
    }
}
