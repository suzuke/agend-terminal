//! Extracted handler functions for the API dispatch loop.
//!
//! Each handler takes `(params, ctx)` and returns a `Value` response.
//! `HandlerCtx` bundles the session-scoped state that handlers need.

pub(crate) mod external;
pub(crate) mod instance;
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
) {
    std::fs::create_dir_all(work_dir).ok();
    let fleet_path = home.join("fleet.yaml");
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
            let ctx = crate::instructions::AgentContext {
                name,
                role: role.as_deref(),
                fleet_peers: &peers,
                team: team_ctx.as_ref(),
            };
            crate::instructions::generate_with_context(work_dir, command, Some(&ctx));
        }
        Err(_) if explicit_role.is_some() => {
            let ctx = crate::instructions::AgentContext {
                name,
                role: explicit_role,
                fleet_peers: &[],
                team: team_ctx.as_ref(),
            };
            crate::instructions::generate_with_context(work_dir, command, Some(&ctx));
        }
        Err(_) => {
            crate::instructions::generate(work_dir, command);
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
}
