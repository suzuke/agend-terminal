//! MCP tool dispatch — handle_tool() routes tool calls to implementations.

use crate::agent_ops::{
    cleanup_working_dir, get_submit_key, list_agents, merge_metadata, save_metadata, send_to,
    validate_branch,
};
use crate::channel::sink_registry::registry as ux_sink_registry;
use crate::channel::telegram;
use crate::channel::ux_event::{FleetEvent, UxEvent};
use crate::identity::Sender;
use serde_json::{json, Value};

/// True iff the MCP handler output should be treated as a success for
/// `FleetEvent` emission purposes. Handlers that wrap `send_to` return
/// `{"target": …}` (API path) or `{"target": …, "note": …}` (fallback
/// path) on success, and `{"error": …}` on failure; we mirror that
/// check here so a failed delegate_task / broadcast doesn't pollute
/// the fleet_binding with events that never actually left the daemon.
fn is_ok_result(value: &Value) -> bool {
    value.get("error").is_none()
}

/// Error payload for cross-instance tools invoked without a resolvable
/// `AGEND_INSTANCE_NAME`. Without this guard the message would land at the
/// receiver as `[from:]` with no originator.
fn err_needs_identity(tool: &str) -> Value {
    json!({
        "error": format!(
            "{tool} requires AGEND_INSTANCE_NAME to be set — cross-instance messaging needs a named sender"
        )
    })
}

