//! Sprint 25 P0 — `proxy_channel_op` daemon API handler.
//!
//! MCP subprocesses (spawned by Claude Code, separate process) cannot
//! access `ACTIVE_CHANNEL` (Telegram client lives in daemon process).
//! This handler bridges the gap: MCP subprocess sends a
//! `proxy_channel_op` request via the existing Unix domain socket API;
//! daemon dispatches to `Channel::send_from_agent` in-process.
//!
//! See `docs/ARCHITECTURE.md` for the full process model.

use super::HandlerCtx;
use serde_json::{json, Value};

/// Handle `proxy_channel_op` — relay a channel operation from an MCP
/// subprocess to the daemon's in-process channel.
///
/// Params:
/// - `instance`: agent name (used for outbound capability gate)
/// - `op`: one of `"reply"`, `"react"`, `"edit"`, `"inject_provenance"`
/// - `args`: op-specific arguments (text, emoji, message_id, etc.)
pub(crate) fn handle_proxy_channel_op(params: &Value, _ctx: &HandlerCtx) -> Value {
    let instance = match params["instance"].as_str() {
        Some(i) => i,
        None => return json!({"ok": false, "error": "missing 'instance'"}),
    };
    let op = match params["op"].as_str() {
        Some(o) => o,
        None => return json!({"ok": false, "error": "missing 'op'"}),
    };
    let args = &params["args"];

    let Some(ch) = crate::channel::active_channel() else {
        return json!({"ok": false, "error": "no active channel (daemon has no channel configured)"});
    };

    let outbound_op = match op {
        "reply" => crate::channel::AgentOutboundOp::Reply {
            text: args["text"].as_str().unwrap_or("").to_string(),
        },
        "react" => crate::channel::AgentOutboundOp::React {
            emoji: args["emoji"].as_str().unwrap_or("").to_string(),
            message_id: args["message_id"].as_str().map(String::from),
        },
        "edit" => {
            let message_id = match args["message_id"].as_str() {
                Some(m) => m.to_string(),
                None => return json!({"ok": false, "error": "missing args.message_id"}),
            };
            let new_text = match args["text"].as_str() {
                Some(t) => t.to_string(),
                None => return json!({"ok": false, "error": "missing args.text"}),
            };
            crate::channel::AgentOutboundOp::Edit {
                message_id,
                new_text,
            }
        }
        "inject_provenance" => crate::channel::AgentOutboundOp::InjectProvenance {
            from: args["from"].as_str().unwrap_or("").to_string(),
            task: args["task"].as_str().unwrap_or("").to_string(),
        },
        _ => return json!({"ok": false, "error": format!("unknown op: {op}")}),
    };

    match ch.send_from_agent(instance, outbound_op) {
        Ok(msg) => json!({"ok": true, "result": {"message_id": msg.id}}),
        Err(e) => json!({"ok": false, "error": format!("{e}")}),
    }
}
