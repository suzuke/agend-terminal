#![allow(dead_code)]
//! Shared operations — called by MCP handlers.

use serde_json::{json, Value};
use std::path::Path;

// ---------------------------------------------------------------------------
// Communication
// ---------------------------------------------------------------------------

pub fn send_message(
    home: &Path,
    from: &str,
    target: &str,
    text: &str,
    kind: Option<&str>,
) -> Value {
    if let Err(e) = crate::agent::validate_name(target) {
        return json!({"error": e});
    }
    match crate::api::call(
        home,
        &json!({
            "method": crate::api::method::SEND,
            "params": { "from": from, "target": target, "text": text, "kind": kind }
        }),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            json!({"target": target})
        }
        Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("send failed")}),
        Err(e) => {
            let submit_key = get_submit_key(home, target);
            crate::inbox::deliver(
                home,
                target,
                &crate::inbox::NotifySource::Agent(from),
                text,
                &submit_key,
                None,
            );
            json!({"target": target, "note": format!("API unavailable, sent direct: {e}")})
        }
    }
}

pub fn delegate_task(
    home: &Path,
    from: &str,
    target: &str,
    task: &str,
    criteria: Option<&str>,
    context: Option<&str>,
) -> Value {
    if let Err(e) = crate::agent::validate_name(target) {
        return json!({"error": e});
    }
    let mut msg = format!("[delegate_task] {task}");
    if let Some(criteria) = criteria {
        msg.push_str(&format!("\n\nSuccess criteria: {criteria}"));
    }
    if let Some(ctx) = context {
        msg.push_str(&format!("\n\nContext: {ctx}"));
    }
    send_to(home, from, target, &msg, "task")
}

pub fn report_result(
    home: &Path,
    from: &str,
    target: &str,
    summary: &str,
    correlation_id: Option<&str>,
    artifacts: Option<&str>,
) -> Value {
    if let Err(e) = crate::agent::validate_name(target) {
        return json!({"error": e});
    }
    let mut msg = format!("[report_result] {summary}");
    if let Some(cid) = correlation_id {
        msg.push_str(&format!("\ncorrelation_id: {cid}"));
    }
    if let Some(artifacts) = artifacts {
        msg.push_str(&format!("\nArtifacts: {artifacts}"));
    }
    send_to(home, from, target, &msg, "report")
}

pub fn request_information(
    home: &Path,
    from: &str,
    target: &str,
    question: &str,
    context: Option<&str>,
) -> Value {
    if let Err(e) = crate::agent::validate_name(target) {
        return json!({"error": e});
    }
    let mut msg = format!("[request_information] {question}");
    if let Some(ctx) = context {
        msg.push_str(&format!("\n\nContext: {ctx}"));
    }
    send_to(home, from, target, &msg, "query")
}

pub fn broadcast(
    home: &Path,
    from: &str,
    message: &str,
    team: Option<&str>,
    targets: Option<&[String]>,
) -> Value {
    // Resolve targets: team > targets > all
    let resolved: Vec<String> = if let Some(team) = team {
        crate::teams::get_members(home, team)
    } else if let Some(t) = targets {
        t.to_vec()
    } else {
        list_agents()
    };
    let resolved: Vec<String> = resolved.into_iter().filter(|t| *t != from).collect();
    let kind = "update";
    let mut sent = Vec::new();
    for target in &resolved {
        let _ = send_to(home, from, target, message, kind);
        sent.push(target.clone());
    }
    json!({"sent_to": sent, "count": sent.len()})
}

pub fn drain_inbox(home: &Path, instance_name: &str) -> Value {
    let messages = crate::inbox::drain(home, instance_name);
    json!({"messages": messages})
}

// ---------------------------------------------------------------------------
// Channel (Telegram)
// ---------------------------------------------------------------------------

