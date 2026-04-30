//! MCP tool proxy handler — daemon-side dispatch for MCP tool calls.
//!
//! Sprint 25 P0 Option F: the MCP subprocess is a zero-state stdio↔TCP
//! relay. All tool calls arrive here via the daemon API and dispatch
//! through the existing [`crate::mcp::handlers::handle_tool`].
//!
//! Sprint 25 P1 F1: per-tool timeout overrides + request budget enforcement.

use serde_json::{json, Value};
use std::time::Duration;

use super::HandlerCtx;

/// Per-tool timeout in milliseconds. Fast read-only tools get a short
/// timeout; slow spawn/deploy tools get a longer one.
fn tool_timeout(tool: &str) -> Duration {
    match tool {
        // Fast read-only tools (~5s)
        "inbox" | "describe_message" | "describe_thread" | "list_instances"
        | "describe_instance" | "list_teams" | "list_decisions" | "list_schedules"
        | "list_deployments" | "set_waiting_on" | "set_display_name" | "set_description"
        | "react" | "report_health" => Duration::from_secs(5),

        // Slow tools that spawn processes or do network I/O (~60s)
        "create_instance" | "deploy_template" | "replace_instance" | "watch_ci"
        | "checkout_repo" => Duration::from_secs(60),

        // Default for everything else (~30s)
        _ => Duration::from_secs(30),
    }
}

/// Handle `mcp_tool` API method: proxy a tool call through the daemon
/// where process-global state (ACTIVE_CHANNEL, heartbeat_pair, etc.)
/// is available. Applies per-tool timeout.
pub(crate) fn handle_mcp_tool(params: &Value, _ctx: &HandlerCtx) -> Value {
    let tool = match params["tool"].as_str() {
        Some(t) if !t.is_empty() => t,
        _ => return json!({"ok": false, "error": "missing 'tool' parameter"}),
    };
    let args = params["arguments"].clone();
    let instance = params["instance"].as_str().unwrap_or("").to_string();
    let timeout = tool_timeout(tool);
    let tool_owned = tool.to_string();

    // Execute handle_tool in a scoped thread with per-tool timeout.
    // This prevents a stuck tool from blocking the API session thread
    // beyond the tool's timeout budget.
    // fire-and-forget: short-lived tool execution thread; dies on completion
    // or timeout. Result sent via mpsc channel; thread is not joined — the
    // recv_timeout below bounds the caller's wait. If the tool outlives the
    // timeout, the thread runs to completion in the background (no leak —
    // handle_tool is stateless and the thread exits when done).
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::Builder::new()
        .name(format!("mcp_tool_{tool_owned}"))
        .spawn(move || {
            let result = crate::mcp::execute_tool(&tool_owned, &args, &instance);
            let _ = tx.send(result);
        });

    match handle {
        Ok(_) => match rx.recv_timeout(timeout) {
            Ok(result) => json!({"ok": true, "result": result}),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                tracing::warn!(tool, ?timeout, "mcp_tool timed out");
                json!({"ok": false, "error": format!("tool '{tool}' timed out after {timeout:?}")})
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                json!({"ok": false, "error": format!("tool '{tool}' thread panicked")})
            }
        },
        Err(e) => json!({"ok": false, "error": format!("spawn failed: {e}")}),
    }
}

/// Handle `mcp_tools_list` API method: return the tool definitions.
pub(crate) fn handle_mcp_tools_list(_params: &Value, _ctx: &HandlerCtx) -> Value {
    json!({"ok": true, "result": crate::mcp::tools::tool_definitions()})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_tools_get_short_timeout() {
        assert_eq!(tool_timeout("inbox"), Duration::from_secs(5));
        assert_eq!(tool_timeout("list_instances"), Duration::from_secs(5));
        assert_eq!(tool_timeout("describe_message"), Duration::from_secs(5));
    }

    #[test]
    fn slow_tools_get_long_timeout() {
        assert_eq!(tool_timeout("create_instance"), Duration::from_secs(60));
        assert_eq!(tool_timeout("deploy_template"), Duration::from_secs(60));
    }

    #[test]
    fn default_tools_get_30s() {
        assert_eq!(tool_timeout("send_to_instance"), Duration::from_secs(30));
        assert_eq!(tool_timeout("delegate_task"), Duration::from_secs(30));
        assert_eq!(tool_timeout("unknown_tool"), Duration::from_secs(30));
    }
}
