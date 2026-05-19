use crate::agent_ops::{list_agents, merge_metadata, save_metadata, save_metadata_batch};
use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

use super::err_needs_identity;

pub(super) fn handle_list_instances(home: &Path, args: &Value, instance_name: &str) -> Value {
    // If `name` param is provided, return detailed info for that instance (replaces describe_instance)
    if let Some(target) = args["name"].as_str().filter(|s| !s.is_empty()) {
        return handle_describe_instance(home, &json!({"name": target}));
    }
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
        super::instance_spawn::spawn_single_instance(home, instance_name, args)
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
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok();
    if let Some(ref config) = fleet {
        if config.channel.is_some()
            && config.instances.contains_key(name)
            && config.instances.len() <= 1
        {
            return json!({"error": "cannot delete the last instance — channel needs at least one instance to receive messages"});
        }
    }
    // Sprint 54 P1-B Bug 1: lifecycle moved to sibling
    // `instance_lifecycle` module so this file stays under the
    // `tests/file_size_invariant.rs` 700-LOC ceiling.
    match super::instance_lifecycle::full_delete_instance(home, name) {
        Ok(()) => json!({"name": name}),
        Err(detail) => json!({
            "name": name,
            "error": format!(
                "delete completed with residual state — fleet may resurrect on next reconcile: {detail}"
            ),
        }),
    }
}

pub(super) fn handle_start_instance(home: &Path, args: &Value) -> Value {
    let name = match args["name"].as_str() {
        Some(n) => n,
        None => return json!({"error": "missing 'name'"}),
    };
    if let Err(e) = crate::agent::validate_name(name) {
        return json!({"error": e});
    }
    let fleet_path = crate::fleet::fleet_yaml_path(home);
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
            // #900: forward the resolved env explicitly so the daemon's
            // SPAWN handler doesn't have to re-read fleet.yaml for what
            // we already have in hand. params.env wins over the fleet
            // fallback in handle_spawn, which keeps this RPC the
            // single-source-of-truth for the instance start.
            let env_json = serde_json::to_value(&resolved.env).unwrap_or(serde_json::Value::Null);
            match crate::api::call(
                home,
                &json!({"method": crate::api::method::SPAWN, "params": {
                    "name": name, "backend": resolved.backend_command, "args": cmd_args,
                    "mode": "resume",
                    "working_directory": resolved.working_directory.map(|p| p.display().to_string()),
                    "env": env_json,
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
                    // Surface topic_id from fleet.yaml for debugging (#415).
                    if info.get("topic_id").is_none() {
                        if let Some(tid) =
                            crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
                                .ok()
                                .and_then(|c| c.instances.get(name)?.topic_id)
                        {
                            info["topic_id"] = json!(tid);
                        }
                    }
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

    // Capture backend + working_directory before kill so we can respawn.
    // Prefer fleet.yaml (short name like "claude") over LIST API (which may
    // store a resolved path). SPAWN expects the short preset name.
    let fleet_resolved = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
        .ok()
        .and_then(|f| f.resolve_instance(name));

    let list_resp = crate::api::call(home, &json!({"method": crate::api::method::LIST}));
    let (backend, handover) = {
        let fleet_backend = fleet_resolved.as_ref().map(|r| r.backend_command.clone());
        let list_info = list_resp.ok().and_then(|resp| {
            resp["result"]["agents"]
                .as_array()?
                .iter()
                .find(|a| a["name"].as_str() == Some(name))
                .map(|a| {
                    let backend = a["backend"].as_str().unwrap_or("claude").to_string();
                    let handover = format!(
                        "Previous instance state: {}, health: {}. Replaced due to: {reason}",
                        a["agent_state"].as_str().unwrap_or("unknown"),
                        a["health_state"].as_str().unwrap_or("unknown")
                    );
                    (backend, handover)
                })
        });
        let backend = fleet_backend.unwrap_or_else(|| {
            list_info
                .as_ref()
                .map(|(b, _)| b.clone())
                .unwrap_or_else(|| "claude".to_string())
        });
        let handover = list_info
            .map(|(_, h)| h)
            .unwrap_or_else(|| format!("Replaced due to: {reason}"));
        (backend, handover)
    };

    // Resolve working_directory from fleet.yaml (survives kill).
    let working_dir = fleet_resolved
        .and_then(|r| r.working_directory)
        .map(|p| p.display().to_string());

    // Kill via DELETE (synchronous — waits for child exit + removes from registry).
    let _ = crate::api::call(
        home,
        &json!({"method": crate::api::method::DELETE, "params": {"name": name}}),
    );

    // Brief pause after kill to let OS reclaim resources (ports, file locks)
    // before spawning the replacement. Prevents startup race on fast exits.
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Enqueue handover context for the new instance.
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
            from_id: None,
            broadcast_context: None,
            sequencing: None,
            eta_minutes: None,
            reporting_cadence: None,
            worktree_binding_required: None,
        },
    );

    // Spawn fresh instance with same backend + working directory.
    let mut spawn_params = json!({"name": name, "backend": backend});
    if let Some(wd) = &working_dir {
        spawn_params["working_directory"] = json!(wd);
    }
    let spawn_result = crate::api::call(
        home,
        &json!({"method": crate::api::method::SPAWN, "params": spawn_params}),
    );

    let spawned = spawn_result
        .as_ref()
        .map(|r| r["ok"].as_bool() == Some(true))
        .unwrap_or(false);

    tracing::info!(%name, %reason, %spawned, "replace_instance");
    json!({"name": name, "reason": reason, "spawned": spawned})
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

// #964: spawn_single_instance + spawn_single_instance_impl live in
// sibling file `instance_spawn.rs` to keep this file under the 750-LOC
// file_size_invariant.
