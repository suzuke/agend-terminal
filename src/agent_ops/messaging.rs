use crate::agent::{self, AgentRegistry};
use std::path::Path;

pub(crate) struct SendRequest {
    pub from: String,
    pub target: String,
    pub text: String,
    pub kind: Option<String>,
    pub thread_id: Option<String>,
    pub parent_id: Option<String>,
    pub correlation_id: Option<String>,
    pub reviewed_head: Option<String>,
    pub report_purpose: Option<String>,
    pub code_review: Option<serde_json::Value>,
    pub eta_minutes: Option<u64>,
    pub reporting_cadence: Option<String>,
    pub worktree_binding_required: Option<bool>,
    pub expect_reply_within_secs: Option<i64>,
    pub terminal: Option<bool>,
    pub no_report_expected: Option<bool>,
    pub delivery_nonce: Option<String>,
    pub task_id: Option<String>,
    pub force_meta: Option<serde_json::Value>,
    pub provenance: Option<serde_json::Value>,
    pub branch: Option<String>,
    pub broadcast_context: Option<crate::inbox::BroadcastContext>,
    pub priority: Option<String>,
}

pub(crate) enum SendOutcome {
    Success {
        delivery_mode: String,
        branch_checked_out: Option<String>,
        auto_task_id: Option<String>,
    },
    Error {
        error: String,
        code: Option<String>,
        hint: Option<String>,
    },
}

pub(crate) fn execute_send(
    home: &Path,
    registry: &AgentRegistry,
    request: SendRequest,
) -> SendOutcome {
    let from = &request.from;
    let target = &request.target;
    let text = &request.text;

    // Phase 1: Validation
    if from.is_empty() {
        return SendOutcome::Error {
            error: "send requires non-empty 'from' (sender identity)".into(),
            code: None,
            hint: None,
        };
    }
    if let Err(e) = agent::validate_name(target) {
        return SendOutcome::Error {
            error: e,
            code: None,
            hint: None,
        };
    }
    let from_resolved = crate::agent::resolve_instance(home, from).ok();
    let target_resolved = crate::agent::resolve_instance(home, target).ok();
    if let (Some((ref fid, _)), Some((ref tid, _))) = (&from_resolved, &target_resolved) {
        if fid == tid {
            return SendOutcome::Error {
                error: "cannot send to self".into(),
                code: None,
                hint: None,
            };
        }
    } else if from == target {
        return SendOutcome::Error {
            error: "cannot send to self".into(),
            code: None,
            hint: None,
        };
    }
    {
        let reg = agent::lock_registry(registry);
        let in_registry = target_resolved
            .as_ref()
            .is_some_and(|(id, _)| reg.contains_key(id));
        drop(reg);
        if !in_registry && target_resolved.is_none() {
            let msg = match crate::teams::find_team_for(home, target) {
                Some(team) => format!(
                    "target '{target}' is registered as a member of team '{team_name}' \
                     but no running instance exists. Either respawn via \
                     `create_instance(name={target}, ...)` or clean stale \
                     membership via `team(action=update, name={team_name}, remove={target})`.",
                    team_name = team.name,
                ),
                None => format!("target '{target}' not found in registry or fleet.yaml"),
            };
            return SendOutcome::Error {
                error: msg,
                code: None,
                hint: None,
            };
        }
    }

    // Phase 2: Policy gates
    let cross_team_bypassed = match check_team_isolation(home, from, target) {
        Ok(()) => false,
        Err(e) => {
            if !is_assignment_backed_code_review(home, &request, from, target) {
                if let SendOutcome::Error { ref error, .. } = e {
                    crate::event_log::log(home, "send_cross_team_blocked", from, error);
                }
                return e;
            }
            true
        }
    };
    if let Err(e) = check_quota_gate(registry, home, target, request.kind.as_deref()) {
        return e;
    }

    // Phase 3: Message construction
    let mut msg = build_message(home, &request, &from_resolved, &None);
    let server_message_id = crate::inbox::stamp_message_id(&mut msg);

    let report_auth = {
        let params = request_to_authorize_params(&request);
        match crate::review_receipt::authorize_report(
            home,
            &params,
            from,
            from_resolved.as_ref().map(|(id, _)| *id),
            target,
            text,
            &server_message_id,
        ) {
            Ok(auth) => auth,
            Err(error) => {
                return SendOutcome::Error {
                    error,
                    code: Some("report_authority_rejected".into()),
                    hint: None,
                };
            }
        }
    };
    msg.report_purpose = report_auth.purpose;
    msg.validated_code_review = report_auth.receipt;
    if let Some(receipt) = msg.validated_code_review.as_ref() {
        msg.reviewed_head = Some(receipt.summary().reviewed_head.clone());
    }
    if cross_team_bypassed && msg.validated_code_review.is_some() {
        crate::event_log::log(
            home,
            "send_cross_team_allowed_assignment",
            from,
            &format!("target={target}"),
        );
    }
    if let Err(e) = check_worktree_enforcement(home, target, &request) {
        return e;
    }

    let auto_task_id = match auto_create_task_if_needed(home, &request) {
        Ok(id) => id,
        Err(e) => return e,
    };
    if let Some(ref tid) = auto_task_id {
        msg.task_id = Some(tid.clone());
        if request.kind.as_deref() == Some("task") {
            msg.correlation_id = Some(tid.clone());
        }
    }

    // Phase 4: Delivery routing
    let delivery_mode = match route_and_deliver(home, registry, from, target, &request, msg.clone())
    {
        Ok(m) => m,
        Err(e) => {
            return SendOutcome::Error {
                error: format!("send failed: message not delivered to '{target}': {e}"),
                code: None,
                hint: None,
            };
        }
    };

    // Phase 5: Post-delivery side effects
    crate::inbox::settle_parent_after_successful_send(home, from, msg.parent_id.as_deref());
    inject_provenance(&request, from, target);
    let branch_checked_out = checkout_branch_if_requested(home, target, request.branch.as_deref());
    let receipt_applied = process_verdicts(home, &msg);
    if receipt_applied || msg.validated_code_review.is_none() {
        track_dispatch(home, &request, from, target, &msg);
    } else {
        let mut inert = msg.clone();
        inert.validated_code_review = None;
        track_dispatch(home, &request, from, target, &inert);
    }

    SendOutcome::Success {
        delivery_mode,
        branch_checked_out: branch_checked_out.map(String::from),
        auto_task_id,
    }
}