pub fn handle_tool(tool: &str, args: &Value, instance_name: &str) -> Value {
    let home = crate::home_dir();
    // Explicit arg beats env var. Cross-instance arms require `Some`;
    // anonymous/standalone arms tolerate the empty `&str` view.
    let sender: Option<Sender> = Sender::new(instance_name).or_else(Sender::from_env);
    let instance_name: &str = sender.as_ref().map(Sender::as_str).unwrap_or("");

    match tool {
        // --- Channel ---
        "reply" => {
            let text = args["text"].as_str().unwrap_or("");
            tracing::info!(from = %instance_name, %text, "reply");
            let fleet_path = home.join("fleet.yaml");
            if fleet_path.exists() {
                match telegram::try_telegram_reply(instance_name, text) {
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
            match telegram::try_telegram_react(instance_name, emoji, message_id) {
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
            match telegram::try_telegram_edit(instance_name, message_id, text) {
                Ok(()) => json!({"message_id": message_id}),
                Err(e) => json!({"error": format!("{e}")}),
            }
        }
        "download_attachment" => {
            let file_id = match args["file_id"].as_str() {
                Some(f) => f,
                None => return json!({"error": "missing 'file_id'"}),
            };
            match telegram::try_download_attachment(instance_name, file_id) {
                Ok(path) => json!({"path": path}),
                Err(e) => json!({"error": format!("{e}")}),
            }
        }

        // --- Cross-instance communication ---
        "send_to_instance" | "send" => {
            let Some(sender) = sender.as_ref() else {
                return err_needs_identity(tool);
            };
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
            if *sender == target {
                return json!({"error": "cannot send to self — use a different instance_name"});
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
                    "method": crate::api::method::SEND,
                    "params": { "from": sender.as_str(), "target": target, "text": text, "kind": kind }
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
                        &crate::inbox::NotifySource::Agent(sender.as_str()),
                        text,
                        &submit_key,
                        None,
                    );
                    json!({"target": target, "note": format!("API unavailable, sent direct: {e}")})
                }
            }
        }
        "delegate_task" => {
            let Some(sender) = sender.as_ref() else {
                return err_needs_identity(tool);
            };
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
            let result = send_to(&home, sender, target, &msg, "task");
            if is_ok_result(&result) {
                // Fleet visibility (plan §4 `DelegateTask`, design §4.3).
                // `task_id: None` — MCP `delegate_task` has no typed id
                // slot; correlation appears later via `report_result`'s
                // `correlation_id`. Renderer omits id when None.
                ux_sink_registry().emit(&UxEvent::Fleet(FleetEvent::DelegateTask {
                    from: sender.as_str().to_string(),
                    to: target.to_string(),
                    summary: task.to_string(),
                    task_id: None,
                }));
            }
            result
        }
        "report_result" => {
            let Some(sender) = sender.as_ref() else {
                return err_needs_identity(tool);
            };
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
            let result = send_to(&home, sender, target, &msg, "report");
            if is_ok_result(&result) {
                // Fleet visibility. `correlation_id` is caller-chosen
                // (e.g. "AGD-42"); an empty string collapses to `None`
                // so the renderer omits the id rather than showing "()".
                let task_id = args["correlation_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                ux_sink_registry().emit(&UxEvent::Fleet(FleetEvent::ReportResult {
                    from: sender.as_str().to_string(),
                    to: target.to_string(),
                    summary: summary.to_string(),
                    task_id,
                }));
            }
            result
        }
        "request_information" => {
            let Some(sender) = sender.as_ref() else {
                return err_needs_identity(tool);
            };
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
            send_to(&home, sender, target, &msg, "query")
        }
        "broadcast" => {
            let Some(sender) = sender.as_ref() else {
                return err_needs_identity(tool);
            };
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
                .filter(|t| *sender != t.as_str())
                .collect();
            let kind = args["request_kind"].as_str().unwrap_or("update");
            let mut sent = Vec::new();
            for target in &targets {
                let _ = send_to(&home, sender, target, message, kind);
                sent.push(target.clone());
            }
            // Fleet visibility. Skip-emit on empty fan-out — a broadcast
            // to an empty target set didn't actually surface anywhere
            // and showing "[a → *0]" in fleet_binding is pure noise.
            if !sent.is_empty() {
                ux_sink_registry().emit(&UxEvent::Fleet(FleetEvent::Broadcast {
                    from: sender.as_str().to_string(),
                    recipients: sent.clone(),
                    summary: message.to_string(),
                }));
            }
            json!({"sent_to": sent, "count": sent.len()})
        }
        "inbox" => {
            let messages = crate::inbox::drain(&home, instance_name);
            json!({"messages": messages})
        }

        // --- Instance management ---
        "list_instances" => {
            match crate::api::call(&home, &json!({"method": crate::api::method::LIST})) {
                Ok(resp) => {
                    if let Some(agents) = resp["result"]["agents"].as_array() {
                        let instances: Vec<Value> = agents
                            .iter()
                            .filter(|a| {
                                // Hide non-agent backends (shells) from MCP tool results
                                let backend = a["backend"].as_str().unwrap_or("");
                                crate::backend::Backend::from_command(backend).is_some()
                            })
                            .map(|a| {
                                let mut info = a.clone();
                                let name = a["name"].as_str().unwrap_or("");
                                merge_metadata(&home, name, &mut info);
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
        "create_instance" => {
            // Team mode: spawn count instances and group them
            if let Some(team_name) = args.get("team").and_then(|v| v.as_str()) {
                let default_backend = args["backend"]
                    .as_str()
                    .or_else(|| args["command"].as_str())
                    .unwrap_or("claude");
                // `backends: [..]` lets a team mix different CLIs (e.g.
                // ["codex","kiro-cli","opencode","gemini"]). When present, it
                // dictates both membership count and per-member backend.
                // Otherwise fall back to `count` copies of the scalar backend.
                let per_member_backends: Vec<String> =
                    match args.get("backends").and_then(|v| v.as_array()) {
                        Some(arr) => arr
                            .iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect(),
                        None => {
                            let count =
                                args.get("count").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
                            vec![default_backend.to_string(); count]
                        }
                    };
                if per_member_backends.is_empty() {
                    return json!({"error": "count must be >= 1 (or backends must be non-empty)"});
                }
                let task = args.get("task").and_then(|v| v.as_str()).map(String::from);
                // Pre-generate instructions/mcp_config for all predicted team member
                // names before the API call. Team names are deterministic
                // ({team_name}-{i}) and the PTY starts immediately on spawn; generating
                // after the API call creates a race where the agent can read configs
                // before they exist. Matches the single-instance ordering above.
                for (i, be) in per_member_backends.iter().enumerate() {
                    let inst_name = format!("{team_name}-{}", i + 1);
                    let wd = home.join("workspace").join(&inst_name);
                    std::fs::create_dir_all(&wd).ok();
                    crate::instructions::generate(&wd, be);
                    crate::mcp_config::configure(&wd, be);
                }
                match crate::api::call(
                    &home,
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

                        std::thread::scope(|s| {
                            for inst_name in &spawned {
                                let h = &home;
                                s.spawn(move || {
                                    telegram::create_topic_for_instance(h, inst_name);
                                });
                            }
                        });

                        // Background task injection (don't block MCP response)
                        if let Some(task_text) = task {
                            let home = home.clone();
                            let names = spawned.clone();
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
                spawn_single_instance(&home, instance_name, args)
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
                &home,
                &json!({"method": crate::api::method::DELETE, "params": {"name": name}}),
            );
            if let Err(e) = crate::fleet::remove_instance_from_yaml(&home, name) {
                tracing::warn!(error = %e, "failed to remove from fleet.yaml");
            }
            // Delete the Telegram topic if one exists
            if let Some(tid) = topic_id {
                telegram::delete_topic(&home, tid);
            }
            // Clean up working directory
            if let Some(ref wd) = working_dir {
                cleanup_working_dir(&home, name, wd);
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
                    let cmd_args = resolved.args.join(" ");
                    match crate::api::call(
                        &home,
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
        "describe_instance" => {
            let name = args["name"].as_str().unwrap_or("");
            if let Err(e) = crate::agent::validate_name(name) {
                return json!({"error": e});
            }
            match crate::api::call(&home, &json!({"method": crate::api::method::LIST})) {
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
            let handover = crate::api::call(&home, &json!({"method": crate::api::method::LIST})).ok()
                .and_then(|resp| resp["result"]["agents"].as_array()?.iter()
                    .find(|a| a["name"].as_str() == Some(name))
                    .map(|a| format!("Previous instance state: {}, health: {}. Replaced due to: {reason}",
                        a["agent_state"].as_str().unwrap_or("unknown"), a["health_state"].as_str().unwrap_or("unknown"))))
                .unwrap_or_else(|| format!("Replaced due to: {reason}"));

            let _ = crate::api::call(
                &home,
                &json!({"method": crate::api::method::KILL, "params": {"name": name}}),
            );
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
            tracing::info!(%name, %reason, "replace_instance");
            json!({"name": name, "reason": reason, "note": "Instance killed. Auto-respawn will create fresh instance with handover context."})
        }
        "set_display_name" => {
            let display_name = args["name"].as_str().unwrap_or("");
            save_metadata(&home, instance_name, "display_name", json!(display_name));
            json!({"display_name": display_name})
        }
        "set_description" => {
            let desc = args["description"].as_str().unwrap_or("");
            save_metadata(&home, instance_name, "description", json!(desc));
            json!({"description": desc})
        }
        "move_pane" => {
            // Route through the API so the running TUI receives a PaneMoved
            // event and relocates the pane. If no daemon is reachable there's
            // no TUI to notify, so returning the API error directly matches
            // behaviour of other TUI-visible tools like `update_team`.
            match crate::api::call(
                &home,
                &json!({"method": crate::api::method::MOVE_PANE, "params": args}),
            ) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => {
                    json!({"ok": true, "agent": args["agent"], "target_tab": args["target_tab"]})
                }
                Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("move_pane failed")}),
                Err(e) => json!({"error": format!("move_pane: {e}")}),
            }
        }

        // --- Decisions ---
        "post_decision" => {
            let result = crate::decisions::post(&home, instance_name, args);
            // Fleet visibility. `post_decision` is anonymous-tolerant
            // (no `err_needs_identity` gate), so we deliberately
            // **skip-emit** when no `Sender` was resolved: an
            // anonymous decision has no identifiable author and
            // landing a blank "[⬛ solo] DECISION ..." row would
            // erode the "who decided" signal that fleet_binding
            // readers rely on. This is by design — decisions'
            // anonymous contract is preserved; only fleet mirroring
            // requires identity. See `docs/DESIGN-stage-b-ux.md` §4.3.
            if let (Some(id), Some(title), Some(sender)) = (
                result.get("id").and_then(|v| v.as_str()),
                args["title"].as_str(),
                sender.as_ref(),
            ) {
                ux_sink_registry().emit(&UxEvent::Fleet(FleetEvent::PostDecision {
                    by: sender.as_str().to_string(),
                    title: title.to_string(),
                    decision_id: id.to_string(),
                }));
            }
            result
        }
        "list_decisions" => crate::decisions::list(&home, args),
        "update_decision" => crate::decisions::update(&home, args),

        // --- Task board ---
        "task" => crate::tasks::handle(&home, instance_name, args),

        // --- Teams ---
        "delete_team" => crate::teams::delete(&home, args),
        "list_teams" => crate::teams::list(&home),
        "update_team" => {
            // Route through the API so the server side can emit a
            // TeamMembersChanged TUI event (migrates panes into / out of the
            // team tab). Falling back to the direct call keeps behavior when
            // the daemon isn't reachable — no TUI running means nothing to
            // migrate anyway.
            match crate::api::call(
                &home,
                &json!({"method": crate::api::method::UPDATE_TEAM, "params": args}),
            ) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => resp["result"].clone(),
                Ok(resp) => {
                    json!({"error": resp["error"].as_str().unwrap_or("update_team failed")})
                }
                Err(_) => crate::teams::update(&home, args),
            }
        }

        // --- Scheduling ---
        "create_schedule" => crate::schedules::create(&home, instance_name, args),
        "list_schedules" => crate::schedules::list(&home, args),
        "update_schedule" => crate::schedules::update(&home, args),
        "delete_schedule" => crate::schedules::delete(&home, args),

        // --- Deployments ---
        "deploy_template" => crate::deployments::deploy(&home, instance_name, args),
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
                    .map(|rest| format!("{}/{rest}", crate::user_home_dir().display()))
                    .unwrap_or_else(|| source.to_string())
            } else {
                crate::api::call(&home, &json!({"method": crate::api::method::LIST}))
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

/// Spawn a single instance (the non-team path of create_instance).
fn spawn_single_instance(home: &std::path::Path, instance_name: &str, args: &Value) -> Value {
    let raw_name = match args["name"].as_str() {
        Some(n) => n,
        None => return json!({"error": "missing 'name'"}),
    };
    if let Err(e) = crate::agent::validate_name(raw_name) {
        return json!({"error": e});
    }
    let name_owned = {
        use std::sync::atomic::{AtomicU16, Ordering};
        static DEDUP_SEQ: AtomicU16 = AtomicU16::new(0);

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
    let layout = args.get("layout").and_then(|v| v.as_str()).unwrap_or("tab");
    let target_pane = args
        .get("target_pane")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty());

    match crate::api::call(
        home,
        &json!({"method": crate::api::method::SPAWN, "params": {
            "name": name, "backend": command, "args": &cmd_args,
            "working_directory": work_dir,
            "layout": layout, "spawner": instance_name,
            "target_pane": target_pane
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
            let topic_id = crate::channel::telegram::create_topic_for_instance(home, name);
            if let Some(task_text) = task {
                let h = home.to_path_buf();
                let n = name.to_string();
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

// Helpers moved to `src/agent_ops.rs` in Task #9 Option C (Commit 2).
// Imports at the top of this file bring them back into scope.
//
// Incidentally fixes a pre-existing drift: the stale 14-entry copy of
// `cleanup_working_dir` that lived here (introduced by 99e8590 on
// 2026-04-14) is now replaced by the canonical 19-entry version in
// agent_ops.rs, so MCP-path cleanup correctly removes the 5 Kiro paths
// (.kiro/agents/{agend.json,agend-prompt.md,default.json},
// .kiro/prompts/agend.md, .kiro/settings.json) that were previously left
// on disk. See commit message for details.

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

    // `validate_branch` tests live in `src/agent_ops.rs` — migrated there
    // as part of Task #9 Option C.

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

    // --- cleanup_working_dir ---

    #[test]
    fn cleanup_agend_workspace_removes_entire_dir() {
        let home = tmp_home("cleanup_ws");
        let ws = home.join("workspace").join("test-agent");
        std::fs::create_dir_all(&ws).ok();
        std::fs::write(ws.join("somefile.txt"), "data").ok();
        std::fs::write(ws.join("opencode.json"), "{}").ok();

        cleanup_working_dir(&home, "test-agent", &ws);
        assert!(!ws.exists(), "workspace dir should be fully removed");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cleanup_user_dir_only_removes_agend_files() {
        let home = tmp_home("cleanup_user");
        let user_dir = tmp_home("cleanup_user_proj");

        // Create user file + agend-generated files
        std::fs::write(user_dir.join("main.rs"), "fn main() {}").ok();
        std::fs::write(user_dir.join("opencode.json"), "{}").ok();
        std::fs::write(user_dir.join("mcp-config.json"), "{}").ok();
        std::fs::create_dir_all(user_dir.join(".claude")).ok();
        std::fs::write(user_dir.join(".claude/settings.local.json"), "{}").ok();

        cleanup_working_dir(&home, "agent1", &user_dir);

        // User file preserved
        assert!(user_dir.join("main.rs").exists(), "user file must survive");
        // Agend files removed
        assert!(!user_dir.join("opencode.json").exists());
        assert!(!user_dir.join("mcp-config.json").exists());
        assert!(!user_dir.join(".claude/settings.local.json").exists());

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&user_dir).ok();
    }

    #[test]
    fn cleanup_removes_metadata() {
        let home = tmp_home("cleanup_meta");
        let ws = home.join("workspace").join("agent1");
        std::fs::create_dir_all(&ws).ok();

        std::fs::create_dir_all(home.join("metadata")).ok();
        std::fs::write(home.join("metadata/agent1.json"), "{}").ok();

        cleanup_working_dir(&home, "agent1", &ws);

        assert!(!home.join("metadata/agent1.json").exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cleanup_nonexistent_dir_no_panic() {
        let home = tmp_home("cleanup_nodir");
        let fake = std::path::PathBuf::from("/tmp/nonexistent-agend-test-dir");
        // Should not panic
        cleanup_working_dir(&home, "agent1", &fake);
        std::fs::remove_dir_all(&home).ok();
    }

    // ---------------------------------------------------------------------
    // FleetEvent emission tests (Stage B-UX PR-A, design §2)
    //
    // These tests share two pieces of global state: (1) `AGEND_HOME` env
    // var, read by `crate::home_dir()` inside `handle_tool`; (2) the
    // crate-wide `ux_sink_registry()` singleton. Both are process-scoped,
    // so the tests serialize through `fleet_test_guard()` and swap in a
    // `RecordingSink` per case via `clear_for_test`.
    //
    // Each positive test carries at least one pin (Reviewer Contract v0.1
    // §4) on the _source_ of the captured field — e.g. `task_id` must come
    // from the handler's `correlation_id` arg, `decision_id` must come
    // from `decisions::post`'s return, `recipients` must come from the
    // filtered `sent` vec — not from the caller's raw args.
    // ---------------------------------------------------------------------

    use crate::channel::sink_registry::registry as ux_sink_registry;
    use crate::channel::ux_event::{FleetEvent, UxEvent, UxEventSink};
    use crate::sync::lock_poisoned;
    use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

    fn fleet_test_guard() -> MutexGuard<'static, ()> {
        static GUARD: StdMutex<()> = StdMutex::new(());
        lock_poisoned(&GUARD, "fleet_test_guard")
    }

    struct Recorder {
        events: StdMutex<Vec<UxEvent>>,
    }

    impl Recorder {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                events: StdMutex::new(Vec::new()),
            })
        }

        fn snapshot(&self) -> Vec<UxEvent> {
            lock_poisoned(&self.events, "recorder").clone()
        }
    }

    impl UxEventSink for Recorder {
        fn emit(&self, event: &UxEvent) {
            lock_poisoned(&self.events, "recorder").push(event.clone());
        }
    }

    /// Set `AGEND_HOME` to a fresh temp dir with a minimal fleet.yaml
    /// (so `get_submit_key` fallbacks resolve), wipe the global sink
    /// registry, and register a fresh `Recorder`. Returns the recorder
    /// and the temp-home path so callers can clean up.
    fn setup_recorder(tag: &str) -> (Arc<Recorder>, std::path::PathBuf) {
        let home = tmp_home(tag);
        std::env::set_var("AGEND_HOME", &home);
        // Minimal fleet so send_to's `get_submit_key` lookup resolves
        // (fallback path on unreachable daemon still needs submit_key).
        let yaml = "defaults:\n  backend: claude\ninstances:\n  target:\n    role: Test\n  sender:\n    role: Test\n";
        std::fs::write(home.join("fleet.yaml"), yaml).ok();
        let rec = Recorder::new();
        ux_sink_registry().clear_for_test();
        ux_sink_registry().register(rec.clone() as Arc<dyn UxEventSink>);
        (rec, home)
    }

    #[test]
    fn delegate_task_emits_fleet_event() {
        let _g = fleet_test_guard();
        let (rec, home) = setup_recorder("fleet_delegate");

        let result = handle_tool(
            "delegate_task",
            &json!({"target_instance": "target", "task": "do the thing"}),
            "sender",
        );
        // Must have succeeded (API-path or fallback); both populate "target".
        assert!(
            is_ok_result(&result),
            "delegate_task should succeed: {result}"
        );

        let events = rec.snapshot();
        assert_eq!(events.len(), 1, "expected one FleetEvent: {events:?}");
        match &events[0] {
            UxEvent::Fleet(FleetEvent::DelegateTask {
                from,
                to,
                summary,
                task_id,
            }) => {
                assert_eq!(from, "sender");
                assert_eq!(to, "target");
                assert_eq!(summary, "do the thing");
                // Pin: `delegate_task` has no id slot; correlation surfaces
                // later via `report_result.correlation_id`. Must be None.
                assert!(task_id.is_none(), "task_id must be None for delegate");
            }
            other => panic!("expected DelegateTask, got {other:?}"),
        }

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn report_result_emits_with_correlation_id() {
        let _g = fleet_test_guard();
        let (rec, home) = setup_recorder("fleet_report");

        // Pin: task_id must source from the `correlation_id` arg, not
        // any other string field. Use a distinctive value so a stray
        // field aliasing bug would fail the assert below.
        let result = handle_tool(
            "report_result",
            &json!({
                "target_instance": "target",
                "summary": "done",
                "correlation_id": "AGD-42",
            }),
            "sender",
        );
        assert!(
            is_ok_result(&result),
            "report_result should succeed: {result}"
        );

        let events = rec.snapshot();
        assert_eq!(events.len(), 1);
        match &events[0] {
            UxEvent::Fleet(FleetEvent::ReportResult {
                from,
                to,
                summary,
                task_id,
            }) => {
                assert_eq!(from, "sender");
                assert_eq!(to, "target");
                assert_eq!(summary, "done");
                assert_eq!(
                    task_id.as_deref(),
                    Some("AGD-42"),
                    "task_id must come from correlation_id arg"
                );
            }
            other => panic!("expected ReportResult, got {other:?}"),
        }

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn report_result_empty_correlation_id_maps_to_none() {
        let _g = fleet_test_guard();
        let (rec, home) = setup_recorder("fleet_report_empty");

        // Pin: empty `correlation_id` must collapse to None so the
        // renderer omits the id rather than showing "()" — filter-empty
        // is the specified normalization.
        let _ = handle_tool(
            "report_result",
            &json!({
                "target_instance": "target",
                "summary": "done",
                "correlation_id": "",
            }),
            "sender",
        );

        let events = rec.snapshot();
        assert_eq!(events.len(), 1);
        match &events[0] {
            UxEvent::Fleet(FleetEvent::ReportResult { task_id, .. }) => {
                assert!(
                    task_id.is_none(),
                    "empty correlation_id must normalize to None, got {task_id:?}"
                );
            }
            other => panic!("expected ReportResult, got {other:?}"),
        }

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn post_decision_with_sender_emits_fleet_event() {
        let _g = fleet_test_guard();
        let (rec, home) = setup_recorder("fleet_decision");

        let result = handle_tool(
            "post_decision",
            &json!({"title": "use X over Y", "content": "because Z"}),
            "sender",
        );
        let posted_id = result["id"]
            .as_str()
            .unwrap_or_else(|| panic!("post_decision must return id: {result}"))
            .to_string();

        let events = rec.snapshot();
        assert_eq!(events.len(), 1);
        match &events[0] {
            UxEvent::Fleet(FleetEvent::PostDecision {
                by,
                title,
                decision_id,
            }) => {
                assert_eq!(by, "sender");
                assert_eq!(title, "use X over Y");
                // Pin: decision_id must come from `decisions::post`'s
                // returned id (authoritative, nanosecond-stamped), NOT
                // from any args the caller passed. Args have no id.
                assert_eq!(
                    decision_id, &posted_id,
                    "decision_id must source from decisions::post result"
                );
            }
            other => panic!("expected PostDecision, got {other:?}"),
        }

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn post_decision_anonymous_does_not_emit_fleet_event() {
        let _g = fleet_test_guard();
        let (rec, home) = setup_recorder("fleet_decision_anon");

        // Ensure no AGEND_INSTANCE_NAME fallback kicks in for the handler.
        std::env::remove_var("AGEND_INSTANCE_NAME");

        // Anonymous call — `instance_name` empty, no env fallback.
        // Decisions module itself must still succeed (anonymous contract),
        // only fleet mirroring is suppressed. Pin: absence of emission.
        let result = handle_tool(
            "post_decision",
            &json!({"title": "anon call", "content": "no author"}),
            "",
        );
        assert!(
            result["id"].as_str().is_some(),
            "post_decision still succeeds anonymously: {result}"
        );

        let events = rec.snapshot();
        assert!(
            events.is_empty(),
            "anonymous post_decision must NOT emit: {events:?}"
        );

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn broadcast_emits_with_resolved_recipients() {
        let _g = fleet_test_guard();
        let (rec, home) = setup_recorder("fleet_broadcast");

        // `targets: ["target", "sender"]` — handler must self-filter
        // `sender` out before computing the recipient set. Pin: the
        // emitted `recipients` field comes from the filtered `sent`
        // vec, NOT from the raw `args["targets"]`.
        let result = handle_tool(
            "broadcast",
            &json!({
                "message": "heads up",
                "targets": ["target", "sender"],
            }),
            "sender",
        );
        // broadcast always returns {"sent_to": [...], "count": ...}
        assert_eq!(result["count"].as_u64(), Some(1));

        let events = rec.snapshot();
        assert_eq!(events.len(), 1);
        match &events[0] {
            UxEvent::Fleet(FleetEvent::Broadcast {
                from,
                recipients,
                summary,
            }) => {
                assert_eq!(from, "sender");
                assert_eq!(summary, "heads up");
                assert_eq!(
                    recipients,
                    &vec!["target".to_string()],
                    "recipients must be the self-filtered `sent` vec"
                );
            }
            other => panic!("expected Broadcast, got {other:?}"),
        }

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn broadcast_empty_targets_does_not_emit() {
        let _g = fleet_test_guard();
        let (rec, home) = setup_recorder("fleet_broadcast_empty");

        // `targets: ["sender"]` — all recipients are the sender itself,
        // so the self-filter leaves `sent` empty. Pin: skip-emit on
        // empty fan-out so fleet_binding isn't spammed with "a → *0".
        let result = handle_tool(
            "broadcast",
            &json!({
                "message": "alone",
                "targets": ["sender"],
            }),
            "sender",
        );
        assert_eq!(result["count"].as_u64(), Some(0));

        let events = rec.snapshot();
        assert!(
            events.is_empty(),
            "empty-recipient broadcast must NOT emit: {events:?}"
        );

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn send_to_instance_does_not_emit_fleet_event() {
        // Negative pin (design §8 exclusion): routine `send_to_instance`
        // is a point-to-point DM and intentionally NOT mirrored into
        // fleet_binding. If a future refactor accidentally routes it
        // through `FleetEvent`, this test fails.
        let _g = fleet_test_guard();
        let (rec, home) = setup_recorder("fleet_send_to_excluded");

        // send_to_instance takes `instance_name` (or `target`), not
        // `target_instance` — use the real arg shape so the negative
        // pin actually exercises the success path.
        let result = handle_tool(
            "send_to_instance",
            &json!({"instance_name": "target", "message": "hi"}),
            "sender",
        );
        assert!(
            is_ok_result(&result),
            "send_to_instance should succeed: {result}"
        );

        let events = rec.snapshot();
        assert!(
            events.is_empty(),
            "send_to_instance must NOT emit FleetEvent (design §8 exclusion): {events:?}"
        );

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn request_information_does_not_emit_fleet_event() {
        // Negative pin (design §8 exclusion): `request_information` is
        // a point-to-point query, not a fleet-visible coordination
        // action. Guards against over-eager emission by future edits.
        let _g = fleet_test_guard();
        let (rec, home) = setup_recorder("fleet_request_info_excluded");

        let result = handle_tool(
            "request_information",
            &json!({"target_instance": "target", "question": "what is X?"}),
            "sender",
        );
        assert!(
            is_ok_result(&result),
            "request_information should succeed: {result}"
        );

        let events = rec.snapshot();
        assert!(
            events.is_empty(),
            "request_information must NOT emit FleetEvent (design §8 exclusion): {events:?}"
        );

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }
}
