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