// ── Helpers ────────────────────────────────────────────────────────────

fn request_to_authorize_params(req: &SendRequest) -> serde_json::Value {
    let mut p = serde_json::json!({});
    if let Some(ref k) = req.kind {
        p["kind"] = serde_json::json!(k);
    }
    if let Some(ref rp) = req.report_purpose {
        p["report_purpose"] = serde_json::json!(rp);
    }
    if let Some(ref cr) = req.code_review {
        p["code_review"] = cr.clone();
    }
    if let Some(ref rh) = req.reviewed_head {
        p["reviewed_head"] = serde_json::json!(rh);
    }
    if let Some(ref cid) = req.correlation_id {
        p["correlation_id"] = serde_json::json!(cid);
    }
    p
}

fn check_team_isolation(home: &Path, from: &str, target: &str) -> Result<(), SendOutcome> {
    let is_general_bus = from == "general" || target == "general";
    let from_team = crate::teams::find_team_for(home, from);
    let target_team = crate::teams::find_team_for(home, target);
    let same_team = match (&from_team, &target_team) {
        (Some(a), Some(b)) => a.name == b.name,
        (None, None) => true,
        _ => false,
    };
    if !is_general_bus && !same_team {
        let allowed_by_allowlist = target_team.as_ref().is_some_and(|t| {
            t.accept_from.contains(&from.to_string()) && t.orchestrator.as_deref() == Some(target)
        });
        if allowed_by_allowlist {
            crate::event_log::log(
                home,
                "send_cross_team_allowed_allowlist",
                from,
                &format!(
                    "target={target}, target_team={:?}",
                    target_team.as_ref().map(|t| &t.name),
                ),
            );
        } else {
            return Err(SendOutcome::Error {
                error: format!(
                    "cross-team send blocked: '{from}' (team={:?}) → '{target}' (team={:?}). \
                     Route via general, add sender to team's accept_from, or use create_instance(team=...) to grow your team.",
                    from_team.as_ref().map(|t| &t.name),
                    target_team.as_ref().map(|t| &t.name),
                ),
                code: None,
                hint: None,
            });
        }
    }
    if is_general_bus && !same_team {
        crate::event_log::log(
            home,
            "send_cross_team_allowed_general",
            from,
            &format!("target={target}"),
        );
    }
    Ok(())
}

