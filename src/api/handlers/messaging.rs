//! Messaging handler: SEND.
//!
//! #1372: `handle_send` orchestrates 5 phases, each extracted into a
//! helper function to keep the orchestrator under 100 lines and
//! nesting ≤ 4 levels.

use super::HandlerCtx;
use crate::agent;
use serde_json::{json, Value};
use std::path::Path;

// ── Phase 1: Validation ─────────────────────────────────────────────

struct ValidatedSend<'a> {
    from: &'a str,
    target: &'a str,
    text: &'a str,
    from_resolved: Option<(crate::types::InstanceId, String)>,
}

fn validate_sender_and_target<'a>(
    params: &'a Value,
    ctx: &HandlerCtx,
) -> Result<ValidatedSend<'a>, Value> {
    let from = params["from"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(
            || json!({"ok": false, "error": "send requires non-empty 'from' (sender identity)"}),
        )?;
    let target = params["target"].as_str().unwrap_or("");
    let text = params["text"].as_str().unwrap_or("");
    if let Err(e) = agent::validate_name(target) {
        return Err(json!({"ok": false, "error": e}));
    }

    let from_resolved = crate::agent::resolve_instance(ctx.home, from).ok();
    let target_resolved = crate::agent::resolve_instance(ctx.home, target).ok();
    if let (Some((ref fid, _)), Some((ref tid, _))) = (&from_resolved, &target_resolved) {
        if fid == tid {
            return Err(json!({"ok": false, "error": "cannot send to self"}));
        }
    } else if from == target {
        return Err(json!({"ok": false, "error": "cannot send to self"}));
    }

    let reg = agent::lock_registry(ctx.registry);
    // #1441: registry is UUID-keyed; reuse the already-resolved target id.
    let in_registry = target_resolved
        .as_ref()
        .is_some_and(|(id, _)| reg.contains_key(id));
    drop(reg);
    if !in_registry && target_resolved.is_none() {
        let msg = match crate::teams::find_team_for(ctx.home, target) {
            Some(team) => format!(
                "target '{target}' is registered as a member of team '{team_name}' \
                 but no running instance exists. Either respawn via \
                 `create_instance(name={target}, ...)` or clean stale \
                 membership via `team(action=update, name={team_name}, remove={target})`.",
                team_name = team.name,
            ),
            None => format!("target '{target}' not found in registry or fleet.yaml"),
        };
        return Err(json!({"ok": false, "error": msg}));
    }
    Ok(ValidatedSend {
        from,
        target,
        text,
        from_resolved,
    })
}

// ── Phase 2: Policy gates ───────────────────────────────────────────

