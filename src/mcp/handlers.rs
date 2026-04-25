//! MCP tool dispatch — handle_tool() routes tool calls to implementations.

use crate::agent_ops::{
    cleanup_working_dir, list_agents, merge_metadata, save_metadata, save_metadata_batch, send_to,
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

    // Implicit heartbeat: any MCP tool call = agent is alive.
    if !instance_name.is_empty() {
        save_metadata(
            &home,
            instance_name,
            "last_heartbeat",
            json!(chrono::Utc::now().to_rfc3339()),
        );
    }

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
            match telegram::try_telegram_react(&home, instance_name, emoji, message_id) {
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
            match telegram::try_telegram_edit(&home, instance_name, message_id, text) {
                Ok(()) => json!({"message_id": message_id}),
                Err(e) => json!({"error": format!("{e}")}),
            }
        }
        "download_attachment" => {
            let file_id = match args["file_id"].as_str() {
                Some(f) => f,
                None => return json!({"error": "missing 'file_id'"}),
            };
            match telegram::try_download_attachment(&home, instance_name, file_id) {
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
            let thread_id = args["thread_id"].as_str();
            let parent_id = args["parent_id"].as_str();

            let result = match crate::api::call(
                &home,
                &json!({
                    "method": crate::api::method::SEND,
                    "params": { "from": sender.as_str(), "target": target, "text": text, "kind": kind, "thread_id": thread_id, "parent_id": parent_id, "correlation_id": args["correlation_id"].as_str() }
                }),
            ) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => {
                    let dm = resp["delivery_mode"].as_str().unwrap_or("pty");
                    json!({"target": target, "delivery_mode": dm})
                }
                Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("send failed")}),
                Err(e) => {
                    // Validate target exists in fleet.yaml before fallback delivery.
                    let in_fleet = crate::fleet::FleetConfig::load(&home.join("fleet.yaml"))
                        .ok()
                        .map(|c| c.instances.contains_key(target))
                        .unwrap_or(false);
                    if !in_fleet {
                        return json!({"error": format!("target instance '{target}' not found (API unavailable: {e})")});
                    }
                    // Resolve thread inheritance for direct delivery
                    let mut resolved_thread = thread_id.map(String::from);
                    let resolved_parent = parent_id.map(String::from);
                    if resolved_thread.is_none() {
                        if let Some(ref pid) = resolved_parent {
                            if let Some(parent_msg) = crate::inbox::find_message(&home, pid) {
                                resolved_thread =
                                    parent_msg.thread_id.or_else(|| parent_msg.id.clone());
                            }
                        }
                    }
                    let msg = crate::inbox::InboxMessage {
                        schema_version: 0,
                        id: None,
                        read_at: None,
                        thread_id: resolved_thread,
                        parent_id: resolved_parent,
                        task_id: None,
                        force_meta: None,
                        correlation_id: args["correlation_id"].as_str().map(String::from),
                        reviewed_head: None,
                        from: format!("from:{}", sender.as_str()),
                        text: text.to_string(),
                        kind: None,
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        delivery_mode: Some("inbox_fallback".to_string()),
                    };
                    let _ = crate::inbox::enqueue(&home, target, msg);
                    crate::inbox::notify_agent(
                        &home,
                        target,
                        &crate::inbox::NotifySource::Agent(sender.as_str()),
                        text,
                    );
                    json!({"target": target, "delivery_mode": "inbox_fallback", "note": format!("API unavailable, sent direct: {e}")})
                }
            };
            // Warn if kind=report without parent_id
            let mut result = result;
            if kind == Some("report") && parent_id.is_none() {
                if let Some(obj) = result.as_object_mut() {
                    obj.insert("warning".to_string(), json!("parent_id recommended for report kind; will be required in future version"));
                }
            }
            result
        }
        "delegate_task" => {
            let Some(sender) = sender.as_ref() else {
                return err_needs_identity(tool);
            };
            let raw_target = match args["target_instance"].as_str() {
                Some(t) => t,
                None => return json!({"error": "missing 'target_instance'"}),
            };
            if let Err(e) = crate::agent::validate_name(raw_target) {
                return json!({"error": e});
            }
            // Resolve team name → orchestrator (align with task create behaviour).
            let resolved_target = match crate::teams::resolve_team_orchestrator(&home, raw_target) {
                Ok(Some(orch)) => orch,
                Ok(None) => raw_target.to_string(), // not a team, use as-is
                Err(e) => return json!({"error": e}),
            };
            let target = resolved_target.as_str();
            let task = match args["task"].as_str() {
                Some(t) => t,
                None => return json!({"error": "missing 'task'"}),
            };

            // Busy gate: check if target has claimed tasks on the task board.
            // New names: force/force_reason. Old names: interrupt/reason (backwards-compat).
            let force = args
                .get("force")
                .and_then(|v| v.as_bool())
                .or_else(|| args.get("interrupt").and_then(|v| v.as_bool()))
                .unwrap_or(false);
            let force_reason = args
                .get("force_reason")
                .and_then(|v| v.as_str())
                .or_else(|| args.get("reason").and_then(|v| v.as_str()));
            let used_deprecated = args.get("interrupt").is_some() || args.get("reason").is_some();
            let claimed_tasks: Vec<_> = crate::tasks::list_all(&home)
                .into_iter()
                .filter(|t| t.assignee.as_deref() == Some(target) && t.status == "claimed")
                .collect();
            if !claimed_tasks.is_empty() {
                if force {
                    if force_reason.is_none() || force_reason == Some("") {
                        return json!({"error": "force=true requires a non-empty 'force_reason' (or deprecated 'reason')"});
                    }
                } else {
                    let current = &claimed_tasks[0];
                    let age_secs = chrono::DateTime::parse_from_rfc3339(&current.updated_at)
                        .ok()
                        .map(|dt| {
                            chrono::Utc::now()
                                .signed_duration_since(dt.with_timezone(&chrono::Utc))
                                .num_seconds()
                        })
                        .unwrap_or(0);
                    return json!({
                        "busy": true,
                        "current_task": {"id": current.id, "title": current.title, "age_seconds": age_secs},
                        "options": ["force=true (with force_reason)", "queue=true"],
                        "suggestion": format!("target busy on task {} ({}s old). Use force=true with force_reason to override.", current.id, age_secs)
                    });
                }
            }

            // Second reviewer flag validation (§3.5 dual review)
            let second_reviewer = args["second_reviewer"].as_bool().unwrap_or(false);
            if second_reviewer {
                let sr_reason = args["second_reviewer_reason"].as_str().unwrap_or("");
                if sr_reason.is_empty() {
                    return json!({"error": "second_reviewer=true requires non-empty second_reviewer_reason"});
                }
            }

            let mut msg = format!("[delegate_task] {task}");
            if force {
                if let Some(r) = force_reason {
                    msg.push_str(&format!("\n\n⚠️ FORCED (reason: {r})"));
                }
            }
            if let Some(tid) = args["task_id"].as_str() {
                msg.push_str(&format!(" (task id: {tid})"));
            }
            if let Some(criteria) = args["success_criteria"].as_str() {
                msg.push_str(&format!("\n\nSuccess criteria: {criteria}"));
            }
            if let Some(ctx) = args["context"].as_str() {
                msg.push_str(&format!("\n\nContext: {ctx}"));
            }
            let force_meta_json = if force {
                Some(json!({
                    "forced": true,
                    "reason": force_reason.unwrap_or(""),
                    "forced_at": chrono::Utc::now().to_rfc3339()
                }))
            } else {
                None
            };
            let task_id_str = args["task_id"].as_str();
            let result = match crate::api::call(
                &home,
                &json!({
                    "method": crate::api::method::SEND,
                    "params": {
                        "from": sender.as_str(),
                        "target": target,
                        "text": msg,
                        "kind": "task",
                        "task_id": task_id_str,
                        "force_meta": force_meta_json,
                    }
                }),
            ) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => json!({"target": target}),
                Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("send failed")}),
                Err(e) => {
                    // Fallback: direct delivery with task_id
                    let in_fleet = crate::fleet::FleetConfig::load(&home.join("fleet.yaml"))
                        .ok()
                        .map(|c| c.instances.contains_key(target))
                        .unwrap_or(false);
                    if !in_fleet {
                        return json!({"error": format!("target instance '{target}' not found in fleet.yaml (API unavailable: {e})")});
                    }
                    let inbox_msg = crate::inbox::InboxMessage {
                        schema_version: 0,
                        id: None,
                        read_at: None,
                        thread_id: None,
                        parent_id: None,
                        task_id: task_id_str.map(String::from),
                        force_meta: force_meta_json.as_ref().and_then(|v| {
                            serde_json::from_value::<crate::inbox::ForceMeta>(v.clone()).ok()
                        }),
                        correlation_id: None,
                        reviewed_head: None,
                        delivery_mode: Some("inbox_fallback".to_string()),
                        from: format!("from:{}", sender.as_str()),
                        text: msg.clone(),
                        kind: Some("task".to_string()),
                        timestamp: chrono::Utc::now().to_rfc3339(),
                    };
                    let _ = crate::inbox::enqueue(&home, target, inbox_msg);
                    crate::inbox::notify_agent(
                        &home,
                        target,
                        &crate::inbox::NotifySource::Agent(sender.as_str()),
                        &msg,
                    );
                    json!({"target": target, "note": format!("API unavailable: {e}")})
                }
            };
            if is_ok_result(&result) {
                let task_id = task_id_str.map(str::to_string);
                // Track dispatch for timeout detection
                crate::dispatch_tracking::track_dispatch(
                    &home,
                    crate::dispatch_tracking::DispatchEntry {
                        task_id: task_id.clone(),
                        from: sender.as_str().to_string(),
                        to: target.to_string(),
                        delegated_at: chrono::Utc::now().to_rfc3339(),
                        status: "pending".to_string(),
                    },
                );
                ux_sink_registry().emit(&UxEvent::Fleet(FleetEvent::DelegateTask {
                    from: sender.as_str().to_string(),
                    to: target.to_string(),
                    summary: task.to_string(),
                    task_id,
                }));
                // S2d provenance injection (DESIGN §6). Only DELEGATE
                // triggers this: the receiving agent's topic gets a
                // "who sent this to you" tag alongside the task body.
                // Non-propagating: `send_to` already succeeded, so even
                // if the provenance side-channel fails we keep the
                // main result untouched. `warn!` (not silent debug) per
                // DESIGN §4 Q4 — provenance failure may signal a real
                // routing bug worth an operator's attention.
                if let Err(e) =
                    crate::channel::telegram::inject_provenance(target, sender.as_str(), task)
                {
                    tracing::warn!(
                        %e,
                        target = %target,
                        from = %sender.as_str(),
                        "S2d provenance injection failed — routing may be broken"
                    );
                }
            }
            // Add deprecation warning if caller used old field names
            let mut result = result;
            if used_deprecated {
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "warning".into(),
                        json!("interrupt/reason fields deprecated, use force/force_reason; will be removed Sprint 11"),
                    );
                }
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
            let result = {
                let correlation_id = args["correlation_id"].as_str();
                let reviewed_head = args["reviewed_head"].as_str();
                match crate::api::call(
                    &home,
                    &json!({
                        "method": crate::api::method::SEND,
                        "params": {
                            "from": sender.as_str(),
                            "target": target,
                            "text": msg,
                            "kind": "report",
                            "correlation_id": correlation_id,
                            "reviewed_head": reviewed_head,
                            "thread_id": args["thread_id"].as_str(),
                            "parent_id": args["parent_id"].as_str(),
                        }
                    }),
                ) {
                    Ok(resp) if resp["ok"].as_bool() == Some(true) => json!({"target": target}),
                    Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("send failed")}),
                    Err(e) => {
                        let inbox_msg = crate::inbox::InboxMessage {
                            schema_version: 0,
                            id: None,
                            read_at: None,
                            thread_id: args["thread_id"].as_str().map(String::from),
                            parent_id: args["parent_id"].as_str().map(String::from),
                            task_id: None,
                            force_meta: None,
                            correlation_id: correlation_id.map(String::from),
                            reviewed_head: reviewed_head.map(String::from),
                            delivery_mode: Some("inbox_fallback".to_string()),
                            from: format!("from:{}", sender.as_str()),
                            text: msg.clone(),
                            kind: Some("report".to_string()),
                            timestamp: chrono::Utc::now().to_rfc3339(),
                        };
                        let _ = crate::inbox::enqueue(&home, target, inbox_msg);
                        crate::inbox::notify_agent(
                            &home,
                            target,
                            &crate::inbox::NotifySource::Agent(sender.as_str()),
                            &msg,
                        );
                        json!({"target": target, "note": format!("API unavailable: {e}")})
                    }
                }
            };
            // Add warning for report kind without parent_id
            let mut result = result;
            if args["parent_id"].as_str().is_none() {
                if let Some(obj) = result.as_object_mut() {
                    obj.insert("warning".to_string(), json!("parent_id recommended for report kind; will be required in future version"));
                }
            }
            if is_ok_result(&result) {
                // Mark dispatch as completed so timeout sweep doesn't false-warn.
                let cid = args["correlation_id"].as_str();
                crate::dispatch_tracking::mark_completed(&home, cid, sender.as_str());
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
            // Emit AgentPickedUp for each pending inbound message so channel
            // adapters can confirm pickup (e.g. Telegram ✅ reaction on each).
            // F2 fix: iterate pending_pickup_ids array, not a single scalar.
            if !messages.is_empty() {
                let meta_path = home.join("metadata").join(format!("{instance_name}.json"));
                if let Some(meta) = std::fs::read_to_string(&meta_path)
                    .ok()
                    .and_then(|c| serde_json::from_str::<Value>(&c).ok())
                {
                    if let Some(arr) = meta["pending_pickup_ids"].as_array() {
                        use crate::channel::binding::BindingRef;
                        use crate::channel::event::MsgRef;
                        use crate::channel::ux_event::UxEvent;
                        for entry in arr {
                            let kind_str = entry["kind"].as_str().unwrap_or("");
                            let msg_id = entry["msg_id"].as_str().unwrap_or("");
                            let kind: &'static str = match kind_str {
                                "telegram" => "telegram",
                                "discord" => "discord",
                                "slack" => "slack",
                                _ => continue,
                            };
                            if msg_id.is_empty() {
                                continue;
                            }
                            let origin_msg = MsgRef {
                                binding: BindingRef::new(kind, Some(instance_name.to_string()), ()),
                                id: msg_id.to_string(),
                            };
                            crate::channel::sink_registry::registry().emit(
                                &UxEvent::AgentPickedUp {
                                    origin_msg,
                                    agent: instance_name.to_string(),
                                },
                            );
                        }
                    }
                }
                // Clear pending_pickup_ids after emitting
                crate::agent_ops::save_metadata(
                    &home,
                    instance_name,
                    "pending_pickup_ids",
                    json!(null),
                );
            }
            json!({"messages": messages})
        }

        "describe_message" => {
            let msg_id = match args["message_id"].as_str() {
                Some(id) => id,
                None => return json!({"error": "missing 'message_id'"}),
            };
            let target = args["instance"].as_str().unwrap_or(instance_name);
            let status = crate::inbox::describe_message(&home, msg_id, target);
            match status {
                crate::inbox::MessageStatus::ReadAt(t, dm) => {
                    let mut resp = json!({"status": "read", "read_at": t});
                    if let Some(mode) = dm {
                        resp["delivery_mode"] = json!(mode);
                    }
                    // Expose typed fields from the message
                    if let Some(msg) = crate::inbox::find_message(&home, msg_id) {
                        if let Some(ref cid) = msg.correlation_id {
                            resp["correlation_id"] = json!(cid);
                        }
                        if let Some(ref rh) = msg.reviewed_head {
                            resp["reviewed_head"] = json!(rh);
                            resp["stale_possible"] = json!(true);
                        }
                    }
                    resp
                }
                crate::inbox::MessageStatus::UnreadExpired => {
                    json!({"status": "unread_expired"})
                }
                crate::inbox::MessageStatus::NotFound => {
                    json!({"status": "not_found"})
                }
            }
        }

        "describe_thread" => {
            let thread_id = match args["thread_id"].as_str() {
                Some(id) => id,
                None => return json!({"error": "missing 'thread_id'"}),
            };
            let instance = args["instance"].as_str();
            let msgs = crate::inbox::get_thread(&home, thread_id, instance);
            json!({"thread_id": thread_id, "messages": msgs, "count": msgs.len()})
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
                // handle_create_team now writes fleet.yaml up front and
                // calls prepare_instructions for each member, so no
                // pre-generation loop is needed here.
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

                        // Topic creation moved into handle_create_team so every
                        // API-level team spawn covers it — no redundant call here.

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
            // Prevent deleting the last fleet.yaml-tracked instance when a
            // channel is configured — a channel needs at least one instance
            // to receive messages. Only applies when the target itself is in
            // fleet.yaml; runtime-only zombies (torn-down deployment members
            // that linger in the registry with no fleet entry) are not
            // protected, otherwise they become un-deletable as soon as
            // fleet.yaml is down to a single real instance.
            let fleet = crate::fleet::FleetConfig::load(&home.join("fleet.yaml")).ok();
            if let Some(ref config) = fleet {
                if config.channel.is_some()
                    && config.instances.contains_key(name)
                    && config.instances.len() <= 1
                {
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
            // Remove from all teams; auto-delete empty teams
            crate::teams::remove_member_from_all(&home, name);
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
                    delivery_mode: None,
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
        "interrupt" => {
            let target = match args["target"].as_str() {
                Some(t) => t,
                None => return json!({"error": "missing 'target'"}),
            };
            if let Err(e) = crate::agent::validate_name(target) {
                return json!({"error": e});
            }
            // Inject ESC byte (0x1b) to interrupt current LLM generation turn
            match crate::api::call(&home, &interrupt_esc_params(target)) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => {
                    // Optionally inject follow-up reason message
                    if let Some(reason) = args["reason"].as_str() {
                        let header =
                            crate::inbox::format_event_header("interrupt", &[("reason", reason)]);
                        crate::inbox::compose_aware_inject(&home, target, &header);
                    }
                    json!({"ok": true, "target": target})
                }
                Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("inject failed")}),
                Err(e) => {
                    json!({"error": format!("interrupt failed — agent '{target}' not reachable (API unavailable: {e})")})
                }
            }
        }
        "set_waiting_on" => {
            let Some(_) = sender.as_ref() else {
                return err_needs_identity(tool);
            };
            let condition = args["condition"].as_str().unwrap_or("");
            if condition.is_empty() {
                save_metadata_batch(
                    &home,
                    instance_name,
                    &[
                        ("waiting_on", json!(null)),
                        ("waiting_on_since", json!(null)),
                    ],
                );
                json!({"cleared": true})
            } else {
                let now = chrono::Utc::now().to_rfc3339();
                save_metadata_batch(
                    &home,
                    instance_name,
                    &[
                        ("waiting_on", json!(condition)),
                        ("waiting_on_since", json!(&now)),
                    ],
                );
                json!({"waiting_on": condition, "since": now})
            }
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
        "create_team" => {
            // Route through the API so the handler emits a TeamCreated TUI
            // event and panes for the listed members get moved into a single
            // team tab. Mirrors the update_team handling below. Fall back to
            // the direct call when the daemon is unreachable — no TUI means
            // there's nothing to migrate anyway.
            match crate::api::call(
                &home,
                &json!({"method": crate::api::method::CREATE_TEAM, "params": args}),
            ) {
                Ok(resp) if resp["ok"].as_bool() == Some(true) => {
                    resp.get("result").cloned().unwrap_or_default()
                }
                Ok(resp) => {
                    json!({"error": resp["error"].as_str().unwrap_or("create_team failed")})
                }
                Err(_) => crate::teams::create(&home, args),
            }
        }
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
            // `last_polled_at: null` signals "never polled" to
            // check_ci_watches, which fires on the next daemon tick
            // (≤10 s) instead of waiting `interval_secs`. Throttle state
            // lives in the schema, not on the filesystem — this is the
            // elegant replacement for PR #119's mtime-backdate kludge.
            let watch = json!({
                "repo": repo,
                "branch": branch,
                "interval_secs": interval,
                "instance": instance_name,
                "last_run_id": null,
                "head_sha": null,
                "last_polled_at": null,
                "last_notified_head_sha": null,
                "expires_at": (chrono::Utc::now() + chrono::Duration::hours(crate::daemon::ci_watch::WATCH_TTL_HOURS)).to_rfc3339(),
                "last_terminal_seen_at": null,
            });
            let filename = crate::daemon::ci_watch::watch_filename(repo, branch);
            let watch_path = ci_dir.join(&filename);
            let _ = std::fs::write(
                &watch_path,
                serde_json::to_string_pretty(&watch).unwrap_or_default(),
            );
            // Surface the unauthenticated-polling gotcha in the response
            // itself so the calling agent can relay it to the operator
            // immediately, rather than waiting for a silent rate-limit
            // storm to drop notifications. See
            // `ci_watch::github_token_warning` for the full rationale.
            let mut resp = json!({"repo": repo, "watching": true});
            if let Some(w) = crate::daemon::ci_watch::github_token_warning_from_env() {
                resp["warning"] = json!(w);
            }
            resp
        }
        "unwatch_ci" => {
            let repo = match args["repo"].as_str() {
                Some(r) => r,
                None => return json!({"error": "missing 'repo'"}),
            };
            let branch = args["branch"].as_str().unwrap_or("main");
            let filename = crate::daemon::ci_watch::watch_filename(repo, branch);
            let path = home.join("ci-watches").join(&filename);
            let _ = std::fs::remove_file(&path);
            json!({"repo": repo, "watching": false})
        }

        // --- Health reporting ---
        "report_health" => {
            let Some(_) = sender.as_ref() else {
                return err_needs_identity(tool);
            };
            let reason = match args["reason"].as_str() {
                Some(r) => r,
                None => return json!({"error": "missing 'reason'"}),
            };
            match crate::api::call(
                &home,
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
        "clear_blocked_reason" => {
            let instance = match args["instance"].as_str() {
                Some(n) => n,
                None => return json!({"error": "missing 'instance'"}),
            };
            let mut params = json!({"name": instance});
            if let Some(r) = args["reason"].as_str() {
                params["reason"] = json!(r);
            }
            match crate::api::call(
                &home,
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

        _ => json!({"error": format!("unknown tool: {tool}")}),
    }
}

/// Spawn a single instance (the non-team path of create_instance).
/// Build the INJECT API params for an interrupt ESC byte injection.
/// Extracted for testability — unit tests verify the exact params
/// without needing a running daemon.
pub fn interrupt_esc_params(target: &str) -> Value {
    json!({
        "method": crate::api::method::INJECT,
        "params": {"name": target, "data": "\x1b", "raw": true}
    })
}

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

    // handle_spawn calls prepare_instructions, which handles both
    // `--mcp-config` and agend.md generation. No pre-gen needed here — but
    // the workspace dir must exist before the worktree helper above can
    // decide whether to promote it into a checkout, so we still ensure it.
    std::fs::create_dir_all(&work_dir).ok();

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
            // handle_spawn now creates the topic and surfaces the id in
            // its result; just pass it through. Keeps a single topic per
            // spawn and removes the MCP-layer redundant call.
            let topic_id = resp["result"]["topic_id"].as_i64();
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
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
    use crate::agent_ops::get_submit_key;

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
        // Still set AGEND_HOME for sub-calls inside handle_tool_with_home
        // that read home_dir() (e.g. get_submit_key fallback, inbox ops).
        // Safe: fleet_test_guard serializes these tests, and the cross-module
        // racers (instructions.rs, telegram.rs) no longer set AGEND_HOME.
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

    // ---------------------------------------------------------------------
    // S2d provenance injection tests (Stage B-UX PR-C, design §6 + §4 Q4)
    //
    // In test env, no Telegram bot is configured, so
    // `inject_provenance` hits `resolve_channel()` and bails with
    // `no_channel_configured`. That's exactly the failure mode these
    // pins exercise: we want the handler to stay clean when provenance
    // can't be delivered (main `send_to` result untouched, fleet event
    // still emitted), AND we want the `tracing::warn!` to actually fire
    // so operators have a signal that routing might be broken.
    // ---------------------------------------------------------------------

    /// Negative pin (main-path isolation): when `inject_provenance`
    /// fails, the handler's returned JSON must NOT carry any provenance
    /// text, and the FleetEvent::DelegateTask must STILL emit.
    ///
    /// Why this pin: a naive refactor that threaded `inject_provenance`
    /// into `send_to`'s pipeline (rather than fanning it out as a
    /// side-channel) could pollute the caller's response or suppress
    /// the fleet event on provenance failure — this pin catches both.
    #[test]
    fn delegate_task_main_response_clean_when_provenance_fails() {
        let _g = fleet_test_guard();
        let (rec, home) = setup_recorder("fleet_prov_main_clean");

        let result = handle_tool(
            "delegate_task",
            &json!({"target_instance": "target", "task": "do the thing"}),
            "sender",
        );

        // Main path untouched: the handler still returns an ok result.
        assert!(
            is_ok_result(&result),
            "delegate_task must succeed even when provenance fails: {result}"
        );

        // Main response clean: no provenance-failure bleed into the
        // caller-visible JSON. Check both the rendered text and raw
        // JSON so a future refactor that tucks the error into a nested
        // field still trips the pin.
        let rendered = result.to_string();
        assert!(
            !rendered.to_lowercase().contains("provenance"),
            "main response leaked provenance text: {rendered}"
        );
        assert!(
            !rendered.contains("⬅️"),
            "main response leaked provenance tag glyph: {rendered}"
        );

        // Fleet visibility preserved: DelegateTask still reaches the sink.
        let events = rec.snapshot();
        assert_eq!(
            events.len(),
            1,
            "FleetEvent must still emit when provenance fails: {events:?}"
        );
        assert!(
            matches!(&events[0], UxEvent::Fleet(FleetEvent::DelegateTask { .. })),
            "unexpected event variant: {:?}",
            events[0]
        );

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    /// Failure-visibility pin (DESIGN §4 Q4): `inject_provenance` failure
    /// MUST produce a `tracing::warn!` record, not a silent drop.
    ///
    /// Why this pin: Q4 explicitly overrode the initial silent-log bias
    /// with warn-level because provenance failures can indicate real
    /// routing bugs (wrong topic_id for target) that otherwise decay
    /// silently. A future edit that downgrades to `debug!` or removes
    /// the `tracing::warn!` call entirely would lose that signal; this
    /// pin asserts the warn record is actually emitted.
    ///
    /// `tracing-test`'s `#[traced_test]` attaches a capturing subscriber
    /// for the duration of this test; `logs_contain` scans captured
    /// records for a substring match.
    #[test]
    #[tracing_test::traced_test]
    fn delegate_task_provenance_failure_logs_tracing_warn() {
        let _g = fleet_test_guard();
        let (_rec, home) = setup_recorder("fleet_prov_warn");

        let result = handle_tool(
            "delegate_task",
            &json!({"target_instance": "target", "task": "do the thing"}),
            "sender",
        );
        assert!(is_ok_result(&result), "handler must succeed: {result}");

        // The warn carries the DESIGN §6 signature phrase. Pinning the
        // exact message substring (not just "warn level fired") keeps
        // the pin from passing on an unrelated warn that happened to
        // fire during the handler run.
        assert!(
            logs_contain("S2d provenance injection failed"),
            "DESIGN §4 Q4 warn record was not emitted",
        );

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    // -----------------------------------------------------------------
    // Track 1 PR-1 tests (design §7)
    // -----------------------------------------------------------------

    #[test]
    fn set_waiting_on_persists_and_clears() {
        let _g = fleet_test_guard();
        let (_rec, home) = setup_recorder("waiting_on_set");

        // Set waiting_on
        let result = handle_tool(
            "set_waiting_on",
            &json!({"condition": "review from at-dev-4"}),
            "sender",
        );
        assert!(
            is_ok_result(&result),
            "set_waiting_on should succeed: {result}"
        );
        assert_eq!(result["waiting_on"], "review from at-dev-4");

        // Value-source pin: return value `since` must match metadata file.
        let returned_since = result["since"].as_str().expect("since in return");
        let meta: Value = serde_json::from_str(
            &std::fs::read_to_string(home.join("metadata/sender.json")).expect("read meta"),
        )
        .expect("parse meta");
        assert_eq!(meta["waiting_on"], "review from at-dev-4");
        assert_eq!(
            meta["waiting_on_since"].as_str().expect("since in file"),
            returned_since,
            "return value since must match persisted timestamp (value-source pin)"
        );

        // Clear
        let result = handle_tool("set_waiting_on", &json!({"condition": ""}), "sender");
        assert_eq!(result["cleared"], true);
        let meta: Value = serde_json::from_str(
            &std::fs::read_to_string(home.join("metadata/sender.json")).expect("read meta"),
        )
        .expect("parse meta");
        assert!(
            meta["waiting_on"].is_null(),
            "waiting_on must be null after clear"
        );
        assert!(
            meta["waiting_on_since"].is_null(),
            "waiting_on_since must be null after clear"
        );

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn implicit_heartbeat_recorded_on_tool_call() {
        let _g = fleet_test_guard();
        let (_rec, home) = setup_recorder("heartbeat_rec");

        // Any tool call should record heartbeat
        let _ = handle_tool("inbox", &json!({}), "sender");

        // Resolve meta_path from home_dir() *after* the tool call — on
        // Windows CI, parallel tests can mutate AGEND_HOME between
        // setup_recorder's set_var and handle_tool's home_dir() read.
        // Using home_dir() here matches wherever handle_tool actually wrote.
        let actual_home = crate::home_dir();
        let meta_path = actual_home.join("metadata/sender.json");
        let meta: Value =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).expect("read meta"))
                .expect("parse meta — atomic write must produce valid JSON");
        let hb = meta["last_heartbeat"]
            .as_str()
            .expect("last_heartbeat must be present after tool call");
        // Must be a valid RFC3339 timestamp
        assert!(
            chrono::DateTime::parse_from_rfc3339(hb).is_ok(),
            "last_heartbeat must be valid RFC3339: {hb}"
        );

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
        if actual_home != home {
            std::fs::remove_dir_all(&actual_home).ok();
        }
    }

    #[test]
    fn waiting_on_exposed_via_merge_metadata() {
        let _g = fleet_test_guard();
        let (_rec, home) = setup_recorder("waiting_on_merge");

        // Set waiting_on
        let _ = handle_tool(
            "set_waiting_on",
            &json!({"condition": "delegation result"}),
            "sender",
        );

        // Resolve home after tool call — parallel tests can shift AGEND_HOME
        // on Windows CI (same class as PR #65).
        let actual_home = crate::home_dir();

        // Simulate what list_instances does: merge_metadata into agent info
        let mut info = json!({"name": "sender", "agent_state": "thinking"});
        merge_metadata(&actual_home, "sender", &mut info);
        assert_eq!(
            info["waiting_on"], "delegation result",
            "merge_metadata must surface waiting_on"
        );
        assert!(
            info["waiting_on_since"].as_str().is_some(),
            "merge_metadata must surface waiting_on_since"
        );
        assert!(
            info["last_heartbeat"].as_str().is_some(),
            "merge_metadata must surface last_heartbeat"
        );

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
        if actual_home != home {
            std::fs::remove_dir_all(&actual_home).ok();
        }
    }

    #[test]
    fn atomic_save_metadata_no_tmp_residue() {
        let home = tmp_home("atomic_no_tmp");
        save_metadata(&home, "agent1", "key", json!("value"));
        let meta_dir = home.join("metadata");
        let tmp_files: Vec<_> = std::fs::read_dir(&meta_dir)
            .expect("read metadata dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "tmp"))
            .collect();
        assert!(
            tmp_files.is_empty(),
            "no .tmp residue after atomic write: {tmp_files:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn set_waiting_on_rejects_anonymous_caller() {
        let _g = fleet_test_guard();
        let (_rec, home) = setup_recorder("waiting_on_anon");
        // Ensure no Sender resolves from env either
        std::env::remove_var("AGEND_INSTANCE_NAME");
        let result = handle_tool("set_waiting_on", &json!({"condition": "whatever"}), "");
        assert!(
            result["error"].is_string(),
            "must err on anonymous caller: {result}"
        );
        // Must NOT have created metadata/.json
        assert!(
            !home.join("metadata/.json").exists(),
            "no metadata written for anon caller"
        );
        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    // -----------------------------------------------------------------
    // PR #66 follow-up: F1 tests + F2 multi-message regression pin
    // -----------------------------------------------------------------

    #[test]
    fn metadata_persisted_on_pending_pickup() {
        let _g = fleet_test_guard();
        let (_rec, home) = setup_recorder("meta_pickup");

        // Simulate what handle_message does: write pending_pickup_ids
        save_metadata(
            &home,
            "sender",
            "pending_pickup_ids",
            json!([{"kind": "telegram", "msg_id": "42"}]),
        );

        let meta: Value = serde_json::from_str(
            &std::fs::read_to_string(home.join("metadata/sender.json")).expect("read"),
        )
        .expect("parse");
        let arr = meta["pending_pickup_ids"]
            .as_array()
            .expect("must be array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["kind"], "telegram");
        assert_eq!(arr[0]["msg_id"], "42");

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn agent_picked_up_emitted_on_inbox_drain() {
        let _g = fleet_test_guard();
        let (rec, home) = setup_recorder("pickup_emit");

        // Seed pending_pickup_ids in metadata
        save_metadata(
            &home,
            "sender",
            "pending_pickup_ids",
            json!([{"kind": "telegram", "msg_id": "99"}]),
        );

        // Seed an inbox message so drain is non-empty
        let _ = crate::inbox::enqueue(
            &home,
            "sender",
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
                from: "user:test".to_string(),
                text: "hello".to_string(),
                kind: Some("telegram".to_string()),
                timestamp: chrono::Utc::now().to_rfc3339(),
                delivery_mode: None,
            },
        );

        let _ = handle_tool("inbox", &json!({}), "sender");

        let events = rec.snapshot();
        let pickups: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, UxEvent::AgentPickedUp { .. }))
            .collect();
        assert_eq!(pickups.len(), 1, "expected 1 AgentPickedUp: {events:?}");
        if let UxEvent::AgentPickedUp { origin_msg, agent } = &pickups[0] {
            assert_eq!(origin_msg.id, "99");
            assert_eq!(agent, "sender");
        }

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn agent_picked_up_fires_for_all_pending_messages() {
        let _g = fleet_test_guard();
        let (rec, home) = setup_recorder("pickup_multi");

        // Seed 3 pending pickup IDs (simulating 3 rapid user messages)
        save_metadata(
            &home,
            "sender",
            "pending_pickup_ids",
            json!([
                {"kind": "telegram", "msg_id": "10"},
                {"kind": "telegram", "msg_id": "11"},
                {"kind": "telegram", "msg_id": "12"},
            ]),
        );

        // Seed inbox message so drain is non-empty
        let _ = crate::inbox::enqueue(
            &home,
            "sender",
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
                from: "user:test".to_string(),
                text: "burst".to_string(),
                kind: Some("telegram".to_string()),
                timestamp: chrono::Utc::now().to_rfc3339(),
                delivery_mode: None,
            },
        );

        let _ = handle_tool("inbox", &json!({}), "sender");

        let events = rec.snapshot();
        let pickups: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, UxEvent::AgentPickedUp { .. }))
            .collect();
        assert_eq!(
            pickups.len(),
            3,
            "F2 pin: must emit AgentPickedUp for ALL pending messages, not just last: {events:?}"
        );
        // Verify IDs match
        let ids: Vec<&str> = pickups
            .iter()
            .filter_map(|e| {
                if let UxEvent::AgentPickedUp { origin_msg, .. } = e {
                    Some(origin_msg.id.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(ids, vec!["10", "11", "12"]);

        // Verify pending_pickup_ids cleared after drain
        let meta: Value = serde_json::from_str(
            &std::fs::read_to_string(home.join("metadata/sender.json")).expect("read"),
        )
        .expect("parse");
        assert!(
            meta["pending_pickup_ids"].is_null(),
            "pending_pickup_ids must be cleared after drain"
        );

        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    // --- Sprint 5: target validation + team routing ---

    #[test]
    fn test_send_to_nonexistent_target_returns_error_and_no_inbox() {
        // F1+F2: daemon down + ghost target → error, NO inbox file created.
        let _g = fleet_test_guard();
        let home = tmp_home("send-nonexist");
        std::env::set_var("AGEND_HOME", &home);
        // No fleet.yaml → target doesn't exist anywhere.
        let result = handle_tool(
            "send_to_instance",
            &json!({"instance_name": "ghost-agent", "message": "hello"}),
            "sender",
        );
        assert!(
            result.get("error").is_some(),
            "send to nonexistent target must return error, got: {result}"
        );
        let ghost_inbox = home.join("inbox").join("ghost-agent.jsonl");
        assert!(
            !ghost_inbox.exists(),
            "inbox file must NOT be created for nonexistent target"
        );
        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_delegate_task_resolves_team_to_orchestrator_inbox() {
        // F3: delegate_task to team name → resolved to orchestrator,
        // verify the actual inbox recipient is the orchestrator.
        let _g = fleet_test_guard();
        let home = tmp_home("delegate-team");
        // fleet.yaml for instance validation, teams.json for team resolution
        std::fs::write(
            home.join("fleet.yaml"),
            "instances:\n  dev-lead:\n    backend: claude\n  dev-impl:\n    backend: claude\n",
        )
        .ok();
        // teams.json is the runtime store used by resolve_team_orchestrator
        std::fs::write(
            home.join("teams.json"),
            r#"{"schema_version":1,"teams":[{"name":"dev","members":["dev-lead","dev-impl"],"orchestrator":"dev-lead","description":null,"created_at":"2026-01-01T00:00:00Z"}]}"#,
        )
        .ok();
        std::env::set_var("AGEND_HOME", &home);

        let result = handle_tool(
            "delegate_task",
            &json!({"target_instance": "dev", "task": "test task"}),
            "dev-impl",
        );
        // Should not error — team resolved to dev-lead.
        let err = result["error"].as_str().unwrap_or("");
        assert!(
            !err.contains("not found"),
            "delegate_task to team name should resolve, got error: {err}"
        );
        // Result should target dev-lead (orchestrator), not "dev" (team).
        assert_eq!(
            result["target"].as_str().unwrap_or(""),
            "dev-lead",
            "delegate_task must resolve team to orchestrator in result"
        );
        // No inbox for the team name itself.
        let team_inbox = home.join("inbox").join("dev.jsonl");
        assert!(
            !team_inbox.exists(),
            "inbox must NOT be created for team name 'dev'"
        );
        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    // --- Sprint 6: delivery_mode ---

    #[test]
    fn test_send_to_inbox_fallback_mode() {
        // Daemon down → fallback path → delivery_mode = "inbox_fallback"
        let _g = fleet_test_guard();
        let home = tmp_home("delivery-fallback");
        std::fs::write(
            home.join("fleet.yaml"),
            "instances:\n  receiver:\n    backend: claude\n",
        )
        .ok();
        std::env::set_var("AGEND_HOME", &home);
        let result = handle_tool(
            "send_to_instance",
            &json!({"instance_name": "receiver", "message": "test"}),
            "sender",
        );
        assert_eq!(
            result["delivery_mode"].as_str(),
            Some("inbox_fallback"),
            "daemon-down path must set delivery_mode=inbox_fallback: {result}"
        );
        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_describe_message_shows_delivery_mode() {
        // Verify describe_message returns delivery_mode when stored on the message.
        let _g = fleet_test_guard();
        let home = tmp_home("describe-dm");
        std::env::set_var("AGEND_HOME", &home);
        // Seed an inbox message with delivery_mode
        let msg = crate::inbox::InboxMessage {
            schema_version: 1,
            id: Some("m-dm-test".into()),
            from: "test".into(),
            text: "hello".into(),
            kind: None,
            timestamp: chrono::Utc::now().to_rfc3339(),
            read_at: Some(chrono::Utc::now().to_rfc3339()),
            thread_id: None,
            parent_id: None,
            delivery_mode: Some("inbox_fallback".into()),
            force_meta: None,
            correlation_id: None,
            reviewed_head: None,
            task_id: None,
        };
        let inbox_dir = home.join("inbox");
        std::fs::create_dir_all(&inbox_dir).ok();
        std::fs::write(
            inbox_dir.join("agent1.jsonl"),
            format!("{}\n", serde_json::to_string(&msg).unwrap()),
        )
        .ok();
        let result = handle_tool(
            "describe_message",
            &json!({"message_id": "m-dm-test", "instance": "agent1"}),
            "agent1",
        );
        assert_eq!(result["status"], "read");
        assert_eq!(
            result["delivery_mode"].as_str(),
            Some("inbox_fallback"),
            "describe_message must show delivery_mode: {result}"
        );
        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    // --- Sprint 8: busy gate + interrupt ---

    #[test]
    fn test_delegate_task_busy_returns_structured_response() {
        let _g = fleet_test_guard();
        let home = tmp_home("busy-gate");
        std::fs::write(
            home.join("fleet.yaml"),
            "instances:\n  target:\n    backend: claude\n  sender:\n    backend: claude\n",
        )
        .ok();
        std::env::set_var("AGEND_HOME", &home);
        // Create and claim a task for target
        crate::tasks::handle(
            &home,
            "target",
            &json!({"action": "create", "title": "busy work"}),
        );
        let tasks = crate::tasks::handle(&home, "target", &json!({"action": "list"}));
        let tid = tasks["tasks"][0]["id"].as_str().unwrap();
        crate::tasks::handle(&home, "target", &json!({"action": "claim", "id": tid}));

        let result = handle_tool(
            "delegate_task",
            &json!({"target_instance": "target", "task": "new work"}),
            "sender",
        );
        assert_eq!(result["busy"], true, "must return busy: {result}");
        assert!(
            result["current_task"]["id"].is_string(),
            "must have current_task.id: {result}"
        );
        assert!(result["options"].is_array(), "must have options: {result}");
        assert!(
            result["suggestion"].is_string(),
            "must have suggestion: {result}"
        );
        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_delegate_task_force_true_bypasses_busy_gate() {
        let _g = fleet_test_guard();
        let home = tmp_home("interrupt-bypass");
        std::fs::write(
            home.join("fleet.yaml"),
            "instances:\n  target:\n    backend: claude\n  sender:\n    backend: claude\n",
        )
        .ok();
        std::env::set_var("AGEND_HOME", &home);
        crate::tasks::handle(
            &home,
            "target",
            &json!({"action": "create", "title": "busy"}),
        );
        let tasks = crate::tasks::handle(&home, "target", &json!({"action": "list"}));
        let tid = tasks["tasks"][0]["id"].as_str().unwrap();
        crate::tasks::handle(&home, "target", &json!({"action": "claim", "id": tid}));

        let result = handle_tool(
            "delegate_task",
            &json!({"target_instance": "target", "task": "urgent", "force": true, "force_reason": "critical bug"}),
            "sender",
        );
        assert!(
            result.get("busy").is_none(),
            "interrupt=true must bypass busy gate: {result}"
        );
        assert!(
            result.get("error").is_none()
                || !result["error"].as_str().unwrap_or("").contains("busy"),
            "must not error on busy: {result}"
        );
        // Verify force_meta persisted in receiver's inbox
        let msgs = crate::inbox::drain(&home, "target");
        assert!(!msgs.is_empty(), "target must have inbox message");
        let msg = &msgs[0];
        assert!(
            msg.force_meta.is_some(),
            "force_meta must be set on inbox message: {:?}",
            msg.force_meta
        );
        let meta = msg.force_meta.as_ref().unwrap();
        assert!(meta.forced);
        assert_eq!(meta.reason, "critical bug");
        assert!(!meta.forced_at.is_empty());
        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_delegate_task_force_true_without_reason_rejected() {
        let _g = fleet_test_guard();
        let home = tmp_home("interrupt-no-reason");
        std::fs::write(
            home.join("fleet.yaml"),
            "instances:\n  target:\n    backend: claude\n  sender:\n    backend: claude\n",
        )
        .ok();
        std::env::set_var("AGEND_HOME", &home);
        crate::tasks::handle(
            &home,
            "target",
            &json!({"action": "create", "title": "busy"}),
        );
        let tasks = crate::tasks::handle(&home, "target", &json!({"action": "list"}));
        let tid = tasks["tasks"][0]["id"].as_str().unwrap();
        crate::tasks::handle(&home, "target", &json!({"action": "claim", "id": tid}));

        let result = handle_tool(
            "delegate_task",
            &json!({"target_instance": "target", "task": "urgent", "force": true}),
            "sender",
        );
        assert!(
            result["error"].as_str().unwrap_or("").contains("reason"),
            "interrupt without reason must error: {result}"
        );
        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_delegate_task_idle_target_normal_delivery() {
        let _g = fleet_test_guard();
        let home = tmp_home("idle-target");
        std::fs::write(
            home.join("fleet.yaml"),
            "instances:\n  target:\n    backend: claude\n  sender:\n    backend: claude\n",
        )
        .ok();
        std::env::set_var("AGEND_HOME", &home);
        // No claimed tasks for target
        let result = handle_tool(
            "delegate_task",
            &json!({"target_instance": "target", "task": "normal work"}),
            "sender",
        );
        assert!(
            result.get("busy").is_none(),
            "idle target must not return busy: {result}"
        );
        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    // --- Sprint 9 Gap 5: second_reviewer flag ---

    #[test]
    fn test_delegate_task_second_reviewer_flag_requires_reason() {
        let _g = fleet_test_guard();
        let home = tmp_home("sr-no-reason");
        std::fs::write(
            home.join("fleet.yaml"),
            "instances:\n  target:\n    backend: claude\n  sender:\n    backend: claude\n",
        )
        .ok();
        std::env::set_var("AGEND_HOME", &home);
        let result = handle_tool(
            "delegate_task",
            &json!({"target_instance": "target", "task": "review PR", "second_reviewer": true}),
            "sender",
        );
        assert!(
            result["error"]
                .as_str()
                .unwrap_or("")
                .contains("second_reviewer_reason"),
            "second_reviewer=true without reason must error: {result}"
        );
        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_delegate_task_second_reviewer_with_reason_ok() {
        let _g = fleet_test_guard();
        let home = tmp_home("sr-with-reason");
        std::fs::write(
            home.join("fleet.yaml"),
            "instances:\n  target:\n    backend: claude\n  sender:\n    backend: claude\n",
        )
        .ok();
        std::env::set_var("AGEND_HOME", &home);
        let result = handle_tool(
            "delegate_task",
            &json!({
                "target_instance": "target",
                "task": "review PR",
                "second_reviewer": true,
                "second_reviewer_reason": "high-risk protocol change"
            }),
            "sender",
        );
        assert!(
            result.get("error").is_none()
                || !result["error"]
                    .as_str()
                    .unwrap_or("")
                    .contains("second_reviewer"),
            "second_reviewer with reason must not error on flag: {result}"
        );
        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_delegate_task_no_second_reviewer_flag_default_behavior() {
        let _g = fleet_test_guard();
        let home = tmp_home("sr-default");
        std::fs::write(
            home.join("fleet.yaml"),
            "instances:\n  target:\n    backend: claude\n  sender:\n    backend: claude\n",
        )
        .ok();
        std::env::set_var("AGEND_HOME", &home);
        let result = handle_tool(
            "delegate_task",
            &json!({"target_instance": "target", "task": "normal work"}),
            "sender",
        );
        // No second_reviewer flag → no error related to it
        let err = result["error"].as_str().unwrap_or("");
        assert!(
            !err.contains("second_reviewer"),
            "default (no flag) must not error on second_reviewer: {result}"
        );
        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    // ── interrupt tool tests ──

    #[test]
    fn test_interrupt_esc_params_contains_exact_esc_byte() {
        let params = super::interrupt_esc_params("my-agent");
        assert_eq!(params["method"], "inject");
        assert_eq!(params["params"]["name"], "my-agent");
        // Verify the data field is exactly the ESC byte (0x1b)
        let data = params["params"]["data"]
            .as_str()
            .expect("data must be string");
        assert_eq!(data.len(), 1, "ESC byte must be exactly 1 byte");
        assert_eq!(data.as_bytes()[0], 0x1b, "data must be ESC byte (0x1b)");
        assert_eq!(params["params"]["raw"], true, "must be raw inject");
    }

    #[test]
    fn test_interrupt_reason_header_format() {
        let header = crate::inbox::format_event_header("interrupt", &[("reason", "priority task")]);
        assert!(header.contains("[AGEND-MSG]"), "must have header prefix");
        assert!(
            header.contains("kind=interrupt"),
            "must have interrupt kind"
        );
        assert!(
            header.contains("reason=priority task"),
            "must contain reason"
        );
        assert!(!header.contains('\n'), "must be single line");
    }

    #[test]
    fn test_interrupt_handler_validates_target() {
        let home = tmp_home("interrupt-validate");
        std::env::set_var("AGEND_HOME", &home);

        // Missing target
        let r = handle_tool("interrupt", &json!({}), "caller");
        assert!(r["error"].as_str().unwrap().contains("missing"));

        // Invalid target name
        let r = handle_tool("interrupt", &json!({"target": "../escape"}), "caller");
        assert!(r.get("error").is_some());

        // Valid target but no daemon → reaches inject path
        let r = handle_tool("interrupt", &json!({"target": "valid-agent"}), "caller");
        let err = r["error"].as_str().unwrap_or("");
        assert!(
            err.contains("not reachable") || err.contains("API unavailable"),
            "valid target must reach inject path: {err}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // --- Sprint 10: backwards-compat for old interrupt/reason names ---

    #[test]
    fn test_delegate_task_old_interrupt_true_still_works() {
        // Old callers using interrupt=true + reason should still work
        let _g = fleet_test_guard();
        let home = tmp_home("old-interrupt-compat");
        std::fs::write(
            home.join("fleet.yaml"),
            "instances:\n  target:\n    backend: claude\n  sender:\n    backend: claude\n",
        )
        .ok();
        std::env::set_var("AGEND_HOME", &home);
        crate::tasks::handle(
            &home,
            "target",
            &json!({"action": "create", "title": "busy"}),
        );
        let tasks = crate::tasks::handle(&home, "target", &json!({"action": "list"}));
        let tid = tasks["tasks"][0]["id"].as_str().unwrap();
        crate::tasks::handle(&home, "target", &json!({"action": "claim", "id": tid}));

        // Use OLD names: interrupt + reason
        let result = handle_tool(
            "delegate_task",
            &json!({"target_instance": "target", "task": "urgent", "interrupt": true, "reason": "legacy caller"}),
            "sender",
        );
        // Should bypass busy gate (backwards-compat) + emit deprecation warning
        assert!(
            result.get("busy").is_none(),
            "old interrupt=true must still bypass busy gate: {result}"
        );
        assert!(
            result["warning"]
                .as_str()
                .unwrap_or("")
                .contains("deprecated"),
            "old names must emit deprecation warning: {result}"
        );
        std::env::remove_var("AGEND_HOME");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_old_inbox_json_with_interrupt_meta_deserializes_into_force_meta() {
        // Sprint 8-9 inbox JSONL uses "interrupt_meta" + "interrupted" + "interrupted_at".
        // Must deserialize into ForceMeta via serde aliases.
        let old_json = r#"{"schema_version":1,"id":"m-old","from":"test","text":"hi","kind":null,"timestamp":"2026-01-01T00:00:00Z","interrupt_meta":{"interrupted":true,"reason":"legacy","interrupted_at":"2026-01-01T00:00:00Z"}}"#;
        let msg: crate::inbox::InboxMessage =
            serde_json::from_str(old_json).expect("deserialize old format");
        assert!(
            msg.force_meta.is_some(),
            "old interrupt_meta must deserialize into force_meta"
        );
        let meta = msg.force_meta.unwrap();
        assert!(meta.forced, "interrupted=true must map to forced=true");
        assert_eq!(meta.reason, "legacy");
        assert!(!meta.forced_at.is_empty());
    }
}
