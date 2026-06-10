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
        .values()
        .map(|handle| {
            let name = handle.name.as_str();
            let (agent_state, health_state, blocked_reason, blocked_note, context) = {
                let c = handle.core.lock();
                (
                    c.state.get_state().display_name().to_string(),
                    c.health.state.display_name().to_string(),
                    // #1933: surface the self-reported blocked reason + its
                    // operator-readable note so an operator can see WHY an agent is
                    // blocked and its free-text annotation (previously internal-only).
                    c.health.current_reason.as_ref().map(|r| r.to_string()),
                    c.health.current_note.clone(),
                    // Context% telemetry: resolved usage + producing source
                    // ("pattern" = the agent's own statusline, "transcript" =
                    // token-usage estimate). Absent = honestly unknown.
                    c.state.resolved_context(),
                )
            };
            let (dispatched_waiting_for, pending_response_to) =
                crate::daemon::dispatch_idle::pending_for_instance(ctx.home, name);
            json!({
                "name": name,
                "backend": handle.backend_command,
                "submit_key": handle.submit_key,
                "inject_prefix": handle.inject_prefix,
                "agent_state": agent_state,
                "health_state": health_state,
                "blocked_reason": blocked_reason,
                "blocked_note": blocked_note,
                "context_pct": context.map(|(pct, _)| pct),
                "context_source": context.map(|(_, source)| source),
                "kind": "managed",
                "dispatched_waiting_for": dispatched_waiting_for,
                "pending_response_to": pending_response_to,
            })
        })
        .collect();
    drop(reg);
    let ext = agent::lock_external(ctx.externals);
    for (name, handle) in ext.iter() {
        let (dispatched_waiting_for, pending_response_to) =
            crate::daemon::dispatch_idle::pending_for_instance(ctx.home, name);
        agents.push(json!({
            "name": name,
            "backend": handle.backend_command,
            "agent_state": "external",
            "health_state": "connected",
            "kind": "external",
            "pid": handle.pid,
            "dispatched_waiting_for": dispatched_waiting_for,
            "pending_response_to": pending_response_to,
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
