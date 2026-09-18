//! #3666 redrive pipeline: re-run the normal dispatch phases for one parked
//! intent whose target is now idle. Split from `mod.rs` per the 750-LOC
//! handler ceiling; the scan/staleness/attempt-cap state machine stays there.

use super::ParkedDispatch;
use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

/// The redrive pipeline: pre-checks → branch authority → auto-create →
/// lease / CI-watch → idle-hint enqueue → post-delivery bookkeeping. Mirrors
/// `handle_delegate_task`'s phase order (validate → create → lease → send →
/// track) minus review-assignment markers (never parked) and minus the runtime
/// transport (tick context has no registry — the inbox row + idle hint IS the
/// delivery, exactly as `execute_send`'s registry path leaves it for the
/// delivery worker).
pub(super) fn deliver_parked(home: &Path, park: &ParkedDispatch) -> Result<(), String> {
    let dispatcher = Sender::new(&park.dispatcher)
        .ok_or_else(|| format!("dispatcher '{}' is not a valid sender", park.dispatcher))?;
    let target = park.target.as_str();
    let mut args = park.args.clone();
    // Re-run the pre-send gates against CURRENT state: a redrive that still
    // conflicts (busy again, branch dedup, test-name gate) is a redrive
    // failure, not a delivery — the caller re-parks it.
    let checks = super::super::dispatch::run_dispatch_pre_checks(
        home,
        &dispatcher,
        &args,
        target,
        &park.task_text,
    )
    .map_err(|rejection| {
        rejection["error"]
            .as_str()
            .or(rejection["suggestion"].as_str())
            .unwrap_or("dispatch pre-checks still reject")
            .to_string()
    })?;
    if checks.review_assignment {
        return Err("review-assignment markers are never parked".to_string());
    }
    // Branch authority (same typed preflight as the live path; branchless
    // dispatches skip it identically). Owned early: `args` is mutated below
    // (task_id fill) and must not stay borrowed across it.
    let branch: Option<String> = args["branch"]
        .as_str()
        .filter(|b| !b.is_empty())
        .map(String::from);
    let branch_ref = branch.as_deref();
    let mut review_class_token: Option<&str> = None;
    let mut resolved_class: Option<crate::daemon::pr_state::ReviewClass> = None;
    if branch.is_some() {
        let task_id = args["task_id"].as_str().unwrap_or("");
        let class = crate::tasks::governance::resolve_dispatch_authority(
            home,
            task_id,
            &args,
            checks.second_reviewer,
        )
        .map_err(|refusal| {
            refusal["error"]
                .as_str()
                .unwrap_or("branch authority unresolved")
                .to_string()
        })?;
        review_class_token = Some(class.as_token());
        resolved_class = Some(class);
    }
    // Board task: reuse the parked id, or auto-create (same field shape as the
    // live `maybe_auto_create_task` — keep in sync with it).
    let mut effective_task_id = park.task_id.clone();
    if effective_task_id.is_none() {
        let created = auto_create_redrive_task(home, &args, &dispatcher, target, &checks)?;
        if branch.is_some() && created.review_class.is_none() {
            return Err("auto-created branch task has no durable review_class".to_string());
        }
        if resolved_class.is_none() {
            resolved_class = created.review_class;
            review_class_token = None;
        }
        effective_task_id = Some(created.task_id);
    }
    let task_id_str = effective_task_id.as_deref().unwrap_or("");
    if !task_id_str.is_empty() {
        args["task_id"] = json!(task_id_str);
    }
    // Lease / CI-watch for branch dispatches (bind:false arms the watch
    // without binding — same fork as the live path).
    if let (Some(branch), Some(class)) = (branch_ref, resolved_class) {
        let next_after_ci =
            crate::daemon::ci_watch::watch_state::normalize_next_after_ci(&args["next_after_ci"]);
        if args["bind"].as_bool() == Some(false) {
            let mut watch_args = args.clone();
            watch_args["task_id"] = json!(task_id_str);
            watch_args["review_class"] = json!(class.as_token());
            let watch_result = super::super::super::ci::handle_watch_ci(home, &watch_args, target);
            if watch_result["error"].is_string() {
                return Err(watch_result["error"]
                    .as_str()
                    .unwrap_or("ci watch arm failed")
                    .to_string());
            }
            tracing::info!(
                %target,
                %branch,
                "busy-park redrive armed typed CI watch without binding (bind:false)"
            );
        } else {
            let expected_head = args["expected_head"].as_str();
            super::super::super::dispatch_hook::dispatch_auto_bind_lease_with_source_and_chain(
                home,
                target,
                task_id_str,
                branch,
                args["repository"].as_str(),
                None,
                expected_head,
                &next_after_ci,
                review_class_token,
                true,
            )
            .map_err(|e| format!("dispatch rejected: {e}"))?;
        }
    }
    // Delivery: the composed body + task-id suffix (same suffix the live path
    // appends post-create), enqueued WITH the idle hint so an idle target is
    // woken — the wake-up path `inbox_only` lacks.
    let mut text = park.msg_text.clone();
    if !task_id_str.is_empty() {
        text.push_str(&format!(" (task id: {task_id_str})"));
    }
    let mut msg = crate::inbox::InboxMessage {
        from: format!("from:{}", dispatcher.as_str()),
        from_id: crate::agent::resolve_instance(home, dispatcher.as_str())
            .ok()
            .map(|(id, _)| id.full()),
        text,
        kind: Some("task".to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        delivery_mode: Some("transport_queued_unverified".to_string()),
        task_id: effective_task_id.clone(),
        correlation_id: effective_task_id.clone(),
        thread_id: args["thread_id"].as_str().map(String::from),
        parent_id: args["parent_id"].as_str().map(String::from),
        terminal: args["terminal"].as_bool(),
        eta_minutes: args["eta_minutes"].as_u64().map(|v| v as u32),
        reporting_cadence: args["reporting_cadence"].as_str().map(String::from),
        worktree_binding_required: args["worktree_binding_required"].as_bool(),
        delivery_nonce: args["delivery_nonce"].as_str().map(String::from),
        ..Default::default()
    };
    crate::inbox::stamp_message_id(&mut msg);
    crate::inbox::enqueue_with_idle_hint(home, target, msg)
        .map_err(|e| format!("redrive enqueue failed: {e}"))?;
    // Post-delivery bookkeeping (mirrors `track_dispatch`'s task-kind branch
    // minus the dispatch_tracking insert — the park-time entry already covers
    // it, and a second insert would stack a duplicate row).
    if let Some(branch) = branch_ref {
        if let Some(tid) = effective_task_id.as_deref() {
            let _ = crate::tasks::link_branch_to_task(home, tid, branch);
        }
    }
    let _ = crate::daemon::ci_handoff_track::resolve_delegated(
        home,
        dispatcher.as_str(),
        effective_task_id.as_deref(),
        branch_ref,
    );
    // Arm the wait-for-reply watchdog FROM DELIVERY (see module doc). Explicit
    // `expect_reply_within_secs` wins; `no_report_expected` opts out — same
    // resolution as the live `track_dispatch`.
    if args["no_report_expected"].as_bool() != Some(true) {
        let threshold = crate::daemon::dispatch_idle::team_nudge::resolve_threshold_for_dispatch(
            home,
            dispatcher.as_str(),
            args["expect_reply_within_secs"].as_i64(),
        );
        if let Some(threshold) = threshold {
            if crate::daemon::dispatch_idle::record_dispatch(
                home,
                dispatcher.as_str(),
                target,
                effective_task_id.as_deref(),
                "task",
                threshold,
            )
            .is_none()
            {
                tracing::warn!(
                    target,
                    park_id = %park.park_id,
                    "busy-park redrive delivered but the dispatch_idle sidecar was not written"
                );
            }
        }
    }
    tracing::info!(
        %target,
        dispatcher = dispatcher.as_str(),
        park_id = %park.park_id,
        task_id = ?effective_task_id,
        "busy-parked dispatch redelivered on idle transition"
    );
    Ok(())
}

struct RedriveCreatedTask {
    task_id: String,
    review_class: Option<crate::daemon::pr_state::ReviewClass>,
}

/// Auto-create mirror for task-less parked intents. Field shape matches
/// `comms_delegate::maybe_auto_create_task` — keep the two in sync (title
/// truncation, assignee/branch/project/priority/plan-ack/review-class/
/// governing-decision).
fn auto_create_redrive_task(
    home: &Path,
    args: &Value,
    dispatcher: &Sender,
    target: &str,
    checks: &super::super::dispatch::DispatchPreChecks,
) -> Result<RedriveCreatedTask, String> {
    let auto_title = args["message"]
        .as_str()
        .or_else(|| args["task"].as_str())
        .unwrap_or("(untitled dispatch)")
        .chars()
        .take(80)
        .collect::<String>();
    let target_project = crate::tasks::resolve_target_project(home, target);
    let create_args = json!({
        "action": "create",
        "title": auto_title,
        "assignee": target,
        "branch": args["branch"].as_str(),
        "priority": "normal",
        "project": target_project,
        "plan_ack_required": checks.plan_ack_required,
        "plan_ack_reason": args["plan_ack_reason"].as_str(),
        "review_class": args["review_class"].as_str(),
        "governing_decision_id": args["governing_decision_id"].as_str(),
    });
    let task_result = crate::tasks::handle(home, dispatcher.as_str(), &create_args);
    if task_result["error"].is_string() {
        return Err(task_result["error"]
            .as_str()
            .unwrap_or("auto-create task failed")
            .to_string());
    }
    let Some(id) = task_result["id"].as_str() else {
        return Err("auto-created task did not return a durable id".to_string());
    };
    let created_review_class = task_result["task"]["metadata"]["review_class"]
        .as_str()
        .and_then(
            |raw| match crate::daemon::pr_state::ReviewClass::parse_fail_closed(Some(raw)) {
                crate::daemon::pr_state::ReviewClass::Single => {
                    Some(crate::daemon::pr_state::ReviewClass::Single)
                }
                crate::daemon::pr_state::ReviewClass::Dual => {
                    Some(crate::daemon::pr_state::ReviewClass::Dual)
                }
                crate::daemon::pr_state::ReviewClass::Unresolved => None,
            },
        );
    crate::daemon::task_progress::touch(
        home,
        id,
        crate::daemon::task_progress::ProgressSource::Broadcast,
    );
    Ok(RedriveCreatedTask {
        task_id: id.to_string(),
        review_class: created_review_class,
    })
}
