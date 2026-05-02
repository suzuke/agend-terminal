//! MCP stdio server — NDJSON JSON-RPC 2.0.
//!
//! Translates MCP tool calls to agent PTY writes via TUI socket.
//! Runs synchronously (no tokio needed).

pub mod handlers;
pub mod tools;

use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{self, BufRead, BufReader, Write};

/// Service boundary: single public entry point for MCP tool execution.
/// API layer calls this instead of reaching into `handlers::handle_tool`
/// directly, keeping timeout policy in API and execution in MCP.
pub fn execute_tool(tool_name: &str, args: &Value, instance_name: &str) -> Value {
    handlers::handle_tool(tool_name, args, instance_name)
}

/// Tool authz: returns `(allow, deny)` sets parsed from
/// `AGEND_MCP_TOOLS_ALLOW` / `AGEND_MCP_TOOLS_DENY` (comma-separated,
/// whitespace-tolerant). An empty allow-set means "allow all" (legacy
/// behaviour). A tool on the deny-set is always rejected; if the allow-set
/// is non-empty, only tools on the allow-set (minus any deny-set entries)
/// are exposed / callable.
fn parse_csv(v: &str) -> HashSet<String> {
    v.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn read_acl_from_env() -> (HashSet<String>, HashSet<String>) {
    let allow = std::env::var("AGEND_MCP_TOOLS_ALLOW")
        .ok()
        .map(|v| parse_csv(&v))
        .unwrap_or_default();
    let deny = std::env::var("AGEND_MCP_TOOLS_DENY")
        .ok()
        .map(|v| parse_csv(&v))
        .unwrap_or_default();
    (allow, deny)
}

/// M3: RwLock ACL cache — allows invalidation via invalidate_acl_cache().
static ACL_CACHE: parking_lot::RwLock<Option<(HashSet<String>, HashSet<String>)>> =
    parking_lot::RwLock::new(None);

fn tool_acl() -> (HashSet<String>, HashSet<String>) {
    // M3 r1-fix: compute under write lock to prevent invalidate-during-compute race.
    // ACL is not a hot path — write lock simplicity > read-mostly concurrency.
    let mut guard = ACL_CACHE.write();
    if let Some(ref cached) = *guard {
        return cached.clone();
    }
    let fresh = read_acl_from_env();
    *guard = Some(fresh.clone());
    fresh
}

/// Invalidate the cached ACL so the next `tool_acl()` call re-reads env vars.
#[allow(dead_code)] // M3: available for runtime config reload, not yet wired
pub(crate) fn invalidate_acl_cache() {
    *ACL_CACHE.write() = None;
}

/// Check if a tool is allowed given explicit allow/deny sets.
fn check_allowed(tool: &str, allow: &HashSet<String>, deny: &HashSet<String>) -> bool {
    if deny.contains(tool) {
        return false;
    }
    if allow.is_empty() {
        return true;
    }
    allow.contains(tool)
}

/// Returns true if `tool` is callable under the configured ACL.
pub(crate) fn tool_is_allowed(tool: &str) -> bool {
    let (allow, deny) = tool_acl();
    check_allowed(tool, &allow, &deny)
}

/// Filter a `tool_definitions()` value in place to drop tools blocked by the
/// ACL. Takes the `{"tools": [..]}` object and returns a filtered clone.
pub(crate) fn filter_tools(defs: Value) -> Value {
    let (allow, deny) = tool_acl();
    let tools = match defs.get("tools").and_then(|v| v.as_array()) {
        Some(a) => a.clone(),
        None => return defs,
    };
    let filtered: Vec<Value> = tools
        .into_iter()
        .filter(|t| {
            let n = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
            check_allowed(n, &allow, &deny)
        })
        .collect();
    json!({ "tools": filtered })
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

/// Read a message from stdin — NDJSON only.
///
/// All known MCP backends (Claude Code, Kiro CLI, Codex, Gemini, OpenCode)
/// send NDJSON over stdio. Content-Length (LSP-style) fallback removed per
/// Sprint 25 P3 framing audit — it was an attack surface (drip-feed DoS,
/// negative Content-Length crash, OOM). See docs/archived/MCP-FRAMING-PER-BACKEND.md.
fn read_message(reader: &mut impl BufRead) -> anyhow::Result<Option<String>> {
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('{') {
            return Ok(Some(trimmed.to_string()));
        }
        // Non-JSON, non-empty line: warn and skip
        tracing::warn!(line = %trimmed.chars().take(80).collect::<String>(), "ignoring non-JSON input line");
    }
}

/// Write a message — NDJSON format (one JSON per line, like Claude expects).
fn write_message(stdout: &mut io::Stdout, json: &str) -> anyhow::Result<()> {
    writeln!(stdout, "{json}")?;
    stdout.flush()?;
    Ok(())
}

pub fn run() -> anyhow::Result<()> {
    let instance_name = std::env::var("AGEND_INSTANCE_NAME").unwrap_or_default();
    tracing::info!(%instance_name, "server starting");

    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = io::stdout();

    loop {
        let body = match read_message(&mut reader)? {
            Some(b) => b,
            None => break,
        };

        let req: JsonRpcRequest = match serde_json::from_str(&body) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "invalid JSON-RPC");
                // Per JSON-RPC 2.0: parse errors MUST produce a response
                // so the caller does not hang. id is Null because the
                // malformed request may not expose a valid one; best-effort
                // salvage if the body was valid JSON but failed our shape.
                let id = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v.get("id").cloned())
                    .unwrap_or(Value::Null);
                let err_resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32700, "message": format!("Parse error: {e}") }
                });
                write_message(&mut stdout, &err_resp.to_string())?;
                continue;
            }
        };

        let id = req.id.clone().unwrap_or(Value::Null);

        let response = match req.method.as_str() {
            "initialize" => json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "agend-terminal", "version": env!("CARGO_PKG_VERSION") }
                }
            }),
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            "notifications/initialized" | "notifications/cancelled" => continue,
            "tools/list" => {
                json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": filter_tools(tools::tool_definitions())
                })
            }
            "tools/call" => {
                let tool = req.params["name"].as_str().unwrap_or("");
                let args = &req.params["arguments"];

                if !tool_is_allowed(tool) {
                    tracing::warn!(tool = %tool, "blocked by AGEND_MCP_TOOLS_ALLOW/DENY");
                    write_message(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": {
                                "code": -32601,
                                "message": format!("Tool '{tool}' not available (blocked by policy)")
                            }
                        })
                        .to_string(),
                    )?;
                    continue;
                }

                // Try daemon proxy first — avoids per-process overhead
                let result = proxy_or_local(tool, args, &instance_name);

                json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default() }],
                        "isError": result.get("error").is_some()
                    }
                })
            }
            method => {
                if method.starts_with("notifications/") {
                    continue;
                }
                json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32601, "message": format!("Method not found: {method}") }
                })
            }
        };

        write_message(&mut stdout, &response.to_string())?;
    }

    tracing::info!("server exiting");
    Ok(())
}

