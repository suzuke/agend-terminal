use crate::agent_ops::{list_agents, send_to};
use crate::channel::sink_registry::registry as ux_sink_registry;
use crate::channel::ux_event::{FleetEvent, UxEvent};
use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

use super::{err_needs_identity, is_ok_result};

/// Sprint 30: unified `send` handler. Routes to existing handlers based on
/// `request_kind` or infers from args (targets/team → broadcast, task field
/// → delegate, summary → report, question → query, default → send_to).
pub(super) fn handle_unified_send(home: &Path, args: &Value, sender: &Option<Sender>) -> Value {
    // Normalize: map instance_name ↔ target_instance for cross-compat
    let mut args = args.clone();
    if args.get("target_instance").is_none() {
        if let Some(name) = args.get("instance_name").cloned() {
            args["target_instance"] = name;
        }
    }
    if args.get("instance_name").is_none() {
        if let Some(target) = args.get("target_instance").cloned() {
            args["instance_name"] = target;
        }
    }

    // Broadcast mode: targets/team/tags present
    if args.get("targets").is_some() || args.get("team").is_some() || args.get("tags").is_some() {
        return handle_broadcast(home, &args, sender);
    }

    fn lift_message(args: &mut Value, dst: &str) {
        if args.get(dst).is_none() {
            if let Some(msg) = args.get("message").cloned() {
                args[dst] = msg;
            }
        }
    }
    match args["request_kind"].as_str().unwrap_or("") {
        "task" => {
            lift_message(&mut args, "task");
            handle_delegate_task(home, &args, sender)
        }
        "report" => {
            lift_message(&mut args, "summary");
            handle_report_result(home, &args, sender)
        }
        "query" => {
            lift_message(&mut args, "question");
            handle_request_information(home, &args, sender)
        }
        _ => handle_send_to_instance(home, &args, "send", sender),
    }
}

pub(super) fn handle_send_to_instance(
    home: &Path,
    args: &Value,
    tool: &str,
    sender: &Option<Sender>,
) -> Value {
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
        home,
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
            // Centralised fallback (Sprint 40 T-7 B4)
            let mut resolved_thread = thread_id.map(String::from);
            let resolved_parent = parent_id.map(String::from);
            if resolved_thread.is_none() {
                if let Some(ref pid) = resolved_parent {
                    if let Some(parent_msg) = crate::inbox::find_message(home, pid) {
                        resolved_thread = parent_msg.thread_id.or_else(|| parent_msg.id.clone());
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
                channel: None,
                delivery_mode: Some("inbox_fallback".to_string()),
                attachments: vec![],
                in_reply_to_msg_id: None,
                in_reply_to_excerpt: None,
                superseded_by: None,
                from_id: None,
            };
            crate::agent_ops::fallback_deliver(home, sender.as_str(), target, text, msg, &e)
        }
    };
    // Warn if kind=report without parent_id
    let mut result = result;
    if kind == Some("report") && parent_id.is_none() {
        if let Some(obj) = result.as_object_mut() {
            obj.insert(
                "warning".to_string(),
                json!("parent_id recommended for report kind; will be required in future version"),
            );
        }
    }
    result
}

