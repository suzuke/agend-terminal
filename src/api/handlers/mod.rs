//! Extracted handler functions for the API dispatch loop.
//!
//! Each handler takes `(params, ctx)` and returns a `Value` response.
//! `HandlerCtx` bundles the session-scoped state that handlers need.

pub(crate) mod external;
pub(crate) mod hook_event;
pub(crate) mod instance;
pub(crate) mod mcp_proxy;
pub(crate) mod messaging;
pub(crate) mod query;
pub(crate) mod team;

use crate::agent::{AgentRegistry, ExternalRegistry};
use crate::api::{ApiNotifier, ConfigRegistry};
use std::path::Path;

/// Write `agend.md` / `GEMINI.md` and the MCP config into `work_dir` before
/// the child process is spawned. Centralises the fleet-aware instruction
/// generation that every API-level spawn should perform.
///
/// Role resolution order:
///   1. `explicit_role` — caller passes it directly (SPAWN/CREATE_TEAM
///      params). Used when the caller has the role in-hand and doesn't
///      want to depend on fleet.yaml read order.
///   2. `fleet.yaml` entry under `name` — used by deploy_template which
///      persists the entry before calling SPAWN.
///   3. None — ad-hoc spawn (verify, shell pane). Identity block still
///      emits name + peers, just no Role line.
///
/// Peers always come from `fleet.yaml`, so any combination of spawn
/// callers produces a coherent peer list.
///
/// Must be called *before* the backend process starts: `backend::spawn_flags`
/// checks the file's presence at flag-build time and silently drops
/// `--append-system-prompt-file` when the instructions file is missing.
pub(crate) fn prepare_instructions(
    home: &Path,
    name: &str,
    command: &str,
    work_dir: &Path,
    explicit_role: Option<&str>,
) -> Result<(), String> {
    std::fs::create_dir_all(work_dir).map_err(|e| {
        format!(
            "prepare_instructions: create {} failed: {e}",
            work_dir.display()
        )
    })?;
    let fleet_path = crate::fleet::fleet_yaml_path(home);
    // Look up team membership so agend.md can split collaborators (team
    // members) from the rest of the fleet. Owned here so we can hand out
    // borrowed TeamContexts into each match arm without moves.
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
        Err(e) => {
            let is_not_found = !fleet_path.exists();
            if !is_not_found {
                return Err(format!(
                    "fleet.yaml unreadable/malformed — refusing provisioning: {e}"
                ));
            }
            let ctx = crate::instructions::AgentContext {
                name,
                role: explicit_role,
                fleet_peers: &[],
                team: team_ctx.as_ref(),
                extra_instructions: None,
            };
            crate::instructions::generate_with_context(work_dir, command, Some(&ctx))
        }
    }
}

/// Shared context passed to extracted handler functions.
///
/// Bundles the session-scoped references that `handle_session` holds.
/// Handlers receive `&HandlerCtx` instead of individual parameters.
///
/// ## Convention (established by Slice A, used by subsequent slices)
/// - Handler signature: `pub(crate) fn handle_X(params: &Value, ctx: &HandlerCtx) -> Value`
/// - Handlers return the full JSON response (including `{"ok": true, ...}`)
/// - Handlers must not write to the TCP stream directly
///
/// Note: the `SHUTDOWN` method is handled inline in `handle_session` and
/// does not go through a `HandlerCtx`-based handler, so the shutdown flag
/// is intentionally absent here.
pub(crate) struct HandlerCtx<'a> {
    pub registry: &'a AgentRegistry,
    pub configs: &'a ConfigRegistry,
    pub externals: &'a ExternalRegistry,
    pub notifier: Option<&'a dyn ApiNotifier>,
    pub home: &'a Path,
    /// #2453 Stage R1: the API-server owner's restart capability, injected at
    /// [`crate::api::serve`] from the composition root and carried into the MCP
    /// `RuntimeContext` so `restart_daemon` dispatches on an injected value.
    pub capability: crate::api::RestartCapability,
    /// #2453 Stage R2: the app owner-restart request channel + gate (app root
    /// only). Carried into the MCP `RuntimeContext`. `None` on daemon/verify.
    pub app_restart: Option<&'a crate::api::app_restart::AppRestart>,
    /// #2453 Stage R2 (flush barrier): a fresh per-request slot for a post-flush
    /// action. `handle_session` creates it, threads a clone into the MCP
    /// `RuntimeContext` (so the restart handler can register its commit-permission
    /// ack), and — after writing the response — calls `run_after_flush`. Cheap
    /// `Arc` clone; unused by non-restart tools.
    pub post_flush: crate::api::app_restart::PostFlushSlot,
}
