//! MCP stdio server — Content-Length framed JSON-RPC 2.0.
//!
//! Translates MCP tool calls to agent PTY writes via TUI socket.
//! Runs synchronously (no tokio needed).

use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{self, BufRead, BufReader, Read, Write};
use teloxide::prelude::*;
use teloxide::net::Download;

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
    let mut tools = Vec::new();
    tools.extend(channel_tools());
    tools.extend(comm_tools());
    tools.extend(instance_tools());
    tools.extend(decision_tools());
    tools.extend(task_tools());
    tools.extend(team_tools());
    tools.extend(schedule_tools());
    tools.extend(repo_tools());
    json!({"tools": tools})
}

fn channel_tools() -> Vec<Value> {
    vec![
        json!({"name": "reply", "description": "Reply to the user via Telegram.",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}),
        json!({"name": "react", "description": "React to a message with an emoji.",
            "inputSchema": {"type": "object", "properties": {"emoji": {"type": "string"}}, "required": ["emoji"]}}),
        json!({"name": "edit_message", "description": "Edit a previously sent message.",
            "inputSchema": {"type": "object", "properties": {"message_id": {"type": "string"}, "text": {"type": "string"}}, "required": ["message_id", "text"]}}),
        json!({"name": "download_attachment", "description": "Download a file attachment. Returns local path.",
            "inputSchema": {"type": "object", "properties": {"file_id": {"type": "string"}}, "required": ["file_id"]}}),
    ]
}

fn comm_tools() -> Vec<Value> {
    vec![
        json!({"name": "send_to_instance", "description": "Send a message to another instance.",
            "inputSchema": {"type": "object", "properties": {
                "instance_name": {"type": "string"}, "message": {"type": "string"},
                "request_kind": {"type": "string", "enum": ["query", "task", "report", "update"]},
                "requires_reply": {"type": "boolean"}, "task_summary": {"type": "string"},
                "correlation_id": {"type": "string"}, "working_directory": {"type": "string"}, "branch": {"type": "string"}
            }, "required": ["instance_name", "message"]}}),
        json!({"name": "delegate_task", "description": "Delegate a task to another instance (expects result report back).",
            "inputSchema": {"type": "object", "properties": {
                "target_instance": {"type": "string"}, "task": {"type": "string"},
                "success_criteria": {"type": "string"}, "context": {"type": "string"}
            }, "required": ["target_instance", "task"]}}),
        json!({"name": "report_result", "description": "Report results back to the delegating instance.",
            "inputSchema": {"type": "object", "properties": {
                "target_instance": {"type": "string"}, "summary": {"type": "string"},
                "correlation_id": {"type": "string"}, "artifacts": {"type": "string"}
            }, "required": ["target_instance", "summary"]}}),
        json!({"name": "request_information", "description": "Ask another instance a question (expects reply).",
            "inputSchema": {"type": "object", "properties": {
                "target_instance": {"type": "string"}, "question": {"type": "string"}, "context": {"type": "string"}
            }, "required": ["target_instance", "question"]}}),
        json!({"name": "broadcast", "description": "Send a message to multiple instances. Priority: team > targets > tags > all.",
            "inputSchema": {"type": "object", "properties": {
                "message": {"type": "string"}, "targets": {"type": "array", "items": {"type": "string"}},
                "team": {"type": "string"}, "tags": {"type": "array", "items": {"type": "string"}},
                "task_summary": {"type": "string"}, "request_kind": {"type": "string", "enum": ["query", "task", "update"]},
                "requires_reply": {"type": "boolean"}
            }, "required": ["message"]}}),
        json!({"name": "inbox", "description": "Check pending messages.",
            "inputSchema": {"type": "object", "properties": {}}}),
    ]
}

