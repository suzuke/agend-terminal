//! W2.2: `handle_delegate_task` as an ordered phase pipeline.
//!
//! Stages (failure order preserved — a reject before lease never leases;
//! a send failure may still have leased/created a task, same as pre-split):
//!
//! 1. **resolve** — identity, instance/team target, self-dispatch reject
//! 2. **validate** — pre-send gates (`comms_gates::run_dispatch_pre_checks`)
//! 3. **compose** — message body + force_meta
//! 4. **create** — optional auto board task after all rejectable checks
//! 5. **lease** — optional `dispatch_auto_bind_lease` when `branch` set
//! 6. **send** — `execute_send` via neutral typed service (or API bridge fallback)
//! 7. **track** — dispatch_tracking + UX + `auto_created_task_id` on success
//!
//! Loaded as a child of `comms` so `file_size_invariant` keeps `comms.rs` under
//! the handler LOC cap while the choreography stays one ordered function.

use crate::channel::sink_registry::registry as ux_sink_registry;
use crate::channel::ux_event::{FleetEvent, UxEvent};
use crate::daemon::pr_state::ReviewClass;
use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::Path;

use super::super::comms_gates::{self, DispatchPreChecks};
use super::super::dispatch::RuntimeContext;
use super::super::dispatch_hook;
use super::super::{err_needs_identity, is_ok_result, require_instance};

// #6: pub(crate) so ci/review_workspace_tests.rs can drive the validation
// function directly (RED-first test for bind/worktree_binding_required rejection).
pub(crate) mod review_assignment;
#[cfg(test)]
pub(crate) mod review_class;

#[cfg(test)]
pub(crate) use review_class::{
    resolve_dispatch_review_class, resolve_existing_task_review_class, ReviewClassRefusal,
};

/// Sprint 55 P0-C — true when the caller passed `bind: false`.
pub(in crate::mcp::handlers) fn dispatch_should_skip_auto_bind(args: &Value) -> bool {
    args["bind"].as_bool() == Some(false)
}

struct ResolvedDelegate<'a> {
    sender: &'a Sender,
    resolved_target: String,
    task: &'a str,
}

/// Phase 1 — identity, target resolution, self-dispatch reject, require `task`.
fn resolve_delegate<'a>(
    home: &Path,
    args: &'a Value,
    sender: &'a Option<Sender>,
) -> Result<ResolvedDelegate<'a>, Value> {
    let Some(sender) = sender.as_ref() else {
        return Err(err_needs_identity("delegate_task"));
    };
    let raw_target = require_instance(args)?;
    if let Err(e) = crate::agent::validate_name(raw_target) {
        return Err(json!({"error": e}));
    }
    // Sprint 46 P2: resolve target via InstanceId — replaces P1 name-lookup bandaid.
    let resolved_target = match crate::agent::resolve_instance(home, raw_target) {
        Ok((_id, name)) => name,
        Err(crate::agent::ResolveError::NotFound(_)) => {
            match crate::teams::resolve_team_orchestrator(home, raw_target) {
                Ok(Some(orch)) => orch,
                Ok(None) => raw_target.to_string(),
                Err(e) => return Err(json!({"error": e})),
            }
        }
    };
    let target = resolved_target.as_str();
    // M5: reject if team-orchestrator resolution collapsed target to sender.
    if *sender == target && raw_target != target {
        return Err(json!({"error": format!(
            "task target '{}' resolved to sender '{}' (team orchestrator loop) \
             — verify instance name does not collide with a team template name",
            raw_target, sender.as_str()
        )}));
    }
    // CR-2026-06-14 (resource-leak): reject plain self-dispatch BEFORE lease.
    if *sender == target {
        return Err(json!({"error": "cannot delegate task to self — use a different instance"}));
    }
    let task = match args["task"].as_str() {
        Some(t) => t,
        None => return Err(json!({"error": "missing 'task'"})),
    };
    Ok(ResolvedDelegate {
        sender,
        resolved_target,
        task,
    })
}

struct ComposedDelegate {
    msg: String,
    force_meta_json: Option<Value>,
    #[allow(dead_code)]
    second_reviewer: bool,
    plan_ack_required: u64,
}

/// Phase 3 — build inject message + force_meta from pre-check scalars.
fn compose_delegate_message(
    task: &str,
    args: &Value,
    checks: &DispatchPreChecks,
) -> ComposedDelegate {
    let force = checks.force;
    let force_reason = checks.force_reason.as_deref();
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
    ComposedDelegate {
        msg,
        force_meta_json,
        second_reviewer: checks.second_reviewer,
        plan_ack_required: checks.plan_ack_required,
    }
}

