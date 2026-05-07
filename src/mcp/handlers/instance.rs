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
    match full_delete_instance(home, name) {
        Ok(()) => json!({"name": name}),
        Err(detail) => json!({
            "name": name,
            "error": format!(
                "delete completed with residual state — fleet may resurrect on next reconcile: {detail}"
            ),
        }),
    }
}

/// Sprint 53 Smoke 2 r1: shared full single-instance teardown used by both
/// the MCP `delete_instance` handler and the TUI close path
/// (`app/overlay.rs::Overlay::ConfirmClose`). Covers everything
/// `handle_delete_instance` historically did EXCEPT the channel-singleton
/// guard, which stays MCP-only — TUI close is operator-driven and we don't
/// want to refuse a close because of channel routing.
///
/// Side effects, all expected for both call sites:
/// - **PTY kill + child-tree reap** via `daemon::lifecycle::delete_transaction`
///   (process-tree kill, synchronous wait-for-exit, registry remove,
///   active-channel binding drop, configs map remove, IPC port remove,
///   event log).
/// - **fleet.yaml entry removal** so daemon restart's `auto_start_fleet`
///   doesn't resurrect the dead agent.
/// - **Telegram topic delete** for the resolved per-instance topic — leaving
///   it would orphan the topic on the chat side.
/// - **Working-dir cleanup** via `cleanup_working_dir` (the shared
///   `home/workspace/<name>` whole-tree branch + the user-dir agend-files
///   branch). Custom-directory deployment subdirs are still cleaned by the
///   reconcile path's `cleanup_deployment_dirs` after this — see
///   `app/overlay.rs` for the layering.
/// - **Team membership removal** so a closed instance doesn't leave a
///   dangling team-member reference.
///
/// Returns `Ok(())` when every fleet store is verified clean post-delete.
/// Returns `Err(detail)` (Sprint 54 P1-B Bug 1 fix — transactional-or-loud)
/// when any store still holds the name after the cleanup run, so the
/// caller can surface the residual rather than silently leaving partial
/// state for `auto_start_fleet` to resurrect on next reconcile. `detail`
/// is a human-readable string listing the residual stores plus any
/// per-step error captured during cleanup.
pub(crate) fn full_delete_instance(home: &Path, name: &str) -> Result<(), String> {
    let fleet = crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).ok();
    let (topic_id, working_dir) = fleet
        .as_ref()
        .and_then(|c| {
            c.resolve_instance(name)
                .map(|r| (r.topic_id, r.working_directory))
        })
        .unwrap_or((None, None));
    let topic_id = topic_id.or_else(|| telegram::lookup_topic_for_instance(home, name));

    // Sprint 54 P1-B Bug 1: collect per-step errors instead of silently
    // swallowing them. Each cleanup step runs best-effort so even when
    // earlier steps fail the later ones still get a chance, but every
    // surfaced error feeds the final audit so the caller knows which
    // stores left residual state.
    let mut step_errors: Vec<String> = Vec::new();

    let _ = crate::api::call(
        home,
        &json!({"method": crate::api::method::DELETE, "params": {"name": name}}),
    );
    if let Err(e) = crate::fleet::remove_instance_from_yaml(home, name) {
        step_errors.push(format!("fleet.yaml removal: {e}"));
        tracing::error!(name, error = %e, "full_delete_instance: fleet.yaml removal failed");
    }
    if let Some(tid) = topic_id {
        telegram::delete_topic(home, tid);
    } else {
        tracing::warn!(%name, "no topic_id found for full_delete_instance — possible orphan");
    }
    if let Some(ref wd) = working_dir {
        cleanup_working_dir(home, name, wd);
    }
    crate::teams::remove_member_from_all(home, name);

    // Sprint 54 P1-B Bug 1 audit: enumerate every store that still holds
    // the name. If any do, surface a loud error instead of returning
    // success — `auto_start_fleet` revival of a half-deleted instance is
    // exactly the silent-drop class pattern this fix prevents.
    let residual = name_residual_anywhere(home, name);
    if residual.is_empty() && step_errors.is_empty() {
        return Ok(());
    }
    let detail = match (residual.is_empty(), step_errors.is_empty()) {
        (true, _) => format!("step errors: {}", step_errors.join("; ")),
        (false, true) => format!("residual stores: {}", residual.join(", ")),
        (false, false) => format!(
            "step errors: {}; residual stores: {}",
            step_errors.join("; "),
            residual.join(", ")
        ),
    };
    tracing::error!(
        name,
        residual = ?residual,
        step_errors = ?step_errors,
        "full_delete_instance left residual state — silent-drop class pattern blocked"
    );
    Err(detail)
}