fn instance_tools() -> Vec<Value> {
    vec![
        json!({"name": "list_instances", "description": "List all active agent instances.",
            "inputSchema": {"type": "object", "properties": {}}}),
        json!({"name": "create_instance", "description": "Create a new agent instance.",
            "inputSchema": {"type": "object", "properties": {
                "name": {"type": "string"}, "command": {"type": "string"}, "args": {"type": "string"},
                "model": {"type": "string"}, "working_directory": {"type": "string"}
            }, "required": ["name", "command"]}}),
        json!({"name": "delete_instance", "description": "Stop and remove an instance.",
            "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}}),
        json!({"name": "start_instance", "description": "Start a stopped instance.",
            "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}}),
        json!({"name": "describe_instance", "description": "Get detailed info about an instance.",
            "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}}),
        json!({"name": "replace_instance", "description": "Replace an instance with a fresh one.",
            "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}, "reason": {"type": "string"}}, "required": ["name"]}}),
        json!({"name": "set_display_name", "description": "Set your display name.",
            "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}}),
        json!({"name": "set_description", "description": "Set a description for this instance.",
            "inputSchema": {"type": "object", "properties": {"description": {"type": "string"}}, "required": ["description"]}}),
    ]
}

fn decision_tools() -> Vec<Value> {
    vec![
        json!({"name": "post_decision", "description": "Record a decision. scope='fleet' visible to all, 'project' to same working dir.",
            "inputSchema": {"type": "object", "properties": {
                "title": {"type": "string"}, "content": {"type": "string"},
                "scope": {"type": "string", "enum": ["project", "fleet"]},
                "tags": {"type": "array", "items": {"type": "string"}},
                "ttl_days": {"type": "number"}, "supersedes": {"type": "string"}
            }, "required": ["title", "content"]}}),
        json!({"name": "list_decisions", "description": "List active decisions.",
            "inputSchema": {"type": "object", "properties": {
                "include_archived": {"type": "boolean"}, "tags": {"type": "array", "items": {"type": "string"}}
            }}}),
        json!({"name": "update_decision", "description": "Update or archive a decision.",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "string"}, "content": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "ttl_days": {"type": "number"}, "archive": {"type": "boolean"}
            }, "required": ["id"]}}),
    ]
}

fn task_tools() -> Vec<Value> {
    vec![
        json!({"name": "task", "description": "Manage task board. Actions: create, list, claim, done, update.",
            "inputSchema": {"type": "object", "properties": {
                "action": {"type": "string", "enum": ["create", "list", "claim", "done", "update"]},
                "title": {"type": "string"}, "description": {"type": "string"},
                "priority": {"type": "string", "enum": ["low", "normal", "high", "urgent"]},
                "assignee": {"type": "string"}, "depends_on": {"type": "array", "items": {"type": "string"}},
                "id": {"type": "string"}, "result": {"type": "string"},
                "status": {"type": "string", "enum": ["open", "claimed", "done", "blocked", "cancelled"]},
                "filter_assignee": {"type": "string"}, "filter_status": {"type": "string"}
            }, "required": ["action"]}}),
    ]
}

fn team_tools() -> Vec<Value> {
    vec![
        json!({"name": "create_team", "description": "Create a named group of instances for broadcast.",
            "inputSchema": {"type": "object", "properties": {
                "name": {"type": "string"}, "members": {"type": "array", "items": {"type": "string"}},
                "description": {"type": "string"}
            }, "required": ["name", "members"]}}),
        json!({"name": "delete_team", "description": "Delete a team.",
            "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}}),
        json!({"name": "list_teams", "description": "List all teams.",
            "inputSchema": {"type": "object", "properties": {}}}),
        json!({"name": "update_team", "description": "Add or remove team members.",
            "inputSchema": {"type": "object", "properties": {
                "name": {"type": "string"}, "add": {"type": "array", "items": {"type": "string"}},
                "remove": {"type": "array", "items": {"type": "string"}}
            }, "required": ["name"]}}),
    ]
}

fn schedule_tools() -> Vec<Value> {
    vec![
        json!({"name": "create_schedule", "description": "Create a cron schedule to inject messages.",
            "inputSchema": {"type": "object", "properties": {
                "cron": {"type": "string"}, "message": {"type": "string"},
                "target": {"type": "string"}, "label": {"type": "string"},
                "timezone": {"type": "string"}
            }, "required": ["cron", "message"]}}),
        json!({"name": "list_schedules", "description": "List all schedules.",
            "inputSchema": {"type": "object", "properties": {"target": {"type": "string"}}}}),
        json!({"name": "update_schedule", "description": "Update a schedule.",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "string"}, "cron": {"type": "string"}, "message": {"type": "string"},
                "target": {"type": "string"}, "label": {"type": "string"},
                "timezone": {"type": "string"}, "enabled": {"type": "boolean"}
            }, "required": ["id"]}}),
        json!({"name": "delete_schedule", "description": "Delete a schedule.",
            "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}}),
    ]
}

fn repo_tools() -> Vec<Value> {
    vec![
        json!({"name": "checkout_repo", "description": "Mount another repo as read-only worktree.",
            "inputSchema": {"type": "object", "properties": {
                "source": {"type": "string"}, "branch": {"type": "string"}
            }, "required": ["source"]}}),
        json!({"name": "release_repo", "description": "Remove a checked-out repo worktree.",
            "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}),
    ]
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
            if len == 0 { continue; }
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

/// List agent sockets in home directory.
fn list_agents() -> Vec<String> {
    let home = crate::home_dir();
    let mut agents = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&home) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".sock") && name != "api.sock" {
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
        // --- Channel ---
        "reply" => {
            let text = args["text"].as_str().unwrap_or("");
            eprintln!("[mcp] reply from {instance_name}: {text}");
            let fleet_path = home.join("fleet.yaml");
            if fleet_path.exists() {
                match try_telegram_reply(&instance_name, text) {
                    Ok(()) => json!({"status": "sent_to_telegram"}),
                    Err(e) => json!({"status": "logged_only", "error": format!("{e}")}),
                }
            } else {
                json!({"status": "logged_only", "note": "No fleet.yaml"})
            }
        }
        "react" => {
            let emoji = args["emoji"].as_str().unwrap_or("");
            eprintln!("[mcp] react from {instance_name}: {emoji}");
            json!({"status": "logged", "emoji": emoji, "note": "Telegram react not yet implemented"})
        }
        "edit_message" => {
            let message_id = args["message_id"].as_str().unwrap_or("");
            eprintln!("[mcp] edit_message from {instance_name}: {message_id}");
            json!({"status": "logged", "message_id": message_id, "note": "Telegram edit not yet implemented"})
        }
        "download_attachment" => {
            let file_id = match args["file_id"].as_str() {
                Some(f) => f,
                None => return json!({"error": "missing 'file_id'"}),
            };
            // Download via Telegram Bot API
            match try_download_attachment(&instance_name, file_id) {
                Ok(path) => json!({"path": path}),
                Err(e) => json!({"error": format!("{e}")}),
            }
        }

        // --- Cross-instance communication ---
        "send_to_instance" | "send" => {
            let target = args["instance_name"].as_str()
                .or_else(|| args["target"].as_str());
            let target = match target {
                Some(t) => t,
                None => return json!({"error": "missing 'instance_name' or 'target'"}),
            };
            let text = args["message"].as_str()
                .or_else(|| args["text"].as_str())
                .unwrap_or("");
            let kind = args["request_kind"].as_str()
                .or_else(|| args["kind"].as_str());

            match crate::api::call(&home, &json!({
                "method": "send",
                "params": {
                    "from": instance_name,
                    "target": target,
                    "text": text,
                    "kind": kind,
                }
            })) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => {
                    json!({"sent": true, "target": target})
                }
                Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("send failed")}),
                Err(e) => {
                    let submit_key = get_submit_key(&home, target);
                    crate::inbox::deliver(&home, target, &format!("from:{instance_name}"), text, &submit_key, None);
                    json!({"sent": true, "target": target, "note": format!("API unavailable, sent direct: {e}")})
                }
            }
        }
        "delegate_task" => {
            let target = match args["target_instance"].as_str() {
                Some(t) => t,
                None => return json!({"error": "missing 'target_instance'"}),
            };
            let task = match args["task"].as_str() {
                Some(t) => t,
                None => return json!({"error": "missing 'task'"}),
            };
            let mut msg = format!("[delegate_task] {task}");
            if let Some(criteria) = args["success_criteria"].as_str() {
                msg.push_str(&format!("\n\nSuccess criteria: {criteria}"));
            }
            if let Some(ctx) = args["context"].as_str() {
                msg.push_str(&format!("\n\nContext: {ctx}"));
            }
            send_to(&home, &instance_name, target, &msg, "task")
        }
        "report_result" => {
            let target = match args["target_instance"].as_str() {
                Some(t) => t,
                None => return json!({"error": "missing 'target_instance'"}),
            };
            let summary = match args["summary"].as_str() {
                Some(s) => s,
                None => return json!({"error": "missing 'summary'"}),
            };
            let mut msg = format!("[report_result] {summary}");
            if let Some(cid) = args["correlation_id"].as_str() {
                msg.push_str(&format!("\ncorrelation_id: {cid}"));
            }
            if let Some(artifacts) = args["artifacts"].as_str() {
                msg.push_str(&format!("\nArtifacts: {artifacts}"));
            }
            send_to(&home, &instance_name, target, &msg, "report")
        }
        "request_information" => {
            let target = match args["target_instance"].as_str() {
                Some(t) => t,
                None => return json!({"error": "missing 'target_instance'"}),
            };
            let question = match args["question"].as_str() {
                Some(q) => q,
                None => return json!({"error": "missing 'question'"}),
            };
            let mut msg = format!("[request_information] {question}");
            if let Some(ctx) = args["context"].as_str() {
                msg.push_str(&format!("\n\nContext: {ctx}"));
            }
            send_to(&home, &instance_name, target, &msg, "query")
        }
        "broadcast" => {
            let message = match args["message"].as_str() {
                Some(m) => m,
                None => return json!({"error": "missing 'message'"}),
            };
            // Resolve targets: team > targets > tags > all
            let targets: Vec<String> = if let Some(team) = args["team"].as_str() {
                crate::teams::get_members(&home, team)
            } else if let Some(t) = args["targets"].as_array() {
                t.iter().filter_map(|v| v.as_str().map(String::from)).collect()
            } else {
                list_agents()
            };
            let targets: Vec<String> = targets.into_iter()
                .filter(|t| *t != instance_name)
                .collect();
            let kind = args["request_kind"].as_str().unwrap_or("update");
            let mut sent = Vec::new();
            for target in &targets {
                let _ = send_to(&home, &instance_name, target, message, kind);
                sent.push(target.clone());
            }
            json!({"sent_to": sent, "count": sent.len()})
        }
        "inbox" => {
            let messages = crate::inbox::drain(&home, &instance_name);
            json!({"messages": messages})
        }

        // --- Instance management ---
        "list_instances" => {
            match crate::api::call(&home, &json!({"method": "list"})) {
                Ok(resp) => {
                    if let Some(agents) = resp["result"]["agents"].as_array() {
                        let instances: Vec<Value> = agents.iter().map(|a| {
                            let name = a["name"].as_str().unwrap_or("");
                            let meta_path = home.join("metadata").join(format!("{name}.json"));
                            let meta = std::fs::read_to_string(&meta_path)
                                .and_then(|c| Ok(serde_json::from_str::<Value>(&c).unwrap_or(json!({}))))
                                .unwrap_or(json!({}));
                            let mut info = a.clone();
                            if let Some(obj) = info.as_object_mut() {
                                if let Some(m) = meta.as_object() {
                                    for (k, v) in m { obj.insert(k.clone(), v.clone()); }
                                }
                            }
                            info
                        }).collect();
                        json!({"instances": instances})
                    } else {
                        json!({"instances": list_agents()})
                    }
                }
                Err(_) => json!({"instances": list_agents()}),
            }
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
            let mut cmd_args = args.get("args").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if let Some(model) = args.get("model").and_then(|v| v.as_str()) {
                if !model.is_empty() {
                    if !cmd_args.is_empty() { cmd_args.push(' '); }
                    cmd_args.push_str(&format!("--model {model}"));
                }
            }
            let work_dir = args.get("working_directory").and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| home.join("workspaces").join(name).display().to_string());
            let wd = std::path::PathBuf::from(&work_dir);
            std::fs::create_dir_all(&wd).ok();
            crate::instructions::generate(&wd, command);
            crate::mcp_config::configure(&wd, command);
            match crate::api::call(&home, &json!({
                "method": "spawn",
                "params": { "name": name, "command": command, "args": &cmd_args, "working_directory": work_dir }
            })) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => json!({"status": "created", "name": name}),
                Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("spawn failed")}),
                Err(e) => json!({"error": format!("API unavailable: {e}")}),
            }
        }
        "delete_instance" => {
            let name = match args["name"].as_str() {
                Some(n) => n,
                None => return json!({"error": "missing 'name'"}),
            };
            let _ = crate::api::call(&home, &json!({"method": "kill", "params": {"name": name}}));
            json!({"status": "deleted", "name": name})
        }
        "start_instance" => {
            let name = match args["name"].as_str() {
                Some(n) => n,
                None => return json!({"error": "missing 'name'"}),
            };
            // Re-spawn from saved config
            let fleet_path = home.join("fleet.yaml");
            if !fleet_path.exists() {
                return json!({"error": "No fleet.yaml — cannot determine instance config"});
            }
            let config = match crate::fleet::FleetConfig::load(&fleet_path) {
                Ok(c) => c,
                Err(e) => return json!({"error": format!("fleet.yaml parse error: {e}")}),
            };
            match config.resolve_instance(name) {
                Some(resolved) => {
                    let mut cmd_args = resolved.args.join(" ");
                    if let Some(ref b) = crate::backend::Backend::from_command(&resolved.command) {
                        let p = b.preset();
                        let resume_args = p.resume_mode.args_for(&home, name);
                        if !resume_args.is_empty() {
                            if !cmd_args.is_empty() { cmd_args.push(' '); }
                            cmd_args.push_str(&resume_args.join(" "));
                        }
                    }
                    match crate::api::call(&home, &json!({
                        "method": "spawn",
                        "params": {
                            "name": name, "command": resolved.command,
                            "args": cmd_args,
                            "working_directory": resolved.working_directory.map(|p| p.display().to_string()),
                        }
                    })) {
                        Ok(resp) if resp["ok"].as_bool() == Some(true) => json!({"status": "started", "name": name}),
                        Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("spawn failed")}),
                        Err(e) => json!({"error": format!("API unavailable: {e}")}),
                    }
                }
                None => json!({"error": format!("Instance '{name}' not in fleet.yaml")}),
            }
        }
        "describe_instance" => {
            let name = args["name"].as_str().unwrap_or("");
            match crate::api::call(&home, &json!({"method": "list"})) {
                Ok(resp) => {
                    if let Some(agents) = resp["result"]["agents"].as_array() {
                        match agents.iter().find(|a| a["name"].as_str() == Some(name)) {
                            Some(agent) => {
                                let meta_path = home.join("metadata").join(format!("{name}.json"));
                                let meta = std::fs::read_to_string(&meta_path)
                                    .and_then(|c| Ok(serde_json::from_str::<Value>(&c).unwrap_or(json!({}))))
                                    .unwrap_or(json!({}));
                                let mut info = agent.clone();
                                if let Some(obj) = info.as_object_mut() {
                                    if let Some(m) = meta.as_object() {
                                        for (k, v) in m { obj.insert(k.clone(), v.clone()); }
                                    }
                                }
                                json!({"instance": info})
                            }
                            None => json!({"error": format!("Instance '{name}' not found")}),
                        }
                    } else {
                        json!({"error": "failed to list instances"})
                    }
                }
                Err(e) => json!({"error": format!("API unavailable: {e}")}),
            }
        }
        "replace_instance" => {
            let name = match args["name"].as_str() {
                Some(n) => n,
                None => return json!({"error": "missing 'name'"}),
            };
            let reason = args["reason"].as_str().unwrap_or("manual replacement");
            // Kill old instance, respawn will handle the rest
            let _ = crate::api::call(&home, &json!({"method": "kill", "params": {"name": name}}));
            eprintln!("[mcp] replace_instance {name}: {reason}");
            json!({"status": "replacing", "name": name, "reason": reason,
                   "note": "Instance killed. Auto-respawn will create fresh instance."})
        }
        "set_display_name" => {
            let display_name = args["name"].as_str().unwrap_or("");
            save_metadata(&home, &instance_name, "display_name", json!(display_name));
            json!({"display_name": display_name})
        }
        "set_description" => {
            let desc = args["description"].as_str().unwrap_or("");
            save_metadata(&home, &instance_name, "description", json!(desc));
            json!({"description": desc})
        }

        // --- Decisions ---
        "post_decision" => crate::decisions::post(&home, &instance_name, args),
        "list_decisions" => crate::decisions::list(&home, args),
        "update_decision" => crate::decisions::update(&home, args),

        // --- Task board ---
        "task" => crate::tasks::handle(&home, &instance_name, args),

        // --- Teams ---
        "create_team" => crate::teams::create(&home, args),
        "delete_team" => crate::teams::delete(&home, args),
        "list_teams" => crate::teams::list(&home),
        "update_team" => crate::teams::update(&home, args),

        // --- Scheduling ---
        "create_schedule" => crate::schedules::create(&home, &instance_name, args),
        "list_schedules" => crate::schedules::list(&home, args),
        "update_schedule" => crate::schedules::update(&home, args),
        "delete_schedule" => crate::schedules::delete(&home, args),

        // --- Repo access ---
        "checkout_repo" => {
            let source = match args["source"].as_str() {
                Some(s) => s,
                None => return json!({"error": "missing 'source'"}),
            };
            let branch = args["branch"].as_str().unwrap_or("HEAD");
            let worktree_dir = home.join("worktrees").join(format!("{}-{}", instance_name,
                source.replace('/', "_").replace('~', "")));
            std::fs::create_dir_all(worktree_dir.parent().unwrap_or(&home)).ok();
            let source_path = if source.starts_with('/') || source.starts_with('~') {
                if let Some(rest) = source.strip_prefix("~/") {
                    let h = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                    format!("{h}/{rest}")
                } else {
                    source.to_string()
                }
            } else {
                // Instance name — look up working directory
                match crate::api::call(&home, &json!({"method": "list"})) {
                    Ok(resp) => {
                        resp["result"]["agents"].as_array()
                            .and_then(|agents| agents.iter()
                                .find(|a| a["name"].as_str() == Some(source))
                                .and_then(|a| a["working_directory"].as_str().map(String::from)))
                            .unwrap_or_else(|| source.to_string())
                    }
                    Err(_) => source.to_string(),
                }
            };
            let output = std::process::Command::new("git")
                .args(["worktree", "add", "--detach", &worktree_dir.display().to_string(), branch])
                .current_dir(&source_path)
                .output();
            match output {
                Ok(o) if o.status.success() => {
                    json!({"path": worktree_dir.display().to_string(), "source": source_path, "branch": branch})
                }
                Ok(o) => json!({"error": String::from_utf8_lossy(&o.stderr).to_string()}),
                Err(e) => json!({"error": format!("{e}")}),
            }
        }
        "release_repo" => {
            let path = match args["path"].as_str() {
                Some(p) => p,
                None => return json!({"error": "missing 'path'"}),
            };
            let output = std::process::Command::new("git")
                .args(["worktree", "remove", "--force", path])
                .output();
            match output {
                Ok(o) if o.status.success() => json!({"status": "released", "path": path}),
                Ok(o) => {
                    let _ = std::fs::remove_dir_all(path);
                    json!({"status": "released", "path": path, "note": String::from_utf8_lossy(&o.stderr).to_string()})
                }
                Err(_) => {
                    let _ = std::fs::remove_dir_all(path);
                    json!({"status": "released", "path": path})
                }
            }
        }

        _ => json!({"error": format!("unknown tool: {tool}")}),
    }
}

