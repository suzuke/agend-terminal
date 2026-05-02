use crate::agent_ops::{
    cleanup_working_dir, list_agents, merge_metadata, save_metadata, save_metadata_batch,
    validate_branch,
};
use crate::channel::telegram;
use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

use super::err_needs_identity;

pub(super) fn handle_list_instances(home: &Path, instance_name: &str) -> Value {
    match crate::api::call(home, &json!({"method": crate::api::method::LIST})) {
        Ok(resp) => {
            if let Some(agents) = resp["result"]["agents"].as_array() {
                let instances: Vec<Value> = agents
                    .iter()
                    .filter(|a| {
                        let backend = a["backend"].as_str().unwrap_or("");
                        crate::backend::Backend::from_command(backend).is_some()
                    })
                    .map(|a| {
                        let mut info = a.clone();
                        let name = a["name"].as_str().unwrap_or("");
                        merge_metadata(home, name, &mut info);
                        if name == instance_name {
                            info["is_self"] = json!(true);
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
    }
}

pub(super) fn handle_create_instance(home: &Path, args: &Value, instance_name: &str) -> Value {
    // Team mode: spawn count instances and group them
    if let Some(team_name) = args.get("team").and_then(|v| v.as_str()) {
        let default_backend = args["backend"]
            .as_str()
            .or_else(|| args["command"].as_str())
            .unwrap_or("claude");
        let per_member_backends: Vec<String> = match args.get("backends").and_then(|v| v.as_array())
        {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            None => {
                let count = args.get("count").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
                vec![default_backend.to_string(); count]
            }
        };
        if per_member_backends.is_empty() {
            return json!({"error": "count must be >= 1 (or backends must be non-empty)"});
        }
        let task = args.get("task").and_then(|v| v.as_str()).map(String::from);
        match crate::api::call(
            home,
            &json!({"method": crate::api::method::CREATE_TEAM, "params": {
                "name": team_name,
                "backends": per_member_backends,
                "description": args.get("description"),
            }}),
        ) {
            Ok(resp) if resp["ok"].as_bool() == Some(true) => {
                let spawned: Vec<String> = resp["spawned"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();

                if let Some(task_text) = task {
                    let home = home.to_path_buf();
                    let names = spawned.clone();
                    // fire-and-forget: team task injection waits 3s for agents to
                    // initialize, then injects task text. No JoinHandle needed —
                    // losing the injection on shutdown is acceptable (M5 §10.5).
                    std::thread::Builder::new()
                        .name("team_task_inject".into())
                        .spawn(move || {
                            std::thread::sleep(std::time::Duration::from_secs(3));
                            for inst_name in &names {
                                let _ = crate::api::call(
                                    &home,
                                    &json!({"method": crate::api::method::INJECT, "params": {"name": inst_name, "data": task_text}}),
                                );
                            }
                        })
                        .ok();
                }
                let mut result = json!({
                    "team": team_name,
                    "spawned": spawned,
                    "backends": per_member_backends,
                });
                if let Some(failed) = resp.get("failed") {
                    result["failed"] = failed.clone();
                }
                result
            }
            Ok(resp) => {
                json!({"error": resp["error"].as_str().unwrap_or("team creation failed")})
            }
            Err(e) => json!({"error": format!("API unavailable: {e}")}),
        }
    } else {
        spawn_single_instance(home, instance_name, args)
    }
}

pub(super) fn handle_delete_instance(home: &Path, args: &Value) -> Value {
    let name = match args["name"].as_str() {
        Some(n) => n,
        None => return json!({"error": "missing 'name'"}),
    };
    if let Err(e) = crate::agent::validate_name(name) {
        return json!({"error": e});
    }
    let fleet = crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).ok();
    if let Some(ref config) = fleet {
        if config.channel.is_some()
            && config.instances.contains_key(name)
            && config.instances.len() <= 1
        {
            return json!({"error": "cannot delete the last instance — channel needs at least one instance to receive messages"});
        }
    }
    let (topic_id, working_dir) = fleet
        .as_ref()
        .and_then(|c| {
            c.resolve_instance(name)
                .map(|r| (r.topic_id, r.working_directory))
        })
        .unwrap_or((None, None));
    let topic_id = topic_id.or_else(|| telegram::lookup_topic_for_instance(home, name));

    let _ = crate::api::call(
        home,
        &json!({"method": crate::api::method::DELETE, "params": {"name": name}}),
    );
    if let Err(e) = crate::fleet::remove_instance_from_yaml(home, name) {
        tracing::warn!(error = %e, "failed to remove from fleet.yaml");
    }
    if let Some(tid) = topic_id {
        telegram::delete_topic(home, tid);
    } else {
        tracing::warn!(%name, "no topic_id found for delete_instance — possible orphan");
    }
    if let Some(ref wd) = working_dir {
        cleanup_working_dir(home, name, wd);
    }
    crate::teams::remove_member_from_all(home, name);
    json!({"name": name})
}

pub(super) fn handle_start_instance(home: &Path, args: &Value) -> Value {
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
            let cmd_args = resolved.args.join(" ");
            match crate::api::call(
                home,
                &json!({"method": crate::api::method::SPAWN, "params": {
                    "name": name, "backend": resolved.backend_command, "args": cmd_args,
                    "mode": "resume",
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

pub(super) fn handle_describe_instance(home: &Path, args: &Value) -> Value {
    let name = args["name"].as_str().unwrap_or("");
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

pub(super) fn handle_replace_instance(home: &Path, args: &Value) -> Value {
    let name = match args["name"].as_str() {
        Some(n) => n,
        None => return json!({"error": "missing 'name'"}),
    };
    if let Err(e) = crate::agent::validate_name(name) {
        return json!({"error": e});
    }
    let reason = args["reason"].as_str().unwrap_or("manual replacement");
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
            schema_version: 0,
            id: None,
            read_at: None,
            thread_id: None,
            parent_id: None,
            task_id: None,
            force_meta: None,
            correlation_id: None,
            reviewed_head: None,
            from: "system:replace".to_string(),
            text: format!("[handover] {handover}"),
            kind: Some("handover".to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            channel: None,
            delivery_mode: None,
            attachments: vec![],
            in_reply_to_msg_id: None,
            in_reply_to_excerpt: None,
            superseded_by: None,
        },
    );
    tracing::info!(%name, %reason, "replace_instance");
    json!({"name": name, "reason": reason, "note": "Instance killed. Auto-respawn will create fresh instance with handover context."})
}

pub(super) fn handle_set_display_name(home: &Path, args: &Value, instance_name: &str) -> Value {
    let display_name = args["name"].as_str().unwrap_or("");
    save_metadata(home, instance_name, "display_name", json!(display_name));
    json!({"display_name": display_name})
}

pub(super) fn handle_set_description(home: &Path, args: &Value, instance_name: &str) -> Value {
    let desc = args["description"].as_str().unwrap_or("");
    save_metadata(home, instance_name, "description", json!(desc));
    json!({"description": desc})
}

pub(super) fn handle_interrupt(home: &Path, args: &Value) -> Value {
    let target = match args["target"].as_str() {
        Some(t) => t,
        None => return json!({"error": "missing 'target'"}),
    };
    if let Err(e) = crate::agent::validate_name(target) {
        return json!({"error": e});
    }
    match crate::api::call(home, &super::interrupt_esc_params(target)) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            if let Some(reason) = args["reason"].as_str() {
                let header = crate::inbox::format_event_header("interrupt", &[("reason", reason)]);
                crate::inbox::compose_aware_inject(home, target, &header);
            }
            json!({"ok": true, "target": target})
        }
        Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("inject failed")}),
        Err(e) => {
            json!({"error": format!("interrupt failed — agent '{target}' not reachable (API unavailable: {e})")})
        }
    }
}

pub(super) fn handle_set_waiting_on(
    home: &Path,
    args: &Value,
    instance_name: &str,
    sender: &Option<Sender>,
) -> Value {
    let Some(_) = sender.as_ref() else {
        return err_needs_identity("set_waiting_on");
    };
    let condition = args["condition"].as_str().unwrap_or("");
    if condition.is_empty() {
        crate::daemon::heartbeat_pair::update_with(instance_name, |p| {
            p.waiting_on_since_ms = None;
        });
        save_metadata_batch(
            home,
            instance_name,
            &[
                ("waiting_on", json!(null)),
                ("waiting_on_since", json!(null)),
            ],
        );
        json!({"cleared": true})
    } else {
        let now_ms = crate::daemon::heartbeat_pair::now_ms();
        crate::daemon::heartbeat_pair::update_with(instance_name, |p| {
            p.heartbeat_at_ms = now_ms;
            p.waiting_on_since_ms = Some(now_ms);
        });
        let now = chrono::Utc::now().to_rfc3339();
        save_metadata_batch(
            home,
            instance_name,
            &[
                ("waiting_on", json!(condition)),
                ("waiting_on_since", json!(&now)),
            ],
        );
        json!({"waiting_on": condition, "since": now})
    }
}

pub(super) fn handle_move_pane(home: &Path, args: &Value) -> Value {
    match crate::api::call(
        home,
        &json!({"method": crate::api::method::MOVE_PANE, "params": args}),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            json!({"ok": true, "agent": args["agent"], "target_tab": args["target_tab"]})
        }
        Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("move_pane failed")}),
        Err(e) => json!({"error": format!("move_pane: {e}")}),
    }
}

pub(super) fn handle_pane_snapshot(home: &Path, args: &Value) -> Value {
    let target = match args["target"].as_str() {
        Some(t) => t,
        None => return json!({"error": "missing 'target'"}),
    };
    if let Err(e) = crate::agent::validate_name(target) {
        return json!({"error": e});
    }
    let lines_u64 = args["lines"].as_u64().unwrap_or(100);
    // M1: explicit bounds check before u64→usize cast (32-bit safety)
    if lines_u64 > 10000 {
        return json!({"error": "lines must be <= 10000 (scrolling_history limit)"});
    }
    let lines = lines_u64 as usize;
    match crate::api::call(
        home,
        &json!({"method": crate::api::method::PANE_SNAPSHOT, "params": {"name": target, "lines": lines}}),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            json!({"ok": true, "text": resp["text"]})
        }
        Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("pane_snapshot failed")}),
        Err(e) => json!({"error": format!("pane_snapshot: {e}")}),
    }
}