/// #2957: lightweight pre-check for a typed code_review report whose active
/// assignment authorises the cross-team return path. Fail-closed: any parse,
/// lookup, or identity mismatch returns `false` and the generic gate fires.
fn is_assignment_backed_code_review(
    home: &Path,
    request: &SendRequest,
    from: &str,
    target: &str,
) -> bool {
    if request.kind.as_deref() != Some("report")
        || request.report_purpose.as_deref() != Some("code_review")
    {
        return false;
    }
    let cr = match &request.code_review {
        Some(v) => v,
        None => return false,
    };
    let id = match cr.get("assignment_id").and_then(|v| v.as_str()) {
        Some(s) => match uuid::Uuid::parse_str(s) {
            Ok(id) => id,
            Err(_) => return false,
        },
        None => return false,
    };
    let Ok(assignment) =
        crate::daemon::assignment_authority::lookup_by_assignment_id_strict(home, id)
    else {
        return false;
    };
    assignment.target == from && assignment.sender == target
}

fn check_quota_gate(
    registry: &AgentRegistry,
    home: &Path,
    target: &str,
    kind: Option<&str>,
) -> Result<(), SendOutcome> {
    if kind != Some("task") {
        return Ok(());
    }
    let blocked = {
        let reg = agent::lock_registry(registry);
        crate::fleet::resolve_uuid(home, target)
            .and_then(|id| reg.get(&id))
            .map(|h| {
                let core = h.core.lock();
                matches!(
                    core.health.current_reason,
                    Some(crate::health::BlockedReason::QuotaExceeded)
                )
            })
    };
    if blocked == Some(true) {
        return Err(SendOutcome::Error {
            error: "target backend quota exceeded".into(),
            code: Some("quota_exceeded".into()),
            hint: None,
        });
    }
    Ok(())
}

fn auto_create_task_if_needed(
    home: &Path,
    req: &SendRequest,
) -> Result<Option<String>, SendOutcome> {
    if req.kind.as_deref() != Some("task")
        || req.task_id.as_ref().filter(|s| !s.is_empty()).is_some()
    {
        return Ok(None);
    }
    let title = req
        .text
        .lines()
        .next()
        .unwrap_or(&req.text)
        .chars()
        .take(80)
        .collect::<String>();
    use std::sync::atomic::{AtomicU64, Ordering};
    static AUTO_TASK_SEQ: AtomicU64 = AtomicU64::new(0);
    let ts = chrono::Utc::now().format("%Y%m%d%H%M%S%6f");
    let seq = AUTO_TASK_SEQ.fetch_add(1, Ordering::Relaxed);
    let id = format!("t-{ts}-{}-{seq}", std::process::id());
    let event = crate::task_events::TaskEvent::Created {
        task_id: crate::task_events::TaskId(id.clone()),
        title,
        description: String::new(),
        priority: req.priority.as_deref().unwrap_or("normal").to_string(),
        owner: Some(crate::task_events::InstanceName(req.target.clone())),
        due_at: None,
        depends_on: Vec::new(),
        routed_to: None,
        branch: req.branch.clone(),
        bind: None,
        eta_secs: None,
        tags: Vec::new(),
        parent_id: None,
    };
    match crate::task_events::append(
        home,
        &crate::task_events::InstanceName(req.from.clone()),
        event,
    ) {
        Ok(_) => Ok(Some(id)),
        Err(e) => Err(SendOutcome::Error {
            error: format!("auto-create task failed: {e}"),
            code: Some("task_create_failed".into()),
            hint: None,
        }),
    }
}

