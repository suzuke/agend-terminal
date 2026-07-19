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
use std::sync::Arc;

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
#[cfg(test)]
pub(crate) fn prepare_instructions(
    home: &Path,
    name: &str,
    command: &str,
    work_dir: &Path,
    explicit_role: Option<&str>,
) -> Result<(), String> {
    crate::agent_ops::spawn::prepare_instructions(home, name, command, work_dir, explicit_role)
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
    pub notifier: Option<&'a Arc<dyn ApiNotifier>>,
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
    /// #2454 Slice 9: shared daemon shutdown authority. Production API sessions
    /// inject it; the inline SHUTDOWN path and MCP `restart_daemon` carry the
    /// same authority. Ordinary helper/test contexts stay standalone.
    pub shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
}