pub(super) fn handle_report_health(
    home: &Path,
    args: &Value,
    instance_name: &str,
    sender: &Option<Sender>,
) -> Value {
    let Some(_) = sender.as_ref() else {
        return err_needs_identity("report_health");
    };
    let reason = match args["reason"].as_str() {
        Some(r) => r,
        None => return json!({"error": "missing 'reason'"}),
    };
    match crate::api::call(
        home,
        &json!({
            "method": crate::api::method::SET_BLOCKED_REASON,
            "params": {
                "name": instance_name,
                "reason": reason,
                "retry_after_secs": args.get("retry_after_secs")
            }
        }),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            json!({
                "status": "reason_set",
                "reason": reason,
                "current_state": resp["current_state"]
            })
        }
        Ok(resp) => {
            json!({"error": resp["error"].as_str().unwrap_or("set_blocked_reason failed")})
        }
        Err(e) => json!({"error": format!("{e}")}),
    }
}

pub(super) fn handle_clear_blocked_reason(home: &Path, args: &Value) -> Value {
    let instance = match args["instance"].as_str() {
        Some(n) => n,
        None => return json!({"error": "missing 'instance'"}),
    };
    let mut params = json!({"name": instance});
    if let Some(r) = args["reason"].as_str() {
        params["reason"] = json!(r);
    }
    match crate::api::call(
        home,
        &json!({
            "method": crate::api::method::CLEAR_BLOCKED_REASON,
            "params": params
        }),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            json!({
                "status": "cleared",
                "instance": instance,
                "was": resp["was"]
            })
        }
        Ok(resp) => {
            json!({"error": resp["error"].as_str().unwrap_or("clear_blocked_reason failed")})
        }
        Err(e) => json!({"error": format!("{e}")}),
    }
}