/// Validate branch-dispatch authority before any branch-side effect. This is
/// intentionally shared by ordinary and reviewer-assignment dispatches so a
/// `bind: false` arm cannot skip the governing-decision reload.
fn preflight_branch_authority(
    home: &Path,
    args: &Value,
    checks: &DispatchPreChecks,
) -> Result<Option<ReviewClass>, Value> {
    let Some(_branch) = args["branch"].as_str().filter(|branch| !branch.is_empty()) else {
        return Ok(None);
    };
    let task_id = args["task_id"].as_str().unwrap_or("");
    let review_class = crate::tasks::governance::resolve_dispatch_authority(
        home,
        task_id,
        args,
        checks.second_reviewer,
    )?;
    Ok(Some(review_class))
}

/// Phase 5 — optional auto-bind lease (rejectable).
fn maybe_auto_bind_lease(
    home: &Path,
    args: &Value,
    target: &str,
    task_id: Option<&str>,
    review_class: ReviewClass,
) -> Result<Option<dispatch_hook::CiWatchOutcome>, Value> {
    let Some(branch) = args["branch"].as_str() else {
        return Ok(None);
    };
    let task_id_val = task_id.unwrap_or("");

    let next_after_ci =
        crate::daemon::ci_watch::watch_state::normalize_next_after_ci(&args["next_after_ci"]);
    let armed_review_class = review_class.as_token();
    // The governing class was reloaded by `preflight_branch_authority`; only
    // that typed result is allowed to arm the watch.
    dispatch_hook::dispatch_auto_bind_lease_with_source_and_chain(
        home,
        target,
        task_id_val,
        branch,
        args["repository"].as_str(),
        None,
        &next_after_ci,
        Some(armed_review_class),
        true,
    )
    .map(|outcome| outcome.ci_watch)
    .map_err(|e| json!({"ok": false, "error": format!("dispatch rejected: {e}")}))
}

/// Arm the typed CI watch for an explicit `bind: false` dispatch. Binding is
/// intentionally skipped, but the authority/watch side of a branch dispatch
/// remains equivalent and fail-closed.
fn maybe_auto_watch_without_bind(
    home: &Path,
    args: &Value,
    target: &str,
    task_id: Option<&str>,
    review_class: ReviewClass,
) -> Result<Option<dispatch_hook::CiWatchOutcome>, Value> {
    let Some(branch) = args["branch"].as_str() else {
        return Ok(None);
    };
    let mut watch_args = args.clone();
    if let Some(task_id) = task_id {
        watch_args["task_id"] = json!(task_id);
    }
    watch_args["review_class"] = json!(review_class.as_token());
    let watch_result = crate::mcp::handlers::ci::handle_watch_ci(home, &watch_args, target);
    if watch_result["error"].is_string() {
        return Err(watch_result);
    }
    let next_after_ci =
        crate::daemon::ci_watch::watch_state::normalize_next_after_ci(&args["next_after_ci"]);
    tracing::info!(
        %target,
        %branch,
        "bind:false branch dispatch armed typed CI watch without binding"
    );
    Ok(Some(dispatch_hook::CiWatchOutcome {
        armed: watch_result["watching"].as_bool().unwrap_or(false),
        next_after_ci,
    }))
}

/// Phase 4 — optional auto-create board task after rejectable checks.
struct AutoCreatedTask {
    effective_task_id: Option<String>,
    auto_created_task_id: Option<String>,
    review_class: Option<ReviewClass>,
}