/// Sprint 54 P1-B Bug 1: enumerate every fleet store that still holds
/// `name` after a delete attempt. Returns the list of store identifiers
/// (`"fleet.yaml"`, `"metadata"`, etc.) so callers can surface the
/// residual loudly. Per the P1-B RCA doc (PR #509 squash 66682d2):
/// three primary stores plus four auxiliary on-disk artefacts where
/// instance-name-bearing state survives delete; this audit covers them
/// all so the daemon-process-internal `agent::registry` /
/// `agent::externals` (which require live registry handles to inspect)
/// are checked separately by their own callers.
pub(crate) fn name_residual_anywhere(home: &Path, name: &str) -> Vec<&'static str> {
    let mut sources: Vec<&'static str> = Vec::new();
    if let Ok(cfg) = crate::fleet::FleetConfig::load(&home.join("fleet.yaml")) {
        if cfg.instances.contains_key(name) {
            sources.push("fleet.yaml");
        }
        if cfg
            .teams
            .values()
            .any(|t| t.members.iter().any(|m| m == name))
        {
            sources.push("fleet.yaml/teams");
        }
    }
    if home.join("metadata").join(format!("{name}.json")).exists() {
        sources.push("metadata");
    }
    if home.join("inbox").join(format!("{name}.jsonl")).exists() {
        sources.push("inbox");
    }
    if home
        .join("runtime")
        .join(name)
        .join("binding.json")
        .exists()
    {
        sources.push("runtime/binding.json");
    }
    if home
        .join("notification-queue")
        .join(format!("{name}.jsonl"))
        .exists()
    {
        sources.push("notification-queue");
    }
    sources
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
                    // Surface topic_id from fleet.yaml for debugging (#415).
                    if info.get("topic_id").is_none() {
                        if let Some(tid) = crate::fleet::FleetConfig::load(&home.join("fleet.yaml"))
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
            from_id: None,
            broadcast_context: None,
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
                instructions: None,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! Sprint 54 P1-B Bug 1 fix: residual-store audit + transactional-or-loud
    //! `full_delete_instance`. Tests cover the audit fn's per-store
    //! detection (clean / each-store-positive / multi-source) and the
    //! delete fn's Result-return contract (Err on residual,
    //! Ok on clean). `full_delete_instance` reaches into the daemon's
    //! `api::call` which fails harmlessly with no daemon — we exercise
    //! the post-cleanup audit branch by pre-seeding residual state
    //! directly, mirroring the silent-drop class production scenario.

    use super::name_residual_anywhere;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> PathBuf {
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-p1b-bug1-test-{}-{}-{}",
            std::process::id(),
            tag,
            id,
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn name_residual_anywhere_clean_home_returns_empty() {
        let home = tmp_home("clean");
        assert!(name_residual_anywhere(&home, "ghost").is_empty());
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn name_residual_anywhere_detects_fleet_yaml_instance_residual() {
        let home = tmp_home("fleet_yaml_inst");
        std::fs::write(
            home.join("fleet.yaml"),
            "instances:\n  zombie:\n    backend: claude\n",
        )
        .unwrap();
        let sources = name_residual_anywhere(&home, "zombie");
        assert!(
            sources.contains(&"fleet.yaml"),
            "fleet.yaml instances residual must surface, got {sources:?}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn name_residual_anywhere_detects_fleet_yaml_team_member_residual() {
        // Sprint 54 PR #507 unification: teams live in fleet.yaml; a
        // delete that misses team membership cleanup leaves the name
        // resolvable as a team member, which the audit must surface
        // separately from the instances: stanza.
        let home = tmp_home("fleet_yaml_team");
        std::fs::write(
            home.join("fleet.yaml"),
            "teams:\n  ops:\n    members: [zombie, alice]\n    orchestrator: alice\n",
        )
        .unwrap();
        let sources = name_residual_anywhere(&home, "zombie");
        assert!(
            sources.contains(&"fleet.yaml/teams"),
            "team-member residual must surface, got {sources:?}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn name_residual_anywhere_detects_metadata_residual() {
        let home = tmp_home("metadata");
        let meta_dir = home.join("metadata");
        std::fs::create_dir_all(&meta_dir).unwrap();
        std::fs::write(meta_dir.join("zombie.json"), "{}").unwrap();
        let sources = name_residual_anywhere(&home, "zombie");
        assert!(sources.contains(&"metadata"), "got {sources:?}");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn name_residual_anywhere_detects_inbox_residual() {
        let home = tmp_home("inbox");
        let inbox_dir = home.join("inbox");
        std::fs::create_dir_all(&inbox_dir).unwrap();
        std::fs::write(inbox_dir.join("zombie.jsonl"), "").unwrap();
        let sources = name_residual_anywhere(&home, "zombie");
        assert!(sources.contains(&"inbox"), "got {sources:?}");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn name_residual_anywhere_detects_runtime_binding_residual() {
        let home = tmp_home("binding");
        let dir = home.join("runtime").join("zombie");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("binding.json"), "{}").unwrap();
        let sources = name_residual_anywhere(&home, "zombie");
        assert!(sources.contains(&"runtime/binding.json"), "got {sources:?}");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn name_residual_anywhere_detects_notification_queue_residual() {
        let home = tmp_home("nq");
        let dir = home.join("notification-queue");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("zombie.jsonl"), "").unwrap();
        let sources = name_residual_anywhere(&home, "zombie");
        assert!(sources.contains(&"notification-queue"), "got {sources:?}");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn name_residual_anywhere_returns_multi_source_when_several_stores_dirty() {
        // Regression-proof: dropping the per-store check must surface
        // as a missing entry in this list, not as a silent skip.
        let home = tmp_home("multi");
        std::fs::write(
            home.join("fleet.yaml"),
            "instances:\n  zombie:\n    backend: claude\n",
        )
        .unwrap();
        std::fs::create_dir_all(home.join("metadata")).unwrap();
        std::fs::write(home.join("metadata").join("zombie.json"), "{}").unwrap();
        std::fs::create_dir_all(home.join("inbox")).unwrap();
        std::fs::write(home.join("inbox").join("zombie.jsonl"), "").unwrap();
        let sources = name_residual_anywhere(&home, "zombie");
        for expected in ["fleet.yaml", "metadata", "inbox"] {
            assert!(
                sources.contains(&expected),
                "multi-source audit must include {expected}, got {sources:?}"
            );
        }
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn full_delete_instance_returns_err_when_residual_remains_post_cleanup() {
        // Pre-seed metadata + inbox files before delete; daemon API is
        // unreachable in the test process, so `api::call` fails
        // (silently). fleet.yaml removal is also a no-op (no fleet.yaml
        // present). The post-cleanup audit must surface the
        // metadata/inbox residual and the fn must return Err.
        let home = tmp_home("full_residual");
        std::fs::create_dir_all(home.join("metadata")).unwrap();
        std::fs::write(home.join("metadata").join("zombie.json"), "{}").unwrap();
        let result = super::full_delete_instance(&home, "zombie");
        let err = result.expect_err(
            "metadata residual after cleanup must surface as Err — silent-drop class blocked",
        );
        assert!(
            err.contains("metadata"),
            "Err detail must name the residual store, got: {err:?}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn full_delete_instance_returns_ok_when_no_residual() {
        // Clean home: no fleet.yaml, no metadata, no inbox — every
        // cleanup step is a no-op AND the audit reports clean.
        // `api::call` failure during DELETE is harmless because there's
        // nothing to clean and the audit returns empty.
        let home = tmp_home("full_clean");
        let result = super::full_delete_instance(&home, "ghost");
        assert!(
            result.is_ok(),
            "clean home + clean post-audit must return Ok, got: {result:?}"
        );
        std::fs::remove_dir_all(home).ok();
    }
}