fn build_message(
    home: &Path,
    req: &SendRequest,
    from_resolved: &Option<(crate::types::InstanceId, String)>,
    auto_task_id: &Option<String>,
) -> crate::inbox::InboxMessage {
    let mut thread_id = req.thread_id.clone();
    let parent_id = req.parent_id.clone();
    if thread_id.is_none() {
        if let Some(ref pid) = parent_id {
            if let Some(parent_msg) = crate::inbox::find_message(home, pid) {
                thread_id = parent_msg.thread_id.or_else(|| parent_msg.id.clone());
            }
        }
    }
    let task_id = req.task_id.clone().or_else(|| auto_task_id.clone());
    let correlation_id = if req.kind.as_deref() == Some("task") {
        task_id.clone()
    } else {
        req.correlation_id.clone()
    };
    crate::inbox::InboxMessage {
        schema_version: 0,
        id: None,
        read_at: None,
        delivering_at: None,
        thread_id,
        parent_id,
        task_id,
        force_meta: req
            .force_meta
            .as_ref()
            .and_then(|v| serde_json::from_value::<crate::inbox::ForceMeta>(v.clone()).ok()),
        correlation_id,
        reviewed_head: req.reviewed_head.clone(),
        report_purpose: crate::review_receipt::ReportPurpose::LegacyUntyped,
        validated_code_review: None,
        from: format!("from:{}", req.from),
        from_id: from_resolved.as_ref().map(|(id, _)| id.full()),
        text: req.text.clone(),
        kind: req.kind.clone(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        channel: None,
        delivery_mode: None,
        attachments: vec![],
        in_reply_to_msg_id: None,
        in_reply_to_excerpt: None,
        reply_target: None,
        superseded_by: None,
        broadcast_context: req.broadcast_context.clone(),
        eta_minutes: req.eta_minutes.map(|v| v as u32),
        reporting_cadence: req.reporting_cadence.clone(),
        worktree_binding_required: req.worktree_binding_required,
        pr_number: None,
        terminal: req.terminal,
        ci_handoff_episode: None,
        ci_handoff_class: None,
        ci_handoff_settlement: None,
        delivery_nonce: req.delivery_nonce.clone(),
        review_assignment: None,
    }
}

fn check_worktree_enforcement(
    home: &Path,
    target: &str,
    req: &SendRequest,
) -> Result<(), SendOutcome> {
    if req.kind.as_deref() != Some("task") || req.worktree_binding_required != Some(true) {
        return Ok(());
    }
    let mode = std::env::var("AGEND_WORKTREE_ENFORCEMENT").unwrap_or_else(|_| "warn".to_string());
    if mode != "off" && !crate::binding::is_agent_in_managed_worktree(home, target) {
        if mode == "enforce" {
            return Err(SendOutcome::Error {
                error: "agent not bound in daemon-managed worktree".into(),
                code: Some("worktree_not_managed".into()),
                hint: Some("call bind_self first".into()),
            });
        }
        tracing::warn!(
            target,
            "worktree marker check: agent not in managed worktree (warn mode, allowing)"
        );
    }
    Ok(())
}

fn route_and_deliver(
    home: &Path,
    registry: &AgentRegistry,
    from: &str,
    target: &str,
    req: &SendRequest,
    msg: crate::inbox::InboxMessage,
) -> anyhow::Result<String> {
    let reg = agent::lock_registry(registry);
    let target_id = crate::fleet::resolve_uuid(home, target);
    let target_in_registry = target_id.is_some_and(|id| reg.contains_key(&id));
    let is_codex = target_id
        .and_then(|id| reg.get(&id))
        .map(|h| h.backend_command == "codex")
        .unwrap_or(false);
    drop(reg);
    let kind = req.kind.as_deref().unwrap_or("");
    let is_cross_team = {
        let st = crate::teams::find_team_for(home, from);
        let tt = crate::teams::find_team_for(home, target);
        match (st, tt) {
            (Some(s), Some(t)) => s.name != t.name,
            _ => true,
        }
    };
    let is_orchestrator = crate::teams::find_team_for(home, target)
        .and_then(|t| t.orchestrator)
        .is_some_and(|orch| orch == target);
    let is_reply_to_drained_blocker = matches!(kind, "update" | "report")
        && req
            .correlation_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .is_some_and(|corr| {
                crate::inbox::has_drained_blocker_for_correlation(home, target, corr)
            });
    let skip_inject = is_codex
        && matches!(kind, "update" | "report")
        && !is_cross_team
        && !is_orchestrator
        && !is_reply_to_drained_blocker;

    if !target_in_registry {
        crate::inbox::enqueue(home, target, msg)?;
        Ok("inbox_only".into())
    } else if skip_inject {
        crate::inbox::enqueue(home, target, msg)?;
        crate::event_log::log(
            home,
            "ack_absorbed",
            target,
            &format!("from={from} kind={kind}"),
        );
        Ok("inbox_only".into())
    } else {
        crate::inbox::enqueue_with_idle_hint(home, target, msg)?;
        Ok("pty".into())
    }
}

fn inject_provenance(req: &SendRequest, from: &str, target: &str) {
    let Some(prov) = req.provenance.as_ref().and_then(|v| v.as_object()) else {
        return;
    };
    let prov_from = prov
        .get("from")
        .and_then(|v| v.as_str())
        .unwrap_or(from)
        .to_string();
    let prov_task = prov
        .get("task")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if let Some(ch) = crate::channel::channel_for_instance(target) {
        if let Err(e) = ch.send_from_agent(
            target,
            crate::channel::AgentOutboundOp::InjectProvenance {
                from: prov_from,
                task: prov_task,
            },
        ) {
            tracing::warn!(%e, target, from, "provenance injection failed");
        }
    } else {
        tracing::warn!(
            target,
            from,
            "provenance injection failed — no active channel"
        );
    }
}

fn checkout_branch_if_requested<'a>(
    home: &Path,
    target: &str,
    branch: Option<&'a str>,
) -> Option<&'a str> {
    let branch = branch.filter(|b| !b.is_empty())?;
    let fleet_path = crate::fleet::fleet_yaml_path(home);
    let config = crate::fleet::FleetConfig::load(&fleet_path).ok()?;
    let resolved = config.resolve_instance(target)?;
    let wd = resolved.working_directory.as_ref()?;
    if !crate::worktree::is_git_repo(wd) {
        return None;
    }
    if wd.starts_with(crate::paths::workspace_dir(home)) {
        return None;
    }
    match crate::worktree::checkout_branch(wd, branch) {
        Ok(()) => Some(branch),
        Err(e) => {
            tracing::warn!(target_name = target, branch, error = %e, "task.branch checkout failed");
            None
        }
    }
}

