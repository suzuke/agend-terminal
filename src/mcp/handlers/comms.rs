use crate::agent_ops::{list_agents, send_to};
use crate::channel::sink_registry::registry as ux_sink_registry;
use crate::channel::ux_event::{FleetEvent, UxEvent};
use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

use super::{comms_gates::enforce_send_invariants, err_needs_identity, is_ok_result};

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
    let target = match super::require_instance(args) {
        Ok(t) => t,
        Err(e) => return e,
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
                // MED-5: carry `kind` (happy path + sibling fallbacks do) — else a
                // fallback `kind=query` lands kind=None and `inbox clear` swallows it.
                kind: kind.map(String::from),
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
        attach_report_parent_warning(&mut result);
    }
    result
}

/// #2050 simplify PR-B (⑫): attach the "parent_id recommended" warning to a
/// result object (no-op when `result` isn't a JSON object). Only the shared
/// insert is deduped — each CALLER keeps its own guard (the send path gates on
/// `kind == Some("report") && parent_id.is_none()`, the report path on
/// `args["parent_id"].as_str().is_none()`), so behavior is byte-identical.
fn attach_report_parent_warning(result: &mut Value) {
    if let Some(obj) = result.as_object_mut() {
        obj.insert(
            "warning".to_string(),
            json!("parent_id recommended for report kind; will be required in future version"),
        );
    }
}