// --- Private helpers (moved from mod.rs) ---

pub(super) fn resolve_team_layout(
    home: &Path,
    name: &str,
    layout_arg: Option<&serde_json::Value>,
    target_pane_arg: Option<&serde_json::Value>,
) -> (&'static str, Option<String>) {
    let caller_set_layout = layout_arg.and_then(|v| v.as_str()).is_some();
    let caller_set_target = target_pane_arg
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .is_some();
    if !caller_set_layout && !caller_set_target {
        if let Some(team) = crate::teams::find_team_for(home, name) {
            let anchor = team.orchestrator.or_else(|| team.members.first().cloned());
            return ("split-right", anchor);
        }
    }
    let layout = layout_arg.and_then(|v| v.as_str()).unwrap_or("tab");
    let target = target_pane_arg
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from);
    let layout = match layout {
        "split-right" => "split-right",
        "split-below" => "split-below",
        _ => "tab",
    };
    (layout, target)
}

fn spawn_single_instance(home: &Path, instance_name: &str, args: &Value) -> Value {
    let raw_name = match args["name"].as_str() {
        Some(n) => n,
        None => return json!({"error": "missing 'name'"}),
    };
    if let Err(e) = crate::agent::validate_name(raw_name) {
        return json!({"error": e});
    }
    let name_owned = {
        // M4: AtomicU64 prevents 65536 wrap-around collision
        use std::sync::atomic::{AtomicU64, Ordering};
        static DEDUP_SEQ: AtomicU64 = AtomicU64::new(0);

        let existing: std::collections::HashSet<String> =
            crate::fleet::FleetConfig::load(&home.join("fleet.yaml"))
                .map(|c| c.instance_names().into_iter().collect())
                .unwrap_or_default();
        if existing.contains(raw_name) {
            let seq = DEDUP_SEQ.fetch_add(1, Ordering::Relaxed);
            let deduped = format!("{raw_name}-{seq:04x}");
            tracing::info!(original = raw_name, deduped = %deduped, "name conflict, auto-deduped");
            deduped
        } else {
            raw_name.to_string()
        }
    };
    let name: &str = &name_owned;
    let command = args["backend"]
        .as_str()
        .or_else(|| args["command"].as_str())
        .unwrap_or("claude");
    let mut cmd_args = args
        .get("args")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_default();
    if let Some(model) = args
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|m| !m.is_empty())
    {
        let model_val = crate::backend::Backend::from_command(command)
            .map(|b| b.format_model_arg(model))
            .unwrap_or_else(|| model.to_string());
        if !cmd_args.is_empty() {
            cmd_args.push(' ');
        }
        cmd_args.push_str(&format!("--model {model_val}"));
    }
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

    std::fs::create_dir_all(&work_dir).ok();

    let task = args.get("task").and_then(|v| v.as_str()).map(String::from);
    let role = args.get("role").and_then(|v| v.as_str()).map(String::from);
    let backend_str = args
        .get("backend")
        .and_then(|v| v.as_str())
        .map(String::from);
    let (layout, target_pane_owned) =
        resolve_team_layout(home, name, args.get("layout"), args.get("target_pane"));
    let target_pane = target_pane_owned.as_deref();

    match crate::api::call(
        home,
        &json!({"method": crate::api::method::SPAWN, "params": {
            "name": name, "backend": command, "args": &cmd_args,
            "working_directory": work_dir,
            "layout": layout, "spawner": instance_name,
            "target_pane": target_pane,
            "role": role,
        }}),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => {
            let entry = crate::fleet::InstanceYamlEntry {
                backend: backend_str
                    .or_else(|| {
                        crate::backend::Backend::from_command(command).map(|b| b.name().to_string())
                    })
                    .or_else(|| Some(command.to_string())),
                working_directory: Some(work_dir.clone()),
                role,
            };
            if let Err(e) = crate::fleet::add_instance_to_yaml(home, name, &entry) {
                tracing::warn!(error = %e, "failed to persist to fleet.yaml");
            }
            let topic_id = resp["result"]["topic_id"].as_i64();
            if let Some(task_text) = task {
                let h = home.to_path_buf();
                let n = name.to_string();
                // fire-and-forget: single-agent task injection (M5 §10.5).
                std::thread::Builder::new()
                    .name("task_inject".into())
                    .spawn(move || {
                        std::thread::sleep(std::time::Duration::from_secs(3));
                        let _ = crate::api::call(
                            &h,
                            &json!({"method": crate::api::method::INJECT, "params": {"name": n, "data": task_text}}),
                        );
                    })
                    .ok();
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