fn maybe_auto_create_task(
    home: &Path,
    args: &Value,
    sender: &Sender,
    target: &str,
    plan_ack_required: u64,
) -> Result<AutoCreatedTask, Value> {
    if !args["task_id"].as_str().unwrap_or("").is_empty() || *sender == target {
        return Ok(AutoCreatedTask {
            effective_task_id: args["task_id"].as_str().map(String::from),
            auto_created_task_id: None,
            review_class: None,
        });
    }
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
        "plan_ack_required": plan_ack_required,
        "plan_ack_reason": args["plan_ack_reason"].as_str(),
        // #2745: forward the dispatch's review_class into the auto-created task's
        // metadata so the durable authority survives past this dispatch (the
        // resolver already validated it during branch preflight).
        "review_class": args["review_class"].as_str(),
        "governing_decision_id": args["governing_decision_id"].as_str(),
    });
    let task_result = crate::tasks::handle(home, sender.as_str(), &create_args);
    if task_result["error"].is_string() {
        return Err(task_result);
    }
    let Some(id) = task_result["id"].as_str() else {
        return Err(json!({
            "error": "auto-created task did not return a durable id",
            "code": "task_create_failed",
        }));
    };
    let created_review_class = task_result["task"]["metadata"]["review_class"]
        .as_str()
        .and_then(|raw| match ReviewClass::parse_fail_closed(Some(raw)) {
            ReviewClass::Single => Some(ReviewClass::Single),
            ReviewClass::Dual => Some(ReviewClass::Dual),
            ReviewClass::Unresolved => None,
        });
    if args["branch"]
        .as_str()
        .is_some_and(|branch| !branch.is_empty())
        && created_review_class.is_none()
    {
        return Err(json!({
            "error": "auto-created branch task has no durable review_class",
            "code": "review_class_unspecified",
        }));
    }
    crate::daemon::task_progress::touch(
        home,
        id,
        crate::daemon::task_progress::ProgressSource::Broadcast,
    );
    Ok(AutoCreatedTask {
        effective_task_id: Some(id.to_string()),
        auto_created_task_id: Some(id.to_string()),
        review_class: created_review_class,
    })
}

/// Shared inputs for send + post-success track (avoids clippy::too_many_arguments).
struct DeliveryCtx<'a> {
    home: &'a Path,
    args: &'a Value,
    sender: &'a Sender,
    target: &'a str,
    task: &'a str,
    msg: &'a str,
    task_id: Option<&'a str>,
    force_meta_json: Option<Value>,
    auto_created_task_id: Option<String>,
}

/// Phase 6 — SEND via neutral service (runtime=Some) or API bridge (runtime=None).
fn deliver_delegate(ctx: &DeliveryCtx<'_>, runtime: Option<&RuntimeContext>) -> Value {
    let req = crate::agent_ops::messaging::SendRequest {
        from: ctx.sender.as_str().to_string(),
        target: ctx.target.to_string(),
        text: ctx.msg.to_string(),
        kind: Some("task".to_string()),
        thread_id: ctx.args["thread_id"].as_str().map(String::from),
        parent_id: ctx.args["parent_id"].as_str().map(String::from),
        task_id: ctx.task_id.map(String::from),
        force_meta: ctx.force_meta_json.clone(),
        provenance: Some(json!({ "from": ctx.sender.as_str(), "task": ctx.task })),
        branch: ctx.args["branch"].as_str().map(String::from),
        correlation_id: ctx.args["correlation_id"].as_str().map(String::from),
        reviewed_head: ctx.args["reviewed_head"].as_str().map(String::from),
        report_purpose: ctx.args["report_purpose"].as_str().map(String::from),
        code_review: ctx
            .args
            .get("code_review")
            .filter(|v| !v.is_null())
            .cloned(),
        eta_minutes: ctx.args["eta_minutes"].as_u64(),
        reporting_cadence: ctx.args["reporting_cadence"].as_str().map(String::from),
        worktree_binding_required: ctx.args["worktree_binding_required"].as_bool(),
        expect_reply_within_secs: ctx.args["expect_reply_within_secs"].as_i64(),
        terminal: ctx.args["terminal"].as_bool(),
        no_report_expected: ctx.args["no_report_expected"].as_bool(),
        delivery_nonce: ctx.args["delivery_nonce"].as_str().map(String::from),
        broadcast_context: None,
        priority: ctx.args["priority"].as_str().map(String::from),
    };
    if let Some(rt) = runtime {
        match crate::agent_ops::messaging::execute_send(ctx.home, &rt.registry, req) {
            crate::agent_ops::messaging::SendOutcome::Success { .. } => {
                json!({"target": ctx.target})
            }
            crate::agent_ops::messaging::SendOutcome::Error { error, .. } => {
                json!({"error": error})
            }
        }
    } else {
        crate::agent_ops::send_via_api_bridge(ctx.home, &req)
    }
}

/// Phase 7 — post-success UX / auto_created_task_id.
fn track_delegate_success(ctx: &DeliveryCtx<'_>, mut result: Value) -> Value {
    if is_ok_result(&result) {
        if let Some(branch) = ctx.args["branch"].as_str() {
            tracing::info!(
                target = %ctx.target,
                branch = %branch,
                task_id = ?ctx.task_id,
                "delegate_task branch hint — implementer should work on this branch"
            );
        }
        ux_sink_registry().emit(&UxEvent::Fleet(FleetEvent::DelegateTask {
            from: ctx.sender.as_str().to_string(),
            to: ctx.target.to_string(),
            summary: ctx.task.to_string(),
            task_id: ctx.task_id.map(str::to_string),
        }));
    }
    if let Some(tid) = ctx.auto_created_task_id.as_ref() {
        if let Some(obj) = result.as_object_mut() {
            obj.insert("auto_created_task_id".into(), json!(tid));
        }
    }
    result
}

