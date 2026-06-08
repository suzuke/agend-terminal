use crate::agent_ops::{list_agents, send_to};
use crate::channel::sink_registry::registry as ux_sink_registry;
use crate::channel::ux_event::{FleetEvent, UxEvent};
use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

use super::{anti_stall::enforce_send_invariants, err_needs_identity, is_ok_result};

/// Sprint 30: unified `send` handler. Routes to existing handlers based on
/// `request_kind` or infers from args (targets/team → broadcast, task field
/// → delegate, summary → report, question → query, default → send_to).
pub(super) fn handle_unified_send(home: &Path, args: &Value, sender: &Option<Sender>) -> Value {
    let mut args = args.clone();
    if let Some(err) = enforce_send_invariants(home, &args, sender) {
        return err;
    }
    // Broadcast mode: targets/team/tags present
    if args.get("instances").is_some() || args.get("team").is_some() || args.get("tags").is_some() {
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
    let target = match args["instance"].as_str() {
        Some(t) => t,
        None => return json!({"error": "missing 'instance'"}),
    };
    crate::validate_name_or_err!(target);
    if *sender == target {
        return json!({"error": "cannot send to self — use a different instance"});
    }
    // #1602: `message` only — the legacy `text` alias is dropped (reply's
    // text→message rename removed the last schema that declared `text`, and the
    // dispatch validator now rejects a message-less `send` before this handler
    // runs anyway). `send`/`reply`/`schedule` are all `message` now.
    let text = match args["message"].as_str() {
        Some(t) if !t.is_empty() => t,
        _ => return json!({"error": "missing or empty 'message'"}),
    };
    let kind = args["request_kind"]
        .as_str()
        .or_else(|| args["kind"].as_str());
    let thread_id = args["thread_id"].as_str();
    let parent_id = args["parent_id"].as_str();

    let result = match crate::api::call(
        home,
        &json!({
            "request_id": uuid::Uuid::new_v4().to_string(),
            "method": crate::api::method::SEND,
            // #1024 (closes #1002 ROOT 2): `reviewed_head` MUST be forwarded; see
            // sibling `handle_report_result` + `auto_release::is_verdict_message`.
            "params": { "from": sender.as_str(), "target": target, "text": text, "kind": kind, "thread_id": thread_id, "parent_id": parent_id, "correlation_id": args["correlation_id"].as_str(), "reviewed_head": args["reviewed_head"].as_str(), "sequencing": args["sequencing"].as_str(), "eta_minutes": args["eta_minutes"].as_u64(), "reporting_cadence": args["reporting_cadence"].as_str(), "worktree_binding_required": args["worktree_binding_required"].as_bool(), "expect_reply_within_secs": args["expect_reply_within_secs"].as_i64(), "terminal": args["terminal"].as_bool() }
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
                thread_id: resolved_thread,
                parent_id: resolved_parent,
                correlation_id: args["correlation_id"].as_str().map(String::from),
                from: format!("from:{}", sender.as_str()),
                text: text.to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                delivery_mode: Some("inbox_fallback".to_string()),
                terminal: args["terminal"].as_bool(),
                ..Default::default()
            };
            crate::agent_ops::fallback_deliver(home, sender.as_str(), target, text, msg, &e)
        }
    };
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
    let raw_target = match args["instance"].as_str() {
        Some(t) => t,
        None => return json!({"error": "missing 'instance'"}),
    };
    crate::validate_name_or_err!(raw_target);
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
             — verify instance name does not collide with a team template name",
            raw_target, sender.as_str()
        )});
    }
    let task = match args["task"].as_str() {
        Some(t) => t,
        None => return json!({"error": "missing 'task'"}),
    };

    let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
    let force_reason = args.get("force_reason").and_then(|v| v.as_str());
    let claimed_tasks: Vec<_> = crate::tasks::list_all(home)
        .into_iter()
        .filter(|t| {
            t.assignee.as_deref() == Some(target)
                && (t.status == crate::task_events::TaskStatus::Claimed
                    || t.status == crate::task_events::TaskStatus::InProgress)
        })
        .collect();
    // #1496 Option 1: a send(kind=task) whose `task_id` is already one of the
    // target's active tasks is ENRICHING that in-flight dispatch (finally
    // delivering its context), not opening a competing one — let it through the
    // busy-gate. Pairs with dropping task(create)'s premature auto-notify so the
    // create→send dispatch sequence no longer needs force=true (#1496 spike).
    let enriching_active = args["task_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .is_some_and(|tid| claimed_tasks.iter().any(|t| t.id.as_str() == tid));
    // #1286: branch-specific dispatch dedup — reject if target already has
    // an active task on the same branch (more specific than generic busy).
    if !force && !enriching_active {
        if let Some(branch) = args["branch"].as_str() {
            if let Some(dup) = claimed_tasks
                .iter()
                .find(|t| t.branch.as_deref() == Some(branch))
            {
                return json!({
                    "error": format!(
                        "dispatch rejected: {} already has active task {} on branch {}",
                        target, dup.id, branch
                    )
                });
            }
        }
    }
    if !claimed_tasks.is_empty() && !enriching_active {
        if force {
            if force_reason.is_none() || force_reason == Some("") {
                return json!({"error": "force=true requires a non-empty 'force_reason'"});
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

    // #812: dispatch-time test-name validation. Extends §4.3
    // hallucinated-fn check to the dispatch path so `cargo test`
    // invocations naming a test that doesn't exist in the PR tree
    // are rejected BEFORE the reviewer wastes a cycle on
    // `no test matched`. Tree resolution priority: sender's bound
    // worktree → recipient's daemon-managed path. None → fail-open
    // with warn-log (don't block when only operator has the tree).
    let branch = args["branch"].as_str();
    if let Some(tree) =
        crate::claim_verifier::resolve_dispatch_tree(home, sender.as_str(), Some(target), branch)
    {
        if let Err(detail) = crate::claim_verifier::validate_dispatch_test_names(task, &tree) {
            return json!({
                "error": detail,
                "code": "test_name_not_found",
            });
        }
    } else {
        tracing::warn!(
            sender = %sender.as_str(),
            target = %target,
            branch = ?branch,
            "#812 dispatch test-name check skipped — no resolvable PR tree (sender unbound + no daemon worktree)"
        );
    }

    let mut msg = format!("[delegate_task] {task}");
    if force {
        if let Some(r) = force_reason {
            msg.push_str(&format!("\n\n⚠️ FORCED (reason: {r})"));
        }
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

    // Sprint 53 P0-1+P0-2: lease + watch_ci gate BEFORE send (Q2 ordering fix).
    if let Some(branch) = args["branch"].as_str() {
        let task_id_val = args["task_id"].as_str().unwrap_or("");
        if dispatch_should_skip_auto_bind(args) {
            tracing::info!(
                %target, %branch, task_id = %task_id_val,
                "dispatch_auto_bind_lease skipped (bind: false)"
            );
        } else {
            let repo_arg = args["repository"].as_str();
            let next_after_ci_arg = args["next_after_ci"].as_str();
            if let Err(e) = super::dispatch_hook::dispatch_auto_bind_lease_with_chain(
                home,
                target,
                task_id_val,
                branch,
                repo_arg,
                next_after_ci_arg,
            ) {
                return json!({"ok": false, "error": format!("dispatch rejected: {e}")});
            }
        }
    }

    // #1050: auto-create board task after ALL rejectable checks pass
    // (validation, busy gate, lease/bind). Only for single-target with
    // empty task_id and sender != target. Task creation is the
    // dispatch-commit step — no orphan tasks on any rejection path.
    let (effective_task_id, auto_created_task_id): (Option<String>, Option<String>) =
        if args["task_id"].as_str().unwrap_or("").is_empty() && *sender != target {
            let auto_title = args["message"]
                .as_str()
                .or_else(|| args["task"].as_str())
                .unwrap_or("(untitled dispatch)")
                .chars()
                .take(80)
                .collect::<String>();
            let create_args = json!({
                "action": "create",
                "title": auto_title,
                "assignee": target,
                "branch": args["branch"].as_str(),
                "priority": "normal",
            });
            let task_result = crate::tasks::handle(home, sender.as_str(), &create_args);
            match task_result["id"].as_str() {
                Some(id) => {
                    crate::daemon::task_progress::touch(
                        home,
                        id,
                        crate::daemon::task_progress::ProgressSource::Broadcast,
                    );
                    (Some(id.to_string()), Some(id.to_string()))
                }
                None => (None, None),
            }
        } else {
            (args["task_id"].as_str().map(String::from), None)
        };
    let task_id_str = effective_task_id.as_deref();
    if let Some(tid) = task_id_str {
        msg.push_str(&format!(" (task id: {tid})"));
    }

    let result = match crate::api::call(
        home,
        &json!({
            "request_id": uuid::Uuid::new_v4().to_string(),
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
                "expect_reply_within_secs": args["expect_reply_within_secs"].as_i64(),
            }
        }),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => json!({"target": target}),
        Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("send failed")}),
        Err(e) => {
            // Centralised fallback (Sprint 40 T-7 B4)
            let inbox_msg = crate::inbox::InboxMessage {
                task_id: task_id_str.map(String::from),
                force_meta: force_meta_json.as_ref().and_then(|v| {
                    serde_json::from_value::<crate::inbox::ForceMeta>(v.clone()).ok()
                }),
                delivery_mode: Some("inbox_fallback".to_string()),
                from: format!("from:{}", sender.as_str()),
                text: msg.clone(),
                kind: Some("task".to_string()),
                timestamp: chrono::Utc::now().to_rfc3339(),
                ..Default::default()
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
        // P0-2: auto-watch_ci moved into `dispatch_auto_bind_lease` above so it
        // fires on agent-to-agent `send` too (Hotfix C #451 was post-SEND and
        // required explicit `repo` arg, which `send`'s schema never carried).
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
    let mut result = result;
    if let Some(tid) = auto_created_task_id {
        if let Some(obj) = result.as_object_mut() {
            obj.insert("auto_created_task_id".into(), json!(tid));
        }
    }
    result
}

pub(super) fn handle_report_result(home: &Path, args: &Value, sender: &Option<Sender>) -> Value {
    let Some(sender) = sender.as_ref() else {
        return err_needs_identity("report_result");
    };
    let target = match args["instance"].as_str() {
        Some(t) => t,
        None => return json!({"error": "missing 'instance'"}),
    };
    crate::validate_name_or_err!(target);
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

        // #1666 Phase A: reviewer-evidence gate. Only fires on actual verdict
        // reports (summary STARTS WITH VERIFIED/REJECTED/UNVERIFIED — §3.12
        // convention, so non-verdict reports are never touched). VERIFIED/
        // REJECTED must carry an evidence token; UNVERIFIED is exempt. Scans
        // summary + artifacts (evidence may live in either) and reuses the
        // sha_gate reject path (`json!({"error"})` → back to the reviewer).
        if let Some(verdict) = super::evidence_gate::detect_verdict(summary) {
            let evidence_body = match args["artifacts"].as_str() {
                Some(a) => format!("{summary}\n{a}"),
                None => summary.to_string(),
            };
            if let Err(e) = super::evidence_gate::check_evidence_gate(&evidence_body, verdict) {
                return json!({"error": e});
            }

            // #1666 Phase B (WARN-first): cross-check the checkable evidence and
            // LOG (never reject) — measures the false-positive rate. See the fn.
            super::evidence_gate::cross_check_and_log(
                home,
                sender.as_str(),
                summary,
                &evidence_body,
                verdict,
            );
        }

        match crate::api::call(
            home,
            &json!({
                "request_id": uuid::Uuid::new_v4().to_string(),
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
                    "terminal": args["terminal"].as_bool(),
                }
            }),
        ) {
            Ok(resp) if resp["ok"].as_bool() == Some(true) => json!({"target": target}),
            Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("send failed")}),
            Err(e) => {
                // Centralised fallback (Sprint 40 T-7 B4)
                let inbox_msg = crate::inbox::InboxMessage {
                    thread_id: args["thread_id"].as_str().map(String::from),
                    parent_id: args["parent_id"].as_str().map(String::from),
                    correlation_id: correlation_id.map(String::from),
                    reviewed_head: reviewed_head.map(String::from),
                    delivery_mode: Some("inbox_fallback".to_string()),
                    from: format!("from:{}", sender.as_str()),
                    text: msg.clone(),
                    kind: Some("report".to_string()),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    terminal: args["terminal"].as_bool(),
                    ..Default::default()
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
        // Sprint 53 P0-Y: do NOT auto-unbind here. Every kind=report reply
        // (progress update OR final done) landed in this handler, so the
        // prior auto-unbind tore down the binding on the first progress
        // update — Phase 1 trailer stopped firing on subsequent commits,
        // P0-X release_worktree refused ("no binding"), Phase 4 GC saw
        // zero candidates (unbind doesn't write released_at; only
        // release()/release_full() do), and orphan worktrees accumulated.
        // `release_worktree` MCP tool is now the single source of truth
        // for binding lifecycle. Sprint 54 candidates: explicit `task_done`
        // flag in report envelope, or lifecycle rework with auto
        // release_full on that explicit signal.
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
    let target = match args["instance"].as_str() {
        Some(t) => t,
        None => return json!({"error": "missing 'instance'"}),
    };
    crate::validate_name_or_err!(target);
    let question = match args["question"].as_str() {
        Some(q) => q,
        None => return json!({"error": "missing 'question'"}),
    };
    let mut msg = format!("[request_information] {question}");
    if let Some(ctx) = args["context"].as_str() {
        msg.push_str(&format!("\n\nContext: {ctx}"));
    }
    send_to(home, sender, target, &msg, "query", None)
}

pub(super) fn handle_broadcast(home: &Path, args: &Value, sender: &Option<Sender>) -> Value {
    let Some(sender) = sender.as_ref() else {
        return err_needs_identity("broadcast");
    };
    let message = match args["message"].as_str() {
        Some(m) => m,
        None => return json!({"error": "missing 'message'"}),
    };
    let team_name = args["team"].as_str().map(String::from);
    let targets: Vec<String> = if let Some(team) = team_name.as_deref() {
        crate::teams::get_members(home, team)
    } else if let Some(t) = args["instances"].as_array() {
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
    let broadcast_ctx = crate::inbox::BroadcastContext {
        team: team_name,
        targets: targets.clone(),
        count: targets.len(),
    };
    let mut sent = Vec::new();
    let mut failed: Vec<Value> = Vec::new();
    for target in &targets {
        let result = send_to(home, sender, target, message, kind, Some(&broadcast_ctx));
        if result.get("error").is_some() {
            failed.push(json!({"target": target, "error": result["error"]}));
        } else {
            sent.push(target.clone());
        }
    }
    if !sent.is_empty() {
        ux_sink_registry().emit(&UxEvent::Fleet(FleetEvent::Broadcast {
            from: sender.as_str().to_string(),
            recipients: sent.clone(),
            summary: message.to_string(),
        }));
    }
    let mut resp = json!({"sent_to": sent, "count": sent.len()});
    if !failed.is_empty() {
        resp["failed"] = json!(failed);
    }
    resp
}

pub(super) fn handle_inbox(home: &Path, instance_name: &str) -> Value {
    let messages = crate::inbox::drain(home, instance_name);
    if !messages.is_empty() {
        let meta_path = crate::agent_ops::metadata_path_resolved(home, instance_name);
        let mut processed_msg_ids: Vec<String> = Vec::new();
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
                    processed_msg_ids.push(msg_id.to_string());
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
        if !processed_msg_ids.is_empty() {
            // Known TOCTOU window: a message arriving between this re-read
            // and the save_metadata call below can lose its pickup_id.
            // The window is narrow (JSON parse + filter) and self-healing
            // (next handle_inbox drains surviving IDs).
            let remaining: Vec<Value> = std::fs::read_to_string(&meta_path)
                .ok()
                .and_then(|c| serde_json::from_str::<Value>(&c).ok())
                .and_then(|m| m["pending_pickup_ids"].as_array().cloned())
                .unwrap_or_default()
                .into_iter()
                .filter(|e| {
                    !processed_msg_ids.contains(&e["msg_id"].as_str().unwrap_or("").to_string())
                })
                .collect();
            crate::agent_ops::save_metadata(
                home,
                instance_name,
                "pending_pickup_ids",
                json!(remaining),
            );
        }
    }
    json!({"messages": messages})
}

// #1286: inbox describe handlers extracted to stay under file_size_invariant.
#[path = "comms_inbox.rs"]
mod comms_inbox;
pub(super) use comms_inbox::{handle_describe_message, handle_describe_thread, handle_inbox_clear};

/// Sprint 55 P0-C — true when the caller passed `bind: false`, signaling
/// a read-only RCA/audit/design dispatch that should NOT trigger
/// `dispatch_auto_bind_lease`. Default (absent or `Some(true)`) preserves
/// the auto-bind behavior all 50+ existing dispatch sites rely on.
fn dispatch_should_skip_auto_bind(args: &Value) -> bool {
    args["bind"].as_bool() == Some(false)
}

// Sprint 55 P0-C — helper tests in sibling file (file_size_invariant ceiling).
#[cfg(test)]
#[path = "comms_p0c_tests.rs"]
mod p0c_tests;
