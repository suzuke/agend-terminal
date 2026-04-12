//! MCP tool dispatch — handle_tool() routes tool calls to implementations.

use super::telegram;
use serde_json::{json, Value};

pub fn handle_tool(tool: &str, args: &Value, _agent_socket: &str) -> Value {
    let home = crate::home_dir();
    let instance_name = std::env::var("AGEND_INSTANCE_NAME").unwrap_or_default();

    match tool {
        // --- Channel ---
        "reply" => {
            let text = args["text"].as_str().unwrap_or("");
            eprintln!("[mcp] reply from {instance_name}: {text}");
            let fleet_path = home.join("fleet.yaml");
            if fleet_path.exists() {
                match telegram::try_telegram_reply(&instance_name, text) {
                    Ok((msg_id, chat_id)) => json!({
                        "message_id": msg_id.to_string(),
                        "chat_id": chat_id.to_string(),
                    }),
                    Err(e) => json!({"error": format!("{e}")}),
                }
            } else {
                json!({"error": "No fleet.yaml — cannot send reply"})
            }
        }
        "react" => {
            let emoji = args["emoji"].as_str().unwrap_or("");
            let message_id = args["message_id"].as_str();
            match telegram::try_telegram_react(&instance_name, emoji, message_id) {
                Ok(()) => json!({"emoji": emoji}),
                Err(e) => json!({"error": format!("{e}")}),
            }
        }
        "edit_message" => {
            let message_id = match args["message_id"].as_str() {
                Some(m) => m,
                None => return json!({"error": "missing 'message_id'"}),
            };
            let text = match args["text"].as_str() {
                Some(t) => t,
                None => return json!({"error": "missing 'text'"}),
            };
            match telegram::try_telegram_edit(&instance_name, message_id, text) {
                Ok(()) => json!({"message_id": message_id}),
                Err(e) => json!({"error": format!("{e}")}),
            }
        }
        "download_attachment" => {
            let file_id = match args["file_id"].as_str() {
                Some(f) => f,
                None => return json!({"error": "missing 'file_id'"}),
            };
            match telegram::try_download_attachment(&instance_name, file_id) {
                Ok(path) => json!({"path": path}),
                Err(e) => json!({"error": format!("{e}")}),
            }
        }

        // --- Cross-instance communication ---
        "send_to_instance" | "send" => {
            let target = args["instance_name"]
                .as_str()
                .or_else(|| args["target"].as_str());
            let target = match target {
                Some(t) => t,
                None => return json!({"error": "missing 'instance_name' or 'target'"}),
            };
            let text = args["message"]
                .as_str()
                .or_else(|| args["text"].as_str())
                .unwrap_or("");
            let kind = args["request_kind"]
                .as_str()
                .or_else(|| args["kind"].as_str());

            match crate::api::call(
                &home,
                &json!({
                    "method": "send",
                    "params": { "from": instance_name, "target": target, "text": text, "kind": kind }
                }),
            ) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => {
                    json!({"target": target})
                }
                Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("send failed")}),
                Err(e) => {
                    let submit_key = get_submit_key(&home, target);
                    crate::inbox::deliver(
                        &home,
                        target,
                        &format!("from:{instance_name}"),
                        text,
                        &submit_key,
                        None,
                    );
                    json!({"target": target, "note": format!("API unavailable, sent direct: {e}")})
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
                t.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            } else {
                list_agents()
            };
            let targets: Vec<String> = targets
                .into_iter()
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
        "list_instances" => match crate::api::call(&home, &json!({"method": "list"})) {
            Ok(resp) => {
                if let Some(agents) = resp["result"]["agents"].as_array() {
                    let instances: Vec<Value> = agents
                        .iter()
                        .map(|a| {
                            let name = a["name"].as_str().unwrap_or("");
                            let meta_path = home.join("metadata").join(format!("{name}.json"));
                            let meta = std::fs::read_to_string(&meta_path)
                                .map(|c| serde_json::from_str::<Value>(&c).unwrap_or(json!({})))
                                .unwrap_or(json!({}));
                            let mut info = a.clone();
                            if let Some(obj) = info.as_object_mut() {
                                if let Some(m) = meta.as_object() {
                                    for (k, v) in m {
                                        obj.insert(k.clone(), v.clone());
                                    }
                                }
                            }
                            info
                        })
                        .collect();
                    json!({"instances": instances})
                } else {
                    json!({"instances": list_agents()})
                }
            }
            Err(_) => json!({"instances": list_agents()}),
        },
        "create_instance" => {
            let name = match args["name"].as_str() {
                Some(n) => n,
                None => return json!({"error": "missing 'name'"}),
            };
            let command = match args["command"].as_str() {
                Some(c) => c,
                None => return json!({"error": "missing 'command'"}),
            };
            let mut cmd_args = args
                .get("args")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(model) = args.get("model").and_then(|v| v.as_str()) {
                if !model.is_empty() {
                    if !cmd_args.is_empty() {
                        cmd_args.push(' ');
                    }
                    cmd_args.push_str(&format!("--model {model}"));
                }
            }
            let mut work_dir = args
                .get("working_directory")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| home.join("workspaces").join(name).display().to_string());

            // Create worktree if branch specified and directory is a git repo
            let wd = std::path::PathBuf::from(&work_dir);
            if let Some(branch) = args.get("branch").and_then(|v| v.as_str()) {
                if let Some(info) = crate::worktree::create(&wd, name, Some(branch)) {
                    work_dir = info.path.display().to_string();
                }
            }

            let wd = std::path::PathBuf::from(&work_dir);
            std::fs::create_dir_all(&wd).ok();
            crate::instructions::generate(&wd, command);
            crate::mcp_config::configure(&wd, command);

            let task = args.get("task").and_then(|v| v.as_str()).map(String::from);

            let role = args.get("role").and_then(|v| v.as_str()).map(String::from);
            let backend_str = args
                .get("backend")
                .and_then(|v| v.as_str())
                .map(String::from);

            match crate::api::call(
                &home,
                &json!({
                    "method": "spawn",
                    "params": { "name": name, "command": command, "args": &cmd_args, "working_directory": work_dir }
                }),
            ) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => {
                    // Persist to fleet.yaml
                    let entry = crate::fleet::InstanceYamlEntry {
                        command: command.to_string(),
                        backend: backend_str,
                        working_directory: Some(work_dir.clone()),
                        role,
                    };
                    if let Err(e) = crate::fleet::add_instance_to_yaml(&home, name, &entry) {
                        eprintln!("[mcp] failed to persist instance to fleet.yaml: {e}");
                    }

                    // Create Telegram topic if channel is configured
                    let topic_id = telegram::create_topic_for_instance(&home, name);

                    // Inject initial task if provided
                    if let Some(ref task_text) = task {
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        let _ = crate::api::call(
                            &home,
                            &json!({
                                "method": "inject",
                                "params": {"name": name, "data": task_text}
                            }),
                        );
                    }
                    let mut result = json!({"name": name, "command": command});
                    if let Some(tid) = topic_id {
                        result["topic_id"] = json!(tid);
                    }
                    result
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
            // Use "delete" (not "kill") — removes from configs to prevent respawn
            let _ = crate::api::call(
                &home,
                &json!({"method": "delete", "params": {"name": name}}),
            );

            // Remove from fleet.yaml
            if let Err(e) = crate::fleet::remove_instance_from_yaml(&home, name) {
                eprintln!("[mcp] failed to remove instance from fleet.yaml: {e}");
            }

            json!({"name": name})
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
                            if !cmd_args.is_empty() {
                                cmd_args.push(' ');
                            }
                            cmd_args.push_str(&resume_args.join(" "));
                        }
                    }
                    match crate::api::call(
                        &home,
                        &json!({
                            "method": "spawn",
                            "params": {
                                "name": name, "command": resolved.command,
                                "args": cmd_args,
                                "working_directory": resolved.working_directory.map(|p| p.display().to_string()),
                            }
                        }),
                    ) {
                        Ok(resp) if resp["ok"].as_bool() == Some(true) => json!({"name": name}),
                        Ok(resp) => {
                            json!({"error": resp["error"].as_str().unwrap_or("spawn failed")})
                        }
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
                                    .map(|c| serde_json::from_str::<Value>(&c).unwrap_or(json!({})))
                                    .unwrap_or(json!({}));
                                let mut info = agent.clone();
                                if let Some(obj) = info.as_object_mut() {
                                    if let Some(m) = meta.as_object() {
                                        for (k, v) in m {
                                            obj.insert(k.clone(), v.clone());
                                        }
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

            // Collect handover context from VTerm screen buffer before killing
            let handover = match crate::api::call(&home, &json!({"method": "list"})) {
                Ok(resp) => {
                    resp["result"]["agents"].as_array()
                        .and_then(|agents| agents.iter()
                            .find(|a| a["name"].as_str() == Some(name)))
                        .map(|a| {
                            let state = a["agent_state"].as_str().unwrap_or("unknown");
                            let health = a["health_state"].as_str().unwrap_or("unknown");
                            format!("Previous instance state: {state}, health: {health}. Replaced due to: {reason}")
                        })
                        .unwrap_or_else(|| format!("Replaced due to: {reason}"))
                }
                Err(_) => format!("Replaced due to: {reason}"),
            };

            // Kill old instance — auto-respawn creates new one
            let _ = crate::api::call(&home, &json!({"method": "kill", "params": {"name": name}}));

            // Queue handover message for the new instance
            let _ = crate::inbox::enqueue(
                &home,
                name,
                crate::inbox::InboxMessage {
                    from: "system:replace".to_string(),
                    text: format!("[handover] {handover}"),
                    kind: Some("handover".to_string()),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                },
            );

            eprintln!("[mcp] replace_instance {name}: {reason}");
            json!({"name": name, "reason": reason,
                   "note": "Instance killed. Auto-respawn will create fresh instance with handover context."})
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

        // --- Deployments ---
        "deploy_template" => crate::deployments::deploy(&home, &instance_name, args),
        "teardown_deployment" => crate::deployments::teardown(&home, args),
        "list_deployments" => crate::deployments::list(&home),

        // --- Repo access ---
        "checkout_repo" => {
            let source = match args["source"].as_str() {
                Some(s) => s,
                None => return json!({"error": "missing 'source'"}),
            };
            let branch = args["branch"].as_str().unwrap_or("HEAD");
            let worktree_dir = home.join("worktrees").join(format!(
                "{}-{}",
                instance_name,
                source.replace('/', "_").replace('~', "")
            ));
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
                    Ok(resp) => resp["result"]["agents"]
                        .as_array()
                        .and_then(|agents| {
                            agents
                                .iter()
                                .find(|a| a["name"].as_str() == Some(source))
                                .and_then(|a| a["working_directory"].as_str().map(String::from))
                        })
                        .unwrap_or_else(|| source.to_string()),
                    Err(_) => source.to_string(),
                }
            };
            let output = std::process::Command::new("git")
                .args([
                    "worktree",
                    "add",
                    "--detach",
                    &worktree_dir.display().to_string(),
                    branch,
                ])
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
                Ok(o) if o.status.success() => json!({"path": path}),
                Ok(o) => {
                    let _ = std::fs::remove_dir_all(path);
                    json!({"path": path, "note": String::from_utf8_lossy(&o.stderr).to_string()})
                }
                Err(_) => {
                    let _ = std::fs::remove_dir_all(path);
                    json!({"path": path})
                }
            }
        }

        _ => json!({"error": format!("unknown tool: {tool}")}),
    }
}

fn send_to(home: &std::path::Path, from: &str, target: &str, text: &str, kind: &str) -> Value {
    match crate::api::call(
        home,
        &json!({
            "method": "send",
            "params": { "from": from, "target": target, "text": text, "kind": kind }
        }),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => json!({"target": target}),
        Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("send failed")}),
        Err(e) => {
            let submit_key = get_submit_key(home, target);
            crate::inbox::deliver(
                home,
                target,
                &format!("from:{from}"),
                text,
                &submit_key,
                None,
            );
            json!({"target": target, "note": format!("API unavailable: {e}")})
        }
    }
}

fn save_metadata(home: &std::path::Path, instance_name: &str, key: &str, value: Value) {
    let meta_dir = home.join("metadata");
    std::fs::create_dir_all(&meta_dir).ok();
    let meta_path = meta_dir.join(format!("{instance_name}.json"));
    let mut meta: Value = std::fs::read_to_string(&meta_path)
        .map(|c| serde_json::from_str(&c).unwrap_or(json!({})))
        .unwrap_or(json!({}));
    meta[key] = value;
    let _ = std::fs::write(
        &meta_path,
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    );
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
