//! MCP stdio server — minimal implementation for backwards compatibility.
//!
//! Tool calls are deprecated in favor of `agend-terminal agent` CLI commands.
//! This server only handles protocol handshake (initialize, ping, tools/list).

pub mod telegram;

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

/// Read a message from stdin — supports both NDJSON and Content-Length framing.
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
        if trimmed.starts_with('{') {
            return Ok(Some(trimmed.to_string()));
        }
        if let Some(val) = trimmed.strip_prefix("Content-Length:") {
            let len: usize = val.trim().parse().unwrap_or(0);
            if len == 0 {
                continue;
            }
            let mut empty = String::new();
            reader.read_line(&mut empty)?;
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body)?;
            return Ok(Some(String::from_utf8(body)?));
        }
    }
}

fn write_message(stdout: &mut io::Stdout, json: &str) -> anyhow::Result<()> {
    writeln!(stdout, "{json}")?;
    stdout.flush()?;
    Ok(())
}

pub fn run(_agent_socket: &str) -> anyhow::Result<()> {
    let instance_name = std::env::var("AGEND_INSTANCE_NAME").unwrap_or_default();
    eprintln!("[mcp] server starting for '{instance_name}' (tools deprecated — use `agend-terminal agent` CLI)");

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
                // Return empty tools list — agents use CLI now
                json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [] } })
            }
            "tools/call" => {
                // Deprecated: tell agent to use CLI instead
                let tool = req.params["name"].as_str().unwrap_or("?");
                json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": format!(
                            "MCP tools are deprecated. Use CLI instead: agend-terminal agent {tool} (run `agend-terminal agent --help` for commands)"
                        )}],
                        "isError": true
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
