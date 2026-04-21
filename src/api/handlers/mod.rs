//! Extracted handler functions for the API dispatch loop.
//!
//! Each handler takes `(params, ctx)` and returns a `Value` response.
//! `HandlerCtx` bundles the session-scoped state that handlers need.

pub(crate) mod external;
pub(crate) mod instance;
pub(crate) mod messaging;
pub(crate) mod query;

use crate::agent::{AgentRegistry, ExternalRegistry};
use crate::api::{ApiNotifier, ConfigRegistry};
use std::path::Path;
use std::sync::atomic::AtomicBool;
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
#[allow(dead_code)] // Fields used by future slices (B/C1/C2/D)
pub(crate) struct HandlerCtx<'a> {
    pub registry: &'a AgentRegistry,
    pub configs: &'a ConfigRegistry,
    pub externals: &'a ExternalRegistry,
    pub notifier: Option<&'a dyn ApiNotifier>,
    pub home: &'a Path,
    pub shutdown: &'a Arc<AtomicBool>,
}