/// Returns true if this process IS the daemon. When true, `handle_tool`
/// can be called directly without TCP round-trip.
///
/// Detection: the daemon sets `AGEND_DAEMON_PID` env var during startup
/// (see `daemon::run_core`). MCP subprocesses spawned by agent backends
/// do NOT inherit this var (it's not in the agent env passthrough list).
pub(crate) fn is_running_inside_daemon_process() -> bool {
    use std::sync::OnceLock;
    static IS_DAEMON: OnceLock<bool> = OnceLock::new();
    *IS_DAEMON.get_or_init(|| {
        // Primary signal: ACTIVE_CHANNEL is only registered in daemon process
        crate::channel::active_channel().is_some()
        // G3 H1: read DaemonConfig instead of env var (thread-safe)
        || crate::daemon_config::get().daemon_pid == std::process::id()
    })
}

/// Try to proxy a tool call through the daemon API port.
/// Falls back to local handling if the daemon is unavailable.
/// Short-circuits to direct `handle_tool` when running inside the daemon.
fn proxy_or_local(tool: &str, args: &Value, instance_name: &str) -> Value {
    // Short-circuit: if we're inside the daemon process, call handle_tool
    // directly — no TCP round-trip needed.
    if is_running_inside_daemon_process() {
        return handlers::handle_tool(tool, args, instance_name);
    }

    // Sprint 31 P0: test isolation — when AGEND_TEST_ISOLATION=1, never
    // try the daemon API. Prevents cargo test from polluting the real
    // fleet via env::set_var race conditions in parallel test threads.
    if std::env::var("AGEND_TEST_ISOLATION").as_deref() == Ok("1") {
        return handlers::handle_tool(tool, args, instance_name);
    }

    let home = crate::home_dir();

    if let Ok(resp) = crate::api::call(
        &home,
        &json!({
            "method": "mcp_tool",
            "params": {
                "tool": tool,
                "arguments": args,
                "instance": instance_name
            }
        }),
    ) {
        if resp["ok"].as_bool() == Some(true) {
            return resp["result"].clone();
        }
    }

    // Daemon unavailable or returned error — handle locally
    handlers::handle_tool(tool, args, instance_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_set(tools: &[&str]) -> HashSet<String> {
        tools.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn acl_empty_allows_everything() {
        let empty = HashSet::new();
        assert!(check_allowed("delete_instance", &empty, &empty));
        assert!(check_allowed("send_to_instance", &empty, &empty));
    }

    #[test]
    fn acl_deny_blocks_tool() {
        let empty = HashSet::new();
        let deny = tool_set(&["delete_instance", "shutdown"]);
        assert!(!check_allowed("delete_instance", &empty, &deny));
        assert!(!check_allowed("shutdown", &empty, &deny));
        assert!(check_allowed("inbox", &empty, &deny));
    }

    #[test]
    fn acl_allow_restricts_to_list() {
        let empty = HashSet::new();
        let allow_set = tool_set(&["inbox", "send_to_instance"]);
        assert!(check_allowed("inbox", &allow_set, &empty));
        assert!(check_allowed("send_to_instance", &allow_set, &empty));
        assert!(!check_allowed("delete_instance", &allow_set, &empty));
    }

    #[test]
    fn acl_deny_overrides_allow() {
        let allow_set = tool_set(&["inbox", "delete_instance"]);
        let deny = tool_set(&["delete_instance"]);
        assert!(check_allowed("inbox", &allow_set, &deny));
        assert!(!check_allowed("delete_instance", &allow_set, &deny));
    }

    #[test]
    fn parse_csv_handles_whitespace_and_empty() {
        let parsed = parse_csv("  foo , bar ,, baz ");
        assert_eq!(parsed.len(), 3);
        assert!(parsed.contains("foo"));
        assert!(parsed.contains("bar"));
        assert!(parsed.contains("baz"));
    }

    #[test]
    fn read_message_non_json_line_skipped_then_eof() {
        // Non-JSON lines (like Content-Length headers) are skipped.
        // EOF after non-JSON → None.
        let input = b"Content-Length: garbage\n";
        let mut reader = io::BufReader::new(&input[..]);
        let result = read_message(&mut reader).expect("should not error");
        assert!(result.is_none(), "expected None on EOF after non-JSON line");
    }

    #[test]
    fn read_message_non_json_lines_skipped_json_parsed() {
        // Non-JSON lines skipped, JSON line parsed.
        let input = b"Content-Length: xyz\n\n{\"ok\":true}\n";
        let mut reader = io::BufReader::new(&input[..]);
        let result = read_message(&mut reader).expect("should not error");
        assert_eq!(result.as_deref(), Some("{\"ok\":true}"));
    }
}
