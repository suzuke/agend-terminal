//! MCP stdio server — Content-Length framed JSON-RPC 2.0.
//!
//! Translates MCP tool calls to agent PTY writes via TUI socket.
//! Runs synchronously (no tokio needed).

pub mod handlers;
pub mod tools;

use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{self, BufRead, BufReader, Read, Write};

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

/// Read a message from stdin — supports both NDJSON (Claude Code) and Content-Length framing.
/// Auto-detects format: if first non-empty char is '{', it's NDJSON. Otherwise Content-Length.
fn read_message(reader: &mut BufReader<io::StdinLock>) -> anyhow::Result<Option<String>> {
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
        // NDJSON: line starts with '{'
        if trimmed.starts_with('{') {
            return Ok(Some(trimmed.to_string()));
        }
        // Content-Length framing
        if let Some(val) = trimmed.strip_prefix("Content-Length:") {
            let len: usize = val.trim().parse().unwrap_or(0);
            if len == 0 {
                continue;
            }
            // Read empty line after headers
            let mut empty = String::new();
            reader.read_line(&mut empty)?;
            // Read body
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body)?;
            return Ok(Some(String::from_utf8(body)?));
        }
    }
}

/// Write a message — NDJSON format (one JSON per line, like Claude expects).
fn write_message(stdout: &mut io::Stdout, json: &str) -> anyhow::Result<()> {
    writeln!(stdout, "{json}")?;
    stdout.flush()?;
    Ok(())
}

pub fn run(agent_socket: &str) -> anyhow::Result<()> {
    let instance_name = std::env::var("AGEND_INSTANCE_NAME").unwrap_or_default();
    eprintln!("[mcp] server starting for '{instance_name}'");

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
                eprintln!("[mcp] invalid JSON-RPC: {e}");
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
                json!({ "jsonrpc": "2.0", "id": id, "result": tools::tool_definitions() })
            }
            "tools/call" => {
                let tool = req.params["name"].as_str().unwrap_or("");
                let args = &req.params["arguments"];

                // Try daemon proxy first — avoids per-process overhead
                let result = proxy_or_local(tool, args, &instance_name, agent_socket);

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

    eprintln!("[mcp] server exiting");
    Ok(())
}

/// Try to proxy a tool call through the daemon API socket.
/// Falls back to local handling if the daemon is unavailable.
fn proxy_or_local(tool: &str, args: &Value, instance_name: &str, agent_socket: &str) -> Value {
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
    handlers::handle_tool(tool, args, agent_socket, instance_name)
}
