//! Daemon-side MCP tool dispatch.
//!
//! Sprint 56 Track I-Phase2c (#531): the stdio JSON-RPC server moved to
//! `src/bin/agend-mcp-bridge.rs` (the canonical bridge binary). This
//! module retains only the daemon-internal tool dispatcher
//! (`execute_tool`) plus the `handlers` and `tools` submodules used by
//! the bridge proxy and `api/handlers/mcp_proxy.rs`.

pub mod handlers;
pub mod tools;

use serde_json::Value;

/// Service boundary: single public entry point for MCP tool execution.
/// API layer calls this instead of reaching into `handlers::handle_tool`
/// directly, keeping timeout policy in API and execution in MCP.
pub fn execute_tool(tool_name: &str, args: &Value, instance_name: &str) -> Value {
    handlers::handle_tool(tool_name, args, instance_name)
}