fn check_team_isolation(home: &Path, from: &str, target: &str) -> Result<(), Value> {
    let is_general_bus = from == "general" || target == "general";
    let from_team = crate::teams::find_team_for(home, from);
    let target_team = crate::teams::find_team_for(home, target);
    let same_team = match (&from_team, &target_team) {
        (Some(a), Some(b)) => a.name == b.name,
        (None, None) => true,
        _ => false,
    };
    if !is_general_bus && !same_team {
        // Rule 4: accept_from allowlist — sender in target team's accept_from
        // AND target is the team's orchestrator.
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
            crate::event_log::log(
                home,
                "send_cross_team_blocked",
                from,
                &format!(
                    "target={target}, sender_team={:?}, target_team={:?}",
                    from_team.as_ref().map(|t| &t.name),
                    target_team.as_ref().map(|t| &t.name),
                ),
            );
            return Err(json!({
                "ok": false,
                "error": format!(
                    "cross-team send blocked: '{from}' (team={:?}) → '{target}' (team={:?}). \
                     Route via general, add sender to team's accept_from, or use create_instance(team=...) to grow your team.",
                    from_team.as_ref().map(|t| &t.name),
                    target_team.as_ref().map(|t| &t.name),
                )
            }));
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

fn check_quota_gate(
    registry: &crate::agent::AgentRegistry,
    home: &std::path::Path,
    params: &Value,
    target: &str,
) -> Result<(), Value> {
    if params["kind"].as_str() != Some("task") {
        return Ok(());
    }
    let blocked = {
        let reg = agent::lock_registry(registry);
        // #1441: registry is UUID-keyed; resolve target name via fleet.yaml.
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
        return Err(
            json!({"ok": false, "error": "target backend quota exceeded", "code": "quota_exceeded"}),
        );
    }
    Ok(())
}

fn auto_create_task_if_needed(
    params: &Value,
    home: &Path,
    from: &str,
    target: &str,
    text: &str,
) -> Result<Option<String>, Value> {
    if params["kind"].as_str() != Some("task")
        || params["task_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .is_some()
    {
        return Ok(None);
    }
    let title = text
        .lines()
        .next()
        .unwrap_or(text)
        .chars()
        .take(80)
        .collect::<String>();
    use std::sync::atomic::{AtomicU64, Ordering};
    static AUTO_TASK_SEQ: AtomicU64 = AtomicU64::new(0);
    let ts = chrono::Utc::now().format("%Y%m%d%H%M%S%6f");
    let seq = AUTO_TASK_SEQ.fetch_add(1, Ordering::Relaxed);
    let id = format!("t-{ts}-{}-{seq}", std::process::id()); // AUDIT2-011: pid → process-unique
    let event = crate::task_events::TaskEvent::Created {
        task_id: crate::task_events::TaskId(id.clone()),
        title,
        description: String::new(),
        priority: params["priority"].as_str().unwrap_or("normal").to_string(),
        owner: Some(crate::task_events::InstanceName(target.to_string())),
        due_at: None,
        depends_on: Vec::new(),
        routed_to: None,
        branch: params["branch"].as_str().map(String::from),
        bind: None,
        eta_secs: None,
        tags: Vec::new(),
        parent_id: None,
    };
    match crate::task_events::append(
        home,
        &crate::task_events::InstanceName(from.to_string()),
        event,
    ) {
        Ok(_) => Ok(Some(id)),
        Err(e) => Err(
            json!({"ok": false, "error": format!("auto-create task failed: {e}"), "code": "task_create_failed"}),
        ),
    }
}

// ── Phase 3: Message construction ───────────────────────────────────

fn build_message(
    params: &Value,
    home: &Path,
    vs: &ValidatedSend<'_>,
    auto_task_id: &Option<String>,
) -> crate::inbox::InboxMessage {
    let mut thread_id = params["thread_id"].as_str().map(String::from);
    let parent_id = params["parent_id"].as_str().map(String::from);
    if thread_id.is_none() {
        if let Some(ref pid) = parent_id {
            if let Some(parent_msg) = crate::inbox::find_message(home, pid) {
                thread_id = parent_msg.thread_id.or_else(|| parent_msg.id.clone());
            }
        }
    }
    crate::inbox::InboxMessage {
        schema_version: 0,
        id: None,
        read_at: None,
        delivering_at: None,
        thread_id,
        parent_id,
        task_id: params["task_id"]
            .as_str()
            .map(String::from)
            .or_else(|| auto_task_id.clone()),
        force_meta: params
            .get("force_meta")
            .and_then(|v| serde_json::from_value::<crate::inbox::ForceMeta>(v.clone()).ok()),
        correlation_id: params["correlation_id"].as_str().map(String::from),
        // Display only. A validated review uses the server-derived exact head;
        // non-review reports may retain the caller's display value, but no
        // authorization logic reads it.
        reviewed_head: params["reviewed_head"].as_str().map(String::from),
        report_purpose: crate::review_receipt::ReportPurpose::LegacyUntyped,
        validated_code_review: None,
        from: format!("from:{}", vs.from),
        from_id: vs.from_resolved.as_ref().map(|(id, _)| id.full()),
        text: vs.text.to_string(),
        kind: params["kind"].as_str().map(String::from),
        timestamp: chrono::Utc::now().to_rfc3339(),
        channel: None,
        delivery_mode: None,
        attachments: vec![],
        in_reply_to_msg_id: None,
        in_reply_to_excerpt: None,
        reply_target: None,
        superseded_by: None,
        broadcast_context: params
            .get("broadcast_context")
            .and_then(|v| serde_json::from_value::<crate::inbox::BroadcastContext>(v.clone()).ok()),
        eta_minutes: params["eta_minutes"].as_u64().map(|v| v as u32),
        reporting_cadence: params["reporting_cadence"].as_str().map(String::from),
        worktree_binding_required: params["worktree_binding_required"].as_bool(),
        pr_number: None,
        terminal: params["terminal"].as_bool(),
        delivery_nonce: params["delivery_nonce"].as_str().map(String::from),
        review_assignment: None,
    }
}

fn check_worktree_enforcement(params: &Value, home: &Path, target: &str) -> Result<(), Value> {
    if params["kind"].as_str() != Some("task")
        || params["worktree_binding_required"].as_bool() != Some(true)
    {
        return Ok(());
    }
    let mode = std::env::var("AGEND_WORKTREE_ENFORCEMENT").unwrap_or_else(|_| "warn".to_string());
    if mode != "off" && !crate::binding::is_agent_in_managed_worktree(home, target) {
        if mode == "enforce" {
            return Err(
                json!({"ok": false, "error": "agent not bound in daemon-managed worktree", "hint": "call bind_self first", "code": "worktree_not_managed"}),
            );
        }
        tracing::warn!(
            target,
            "worktree marker check: agent not in managed worktree (warn mode, allowing)"
        );
    }
    Ok(())
}

// ── Phase 4: Delivery routing ───────────────────────────────────────

/// #bughunt2: returns `Err` when the inbox enqueue fails (disk read-only / I/O)
/// so `handle_send` can surface a real failure instead of reporting `ok:true`
/// for a message that was silently dropped — critical for the `inbox_only` /
/// codex `skip_inject` branches where the inbox is the SOLE delivery channel.
fn route_and_deliver(
    ctx: &HandlerCtx,
    params: &Value,
    from: &str,
    target: &str,
    msg: crate::inbox::InboxMessage,
) -> anyhow::Result<&'static str> {
    let reg = agent::lock_registry(ctx.registry);
    // #1441: registry is UUID-keyed; resolve target name via fleet.yaml.
    let target_id = crate::fleet::resolve_uuid(ctx.home, target);
    let target_in_registry = target_id.is_some_and(|id| reg.contains_key(&id));
    let is_codex = target_id
        .and_then(|id| reg.get(&id))
        .map(|h| h.backend_command == "codex")
        .unwrap_or(false);
    drop(reg);
    let kind = params["kind"].as_str().unwrap_or("");
    let is_cross_team = {
        let st = crate::teams::find_team_for(ctx.home, from);
        let tt = crate::teams::find_team_for(ctx.home, target);
        match (st, tt) {
            (Some(s), Some(t)) => s.name != t.name,
            _ => true,
        }
    };
    let is_orchestrator = crate::teams::find_team_for(ctx.home, target)
        .and_then(|t| t.orchestrator)
        .is_some_and(|orch| orch == target);
    let is_reply_to_drained_blocker = matches!(kind, "update" | "report")
        && params["correlation_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .is_some_and(|corr| {
                crate::inbox::has_drained_blocker_for_correlation(ctx.home, target, corr)
            });
    let skip_inject = is_codex
        && matches!(kind, "update" | "report")
        && !is_cross_team
        && !is_orchestrator
        && !is_reply_to_drained_blocker;

    if !target_in_registry {
        crate::inbox::enqueue(ctx.home, target, msg)?;
        Ok("inbox_only")
    } else if skip_inject {
        crate::inbox::enqueue(ctx.home, target, msg)?;
        crate::event_log::log(
            ctx.home,
            "ack_absorbed",
            target,
            &format!("from={from} kind={kind}"),
        );
        Ok("inbox_only")
    } else {
        crate::inbox::enqueue_with_idle_hint(ctx.home, target, msg)?;
        Ok("pty")
    }
}

// ── Phase 5: Post-delivery side effects ─────────────────────────────

fn inject_provenance(params: &Value, from: &str, target: &str) {
    let Some(prov) = params.get("provenance").and_then(|v| v.as_object()) else {
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
    params: &'a Value,
    home: &Path,
    target: &str,
) -> Option<&'a str> {
    let branch = params["branch"].as_str().filter(|b| !b.is_empty())?;
    let fleet_path = crate::fleet::fleet_yaml_path(home);
    let config = crate::fleet::FleetConfig::load(&fleet_path).ok()?;
    let resolved = config.resolve_instance(target)?;
    let wd = resolved.working_directory.as_ref()?;
    if !crate::worktree::is_git_repo(wd) {
        return None;
    }
    // #1834: the Claude backend git-inits the agent's metadata workspace stub
    // (`mcp_config::configure_claude` — "Claude needs a git root to find
    // .claude/"), so `is_git_repo` is TRUE for it — but the stub is NOT a code
    // worktree. The real work happens in the daemon worktree (bound separately
    // under `<home>/worktrees/`, never the working_directory). Checking out the
    // task branch on the stub just runs `switch -c` from its init commit →
    // a stray branch per task (accumulation) + a misleading statusline, with no
    // functional effect. Skip any working_directory under the daemon-managed
    // workspace; a real source/worktree target (operator working_directory
    // outside `<home>/workspace/`) still gets the checkout.
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

fn process_verdicts(home: &Path, msg: &crate::inbox::InboxMessage) -> bool {
    // task66: the validated receipt is the sole code-review authority. Visible
    // text, reviewed_head, names and correlation never select these effects.
    let Some(receipt) = msg.validated_code_review.as_ref() else {
        return false;
    };
    let summary = receipt.summary();
    // Apply/buffer first. A raced revoke, duplicate receipt/source, or storage
    // failure must not release a worktree or bridge/close review work.
    if !crate::daemon::pr_state::record_validated_receipt(home, receipt) {
        return false;
    }
    if crate::daemon::auto_release::is_verdict_message(msg) {
        {
            let task_id = &summary.task_id;
            let intent = crate::daemon::auto_release::AutoReleaseIntent {
                task_id: task_id.clone(),
                reviewer: summary.reviewer_name.clone(),
                verdict_msg_id: msg.id.clone(),
                reviewed_head: Some(summary.reviewed_head.clone()),
                enqueued_at: chrono::Utc::now().to_rfc3339(),
                // t-worktree-leak (PR-1) Q1(b): a verdict no longer releases an
                // OPEN PR's worktree by default — the sweeper gates it through the
                // release invariant, so an IMPLEMENTER's release waits for the
                // terminal (merge/close) or no-PR+task-done event. The #2010 2a
                // reviewer-binding bypass is the sole exception, scoped to the
                // verdict sender's own binding. repo/branch/lease are derived by
                // the sweeper from the live binding.
                event_kind: Some("verdict".to_string()),
                repo: None,
                branch: None,
                lease: None,
            };
            if let Err(e) = crate::daemon::auto_release::enqueue_intent(home, &intent) {
                tracing::warn!(task_id = %task_id, error = %e, "#870 auto_release: enqueue failed");
            }
        }
    }
    true
}

pub(crate) fn track_dispatch(
    home: &Path,
    params: &Value,
    from: &str,
    target: &str,
    msg: &crate::inbox::InboxMessage,
) {
    let kind_str = msg.kind.as_deref().unwrap_or("");
    if matches!(kind_str, "task" | "query") {
        let outbound_corr = msg.correlation_id.as_deref().or(msg.task_id.as_deref());
        let explicit_threshold = params["expect_reply_within_secs"].as_i64();
        // #2099: a fire-and-forget dispatch (`no_report_expected`) records NO
        // dispatch-idle sidecar, so the ~threshold watchdog never nags the
        // dispatcher (`dispatch_idle_threshold_exceeded`) nor [team-watchdog]s
        // the target — this is the SECOND ~30min nag channel (the DispatchEntry
        // sweep already skips the same flag). read-first: `pending_for_instance`
        // (query.rs status) then correctly omits a no-reply dispatch, and
        // `cleanup_pending_for_task_id` no-ops with no sidecar — the nag + that
        // (correct) observability are the ONLY effects.
        let threshold = if params["no_report_expected"].as_bool().unwrap_or(false) {
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
            // #2004: a swallowed record failure means this dispatch never gets
            // its idle-timeout nudge — surface it (non-fatal: the dispatch
            // itself succeeded). This branch handles task AND query, but
            // `record_dispatch` deliberately returns None for every
            // non-"task" kind (queries never get a sidecar — pinned by the
            // kind-contract test in dispatch_idle) — so the warn is gated to
            // kind=task, where the remaining None arms (non-empty names,
            // resolver-positive threshold already hold here) mean disk
            // failure. Warning on a query's designed skip would false-alarm
            // on every ordinary query dispatch (codex P1 on the first
            // #2004 review).
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
        // #1942: link the dispatched branch to the correlated task. The lead
        // dispatches `kind=task` with `branch=`, but the task is often created
        // separately with `branch: None` — so without this the task↔branch link
        // never exists and `auto_close_merged_tasks` can't auto-close on merge
        // (the lead-merges strand). Idempotent + no-op if no branch / no task.
        if let (Some(branch), Some(corr)) = (
            params["branch"].as_str().filter(|b| !b.is_empty()),
            outbound_corr,
        ) {
            let _ = crate::tasks::link_branch_to_task(home, corr, branch);
        }
        // #35896-11 ①: if the DISPATCHER (`from`) holds a ci-ready handoff for this
        // work, delegating it (a kind=TASK dispatch) IS their discharge → resolve
        // their OWN track. Matches the reused task id (our review-dispatch convention
        // reuses the implementer's id — see `resolve_delegated`) or the dispatched
        // branch. Stops the dispatcher-role re-nudge (the #35896-11 acceptance core).
        //
        // #2667 F1 (reviewer4): GATED to kind=="task" only. This chokepoint also
        // handles kind=="query", but a query is NOT a delegation — the design + vet
        // authorize only a task dispatch as the discharge signal. Resolving on a
        // query carrying the same correlation would be a non-explicit, non-delegation
        // false-stop = obligation loss. (Contrast the shared dispatch_idle/link work
        // above, which correctly spans task AND query.)
        if kind_str == "task" {
            let _ = crate::daemon::ci_handoff_track::resolve_delegated(
                home,
                from,
                outbound_corr,
                params["branch"].as_str(),
            );
        }
    } else if kind_str == "report" {
        // #1525: clear the dispatch-idle sidecar with the SAME key the record
        // path uses — `correlation_id.or(task_id)` (see the kind=task branch
        // above). `record_dispatch` keys the sidecar via that fallback, so a
        // report that carries the id only in `task_id` (correlation_id empty)
        // must match by task_id too; otherwise `mark_resolved`'s exact lookup
        // silently no-ops and the completed dispatch's sidecar lingers until it
        // fires a spurious `dispatch_idle_threshold_exceeded` nudge once the
        // target goes Idle (#1516's working-state gate does not cover that).
        if let Some(corr) = msg.correlation_id.as_deref().or(msg.task_id.as_deref()) {
            // #2004: `None` here is NORMAL (a report whose correlation has no
            // pending sidecar — most reports). The real swallowed failure —
            // a matching sidecar whose DELETE fails, leaving it to fire a
            // spurious idle nudge later — warns inside `mark_resolved` at the
            // actual failure point, where the dispatch_id is known.
            let _ = crate::daemon::dispatch_idle::mark_resolved(home, corr);
            // #1888 phase-2: a report carrying the handoff's `repo@branch`
            // correlation (reviewer verdicts do) RESOLVES the ci-handoff track —
            // the re-nudge stops on this signal, not on inbox read. A task-id
            // correlation simply matches no track (no-op).
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
        // #t-127: bridge a reviewer VERDICT back to its review TASK + sidecar. A
        // verdict is keyed on `repo@branch` (pr-state pipeline) OR the task id, but
        // the review task + dispatch sidecar are `t-…`-keyed — so the `corr`-keyed
        // handling above MISSES a `repo@branch` verdict (left review tasks ghosting
        // + the sidecar firing spurious "stuck" nudges after the reviewer replied).
        bridge_verdict_to_review_task(home, from, msg);
    } else if matches!(kind_str, "update" | "query") {
        // #1923 G8: key the dispatch-idle refresh by the SAME correlation as the
        // WRITE side above (~:496 records the pending-dispatch sidecar under
        // `correlation_id.or(task_id)`). A reply carrying only `task_id` (no
        // explicit `correlation_id`) was refreshed under `correlation_id` (None)
        // → its sidecar never got refreshed → the dispatch-idle watchdog fired a
        // FALSE idle nudge despite the reply arriving. Aligning the key is a
        // superset — behaviour is unchanged when `correlation_id` is set.
        if let Some(corr) = msg.correlation_id.as_deref().or(msg.task_id.as_deref()) {
            let _ = crate::daemon::dispatch_idle::refresh_issued_at(home, corr);
        }
    }
}

/// #t-127/task66: bridge a validated code-review receipt back to its exact review
/// task + dispatch sidecar, root-fixing two symptoms with one mechanism:
/// - **ghost review tasks** — VERIFIED verdicts never auto-closed their review
///   task (the `auto_close_on_report` call above is gated on `corr.starts_with("t-")`,
///   but a verdict's `corr` is `repo@branch`), so they piled up unclosed.
/// - **spurious stuck-nudges** — the dispatch sidecar is `t-…`-keyed, so
///   `mark_resolved(repo@branch)` never cleared it and the watchdog kept pinging
///   "stuck 30min" after the reviewer had already replied.
///
/// Resolution comes only from the receipt's assignment-bound `task_id`. Reporter
/// names, correlation strings, visible text, and open-sidecar reverse lookups are
/// deliberately not authority.
///
/// Then: ANY verdict clears the sidecar (the reviewer responded → not stuck);
/// only VERIFIED auto-closes the review task (REJECTED/UNVERIFIED stay open for the
/// re-review cycle). `terminal=true` is synthesized internally for VERIFIED, so the
/// close does NOT depend on the reviewer setting the flag (the root fix). Closing
/// the task is orthogonal to the pr-state merge gate (the scanner aggregates
/// verdicts by `reviewed_head`, independent of task lifecycle).
fn bridge_verdict_to_review_task(home: &Path, reporter: &str, msg: &crate::inbox::InboxMessage) {
    let Some(receipt) = msg.validated_code_review.as_ref() else {
        return;
    };
    let summary = receipt.summary();
    let task_id = &summary.task_id;
    // Any verdict → the reviewer responded → clear the dispatch sidecar (kills the
    // post-response stuck-nudge), regardless of VERIFIED vs REJECTED.
    let _ = crate::daemon::dispatch_idle::mark_resolved(home, task_id);
    // Only VERIFIED closes the review task. terminal=true synthesized internally.
    if matches!(
        summary.verdict,
        crate::review_receipt::ReviewVerdict::Verified
    ) {
        let _ = crate::tasks::auto_close::auto_close_on_report(
            home, "report", task_id, reporter, &msg.text, true,
        );
    }
}

// ── Orchestrator ────────────────────────────────────────────────────

pub(crate) fn handle_send(params: &Value, ctx: &HandlerCtx) -> Value {
    let vs = match validate_sender_and_target(params, ctx) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if let Err(e) = check_team_isolation(ctx.home, vs.from, vs.target) {
        return e;
    }
    if let Err(e) = check_quota_gate(ctx.registry, ctx.home, params, vs.target) {
        return e;
    }
    // Build message with auto_task_id=None so all rejection-capable preflights
    // run before any side effects. The task is created only after authorize_report
    // and check_worktree_enforcement pass.
    let mut msg = build_message(params, ctx.home, &vs, &None);
    let server_message_id = crate::inbox::stamp_message_id(&mut msg);
    let report_auth = match crate::review_receipt::authorize_report(
        ctx.home,
        params,
        vs.from,
        vs.from_resolved.as_ref().map(|(id, _)| *id),
        vs.target,
        vs.text,
        &server_message_id,
    ) {
        Ok(auth) => auth,
        Err(error) => {
            return json!({"ok": false, "error": error, "code": "report_authority_rejected"})
        }
    };
    msg.report_purpose = report_auth.purpose;
    msg.validated_code_review = report_auth.receipt;
    if let Some(receipt) = msg.validated_code_review.as_ref() {
        msg.reviewed_head = Some(receipt.summary().reviewed_head.clone());
    }
    if let Err(e) = check_worktree_enforcement(params, ctx.home, vs.target) {
        return e;
    }
    // All preflights passed — now create the auto-task (side effect).
    let auto_task_id =
        match auto_create_task_if_needed(params, ctx.home, vs.from, vs.target, vs.text) {
            Ok(id) => id,
            Err(e) => return e,
        };
    if let Some(ref tid) = auto_task_id {
        msg.task_id = Some(tid.clone());
    }
    // #bughunt2: a failed enqueue (disk read-only / I/O) must surface as a real
    // failure — never report ok:true for a silently-dropped message. Return
    // BEFORE the provenance/dispatch side-effects so they don't fire for an
    // undelivered message.
    let delivery_mode = match route_and_deliver(ctx, params, vs.from, vs.target, msg.clone()) {
        Ok(m) => m,
        Err(e) => {
            return json!({
                "ok": false,
                "error": format!("send failed: message not delivered to '{}': {e}", vs.target)
            });
        }
    };
    // Answered-parent settlement: a confirmed-successful parented send discharges
    // the SENDER's own parent row so it stops re-nagging via poll-reminder. Runs
    // only past the failed-send early return above; no-ops when parent_id is None.
    crate::inbox::settle_parent_after_successful_send(ctx.home, vs.from, msg.parent_id.as_deref());
    inject_provenance(params, vs.from, vs.target);
    let branch_checked_out = checkout_branch_if_requested(params, ctx.home, vs.target);
    let receipt_applied = process_verdicts(ctx.home, &msg);
    if receipt_applied || msg.validated_code_review.is_none() {
        track_dispatch(ctx.home, params, vs.from, vs.target, &msg);
    } else {
        // Preserve ordinary report/dispatch semantics while suppressing every
        // code-review consumer when the post-delivery assignment recheck raced.
        let mut inert = msg.clone();
        inert.validated_code_review = None;
        track_dispatch(ctx.home, params, vs.from, vs.target, &inert);
    }

    let mut resp = json!({"ok": true, "delivery_mode": delivery_mode});
    if let Some(branch) = branch_checked_out {
        resp["branch_checked_out"] = json!(branch);
    }
    if let Some(ref tid) = auto_task_id {
        resp["task_id"] = json!(tid);
    }
    resp
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