pub fn reply(home: &Path, instance_name: &str, text: &str) -> Value {
    tracing::info!(from = %instance_name, %text, "reply");
    let fleet_path = home.join("fleet.yaml");
    if fleet_path.exists() {
        match crate::telegram::try_telegram_reply(instance_name, text) {
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

pub fn react(instance_name: &str, emoji: &str, message_id: Option<&str>) -> Value {
    match crate::telegram::try_telegram_react(instance_name, emoji, message_id) {
        Ok(()) => json!({"emoji": emoji}),
        Err(e) => json!({"error": format!("{e}")}),
    }
}

pub fn edit_message(instance_name: &str, message_id: &str, text: &str) -> Value {
    match crate::telegram::try_telegram_edit(instance_name, message_id, text) {
        Ok(()) => json!({"message_id": message_id}),
        Err(e) => json!({"error": format!("{e}")}),
    }
}

pub fn download_attachment(instance_name: &str, file_id: &str) -> Value {
    match crate::telegram::try_download_attachment(instance_name, file_id) {
        Ok(path) => json!({"path": path}),
        Err(e) => json!({"error": format!("{e}")}),
    }
}

// ---------------------------------------------------------------------------
// Instance management
// ---------------------------------------------------------------------------

pub fn list_instances(home: &Path) -> Value {
    match crate::api::call(home, &json!({"method": crate::api::method::LIST})) {
        Ok(resp) => {
            if let Some(agents) = resp["result"]["agents"].as_array() {
                let instances: Vec<Value> = agents
                    .iter()
                    .map(|a| {
                        let mut info = a.clone();
                        merge_metadata(home, a["name"].as_str().unwrap_or(""), &mut info);
                        info
                    })
                    .collect();
                json!({"instances": instances})
            } else {
                json!({"instances": list_agents()})
            }
        }
        Err(_) => json!({"instances": list_agents()}),
    }
}

pub fn create_instance(home: &Path, args: &Value) -> Value {
    let name = match args["name"].as_str() {
        Some(n) => n,
        None => return json!({"error": "missing 'name'"}),
    };
    if let Err(e) = crate::agent::validate_name(name) {
        return json!({"error": e});
    }
    // Resolve to actual CLI command via preset (e.g. "kiro" → "kiro-cli").
    let raw_backend = args["backend"].as_str().unwrap_or("claude");
    let command = crate::backend::Backend::from_command(raw_backend)
        .map(|b| b.preset().command)
        .unwrap_or(raw_backend);
    // Start with backend fresh_args (no resume flags — this is a new instance).
    // Falls back to preset args if fresh_args is not defined.
    let mut cmd_args = crate::backend::Backend::from_command(command)
        .map(|b| {
            let p = b.preset();
            p.fresh_args
                .unwrap_or(p.args)
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
        .unwrap_or_else(|| home.join("workspace").join(name).display().to_string());

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

    let task = args.get("task").and_then(|v| v.as_str()).map(String::from);
    let role = args.get("role").and_then(|v| v.as_str()).map(String::from);
    let backend_str = args
        .get("backend")
        .and_then(|v| v.as_str())
        .map(String::from);

    let work_dir_clone = work_dir.clone();
    match crate::api::call(
        home,
        &json!({"method": crate::api::method::SPAWN, "params": {"name": name, "backend": command, "args": &cmd_args, "working_directory": work_dir}}),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            let entry = crate::fleet::InstanceYamlEntry {
                backend: backend_str
                    .or_else(|| {
                        crate::backend::Backend::from_command(command).map(|b| b.name().to_string())
                    })
                    .or_else(|| Some(command.to_string())),
                working_directory: Some(work_dir_clone),
                role,
            };
            if let Err(e) = crate::fleet::add_instance_to_yaml(home, name, &entry) {
                tracing::warn!(error = %e, "failed to persist to fleet.yaml");
            }
            let topic_id = crate::telegram::create_topic_for_instance(home, name);
            if let Some(ref task_text) = task {
                std::thread::sleep(std::time::Duration::from_secs(3));
                let _ = crate::api::call(
                    home,
                    &json!({"method": crate::api::method::INJECT, "params": {"name": name, "data": task_text}}),
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

pub fn delete_instance(home: &Path, args: &Value) -> Value {
    let name = match args["name"].as_str() {
        Some(n) => n,
        None => return json!({"error": "missing 'name'"}),
    };
    if let Err(e) = crate::agent::validate_name(name) {
        return json!({"error": e});
    }
    // Prevent deleting the last instance when a channel is configured
    let fleet = crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).ok();
    if let Some(ref config) = fleet {
        if config.channel.is_some() && config.instances.len() <= 1 {
            return json!({"error": "cannot delete the last instance — channel needs at least one instance to receive messages"});
        }
    }
    // Read instance info before removing from fleet.yaml
    let (topic_id, working_dir) = fleet
        .as_ref()
        .and_then(|c| {
            c.resolve_instance(name)
                .map(|r| (r.topic_id, r.working_directory))
        })
        .unwrap_or((None, None));

    let _ = crate::api::call(
        home,
        &json!({"method": crate::api::method::DELETE, "params": {"name": name}}),
    );
    if let Err(e) = crate::fleet::remove_instance_from_yaml(home, name) {
        tracing::warn!(error = %e, "failed to remove from fleet.yaml");
    }
    // Delete the Telegram topic if one exists
    if let Some(tid) = topic_id {
        crate::telegram::delete_topic(home, tid);
    }
    // Clean up working directory
    if let Some(ref wd) = working_dir {
        cleanup_working_dir(home, name, wd);
    }
    json!({"name": name})
}

pub fn start_instance(home: &Path, args: &Value) -> Value {
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
            if let Some(ref b) = crate::backend::Backend::from_command(&resolved.backend_command) {
                let resume = b.preset().resume_mode.args_for(home, name);
                if !resume.is_empty() {
                    if !cmd_args.is_empty() {
                        cmd_args.push(' ');
                    }
                    cmd_args.push_str(&resume.join(" "));
                }
            }
            match crate::api::call(
                home,
                &json!({"method": crate::api::method::SPAWN, "params": {
                    "name": name, "backend": resolved.backend_command, "args": cmd_args,
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

pub fn describe_instance(home: &Path, name: &str) -> Value {
    if let Err(e) = crate::agent::validate_name(name) {
        return json!({"error": e});
    }
    match crate::api::call(home, &json!({"method": crate::api::method::LIST})) {
        Ok(resp) => {
            match resp["result"]["agents"]
                .as_array()
                .and_then(|a| a.iter().find(|x| x["name"].as_str() == Some(name)))
            {
                Some(agent) => {
                    let mut info = agent.clone();
                    merge_metadata(home, name, &mut info);
                    json!({"instance": info})
                }
                None => json!({"error": format!("Instance '{name}' not found")}),
            }
        }
        Err(e) => json!({"error": format!("API unavailable: {e}")}),
    }
}

pub fn replace_instance(home: &Path, name: &str, reason: &str) -> Value {
    if let Err(e) = crate::agent::validate_name(name) {
        return json!({"error": e});
    }
    let handover = crate::api::call(home, &json!({"method": crate::api::method::LIST}))
        .ok()
        .and_then(|resp| {
            resp["result"]["agents"]
                .as_array()?
                .iter()
                .find(|a| a["name"].as_str() == Some(name))
                .map(|a| {
                    format!(
                        "Previous instance state: {}, health: {}. Replaced due to: {reason}",
                        a["agent_state"].as_str().unwrap_or("unknown"),
                        a["health_state"].as_str().unwrap_or("unknown")
                    )
                })
        })
        .unwrap_or_else(|| format!("Replaced due to: {reason}"));

    let _ = crate::api::call(
        home,
        &json!({"method": crate::api::method::KILL, "params": {"name": name}}),
    );
    let _ = crate::inbox::enqueue(
        home,
        name,
        crate::inbox::InboxMessage {
            from: "system:replace".to_string(),
            text: format!("[handover] {handover}"),
            kind: Some("handover".to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
        },
    );
    tracing::info!(%name, %reason, "replace_instance");
    json!({"name": name, "reason": reason, "note": "Instance killed. Auto-respawn will create fresh instance with handover context."})
}

pub fn set_display_name(home: &Path, instance_name: &str, display_name: &str) -> Value {
    save_metadata(home, instance_name, "display_name", json!(display_name));
    json!({"display_name": display_name})
}

pub fn set_description(home: &Path, instance_name: &str, description: &str) -> Value {
    save_metadata(home, instance_name, "description", json!(description));
    json!({"description": description})
}

// ---------------------------------------------------------------------------
// CI
// ---------------------------------------------------------------------------

pub fn watch_ci(
    home: &Path,
    instance_name: &str,
    repo: &str,
    branch: &str,
    interval: u64,
) -> Value {
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

pub fn unwatch_ci(home: &Path, repo: &str) -> Value {
    let safe_name = repo.replace('/', "_");
    let path = home.join("ci-watches").join(format!("{safe_name}.json"));
    let _ = std::fs::remove_file(&path);
    json!({"repo": repo, "watching": false})
}

// ---------------------------------------------------------------------------
// Repo
// ---------------------------------------------------------------------------

pub fn checkout_repo(home: &Path, instance_name: &str, source: &str, branch: &str) -> Value {
    if !validate_branch(branch) {
        return json!({"error": format!("invalid branch name '{branch}'")});
    }
    let worktree_dir = home.join("worktrees").join(format!(
        "{}-{}",
        instance_name,
        source.replace('/', "_").replace('~', "")
    ));
    std::fs::create_dir_all(worktree_dir.parent().unwrap_or(home)).ok();
    let source_path = if source.starts_with('/') || source.starts_with('~') {
        source
            .strip_prefix("~/")
            .map(|rest| format!("{}/{rest}", crate::user_home_dir().display()))
            .unwrap_or_else(|| source.to_string())
    } else {
        crate::api::call(home, &json!({"method": crate::api::method::LIST}))
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

pub fn release_repo(path: &str) -> Value {
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

// ---------------------------------------------------------------------------
// Helpers (pub for handlers.rs to use too)
// ---------------------------------------------------------------------------

/// Load metadata for an instance and merge it into the given JSON value.
pub fn merge_metadata(home: &Path, name: &str, info: &mut Value) {
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

pub fn save_metadata(home: &Path, instance_name: &str, key: &str, value: Value) {
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
pub fn get_submit_key(home: &Path, target: &str) -> String {
    let fleet_path = home.join("fleet.yaml");
    if let Ok(config) = crate::fleet::FleetConfig::load(&fleet_path) {
        if let Some(resolved) = config.resolve_instance(target) {
            return resolved.submit_key;
        }
    }
    "\r".to_string()
}

/// Validate a git branch name. Only allows [a-zA-Z0-9/_.-], rejects ".." and leading "-".
pub fn validate_branch(branch: &str) -> bool {
    !branch.is_empty()
        && !branch.contains("..")
        && !branch.starts_with('-')
        && branch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '/' || c == '_' || c == '-' || c == '.')
}

/// Clean up files generated by agend-terminal in an instance's working directory.
/// If the directory is under $AGEND_HOME/workspace/, remove the entire directory.
/// Otherwise, only remove agend-generated files to avoid deleting user code.
pub fn cleanup_working_dir(home: &Path, name: &str, working_dir: &Path) {
    let workspaces = home.join("workspace");

    // If under $AGEND_HOME/workspace/, remove the whole directory
    if working_dir.starts_with(&workspaces) {
        if let Err(e) = std::fs::remove_dir_all(working_dir) {
            tracing::debug!(dir = %working_dir.display(), error = %e, "cleanup: remove workspace");
        } else {
            tracing::info!(dir = %working_dir.display(), "removed workspace");
        }
    } else {
        // User-provided working directory: only remove agend-generated files
        let agend_files = [
            // Claude
            ".claude/settings.local.json",
            "mcp-config.json",
            "claude-settings.json",
            "statusline.sh",
            "statusline.json",
            ".claude/rules/agend.md",
            // Gemini
            ".gemini/settings.json",
            // OpenCode
            "opencode.json",
            "instructions/agend.md",
            // Codex
            ".codex/config.toml",
            "AGENTS.md",
            // Kiro
            ".kiro/settings/mcp.json",
            ".kiro/settings/agend-mcp-wrapper.sh",
            ".kiro/steering/agend.md",
            ".kiro/agents/agend.json",
            ".kiro/agents/agend-prompt.md",
            ".kiro/agents/default.json",
            ".kiro/prompts/agend.md",
            ".kiro/settings.json",
        ];
        for file in &agend_files {
            let path = working_dir.join(file);
            if path.exists() {
                let _ = std::fs::remove_file(&path);
            }
        }

        // Clean up worktree if exists
        let wt_dir = working_dir.join(".worktrees").join(name);
        if wt_dir.exists() {
            let _ = std::process::Command::new("git")
                .args([
                    "worktree",
                    "remove",
                    "--force",
                    &wt_dir.display().to_string(),
                ])
                .current_dir(working_dir)
                .output();
            tracing::info!(dir = %wt_dir.display(), "removed worktree");
        }
    }

    // Always clean up metadata + session (regardless of workspace vs user dir)
    // Remove metadata
    let meta = home.join("metadata").join(format!("{name}.json"));
    let _ = std::fs::remove_file(&meta);

    // Remove session ID
    let sid = home.join("sessions").join(format!("{name}.sid"));
    let _ = std::fs::remove_file(&sid);
}

/// Internal helper: send a message to a target instance via API, falling back to direct inbox delivery.
pub fn send_to(home: &Path, from: &str, target: &str, text: &str, kind: &str) -> Value {
    match crate::api::call(
        home,
        &json!({
            "method": crate::api::method::SEND,
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
                &crate::inbox::NotifySource::Agent(from),
                text,
                &submit_key,
                None,
            );
            json!({"target": target, "note": format!("API unavailable: {e}")})
        }
    }
}

/// List agents published in the active daemon's run directory.
pub fn list_agents() -> Vec<String> {
    let home = crate::home_dir();
    let run = match crate::daemon::find_active_run_dir(&home) {
        Some(r) => r,
        None => return Vec::new(),
    };
    let mut agents = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&run) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".port") && name != "api.port" {
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
            "agend-ops-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn branch_valid() {
        assert!(validate_branch("main"));
        assert!(validate_branch("feature/foo"));
        assert!(validate_branch("v1.0.0"));
    }

    #[test]
    fn branch_rejects_dotdot() {
        assert!(!validate_branch(".."));
        assert!(!validate_branch("foo/.."));
    }

    #[test]
    fn branch_rejects_special() {
        assert!(!validate_branch(""));
        assert!(!validate_branch("-main"));
        assert!(!validate_branch("foo;bar"));
    }

    #[test]
    fn metadata_merge_no_file() {
        let home = tmp_home("meta_no_file");
        let mut info = json!({"name": "a"});
        merge_metadata(&home, "a", &mut info);
        assert_eq!(info["name"], "a");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn metadata_merge_fields() {
        let home = tmp_home("meta_fields");
        std::fs::create_dir_all(home.join("metadata")).ok();
        std::fs::write(
            home.join("metadata/a.json"),
            r#"{"display_name":"Dev","x":1}"#,
        )
        .ok();
        let mut info = json!({"name": "a"});
        merge_metadata(&home, "a", &mut info);
        assert_eq!(info["display_name"], "Dev");
        assert_eq!(info["x"], 1);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn metadata_save_roundtrip() {
        let home = tmp_home("meta_save");
        save_metadata(&home, "a", "key", json!("val"));
        let c = std::fs::read_to_string(home.join("metadata/a.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        assert_eq!(v["key"], "val");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn submit_key_default() {
        let home = tmp_home("sk");
        assert_eq!(get_submit_key(&home, "x"), "\r");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cleanup_workspace_removes_dir() {
        let home = tmp_home("cw");
        let ws = home.join("workspace/agent1");
        std::fs::create_dir_all(&ws).ok();
        std::fs::write(ws.join("f.txt"), "x").ok();
        cleanup_working_dir(&home, "agent1", &ws);
        assert!(!ws.exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cleanup_user_dir_selective() {
        let home = tmp_home("cu");
        let ud = tmp_home("cu_proj");
        std::fs::write(ud.join("main.rs"), "fn main(){}").ok();
        std::fs::write(ud.join("opencode.json"), "{}").ok();
        cleanup_working_dir(&home, "a", &ud);
        assert!(ud.join("main.rs").exists());
        assert!(!ud.join("opencode.json").exists());
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&ud).ok();
    }

    #[test]
    fn cleanup_metadata_and_session() {
        let home = tmp_home("cms");
        let ws = home.join("workspace/a");
        std::fs::create_dir_all(&ws).ok();
        std::fs::create_dir_all(home.join("metadata")).ok();
        std::fs::write(home.join("metadata/a.json"), "{}").ok();
        std::fs::create_dir_all(home.join("sessions")).ok();
        std::fs::write(home.join("sessions/a.sid"), "x").ok();
        cleanup_working_dir(&home, "a", &ws);
        assert!(!home.join("metadata/a.json").exists());
        assert!(!home.join("sessions/a.sid").exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn no_mcp_prefix_in_ops() {
        // Verify old prefix was fully replaced with [agend]
        let source = include_str!("ops.rs");
        let old_prefix = format!("[{}]", "mcp");
        let lines_with_old: Vec<_> = source
            .lines()
            .filter(|l| l.contains(&old_prefix) && !l.contains("test"))
            .collect();
        assert!(
            lines_with_old.is_empty(),
            "ops.rs has old prefix: {:?}",
            lines_with_old
        );
    }

    #[test]
    fn reply_returns_error_without_fleet() {
        let home = tmp_home("reply_nofleet");
        let result = reply(&home, "test", "hello");
        assert!(result.get("error").is_some());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn drain_inbox_empty() {
        let home = tmp_home("drain_empty");
        let result = drain_inbox(&home, "test");
        assert_eq!(result["messages"].as_array().unwrap().len(), 0);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn backend_resolves_to_preset_command() {
        // "kiro" should resolve to "kiro-cli" via preset
        let resolved = crate::backend::Backend::from_command("kiro").map(|b| b.preset().command);
        assert_eq!(resolved, Some("kiro-cli"));

        // "claude" stays "claude"
        let resolved = crate::backend::Backend::from_command("claude").map(|b| b.preset().command);
        assert_eq!(resolved, Some("claude"));
    }
}