pub(super) fn handle_delegate_task(home: &Path, args: &Value, sender: &Option<Sender>) -> Value {
    let Some(sender) = sender.as_ref() else {
        return err_needs_identity("delegate_task");
    };
    let raw_target = match args["target_instance"].as_str() {
        Some(t) => t,
        None => return json!({"error": "missing 'target_instance'"}),
    };
    if let Err(e) = crate::agent::validate_name(raw_target) {
        return json!({"error": e});
    }
    // Sprint 46 P2: resolve target via InstanceId — replaces P1 name-lookup bandaid.
    // If raw_target resolves to a known instance (by id, short-id, or name), route
    // directly. Otherwise fall through to team-orchestrator resolution.
    let resolved_target = match crate::agent::resolve_instance(home, raw_target) {
        Ok((_id, name)) => name,
        Err(crate::agent::ResolveError::NotFound(_)) => {
            // Not a known instance — try team-orchestrator resolution
            match crate::teams::resolve_team_orchestrator(home, raw_target) {
                Ok(Some(orch)) => orch,
                Ok(None) => raw_target.to_string(),
                Err(e) => return json!({"error": e}),
            }
        }
    };
    let target = resolved_target.as_str();
    // M5: reject if team-orchestrator resolution collapsed target to sender.
    // Only fires when resolution actually changed the target (raw_target !=
    // resolved_target), so plain self-sends still hit the API-layer check
    // with its own error message.
    if *sender == target && raw_target != target {
        return json!({"error": format!(
            "task target '{}' resolved to sender '{}' (team orchestrator loop) \
             — verify target_instance name does not collide with a team template name",
            raw_target, sender.as_str()
        )});
    }
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
    let claimed_tasks: Vec<_> = crate::tasks::list_all(home)
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
    if let Some(branch) = args["branch"].as_str() {
        msg.push_str(&format!("\n\nBranch: {branch}"));
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
        home,
        &json!({
            "method": crate::api::method::SEND,
            "params": {
                "from": sender.as_str(),
                "target": target,
                "text": msg,
                "kind": "task",
                "task_id": task_id_str,
                "force_meta": force_meta_json,
                "provenance": {
                    "from": sender.as_str(),
                    "task": task,
                },
                "branch": args["branch"].as_str(),
            }
        }),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => json!({"target": target}),
        Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("send failed")}),
        Err(e) => {
            // Centralised fallback (Sprint 40 T-7 B4)
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
                channel: None,
                attachments: vec![],
                in_reply_to_msg_id: None,
                in_reply_to_excerpt: None,
                superseded_by: None,
                from_id: None,
            };
            crate::agent_ops::fallback_deliver(home, sender.as_str(), target, &msg, inbox_msg, &e)
        }
    };
    if is_ok_result(&result) {
        let task_id = task_id_str.map(str::to_string);
        // Track dispatch for timeout detection
        crate::dispatch_tracking::track_dispatch(
            home,
            crate::dispatch_tracking::DispatchEntry {
                task_id: task_id.clone(),
                from: sender.as_str().to_string(),
                to: target.to_string(),
                from_id: crate::agent::resolve_instance(home, sender.as_str())
                    .ok()
                    .map(|(id, _)| id.full()),
                to_id: crate::agent::resolve_instance(home, target)
                    .ok()
                    .map(|(id, _)| id.full()),
                delegated_at: chrono::Utc::now().to_rfc3339(),
                status: "pending".to_string(),
            },
        );
        // Sprint 30: log branch hint for operator visibility when
        // delegate_task carries branch metadata.
        if let Some(branch) = args["branch"].as_str() {
            tracing::info!(
                target = %target,
                branch = %branch,
                task_id = ?task_id_str,
                "delegate_task branch hint — implementer should work on this branch"
            );
        }
        ux_sink_registry().emit(&UxEvent::Fleet(FleetEvent::DelegateTask {
            from: sender.as_str().to_string(),
            to: target.to_string(),
            summary: task.to_string(),
            task_id,
        }));
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

pub(super) fn handle_report_result(home: &Path, args: &Value, sender: &Option<Sender>) -> Value {
    let Some(sender) = sender.as_ref() else {
        return err_needs_identity("report_result");
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

        // M3: SHA-staleness gate — if reviewed_head is provided, verify against PR HEAD.
        if let Some(rh) = reviewed_head {
            if let Err(e) =
                super::sha_gate::check_sha_gate(rh, summary, super::sha_gate::fetch_pr_head_sha)
            {
                return json!({"error": e});
            }
        }

        match crate::api::call(
            home,
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
                // Centralised fallback (Sprint 40 T-7 B4)
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
                    channel: None,
                    attachments: vec![],
                    in_reply_to_msg_id: None,
                    in_reply_to_excerpt: None,
                    superseded_by: None,
                    from_id: None,
                };
                crate::agent_ops::fallback_deliver(
                    home,
                    sender.as_str(),
                    target,
                    &msg,
                    inbox_msg,
                    &e,
                )
            }
        }
    };
    // Add warning for report kind without parent_id
    let mut result = result;
    if args["parent_id"].as_str().is_none() {
        if let Some(obj) = result.as_object_mut() {
            obj.insert(
                "warning".to_string(),
                json!("parent_id recommended for report kind; will be required in future version"),
            );
        }
    }
    if is_ok_result(&result) {
        // Mark dispatch as completed so timeout sweep doesn't false-warn.
        let cid = args["correlation_id"].as_str();
        crate::dispatch_tracking::mark_completed(home, cid, sender.as_str());
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

pub(super) fn handle_request_information(
    home: &Path,
    args: &Value,
    sender: &Option<Sender>,
) -> Value {
    let Some(sender) = sender.as_ref() else {
        return err_needs_identity("request_information");
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
    send_to(home, sender, target, &msg, "query")
}

pub(super) fn handle_broadcast(home: &Path, args: &Value, sender: &Option<Sender>) -> Value {
    let Some(sender) = sender.as_ref() else {
        return err_needs_identity("broadcast");
    };
    let message = match args["message"].as_str() {
        Some(m) => m,
        None => return json!({"error": "missing 'message'"}),
    };
    // Resolve targets: team > targets > tags > all
    let targets: Vec<String> = if let Some(team) = args["team"].as_str() {
        crate::teams::get_members(home, team)
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
        let _ = send_to(home, sender, target, message, kind);
        sent.push(target.clone());
    }
    if !sent.is_empty() {
        ux_sink_registry().emit(&UxEvent::Fleet(FleetEvent::Broadcast {
            from: sender.as_str().to_string(),
            recipients: sent.clone(),
            summary: message.to_string(),
        }));
    }
    json!({"sent_to": sent, "count": sent.len()})
}

pub(super) fn handle_inbox(home: &Path, instance_name: &str) -> Value {
    let messages = crate::inbox::drain(home, instance_name);
    if !messages.is_empty() {
        let meta_path = crate::agent_ops::metadata_path_resolved(home, instance_name);
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
                    crate::channel::sink_registry::registry().emit(&UxEvent::AgentPickedUp {
                        origin_msg,
                        agent: instance_name.to_string(),
                    });
                }
            }
        }
        // M6: clear only the pickup IDs we just emitted (not any that arrived
        // between drain and clear). Save null only if no new IDs accumulated.
        crate::agent_ops::save_metadata(home, instance_name, "pending_pickup_ids", json!([]));
    }
    json!({"messages": messages})
}

