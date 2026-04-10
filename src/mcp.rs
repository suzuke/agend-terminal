//! MCP (Model Context Protocol) stdio server.
//! Translates JSON-RPC requests to daemon UDS protocol.

use crate::protocol::{self, Request, Response};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing::{error, info};

// --- JSON-RPC types ---

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

// --- MCP tool definitions ---

fn tool_definitions() -> Value {
    json!({
        "tools": [
            {
                "name": "reply",
                "description": "Reply to the user who sent you a message. Use this to respond to [user:... via telegram] messages.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "The reply text to send back to the user"
                        }
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
                        "target": {
                            "type": "string",
                            "description": "Name of the target instance (e.g., 'general', 'blog-writer')"
                        },
                        "text": {
                            "type": "string",
                            "description": "Message text to send"
                        },
                        "kind": {
                            "type": "string",
                            "enum": ["query", "task", "report", "update"],
                            "description": "Message type/intent"
                        },
                        "correlation_id": {
                            "type": "string",
                            "description": "ID to link request-response pairs"
                        }
                    },
                    "required": ["target", "text"]
                }
            },
            {
                "name": "inbox",
                "description": "Check and retrieve pending messages from other agents or users.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "list_instances",
                "description": "List all active agent instances in the fleet.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "create_instance",
                "description": "Create a new agent instance dynamically.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Instance name"
                        },
                        "command": {
                            "type": "string",
                            "description": "Command to run (e.g., 'claude', 'codex')"
                        },
                        "args": {
                            "type": "string",
                            "description": "Space-separated command arguments"
                        },
                        "working_directory": {
                            "type": "string",
                            "description": "Working directory path"
                        },
                        "topic_name": {
                            "type": "string",
                            "description": "Telegram topic name (defaults to instance name)"
                        }
                    },
                    "required": ["name", "command"]
                }
            },
            {
                "name": "delete_instance",
                "description": "Stop and remove an agent instance.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Instance name to delete"
                        }
                    },
                    "required": ["name"]
                }
            }
        ]
    })
}

// --- Daemon communication ---

async fn send_to_daemon(socket_path: &Path, req: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .context("Failed to connect to daemon")?;

    let json = serde_json::to_vec(req)?;
    let frame = protocol::encode(&json);
    stream.write_all(&frame).await?;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    let resp: Response = serde_json::from_slice(&buf)?;
    Ok(resp)
}

// --- Tool handlers ---

async fn handle_tool_call(
    socket_path: &Path,
    session_id: u32,
    tool_name: &str,
    args: &Value,
) -> Result<Value> {
    match tool_name {
        "reply" => {
            let text = args["text"].as_str().context("Missing 'text'")?;
            let resp = send_to_daemon(
                socket_path,
                &Request::Reply {
                    session_id,
                    text: text.to_string(),
                },
            )
            .await?;
            match resp {
                Response::Sent => Ok(json!({"status": "sent"})),
                Response::Error { message } => Ok(json!({"error": message})),
                _ => Ok(json!({"error": "unexpected response"})),
            }
        }
        "send" => {
            let target = args["target"].as_str().context("Missing 'target'")?;
            let text = args["text"].as_str().context("Missing 'text'")?;
            let kind = args.get("kind").and_then(|v| v.as_str()).map(String::from);
            let correlation_id = args
                .get("correlation_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            let resp = send_to_daemon(
                socket_path,
                &Request::SendMessage {
                    session_id,
                    target: target.to_string(),
                    text: text.to_string(),
                    kind,
                    correlation_id,
                },
            )
            .await?;
            match resp {
                Response::Sent => Ok(json!({"status": "sent"})),
                Response::Error { message } => Ok(json!({"error": message})),
                _ => Ok(json!({"error": "unexpected response"})),
            }
        }
        "inbox" => {
            let resp =
                send_to_daemon(socket_path, &Request::Inbox { session_id }).await?;
            match resp {
                Response::Messages { messages } => {
                    Ok(json!({"messages": messages}))
                }
                Response::Error { message } => Ok(json!({"error": message})),
                _ => Ok(json!({"error": "unexpected response"})),
            }
        }
        "list_instances" => {
            let resp = send_to_daemon(socket_path, &Request::List).await?;
            match resp {
                Response::Sessions { sessions } => {
                    let instances: Vec<Value> = sessions
                        .iter()
                        .map(|s| {
                            json!({
                                "id": s.id,
                                "name": s.name,
                                "command": s.command,
                                "running": s.running,
                                "ready": s.ready,
                            })
                        })
                        .collect();
                    Ok(json!({"instances": instances}))
                }
                Response::Error { message } => Ok(json!({"error": message})),
                _ => Ok(json!({"error": "unexpected response"})),
            }
        }
        "create_instance" => {
            let name = args["name"].as_str().context("Missing 'name'")?;
            let command = args["command"].as_str().context("Missing 'command'")?;
            let cmd_args: Vec<String> = args
                .get("args")
                .and_then(|v| v.as_str())
                .map(|s| s.split_whitespace().map(String::from).collect())
                .unwrap_or_default();
            let working_directory = args
                .get("working_directory")
                .and_then(|v| v.as_str())
                .map(String::from);
            let topic_name = args
                .get("topic_name")
                .and_then(|v| v.as_str())
                .map(String::from);

            let resp = send_to_daemon(
                socket_path,
                &Request::CreateInstance {
                    name: name.to_string(),
                    command: command.to_string(),
                    args: cmd_args,
                    env: None,
                    working_directory,
                    topic_name,
                    ready_pattern: None,
                    cols: None,
                    rows: None,
                },
            )
            .await?;
            match resp {
                Response::InstanceCreated {
                    name,
                    session_id,
                    topic_id,
                } => Ok(json!({
                    "status": "created",
                    "name": name,
                    "session_id": session_id,
                    "topic_id": topic_id,
                })),
                Response::Error { message } => Ok(json!({"error": message})),
                _ => Ok(json!({"error": "unexpected response"})),
            }
        }
        "delete_instance" => {
            let name = args["name"].as_str().context("Missing 'name'")?;
            // Find session by name first via list, then kill
            let list_resp = send_to_daemon(socket_path, &Request::List).await?;
            if let Response::Sessions { sessions } = list_resp {
                let session = sessions.iter().find(|s| {
                    s.name.as_deref() == Some(name)
                });
                match session {
                    Some(s) => {
                        let kill_resp = send_to_daemon(
                            socket_path,
                            &Request::Kill {
                                session_id: s.id,
                                quit_command: None,
                                grace_seconds: Some(5),
                            },
                        )
                        .await?;
                        match kill_resp {
                            Response::Killed { session_id } => {
                                Ok(json!({"status": "deleted", "name": name, "session_id": session_id}))
                            }
                            Response::Error { message } => Ok(json!({"error": message})),
                            _ => Ok(json!({"error": "unexpected response"})),
                        }
                    }
                    None => Ok(json!({"error": format!("Instance '{name}' not found")})),
                }
            } else {
                Ok(json!({"error": "failed to list instances"}))
            }
        }
        _ => Ok(json!({"error": format!("Unknown tool: {tool_name}")})),
    }
}

