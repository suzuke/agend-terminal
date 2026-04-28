//! Query handlers: LIST and STATUS.
//!
//! These are read-only handlers that return agent/snapshot information
//! without mutating state.

use super::HandlerCtx;
use crate::agent;
use serde_json::{json, Value};

pub(crate) fn handle_list(_params: &Value, ctx: &HandlerCtx) -> Value {
    let reg = agent::lock_registry(ctx.registry);
    let mut agents: Vec<Value> = reg
        .iter()
        .map(|(name, handle)| {
            let (agent_state, health_state) = {
                let c = handle.core.lock();
                (
                    c.state.get_state().display_name().to_string(),
                    c.health.state.display_name().to_string(),
                )
            };
            json!({
                "name": name,
                "backend": handle.backend_command,
                "submit_key": handle.submit_key,
                "inject_prefix": handle.inject_prefix,
                "agent_state": agent_state,
                "health_state": health_state,
                "kind": "managed"
            })
        })
        .collect();
    drop(reg);
    let ext = agent::lock_external(ctx.externals);
    for (name, handle) in ext.iter() {
        agents.push(json!({
            "name": name,
            "backend": handle.backend_command,
            "agent_state": "external",
            "health_state": "connected",
            "kind": "external",
            "pid": handle.pid
        }));
    }
    json!({"ok": true, "result": {"protocol_version": crate::framing::PROTOCOL_VERSION, "agents": agents}})
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