fn send_to(home: &std::path::Path, from: &str, target: &str, text: &str, kind: &str) -> Value {
    match crate::api::call(home, &json!({
        "method": "send",
        "params": { "from": from, "target": target, "text": text, "kind": kind }
    })) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => json!({"sent": true, "target": target}),
        Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("send failed")}),
        Err(e) => {
            let submit_key = get_submit_key(home, target);
            crate::inbox::deliver(home, target, &format!("from:{from}"), text, &submit_key, None);
            json!({"sent": true, "target": target, "note": format!("API unavailable: {e}")})
        }
    }
}

fn save_metadata(home: &std::path::Path, instance_name: &str, key: &str, value: Value) {
    let meta_dir = home.join("metadata");
    std::fs::create_dir_all(&meta_dir).ok();
    let meta_path = meta_dir.join(format!("{instance_name}.json"));
    let mut meta: Value = std::fs::read_to_string(&meta_path)
        .and_then(|c| Ok(serde_json::from_str(&c).unwrap_or(json!({}))))
        .unwrap_or(json!({}));
    meta[key] = value;
    let _ = std::fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap_or_default());
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

fn try_download_attachment(instance_name: &str, file_id: &str) -> anyhow::Result<String> {
    let home = crate::home_dir();
    let fleet_path = home.join("fleet.yaml");
    if !fleet_path.exists() {
        anyhow::bail!("No fleet.yaml");
    }
    let config = crate::fleet::FleetConfig::load(&fleet_path)?;
    match &config.channel {
        Some(crate::fleet::ChannelConfig::Telegram { bot_token_env, .. }) => {
            let token = std::env::var(bot_token_env)?;
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(async {
                let bot = teloxide::Bot::new(&token);
                let file = bot.get_file(file_id).await?;
                let download_dir = home.join("downloads").join(instance_name);
                std::fs::create_dir_all(&download_dir)?;
                let filename = file.path.split('/').last().unwrap_or("attachment");
                let dest = download_dir.join(filename);
                let mut dst = tokio::fs::File::create(&dest).await?;
                bot.download_file(&file.path, &mut dst).await?;
                Ok(dest.display().to_string())
            })
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