/// Ordered choreography for MCP `delegate_task` / unified send kind=task.
pub(crate) fn handle_delegate_task(
    home: &Path,
    args: &Value,
    sender: &Option<Sender>,
    runtime: Option<&RuntimeContext>,
) -> Value {
    let resolved = match resolve_delegate(home, args, sender) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let target = resolved.resolved_target.as_str();
    let sender = resolved.sender;
    let task = resolved.task;

    // Phase 2 — pre-send gates (busy / branch-dedup / enrich / second-reviewer / …)
    let checks = match comms_gates::run_dispatch_pre_checks(home, sender, args, target, task) {
        Ok(c) => c,
        Err(rejection) => return rejection,
    };

    let composed = compose_delegate_message(task, args, &checks);
    let review_assignment_repo = if checks.review_assignment {
        match review_assignment::validate_review_assignment_marker(
            home, sender, target, args, &checks,
        ) {
            Ok(slug) => Some(slug),
            Err(e) => return e,
        }
    } else {
        None
    };
    // #2454 atomicity: runtime=None + non-empty branch → fail closed BEFORE
    // durable mutations (task-create/delivery) AND before the authority
    // preflight, so a runtime-less caller still gets the #2454 code rather than
    // an authority diagnostic. Skipped for review_assignment, which dispatches
    // via the store and never needed the runtime — that path stays byte-for-byte.
    if !checks.review_assignment
        && runtime.is_none()
        && args["branch"].as_str().is_some_and(|b| !b.is_empty())
    {
        return json!({
            "ok": false,
            "error": "branch dispatch requires in-process runtime",
            "code": "runtime_unavailable_branch_2454",
            "remediation": "ensure MCP handler receives RuntimeContext from daemon dispatch",
        });
    }
    let preflight_review_class = match preflight_branch_authority(home, args, &checks) {
        Ok(class) => class,
        Err(e) => return e,
    };

    if let Some(repo_slug) = review_assignment_repo {
        return review_assignment::dispatch_review_assignment_with_workspace(
            home, sender, target, task, args, &checks, &composed, &repo_slug,
        );
    }

    let created = if runtime.is_some() {
        match maybe_auto_create_task(home, args, sender, target, composed.plan_ack_required) {
            Ok(created) => created,
            Err(error) => return error,
        }
    } else {
        // runtime=None: let the daemon auto-create atomically via the
        // API bridge — pass the original (possibly missing) task_id through.
        AutoCreatedTask {
            effective_task_id: args["task_id"].as_str().map(String::from),
            auto_created_task_id: None,
            review_class: None,
        }
    };
    let task_id_str = created.effective_task_id.as_deref();
    let branch_review_class = if created.auto_created_task_id.is_some() {
        created.review_class
    } else {
        preflight_review_class
    };
    let mut ci_watch = None;
    if let Some(review_class) = branch_review_class {
        let outcome = if dispatch_should_skip_auto_bind(args) {
            maybe_auto_watch_without_bind(home, args, target, task_id_str, review_class)
        } else {
            maybe_auto_bind_lease(home, args, target, task_id_str, review_class)
        };
        match outcome {
            Ok(outcome) => ci_watch = outcome,
            Err(e) => return e,
        }
    }
    let mut msg = composed.msg;
    if let Some(tid) = task_id_str {
        msg.push_str(&format!(" (task id: {tid})"));
    }

    let ctx = DeliveryCtx {
        home,
        args,
        sender,
        target,
        task,
        msg: &msg,
        task_id: task_id_str,
        force_meta_json: composed.force_meta_json,
        auto_created_task_id: created.auto_created_task_id,
    };
    let result = deliver_delegate(&ctx, runtime);
    let mut result = track_delegate_success(&ctx, result);
    if let Some(watch) = ci_watch {
        let degraded = !watch.armed && is_ok_result(&result);
        if let Some(obj) = result.as_object_mut() {
            obj.insert(
                "ci_watch".into(),
                json!({
                    "armed": watch.armed,
                    "next_after_ci": watch.next_after_ci,
                }),
            );
            if degraded {
                obj.insert("degraded".into(), json!(true));
                obj.insert(
                    "warning".into(),
                    json!({
                        "code": "ci_watch_arm_failed",
                        "remediation": "CI watch could not be armed for this dispatch; \
                        run `ci action=watch` manually to enable CI-ready notifications",
                    }),
                );
            }
        }
    }
    result
}