pub(super) fn handle_delegate_task(home: &Path, args: &Value, sender: &Option<Sender>) -> Value {
    let Some(sender) = sender.as_ref() else {
        return err_needs_identity("delegate_task");
    };
    let raw_target = match super::require_instance(args) {
        Ok(t) => t,
        Err(e) => return e,
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
    // CR-2026-06-14 (resource-leak): reject a plain self-dispatch BEFORE the
    // auto-bind/lease. The qualified guard above only fires when team-orchestrator
    // resolution COLLAPSED the target onto the sender; a plain self-dispatch
    // (raw_target == resolved == sender) skipped it and fell through to
    // `dispatch_auto_bind_lease_with_chain` (which leases a worktree + writes a
    // binding) before the API-layer self-send check rejected the send — orphaning
    // the leased worktree with no rollback on this path. Reject unconditionally
    // here so no lease happens for a dispatch the send would reject anyway.
    if *sender == target {
        return json!({"error": "cannot delegate task to self — use a different instance"});
    }
    let task = match args["task"].as_str() {
        Some(t) => t,
        None => return json!({"error": "missing 'task'"}),
    };

    // Pre-send gates (busy / #1286 branch-dedup / #1496 enrich / §3.5
    // second-reviewer / #812 test-name) — side-effect-free, short-circuit in
    // order; see comms_gates::dispatch. The returned scalars (force /
    // force_reason / second_reviewer) feed the message build, force_meta, and
    // lease stages below, so they are derived exactly once.
    let checks = match super::comms_gates::run_dispatch_pre_checks(home, sender, args, target, task)
    {
        Ok(checks) => checks,
        Err(rejection) => return rejection,
    };
    let force = checks.force;
    let force_reason = checks.force_reason.as_deref();
    let second_reviewer = checks.second_reviewer;

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
            // #1877: second_reviewer → review_class="dual" on the auto-armed watch.
            if let Err(e) = super::dispatch_hook::dispatch_auto_bind_lease_with_chain(
                home,
                target,
                task_id_val,
                branch,
                args["repository"].as_str(),
                args["next_after_ci"].as_str(),
                if second_reviewer { Some("dual") } else { None },
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
            // #2117 P2: route the auto-created task to the TARGET's board, not
            // the dispatcher's. `handle` is called with `sender` as the emitter,
            // so without an explicit `project` the create would default to the
            // *caller's* project (resolve_current_project(sender)) — the P1 leak
            // the epic flagged. Stamp the target's project so the task is born on
            // the board the assignee actually works. Single-project → both resolve
            // to DEFAULT → home board → byte-identical.
            let target_project = crate::tasks::resolve_target_project(home, target);
            let create_args = json!({
                "action": "create",
                "title": auto_title,
                "assignee": target,
                "branch": args["branch"].as_str(),
                "priority": "normal",
                "project": target_project,
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
            // HIGH-1: forward the dispatch directives the SEND handler reads that
            // this re-marshal dropped (#1833 class; worktree_binding_required gates).
            "params": {
                "from": sender.as_str(), "target": target, "text": msg, "kind": "task",
                "task_id": task_id_str, "force_meta": force_meta_json,
                "provenance": { "from": sender.as_str(), "task": task },
                "branch": args["branch"].as_str(), "expect_reply_within_secs": args["expect_reply_within_secs"].as_i64(),
                "correlation_id": args["correlation_id"].as_str(), "reviewed_head": args["reviewed_head"].as_str(), "sequencing": args["sequencing"].as_str(), "eta_minutes": args["eta_minutes"].as_u64(), "reporting_cadence": args["reporting_cadence"].as_str(),
                "worktree_binding_required": args["worktree_binding_required"].as_bool(), "terminal": args["terminal"].as_bool(), "thread_id": args["thread_id"].as_str(), "parent_id": args["parent_id"].as_str(),
                // #2099: forward the fire-and-forget flag so the API-side
                // dispatch_idle record path (the SECOND ~30min nag channel,
                // besides the DispatchEntry sweep) can also skip it.
                "no_report_expected": args["no_report_expected"].as_bool(),
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
                // HIGH-1: carry the dispatch directives so an API-blip fallback doesn't lose them.
                correlation_id: args["correlation_id"].as_str().map(String::from),
                reviewed_head: args["reviewed_head"].as_str().map(String::from),
                sequencing: args["sequencing"].as_str().map(String::from),
                eta_minutes: args["eta_minutes"].as_u64().map(|v| v as u32),
                reporting_cadence: args["reporting_cadence"].as_str().map(String::from),
                worktree_binding_required: args["worktree_binding_required"].as_bool(),
                terminal: args["terminal"].as_bool(),
                thread_id: args["thread_id"].as_str().map(String::from),
                parent_id: args["parent_id"].as_str().map(String::from),
                timestamp: chrono::Utc::now().to_rfc3339(),
                ..Default::default()
            };
            crate::agent_ops::fallback_deliver(home, sender.as_str(), target, &msg, inbox_msg, &e)
        }
    };
    if is_ok_result(&result) {
        let task_id = task_id_str.map(str::to_string);
        // #2099: a fire-and-forget dispatch (`no_report_expected`) is recorded
        // with a terminal-like status so the 30-min stuck sweep never false-fires
        // for it — the audit row is kept, but sweep_stuck/sweep_orphans skip it.
        // Default (flag absent/false) stays "pending" → normal stuck tracking.
        let status = if args["no_report_expected"].as_bool().unwrap_or(false) {
            "no_report_expected"
        } else {
            "pending"
        };
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
                status: status.to_string(),
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
    let target = match super::require_instance(args) {
        Ok(t) => t,
        Err(e) => return e,
    };
    crate::validate_name_or_err!(target);
    let summary = match args["summary"].as_str() {
        Some(s) => s,
        None => return json!({"error": "missing 'summary'"}),
    };
    let msg = comms_inbox::build_report_text(
        summary,
        args["correlation_id"].as_str(),
        args["artifacts"].as_str(),
    );
    let result = {
        let correlation_id = args["correlation_id"].as_str();
        let reviewed_head = args["reviewed_head"].as_str();

        // M3: SHA-staleness gate — if reviewed_head is provided, verify against PR HEAD.
        if let Some(rh) = reviewed_head {
            if let Err(e) = super::comms_gates::check_sha_gate(
                rh,
                summary,
                super::comms_gates::fetch_pr_head_sha,
            ) {
                return json!({"error": e});
            }
        }

        // #1666 Phase A: reviewer-evidence gate. Only fires on actual verdict
        // reports (summary STARTS WITH VERIFIED/REJECTED/UNVERIFIED — §3.12
        // convention, so non-verdict reports are never touched). VERIFIED/
        // REJECTED must carry an evidence token; UNVERIFIED is exempt. Scans
        // summary + artifacts (evidence may live in either) and reuses the
        // sha_gate reject path (`json!({"error"})` → back to the reviewer).
        if let Some(verdict) = super::comms_gates::detect_verdict(summary) {
            let evidence_body = match args["artifacts"].as_str() {
                Some(a) => format!("{summary}\n{a}"),
                None => summary.to_string(),
            };
            if let Err(e) = super::comms_gates::check_evidence_gate(&evidence_body, verdict) {
                return json!({"error": e});
            }

            // #1666 Phase B (WARN-first): cross-check the checkable evidence and
            // LOG (never reject) — measures the false-positive rate. See the fn.
            super::comms_gates::cross_check_and_log(
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
        attach_report_parent_warning(&mut result);
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
    let target = match super::require_instance(args) {
        Ok(t) => t,
        Err(e) => return e,
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
            // CR-2026-06-14 (concurrency): filter the processed pickup ids out of
            // `pending_pickup_ids` under the metadata flock (atomic read-modify-
            // write). The prior code re-read the file UNLOCKED, filtered, then
            // wrote the remainder via save_metadata — a pickup id appended between
            // the unlocked re-read and the write was clobbered (lost update).
            // Filtering inside the locked closure reads the CURRENT on-disk set,
            // so a concurrently-appended id is preserved.
            crate::agent_ops::update_metadata(
                home,
                instance_name,
                "pending_pickup_ids",
                |current| {
                    let remaining: Vec<Value> = current
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|e| {
                            !processed_msg_ids
                                .contains(&e["msg_id"].as_str().unwrap_or("").to_string())
                        })
                        .collect();
                    json!(remaining)
                },
            );
        }
    }
    json!({"messages": messages})
}

// #1286: inbox describe handlers extracted to stay under file_size_invariant.
#[path = "comms_inbox.rs"]
mod comms_inbox;
pub(super) use comms_inbox::{
    handle_describe_message, handle_describe_thread, handle_inbox_ack, handle_inbox_clear,
};

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
