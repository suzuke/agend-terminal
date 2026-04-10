//! MCP stdio server — Content-Length framed JSON-RPC 2.0.
//!
//! Translates MCP tool calls to agent PTY writes via TUI socket.
//! Runs synchronously (no tokio needed).

use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{self, BufRead, BufReader, Read, Write};
use teloxide::prelude::*;

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
            },
            {
                "name": "create_instance",
                "description": "Create a new agent instance.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Instance name" },
                        "command": { "type": "string", "description": "Command (e.g. claude, codex)" },
                        "args": { "type": "string", "description": "Space-separated args" },
                        "working_directory": { "type": "string", "description": "Working directory" }
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
                        "name": { "type": "string", "description": "Instance name" }
                    },
                    "required": ["name"]
                }
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

fn handle_tool(tool: &str, args: &Value, _agent_socket: &str) -> Value {
    let home = crate::home_dir();
    let instance_name = std::env::var("AGEND_INSTANCE_NAME").unwrap_or_default();

    match tool {
        "reply" => {
            let text = args["text"].as_str().unwrap_or("");
            eprintln!("[mcp] reply from {instance_name}: {text}");

            // Try Telegram if state file exists
            let tg_state_path = home.join("telegram.state");
            if tg_state_path.exists() {
                // Telegram is configured — try to send via short-lived runtime
                match try_telegram_reply(&instance_name, text) {
                    Ok(()) => json!({"status": "sent_to_telegram"}),
                    Err(e) => json!({"status": "logged_only", "error": format!("{e}")}),
                }
            } else {
                json!({"status": "logged_only", "note": "Telegram not connected"})
            }
        }
        "send" => {
            let target = match args["target"].as_str() {
                Some(t) => t,
                None => return json!({"error": "missing 'target'"}),
            };
            let text = args["text"].as_str().unwrap_or("");

            // Route through daemon API socket for logging/visibility
            match crate::api::call(&home, &json!({
                "method": "send",
                "params": {
                    "from": instance_name,
                    "target": target,
                    "text": text,
                    "kind": args.get("kind").and_then(|v| v.as_str()),
                }
            })) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => {
                    json!({"status": "sent", "target": target})
                }
                Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("send failed")}),
                Err(e) => {
                    // Fallback: direct delivery if API socket not available
                    let submit_key = get_submit_key(&home, target);
                    crate::inbox::deliver(&home, target, &format!("from:{instance_name}"), text, &submit_key, None);
                    json!({"status": "sent_direct", "target": target, "note": format!("API unavailable: {e}")})
                }
            }
        }
        "inbox" => {
            let messages = crate::inbox::drain(&home, &instance_name);
            json!({"messages": messages})
        }
        "list_instances" => {
            let agents = list_agents();
            json!({"instances": agents})
        }
        "create_instance" => {
            let name = match args["name"].as_str() {
                Some(n) => n,
                None => return json!({"error": "missing 'name'"}),
            };
            let command = match args["command"].as_str() {
                Some(c) => c,
                None => return json!({"error": "missing 'command'"}),
            };
            let cmd_args = args.get("args").and_then(|v| v.as_str()).unwrap_or("");
            let work_dir = args
                .get("working_directory")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| home.join("workspaces").join(name).display().to_string());

            // Generate instructions + MCP config
            let wd = std::path::PathBuf::from(&work_dir);
            std::fs::create_dir_all(&wd).ok();
            crate::instructions::generate(&wd, command);
            crate::mcp_config::configure(&wd, command);

            // Spawn via daemon API socket
            match crate::api::call(&home, &json!({
                "method": "spawn",
                "params": {
                    "name": name,
                    "command": command,
                    "args": cmd_args,
                    "working_directory": work_dir,
                }
            })) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => {
                    json!({"status": "created", "name": name})
                }
                Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("spawn failed")}),
                Err(e) => json!({"error": format!("API unavailable: {e}")}),
            }
        }
        "delete_instance" => {
            let name = match args["name"].as_str() {
                Some(n) => n,
                None => return json!({"error": "missing 'name'"}),
            };
            // Kill via daemon API socket
            match crate::api::call(&home, &json!({"method": "kill", "params": {"name": name}})) {
                Ok(_) => {}
                Err(_) => {}
            }
            let pending = home.join("pending").join(format!("{name}.json"));
            let _ = std::fs::remove_file(&pending);
            json!({"status": "deleted", "name": name, "note": "Socket removed. Process will exit when PTY closes."})
        }
        _ => json!({"error": format!("unknown tool: {tool}")}),
    }
}

fn try_telegram_reply(instance_name: &str, text: &str) -> anyhow::Result<()> {
    // Read Telegram config to send reply
    // This is a simplified approach — in production, the daemon would hold the Telegram state
    let home = crate::home_dir();
    let fleet_path = home.join("fleet.yaml");
    if !fleet_path.exists() {
        anyhow::bail!("No fleet.yaml");
    }
    let config = crate::fleet::FleetConfig::load(&fleet_path)?;

    match &config.channel {
        Some(crate::fleet::ChannelConfig::Telegram { bot_token_env, group_id, .. }) => {
            let token = std::env::var(bot_token_env)?;
            let topic_id = config.instances.get(instance_name)
                .and_then(|inst| inst.topic_id);

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(async {
                let bot = teloxide::Bot::new(&token);
                let chat_id = teloxide::types::ChatId(*group_id);
                if let Some(tid) = topic_id {
                    if tid == 1 {
                        bot.send_message(chat_id, text).await?;
                    } else {
                        bot.send_message(chat_id, text)
                            .message_thread_id(teloxide::types::ThreadId(teloxide::types::MessageId(tid)))
                            .await?;
                    }
                }
                Ok::<(), anyhow::Error>(())
            })?;
            Ok(())
        }
        None => anyhow::bail!("No Telegram channel configured"),
    }
}

/// Look up submit_key for a target instance from fleet config.
fn get_submit_key(home: &std::path::Path, target: &str) -> String {
    let fleet_path = home.join("fleet.yaml");
    if let Ok(config) = crate::fleet::FleetConfig::load(&fleet_path) {
        if let Some(resolved) = config.resolve_instance(target) {
            return resolved.submit_key;
        }
    }
    "\r".to_string()
}