#[cfg(test)]
mod tests;

/// #2760 Slice A — strict-RESOLUTION unit test (NOT a production dispatch-entry
/// proof; Slice B owns the true `handle_delegate_task`/`send` production entry and
/// its ordering/atomicity).
///
/// Motivating bug (t-…-35): a project-board task with `review_class=single` was
/// dispatched as `review_class_unspecified` because the merge-authority preflight
/// read the task's durable `review_class` via the default-only `load_by_id` seam,
/// invisible to per-project boards.
///
/// This exercises the STRICT router (`crate::tasks::load_routed`) reading a
/// project-board task's `review_class` metadata — the task created through the real
/// `tasks::handle` create path on a non-default board — and feeds it to the pure
/// `resolve_existing_task_review_class` classifier, proving the strict route
/// surfaces `single` where the default-only read surfaced absent. It does NOT drive
/// the production dispatch preflight or its bind/deliver ordering (Slice B).
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod routing_red_2760 {
    use super::{resolve_existing_task_review_class, ReviewClass};
    use serde_json::json;
    use std::path::PathBuf;

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "agend-comms-routing-red-2760-{}-{}-{tag}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// The task's durable `review_class`, read the way the merge-authority
    /// preflight reads it — but via the STRICT router instead of the default-only
    /// `load_by_id`.
    fn preflight_review_class(home: &std::path::Path, task_id: &str) -> Option<String> {
        crate::tasks::load_routed(home, task_id)
            .ok()
            .and_then(|rt| {
                rt.task
                    .metadata
                    .get("review_class")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
    }

    #[test]
    fn load_routed_resolves_project_board_review_class_single_2760() {
        let home = tmp_home("single");
        // Create the task through the REAL create handler, routed to a NON-DEFAULT
        // project board, carrying the durable `review_class=single` authority.
        let created = crate::tasks::handle(
            &home,
            "orchestrator",
            &json!({
                "action": "create",
                "title": "impl the feature",
                "project": "proj-2760",
                "review_class": "single",
            }),
        );
        let task_id = created["id"]
            .as_str()
            .expect("create returns an id")
            .to_string();

        let class = preflight_review_class(&home, &task_id);
        let resolved = resolve_existing_task_review_class(class.as_deref(), None, false);
        assert_eq!(
            resolved,
            Ok(ReviewClass::Single),
            "t-…-35: a project-board task with review_class=single must route strictly \
             and the merge-authority preflight must resolve Single — pre-fix the \
             default-only load_by_id seam returned review_class_unspecified"
        );
    }

    /// #2760: the live t-…93 false-negative reproduction (codex 2026-07-13 16:01Z):
    /// an EXISTING project-board task with durable `review_class=dual`, dispatched
    /// with `review_class=dual` (+ `second_reviewer=true`), was refused as
    /// `review_class_unspecified` because the merge-authority preflight read the
    /// durable class via the default-only seam (invisible to the project board).
    /// The strict router surfaces `dual`, so the preflight resolves `Dual` (a
    /// supplied matching `dual` + second_reviewer are consistency-only, never a
    /// mismatch). Mirrors the `single` unit for the two-reviewer authority.
    #[test]
    fn load_routed_resolves_project_board_review_class_dual_2760() {
        let home = tmp_home("dual");
        let created = crate::tasks::handle(
            &home,
            "orchestrator",
            &json!({
                "action": "create",
                "title": "impl the feature",
                "project": "Hack_agend-terminal",
                "review_class": "dual",
            }),
        );
        let task_id = created["id"]
            .as_str()
            .expect("create returns an id")
            .to_string();

        let class = preflight_review_class(&home, &task_id);
        // The dispatch supplied review_class="dual" and second_reviewer=true — both
        // are consistency-evidence against the durable class, not a mismatch.
        let resolved = resolve_existing_task_review_class(class.as_deref(), Some("dual"), true);
        assert_eq!(
            resolved,
            Ok(ReviewClass::Dual),
            "t-…93: a project-board task with review_class=dual must route strictly and \
             the merge-authority preflight must resolve Dual — pre-fix the default-only \
             seam returned review_class_unspecified despite the durable dual authority"
        );
    }
}