// --- Main MCP server loop ---

pub async fn run(socket_path: &Path) -> Result<()> {
    // Resolve session_id: try AGEND_SESSION_ID first, then lookup by AGEND_INSTANCE_NAME
    let session_id: u32 = if let Ok(id) = std::env::var("AGEND_SESSION_ID") {
        id.parse().context("Invalid AGEND_SESSION_ID")?
    } else if let Ok(name) = std::env::var("AGEND_INSTANCE_NAME") {
        // Resolve instance name → session_id via daemon List
        let resp = send_to_daemon(socket_path, &Request::List).await?;
        if let Response::Sessions { sessions } = resp {
            sessions
                .iter()
                .find(|s| s.name.as_deref() == Some(&name))
                .map(|s| s.id)
                .with_context(|| format!("Instance '{name}' not found in daemon"))?
        } else {
            anyhow::bail!("Failed to list sessions for name resolution");
        }
    } else {
        anyhow::bail!(
            "Neither AGEND_SESSION_ID nor AGEND_INSTANCE_NAME set — \
             MCP server must run inside an agend-terminal session"
        );
    };

    info!("MCP server starting (session_id: {session_id})");

    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = io::stdout();

    loop {
        // Read Content-Length framed message from stdin
        let body = match read_content_length_message(&mut reader) {
            Ok(Some(b)) => b,
            Ok(None) => break, // EOF
            Err(e) => {
                error!("Read error: {e}");
                break;
            }
        };

        let req: JsonRpcRequest = match serde_json::from_str(&body) {
            Ok(r) => r,
            Err(e) => {
                error!("Invalid JSON-RPC: {e}");
                continue;
            }
        };

        let id = req.id.clone().unwrap_or(Value::Null);

        let response = match req.method.as_str() {
            "initialize" => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "agend-terminal",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                })),
                error: None,
            },
            "notifications/initialized" => continue,
            "ping" => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(json!({})),
                error: None,
            },
            "tools/list" => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(tool_definitions()),
                error: None,
            },
            "tools/call" => {
                let tool_name = req.params["name"]
                    .as_str()
                    .unwrap_or("");
                let arguments = req.params.get("arguments").cloned().unwrap_or(json!({}));

                match handle_tool_call(socket_path, session_id, tool_name, &arguments).await
                {
                    Ok(result) => {
                        let is_error = result.get("error").is_some();
                        JsonRpcResponse {
                            jsonrpc: "2.0".to_string(),
                            id,
                            result: Some(json!({
                                "content": [{
                                    "type": "text",
                                    "text": serde_json::to_string_pretty(&result).unwrap_or_default()
                                }],
                                "isError": is_error
                            })),
                            error: None,
                        }
                    }
                    Err(e) => JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id,
                        result: Some(json!({
                            "content": [{
                                "type": "text",
                                "text": format!("Error: {e}")
                            }],
                            "isError": true
                        })),
                        error: None,
                    },
                }
            }
            method => {
                if method.starts_with("notifications/") {
                    continue; // Ignore notifications
                }
                JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id,
                    error: Some(JsonRpcError {
                        code: -32601,
                        message: format!("Method not found: {method}"),
                    }),
                    result: None,
                }
            }
        };

        let json = serde_json::to_string(&response)?;
        write_content_length_message(&mut stdout, &json)?;
    }

    info!("MCP server exiting");
    Ok(())
}

/// Read a Content-Length framed message from a BufReader.
/// Format: `Content-Length: N\r\n\r\n{json body of N bytes}`
fn read_content_length_message(reader: &mut BufReader<io::StdinLock>) -> Result<Option<String>> {
    // Read headers until empty line
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(None); // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break; // End of headers
        }
        if let Some(val) = trimmed.strip_prefix("Content-Length:") {
            content_length = val.trim().parse().ok();
        }
    }

    let len = content_length.context("Missing Content-Length header")?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    let s = String::from_utf8(body).context("Invalid UTF-8 in message body")?;
    Ok(Some(s))
}

/// Write a Content-Length framed message to stdout.
fn write_content_length_message(stdout: &mut io::Stdout, json: &str) -> Result<()> {
    write!(stdout, "Content-Length: {}\r\n\r\n{}", json.len(), json)?;
    stdout.flush()?;
    Ok(())
}