pub(crate) fn process_verdicts(home: &Path, msg: &crate::inbox::InboxMessage) -> bool {
    let Some(receipt) = msg.validated_code_review.as_ref() else {
        return false;
    };
    let summary = receipt.summary();
    if !crate::daemon::pr_state::record_validated_receipt(home, receipt) {
        return false;
    }
    if crate::daemon::auto_release::is_verdict_message(msg) {
        let task_id = &summary.task_id;
        let intent = crate::daemon::auto_release::AutoReleaseIntent {
            task_id: task_id.clone(),
            reviewer: summary.reviewer_name.clone(),
            verdict_msg_id: msg.id.clone(),
            reviewed_head: Some(summary.reviewed_head.clone()),
            enqueued_at: chrono::Utc::now().to_rfc3339(),
            event_kind: Some("verdict".to_string()),
            repo: None,
            branch: None,
            lease: None,
        };
        if let Err(e) = crate::daemon::auto_release::enqueue_intent(home, &intent) {
            tracing::warn!(task_id = %task_id, error = %e, "#870 auto_release: enqueue failed");
        }
    }
    true
}

pub(crate) fn track_dispatch(
    home: &Path,
    req: &SendRequest,
    from: &str,
    target: &str,
    msg: &crate::inbox::InboxMessage,
) {
    let kind_str = msg.kind.as_deref().unwrap_or("");
    if matches!(kind_str, "task" | "query") {
        let outbound_corr = if kind_str == "task" {
            msg.task_id.as_deref()
        } else {
            msg.correlation_id.as_deref().or(msg.task_id.as_deref())
        };
        let explicit_threshold = req.expect_reply_within_secs;
        let threshold = if req.no_report_expected == Some(true) {
            None
        } else {
            crate::daemon::dispatch_idle::team_nudge::resolve_threshold_for_dispatch(
                home,
                from,
                explicit_threshold,
            )
        };
        if let Some(threshold) = threshold {
            let outbound_corr = outbound_corr.map(String::from);
            let recorded = crate::daemon::dispatch_idle::record_dispatch(
                home,
                from,
                target,
                outbound_corr.as_deref(),
                kind_str,
                threshold,
            );
            if kind_str == "task" && recorded.is_none() {
                tracing::warn!(
                    from = %from,
                    target = %target,
                    "dispatch_idle record_dispatch failed (sidecar not written) — this dispatch will get NO idle-timeout nudge"
                );
            }
        }
        if let (Some(branch), Some(corr)) = (
            req.branch.as_deref().filter(|b| !b.is_empty()),
            outbound_corr,
        ) {
            let _ = crate::tasks::link_branch_to_task(home, corr, branch);
        }
        if kind_str == "task" {
            let _ = crate::daemon::ci_handoff_track::resolve_delegated(
                home,
                from,
                outbound_corr,
                req.branch.as_deref(),
            );
            let status = if req.no_report_expected == Some(true) {
                "no_report_expected"
            } else {
                "pending"
            };
            crate::dispatch_tracking::track_dispatch(
                home,
                crate::dispatch_tracking::DispatchEntry {
                    task_id: msg.task_id.clone(),
                    from: from.to_string(),
                    to: target.to_string(),
                    from_id: crate::agent::resolve_instance(home, from)
                        .ok()
                        .map(|(id, _)| id.full()),
                    to_id: crate::agent::resolve_instance(home, target)
                        .ok()
                        .map(|(id, _)| id.full()),
                    delegated_at: chrono::Utc::now().to_rfc3339(),
                    status: status.to_string(),
                },
            );
        }
    } else if kind_str == "report" {
        if let Some(corr) = msg.correlation_id.as_deref().or(msg.task_id.as_deref()) {
            let _ = crate::daemon::dispatch_idle::mark_resolved(home, corr, from);
            if msg.validated_code_review.is_some() {
                let _ = crate::daemon::ci_handoff_track::resolve_by_correlation(
                    home,
                    corr,
                    "validated_code_review_arrived",
                );
            }
            if corr.starts_with("t-") {
                let _ = crate::tasks::auto_close::auto_close_on_report(
                    home,
                    kind_str,
                    corr,
                    from,
                    &msg.text,
                    msg.terminal.unwrap_or(false),
                );
            }
        }
        bridge_verdict_to_review_task(home, from, msg);
    } else if matches!(kind_str, "update" | "query") {
        if let Some(corr) = msg.correlation_id.as_deref().or(msg.task_id.as_deref()) {
            let _ = crate::daemon::dispatch_idle::refresh_issued_at(home, corr, from);
        }
    }
}

fn bridge_verdict_to_review_task(home: &Path, reporter: &str, msg: &crate::inbox::InboxMessage) {
    let Some(receipt) = msg.validated_code_review.as_ref() else {
        return;
    };
    let summary = receipt.summary();
    let task_id = &summary.task_id;
    let _ = crate::daemon::dispatch_idle::mark_resolved(home, task_id, reporter);
    if matches!(
        summary.verdict,
        crate::review_receipt::ReviewVerdict::Verified
    ) {
        let _ = crate::tasks::auto_close::auto_close_on_report(
            home, "report", task_id, reporter, &msg.text, true,
        );
    }
}
