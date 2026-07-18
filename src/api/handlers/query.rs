//! Query handlers: LIST and STATUS.
//!
//! These are read-only handlers that return agent/snapshot information
//! without mutating state.

use super::HandlerCtx;
use crate::agent::{AgentRegistry, ExternalRegistry};
use serde_json::{json, Value};

pub(crate) fn handle_list(_params: &Value, ctx: &HandlerCtx) -> Value {
    list_response(ctx.home, ctx.registry, ctx.externals)
}

/// #2454 S3: thin wire adapter — delegates to the neutral
/// `agent_ops::list_snapshot` service that owns the implementation.
pub(crate) fn list_response(
    home: &std::path::Path,
    registry: &AgentRegistry,
    externals: &ExternalRegistry,
) -> Value {
    crate::agent_ops::list_snapshot(home, registry, externals)
}

pub(crate) fn handle_status(_params: &Value, ctx: &HandlerCtx) -> Value {
    match crate::snapshot::load(ctx.home) {
        Some(snapshot) => {
            json!({"ok": true, "result": {
                "protocol_version": crate::framing::PROTOCOL_VERSION,
                "timestamp": snapshot.timestamp,
                "agents": snapshot.agents.iter().map(|a| {
                    json!({
                        "name": a.name,
                        "backend": a.backend_command,
                        "args": a.args,
                        "working_dir": a.working_dir,
                        "submit_key": a.submit_key,
                        "health_state": a.health_state,
                        "agent_state": a.agent_state,
                    })
                }).collect::<Vec<_>>()
            }})
        }
        None => json!({"ok": true, "result": {"agents": [], "timestamp": null}}),
    }
}
