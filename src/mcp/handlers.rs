//! MCP tool dispatch — handle_tool() routes tool calls to implementations.

use super::telegram;
use serde_json::{json, Value};

pub fn handle_tool(tool: &str, args: &Value, _agent_socket: &str, instance_name: &str) -> Value {
    let home = crate::home_dir();
    let instance_name = if instance_name.is_empty() {
        std::env::var("AGEND_INSTANCE_NAME").unwrap_or_default()
    } else {
        instance_name.to_string()
    };

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
            if let Err(e) = crate::agent::validate_name(target) {
                return json!({"error": e});
            }
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
            if let Err(e) = crate::agent::validate_name(target) {
                return json!({"error": e});
            }
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
            if let Err(e) = crate::agent::validate_name(target) {
                return json!({"error": e});
            }
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
            if let Err(e) = crate::agent::validate_name(target) {
                return json!({"error": e});
            }
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
                            let mut info = a.clone();
                            merge_metadata(&home, a["name"].as_str().unwrap_or(""), &mut info);
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
            if let Err(e) = crate::agent::validate_name(name) {
                return json!({"error": e});
            }
            // Accept "backend" (preferred) or "command" (deprecated) for the CLI tool name.
            // Default to "claude" if neither is specified.
            let command = args["backend"]
                .as_str()
                .or_else(|| args["command"].as_str())
                .unwrap_or("claude");
            // Start with backend preset args (e.g. --yolo for gemini, --dangerously-skip-permissions for claude)
            let mut cmd_args = crate::backend::Backend::from_command(command)
                .map(|b| {
                    b.preset()
                        .args
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            // Append user-specified args
            if let Some(extra) = args
                .get("args")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                if !cmd_args.is_empty() {
                    cmd_args.push(' ');
                }
                cmd_args.push_str(extra);
            }
            if let Some(model) = args
                .get("model")
                .and_then(|v| v.as_str())
                .filter(|m| !m.is_empty())
            {
                if !cmd_args.is_empty() {
                    cmd_args.push(' ');
                }
                cmd_args.push_str(&format!("--model {model}"));
            }
            // Validate working_directory: reject paths with ".." or relative paths
            if let Some(dir) = args.get("working_directory").and_then(|v| v.as_str()) {
                if dir.contains("..") {
                    return json!({"error": "working_directory must not contain '..'"});
                }
                if !dir.starts_with('/') {
                    return json!({"error": "working_directory must be an absolute path"});
                }
            }
            let mut work_dir = args
                .get("working_directory")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| home.join("workspaces").join(name).display().to_string());

            if let Some(branch) = args.get("branch").and_then(|v| v.as_str()) {
                if !validate_branch(branch) {
                    return json!({"error": format!("invalid branch name '{branch}'")});
                }
                let wd = std::path::PathBuf::from(&work_dir);
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
                &json!({"method": "spawn", "params": {"name": name, "command": command, "args": &cmd_args, "working_directory": work_dir}}),
            ) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => {
                    let entry = crate::fleet::InstanceYamlEntry {
                        backend: backend_str
                            .or_else(|| {
                                crate::backend::Backend::from_command(command)
                                    .map(|b| b.name().to_string())
                            })
                            .or_else(|| Some(command.to_string())),
                        working_directory: Some(work_dir.clone()),
                        role,
                    };
                    if let Err(e) = crate::fleet::add_instance_to_yaml(&home, name, &entry) {
                        eprintln!("[mcp] failed to persist to fleet.yaml: {e}");
                    }
                    let topic_id = telegram::create_topic_for_instance(&home, name);
                    if let Some(ref task_text) = task {
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        let _ = crate::api::call(
                            &home,
                            &json!({"method": "inject", "params": {"name": name, "data": task_text}}),
                        );
                    }
                    let mut result = json!({"name": name, "backend": command});
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
            if let Err(e) = crate::agent::validate_name(name) {
                return json!({"error": e});
            }
            // Read topic_id before removing from fleet.yaml
            let topic_id = crate::fleet::FleetConfig::load(&home.join("fleet.yaml"))
                .ok()
                .and_then(|c| c.instances.get(name).and_then(|i| i.topic_id));

            let _ = crate::api::call(
                &home,
                &json!({"method": "delete", "params": {"name": name}}),
            );
            if let Err(e) = crate::fleet::remove_instance_from_yaml(&home, name) {
                eprintln!("[mcp] failed to remove from fleet.yaml: {e}");
            }
            // Close and delete the Telegram topic if one exists
            if let Some(tid) = topic_id {
                telegram::close_topic(&home, tid);
            }
            json!({"name": name})
        }
        "start_instance" => {
            let name = match args["name"].as_str() {
                Some(n) => n,
                None => return json!({"error": "missing 'name'"}),
            };
            if let Err(e) = crate::agent::validate_name(name) {
                return json!({"error": e});
            }
            let fleet_path = home.join("fleet.yaml");
            if !fleet_path.exists() {
                return json!({"error": "No fleet.yaml"});
            }
            let config = match crate::fleet::FleetConfig::load(&fleet_path) {
                Ok(c) => c,
                Err(e) => return json!({"error": format!("fleet.yaml: {e}")}),
            };
            match config.resolve_instance(name) {
                Some(resolved) => {
                    let mut cmd_args = resolved.args.join(" ");
                    if let Some(ref b) = crate::backend::Backend::from_command(&resolved.command) {
                        let resume = b.preset().resume_mode.args_for(&home, name);
                        if !resume.is_empty() {
                            if !cmd_args.is_empty() {
                                cmd_args.push(' ');
                            }
                            cmd_args.push_str(&resume.join(" "));
                        }
                    }
                    match crate::api::call(
                        &home,
                        &json!({"method": "spawn", "params": {
                            "name": name, "command": resolved.command, "args": cmd_args,
                            "working_directory": resolved.working_directory.map(|p| p.display().to_string()),
                        }}),
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
            if let Err(e) = crate::agent::validate_name(name) {
                return json!({"error": e});
            }
            match crate::api::call(&home, &json!({"method": "list"})) {
                Ok(resp) => {
                    match resp["result"]["agents"]
                        .as_array()
                        .and_then(|a| a.iter().find(|x| x["name"].as_str() == Some(name)))
                    {
                        Some(agent) => {
                            let mut info = agent.clone();
                            merge_metadata(&home, name, &mut info);
                            json!({"instance": info})
                        }
                        None => json!({"error": format!("Instance '{name}' not found")}),
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
            if let Err(e) = crate::agent::validate_name(name) {
                return json!({"error": e});
            }
            let reason = args["reason"].as_str().unwrap_or("manual replacement");
            let handover = crate::api::call(&home, &json!({"method": "list"})).ok()
                .and_then(|resp| resp["result"]["agents"].as_array()?.iter()
                    .find(|a| a["name"].as_str() == Some(name))
                    .map(|a| format!("Previous instance state: {}, health: {}. Replaced due to: {reason}",
                        a["agent_state"].as_str().unwrap_or("unknown"), a["health_state"].as_str().unwrap_or("unknown"))))
                .unwrap_or_else(|| format!("Replaced due to: {reason}"));

            let _ = crate::api::call(&home, &json!({"method": "kill", "params": {"name": name}}));
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
            json!({"name": name, "reason": reason, "note": "Instance killed. Auto-respawn will create fresh instance with handover context."})
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
            if !validate_branch(branch) {
                return json!({"error": format!("invalid branch name '{branch}'")});
            }
            let worktree_dir = home.join("worktrees").join(format!(
                "{}-{}",
                instance_name,
                source.replace('/', "_").replace('~', "")
            ));
            std::fs::create_dir_all(worktree_dir.parent().unwrap_or(&home)).ok();
            let source_path = if source.starts_with('/') || source.starts_with('~') {
                source
                    .strip_prefix("~/")
                    .map(|rest| {
                        format!(
                            "{}/{rest}",
                            std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
                        )
                    })
                    .unwrap_or_else(|| source.to_string())
            } else {
                crate::api::call(&home, &json!({"method": "list"}))
                    .ok()
                    .and_then(|r| {
                        r["result"]["agents"]
                            .as_array()?
                            .iter()
                            .find(|a| a["name"].as_str() == Some(source))
                            .and_then(|a| a["working_directory"].as_str().map(String::from))
                    })
                    .unwrap_or_else(|| source.to_string())
            };
            match std::process::Command::new("git")
                .args([
                    "worktree",
                    "add",
                    "--detach",
                    &worktree_dir.display().to_string(),
                    branch,
                ])
                .current_dir(&source_path)
                .output()
            {
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
            match std::process::Command::new("git")
                .args(["worktree", "remove", "--force", path])
                .output()
            {
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

        // --- CI watch ---
        "watch_ci" => {
            let repo = match args["repo"].as_str() {
                Some(r) => r,
                None => return json!({"error": "missing 'repo'"}),
            };
            let branch = args["branch"].as_str().unwrap_or("main");
            let interval = args["interval_secs"].as_u64().unwrap_or(60);
            let ci_dir = home.join("ci-watches");
            std::fs::create_dir_all(&ci_dir).ok();
            let watch = json!({
                "repo": repo, "branch": branch, "interval_secs": interval,
                "instance": instance_name, "last_run_id": null
            });
            let safe_name = repo.replace('/', "_");
            let _ = std::fs::write(
                ci_dir.join(format!("{safe_name}.json")),
                serde_json::to_string_pretty(&watch).unwrap_or_default(),
            );
            json!({"repo": repo, "watching": true})
        }
        "unwatch_ci" => {
            let repo = match args["repo"].as_str() {
                Some(r) => r,
                None => return json!({"error": "missing 'repo'"}),
            };
            let safe_name = repo.replace('/', "_");
            let path = home.join("ci-watches").join(format!("{safe_name}.json"));
            let _ = std::fs::remove_file(&path);
            json!({"repo": repo, "watching": false})
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

/// Load metadata for an instance and merge it into the given JSON value.
fn merge_metadata(home: &std::path::Path, name: &str, info: &mut Value) {
    let meta_path = home.join("metadata").join(format!("{name}.json"));
    if let Ok(meta) = std::fs::read_to_string(&meta_path)
        .and_then(|c| serde_json::from_str::<Value>(&c).map_err(std::io::Error::other))
    {
        if let (Some(obj), Some(m)) = (info.as_object_mut(), meta.as_object()) {
            for (k, v) in m {
                obj.insert(k.clone(), v.clone());
            }
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

/// Validate a git branch name. Only allows [a-zA-Z0-9/_.-], rejects ".." and leading "-".
fn validate_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.contains("..")
        && !branch.starts_with('-')
        && branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '_' || c == '-' || c == '.')
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_home(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-handlers-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    // validate_branch tests
    #[test]
    fn branch_valid_simple() {
        assert!(validate_branch("main"));
        assert!(validate_branch("feature/foo"));
        assert!(validate_branch("v1.0.0"));
        assert!(validate_branch("fix-123"));
        assert!(validate_branch("release_2.0"));
    }

    #[test]
    fn branch_rejects_empty() {
        assert!(!validate_branch(""));
    }

    #[test]
    fn branch_rejects_dotdot() {
        assert!(!validate_branch(".."));
        assert!(!validate_branch("foo/.."));
        assert!(!validate_branch("../bar"));
    }

    #[test]
    fn branch_rejects_leading_dash() {
        assert!(!validate_branch("-main"));
        assert!(!validate_branch("-"));
    }

    #[test]
    fn branch_rejects_special_chars() {
        assert!(!validate_branch("main branch"));
        assert!(!validate_branch("foo;bar"));
        assert!(!validate_branch("$(echo)"));
        assert!(!validate_branch("main\ninjected"));
    }

    // merge_metadata tests
    #[test]
    fn merge_metadata_no_file() {
        let home = tmp_home("merge_meta_no_file");
        let mut info = json!({"name": "agent1", "state": "idle"});
        merge_metadata(&home, "agent1", &mut info);
        // Should not crash, info unchanged
        assert_eq!(info["name"], "agent1");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn merge_metadata_merges_fields() {
        let home = tmp_home("merge_meta_fields");
        let meta_dir = home.join("metadata");
        std::fs::create_dir_all(&meta_dir).ok();
        std::fs::write(
            meta_dir.join("agent1.json"),
            r#"{"display_name": "Dev Agent", "custom": 42}"#,
        )
        .ok();
        let mut info = json!({"name": "agent1", "state": "idle"});
        merge_metadata(&home, "agent1", &mut info);
        assert_eq!(info["display_name"], "Dev Agent");
        assert_eq!(info["custom"], 42);
        assert_eq!(info["name"], "agent1"); // original preserved
        std::fs::remove_dir_all(&home).ok();
    }

    // save_metadata tests
    #[test]
    fn save_and_load_metadata() {
        let home = tmp_home("save_meta");
        save_metadata(&home, "agent1", "display_name", json!("My Agent"));
        save_metadata(&home, "agent1", "version", json!(2));
        let content = std::fs::read_to_string(home.join("metadata/agent1.json")).expect("read");
        let meta: serde_json::Value = serde_json::from_str(&content).expect("parse");
        assert_eq!(meta["display_name"], "My Agent");
        assert_eq!(meta["version"], 2);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn save_metadata_creates_dir() {
        let home = tmp_home("save_meta_dir");
        assert!(!home.join("metadata").exists());
        save_metadata(&home, "agent1", "key", json!("value"));
        assert!(home.join("metadata").exists());
        std::fs::remove_dir_all(&home).ok();
    }

    // get_submit_key tests
    #[test]
    fn get_submit_key_default() {
        let home = tmp_home("submit_key");
        // No fleet.yaml → default \r
        let key = get_submit_key(&home, "agent1");
        assert_eq!(key, "\r");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn get_submit_key_from_fleet() {
        let home = tmp_home("submit_key_fleet");
        let yaml = r#"defaults:
  backend: claude
instances:
  dev:
    role: "Developer"
"#;
        std::fs::write(home.join("fleet.yaml"), yaml).ok();
        let key = get_submit_key(&home, "dev");
        // Claude Code preset submit_key is "\r" or similar
        assert!(!key.is_empty());
        std::fs::remove_dir_all(&home).ok();
    }
}
