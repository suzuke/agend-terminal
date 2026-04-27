//! MCP tool proxy handler — daemon-side dispatch for MCP tool calls.
//!
//! Sprint 25 P0 Option F: the MCP subprocess is a zero-state stdio↔TCP
//! relay. All tool calls arrive here via the daemon API and dispatch
//! through the existing [`crate::mcp::handlers::handle_tool`].

use serde_json::{json, Value};

use super::HandlerCtx;

/// Handle `mcp_tool` API method: proxy a tool call through the daemon
/// where process-global state (ACTIVE_CHANNEL, heartbeat_pair, etc.)
/// is available.
pub(crate) fn handle_mcp_tool(params: &Value, _ctx: &HandlerCtx) -> Value {
    let tool = match params["tool"].as_str() {
        Some(t) if !t.is_empty() => t,
        _ => return json!({"ok": false, "error": "missing 'tool' parameter"}),
    };
    let args = &params["arguments"];
    let instance = params["instance"].as_str().unwrap_or("");

    let result = crate::mcp::handlers::handle_tool(tool, args, instance);
    json!({"ok": true, "result": result})
}

/// Handle `mcp_tools_list` API method: return the tool definitions.
pub(crate) fn handle_mcp_tools_list(_params: &Value, _ctx: &HandlerCtx) -> Value {
    json!({"ok": true, "result": crate::mcp::tools::tool_definitions()})
}