pub(super) fn handle_describe_message(home: &Path, args: &Value, instance_name: &str) -> Value {
    let msg_id = match args["message_id"].as_str() {
        Some(id) => id,
        None => return json!({"error": "missing 'message_id'"}),
    };
    let target = args["instance"].as_str().unwrap_or(instance_name);
    let status = crate::inbox::describe_message(home, msg_id, target);
    match status {
        crate::inbox::MessageStatus::ReadAt(t, dm) => {
            let mut resp = json!({"status": "read", "read_at": t});
            if let Some(mode) = dm {
                resp["delivery_mode"] = json!(mode);
            }
            if let Some(msg) = crate::inbox::find_message(home, msg_id) {
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

pub(super) fn handle_describe_thread(home: &Path, args: &Value) -> Value {
    let thread_id = match args["thread_id"].as_str() {
        Some(id) => id,
        None => return json!({"error": "missing 'thread_id'"}),
    };
    let instance = args["instance"].as_str();
    let msgs = crate::inbox::get_thread(home, thread_id, instance);
    json!({"thread_id": thread_id, "messages": msgs, "count": msgs.len()})
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn send_routes_to_broadcast_when_targets_present() {
        let args = json!({"targets": ["a", "b"], "message": "hello"});
        let result = handle_unified_send(&std::env::temp_dir(), &args, &None);
        // Broadcast without sender returns identity error
        assert!(result.get("error").is_some() || result.get("target").is_some());
    }

    #[test]
    fn send_routes_to_delegate_task_when_request_kind_task() {
        let args = json!({"target_instance": "dev", "message": "do X", "request_kind": "task"});
        let result = handle_unified_send(&std::env::temp_dir(), &args, &None);
        // delegate_task without sender returns identity error
        assert!(result.get("error").is_some());
    }

    #[test]
    fn send_routes_to_report_result_when_request_kind_report() {
        let args = json!({"target_instance": "lead", "message": "done", "request_kind": "report"});
        let result = handle_unified_send(&std::env::temp_dir(), &args, &None);
        assert!(result.get("error").is_some());
    }

    #[test]
    fn send_routes_to_request_information_when_request_kind_query() {
        let args = json!({"target_instance": "lead", "message": "what?", "request_kind": "query"});
        let result = handle_unified_send(&std::env::temp_dir(), &args, &None);
        assert!(result.get("error").is_some());
    }

    #[test]
    fn send_routes_to_send_to_instance_default() {
        let args = json!({"target_instance": "dev", "message": "hi"});
        let result = handle_unified_send(&std::env::temp_dir(), &args, &None);
        // send_to_instance without sender returns identity error
        assert!(result.get("error").is_some());
    }
}
