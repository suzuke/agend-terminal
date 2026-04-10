//! MCP stdio server — Content-Length framed JSON-RPC 2.0.
//!
//! Translates MCP tool calls to agent PTY writes via TUI socket.
//! Runs synchronously (no tokio needed).

use crate::framing;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

fn tool_definitions() -> Value {
    json!({
        "tools": [
            {
                "name": "reply",
                "description": "Reply to the user who sent you a message via Telegram.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "Reply text" }
                    },
                    "required": ["text"]
                }
            },
            {
                "name": "send",
                "description": "Send a message to another agent instance.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "target": { "type": "string", "description": "Target instance name" },
                        "text": { "type": "string", "description": "Message text" },
                        "kind": { "type": "string", "enum": ["query", "task", "report", "update"] }
                    },
                    "required": ["target", "text"]
                }
            },
            {
                "name": "inbox",
                "description": "Check pending messages.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "list_instances",
                "description": "List all active agent instances.",
                "inputSchema": { "type": "object", "properties": {} }
            }
        ]
    })
}

/// Read a Content-Length framed message.
fn read_message(reader: &mut BufReader<io::StdinLock>) -> anyhow::Result<Option<String>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(val) = trimmed.strip_prefix("Content-Length:") {
            content_length = val.trim().parse().ok();
        }
    }
    let len = match content_length {
        Some(l) => l,
        None => return Ok(None),
    };
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Ok(Some(String::from_utf8(body)?))
}

/// Write a Content-Length framed message.
fn write_message(stdout: &mut io::Stdout, json: &str) -> anyhow::Result<()> {
    write!(stdout, "Content-Length: {}\r\n\r\n{}", json.len(), json)?;
    stdout.flush()?;
    Ok(())
}

/// Write data to agent's PTY via TUI socket.
fn write_to_agent(socket_path: &str, data: &[u8]) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(socket_path)?;
    framing::write_frame(&mut stream, data)?;
    Ok(())
}

/// List agent sockets in home directory.
fn list_agents() -> Vec<String> {
    let home = crate::home_dir();
    let mut agents = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&home) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".sock") {
                agents.push(name[..name.len() - 5].to_string());
            }
        }
    }
    agents
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
            "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": tool_definitions() }),
            "tools/call" => {
                let tool = req.params["name"].as_str().unwrap_or("");
                let args = &req.params["arguments"];
                let result = handle_tool(tool, args, agent_socket);
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

fn handle_tool(tool: &str, args: &Value, agent_socket: &str) -> Value {
    match tool {
        "reply" => {
            let text = args["text"].as_str().unwrap_or("");
            // For now, log the reply. Telegram integration will route it.
            eprintln!("[mcp] reply: {text}");
            json!({"status": "sent"})
        }
        "send" => {
            let target = match args["target"].as_str() {
                Some(t) => t,
                None => return json!({"error": "missing 'target'"}),
            };
            let text = args["text"].as_str().unwrap_or("");
            let home = crate::home_dir();
            let target_sock = crate::daemon::agent_socket_path(&home, target);
            let instance_name = std::env::var("AGEND_INSTANCE_NAME").unwrap_or_default();
            let msg = format!("[from:{instance_name}] {text}\r");
            match write_to_agent(&target_sock, msg.as_bytes()) {
                Ok(()) => json!({"status": "sent", "target": target}),
                Err(e) => json!({"error": format!("send failed: {e}")}),
            }
        }
        "inbox" => {
            // Placeholder — will be implemented with message queue
            json!({"messages": []})
        }
        "list_instances" => {
            let agents = list_agents();
            json!({"instances": agents})
        }
        _ => json!({"error": format!("unknown tool: {tool}")}),
    }
}
