//! W2.2: `handle_delegate_task` as an ordered phase pipeline.
//!
//! Stages (failure order preserved — a reject before lease never leases;
//! a send failure may still have leased/created a task, same as pre-split):
//!
//! 1. **resolve** — identity, instance/team target, self-dispatch reject
//! 2. **validate** — pre-send gates (`comms_gates::run_dispatch_pre_checks`)
//! 3. **compose** — message body + force_meta
//! 4. **lease** — optional `dispatch_auto_bind_lease` when `branch` set
//! 5. **create** — optional auto board task after all rejectable checks
//! 6. **send** — API SEND / inbox fallback via [`SendEnvelope`]
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
use super::super::dispatch_hook;
use super::super::send_envelope::SendEnvelope;
use super::super::{err_needs_identity, is_ok_result, require_instance};

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

/// #2745 fail-closed (decision d-…-11 + codex seam correction): why a
/// merge-authority dispatch's `review_class` could NOT be resolved. The caller
/// refuses to arm the ci-watch and emits [`ReviewClassRefusal::diagnostic`] —
/// NEVER a silent Single/Dual default.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReviewClassRefusal {
    /// The task carried no resolvable `review_class` (absent / null / typo /
    /// wrong-type). `second_reviewer=true` alone is NOT a fallback — it still
    /// refuses.
    Unspecified,
    /// The task's explicit class contradicts the deprecated `second_reviewer`
    /// alias (task=`single` vs `second_reviewer=true`, which implies dual).
    Mismatch { task_class: &'static str },
}

impl ReviewClassRefusal {
    /// Actionable operator-facing diagnostic for the refused dispatch.
    pub(crate) fn diagnostic(&self, branch: &str) -> String {
        match self {
            ReviewClassRefusal::Unspecified => format!(
                "review_class unspecified for merge-authority dispatch on `{branch}` — \
                 set the task's `review_class` metadata to `single` or `dual` and \
                 re-dispatch. A PR-producing dispatch must declare its review threshold; \
                 the dispatch was refused (fail-closed #2745)."
            ),
            ReviewClassRefusal::Mismatch { task_class } => format!(
                "review_class MISMATCH for dispatch on `{branch}` — task authority is \
                 `{task_class}` but second_reviewer=true implies dual. second_reviewer \
                 cannot override the task's declared class; reconcile them and re-dispatch. \
                 the dispatch was refused (fail-closed #2745)."
            ),
        }
    }

    /// Stable machine code for the structured dispatch-refusal error — lets the
    /// caller distinguish "no class declared" from "class contradicted".
    pub(crate) fn code(&self) -> &'static str {
        match self {
            ReviewClassRefusal::Unspecified => "review_class_unspecified",
            ReviewClassRefusal::Mismatch { .. } => "review_class_mismatch",
        }
    }
}

/// #2745 (decision d-…-11 + codex seam correction): resolve the durable
/// `review_class` for a MERGE-AUTHORITY (PR-producing) dispatch. Called ONLY from
/// the merge-authority branch of [`maybe_auto_bind_lease`] — non-merge dispatches
/// bypass it structurally, so there is no `merge_authority` bool to get wrong.
///
/// The TASK's `review_class` metadata is the sole AUTHORITY — parsed exactly once
/// via [`ReviewClass::parse_fail_closed`]. `second_reviewer` is compatibility
/// EVIDENCE only, never an independent source of dual:
/// - task `dual` → `Ok(Dual)` (`second_reviewer` either value is consistent)
/// - task `single`, `sr=false` → `Ok(Single)`
/// - task `single`, `sr=true` → `Err(Mismatch)` (sr cannot override the task)
/// - task Unresolved (absent/typo), any `sr` → `Err(Unspecified)` (missing+true
///   still refuses; no fallback)
pub(crate) fn resolve_dispatch_review_class(
    task_review_class_raw: Option<&str>,
    second_reviewer: bool,
) -> Result<ReviewClass, ReviewClassRefusal> {
    match ReviewClass::parse_fail_closed(task_review_class_raw) {
        ReviewClass::Dual => Ok(ReviewClass::Dual),
        ReviewClass::Single if second_reviewer => Err(ReviewClassRefusal::Mismatch {
            task_class: "single",
        }),
        ReviewClass::Single => Ok(ReviewClass::Single),
        ReviewClass::Unresolved => Err(ReviewClassRefusal::Unspecified),
    }
}

/// #2745 R3 (root R2 finding 2): resolve the review_class for an EXISTING-TASK
/// merge-authority dispatch. The task's `review_class` metadata is the SOLE durable
/// AUTHORITY — a supplied `send review_class` arg (and `second_reviewer`) is
/// CONSISTENCY EVIDENCE only: it may confirm the task's class but can NEVER fill a
/// missing-metadata gap or contradict it. Closes the fallback where an untagged
/// existing task passed by supplying `send.review_class` (leaving the task
/// authority-less, against the schema + remediation contract).
/// - task Unresolved (absent/typo metadata), any arg → `Err(Unspecified)` (the arg
///   can't supply durable authority — the task must be tagged first).
/// - task `single`/`dual` + a DIFFERING arg → `Err(Mismatch)`.
/// - task `single` + `second_reviewer=true` (implies dual) → `Err(Mismatch)`.
/// - otherwise `Ok(task_class)`.
pub(crate) fn resolve_existing_task_review_class(
    task_review_class_raw: Option<&str>,
    arg_review_class_raw: Option<&str>,
    second_reviewer: bool,
) -> Result<ReviewClass, ReviewClassRefusal> {
    let resolved = match ReviewClass::parse_fail_closed(task_review_class_raw) {
        ReviewClass::Unresolved => return Err(ReviewClassRefusal::Unspecified),
        c => c,
    };
    // A supplied send review_class is consistency-evidence only — it must match the
    // task's durable class, never fill a gap or override it.
    if let Some(arg) = arg_review_class_raw.filter(|s| !s.is_empty()) {
        if ReviewClass::parse_fail_closed(Some(arg)) != resolved {
            return Err(ReviewClassRefusal::Mismatch {
                task_class: resolved.as_token(),
            });
        }
    }
    // second_reviewer=true implies dual; it must not contradict a Single task.
    if second_reviewer && resolved == ReviewClass::Single {
        return Err(ReviewClassRefusal::Mismatch {
            task_class: "single",
        });
    }
    Ok(resolved)
}

/// Phase 4 — optional auto-bind lease (rejectable).
fn maybe_auto_bind_lease(
    home: &Path,
    args: &Value,
    target: &str,
    second_reviewer: bool,
) -> Result<(), Value> {
    let Some(branch) = args["branch"].as_str() else {
        return Ok(());
    };
    let task_id_val = args["task_id"].as_str().unwrap_or("");
    if dispatch_should_skip_auto_bind(args) {
        tracing::info!(
            %target, %branch, task_id = %task_id_val,
            "dispatch_auto_bind_lease skipped (bind: false)"
        );
        return Ok(());
    }
    let next_after_ci =
        crate::daemon::ci_watch::watch_state::normalize_next_after_ci(&args["next_after_ci"]);
    // #2745 (decision d-…-11 + R3 finding 2): this is the MERGE-AUTHORITY branch — a
    // `branch` was supplied → PR-producing / auto-watched. Resolve the durable
    // review_class authority BEFORE arming, with the authority source keyed on the
    // dispatch shape:
    // - EXISTING task (task_id present): Task.metadata is the SOLE durable authority.
    //   A `send review_class` arg is consistency-evidence only — it can neither fill
    //   a missing-metadata gap nor contradict the task (else the task would stay
    //   authority-less despite a "successful" dispatch).
    // - AUTO-CREATE (empty task_id): the `send review_class` arg declares the class
    //   (it also seeds the created task's metadata downstream).
    // Fail-closed: an absent / unknown / mismatched class REJECTS the whole dispatch
    // atomically (structured error, no bind/create/send) — never a silent Single.
    let arg_review_class = args["review_class"].as_str();
    let resolved_review_class = if task_id_val.is_empty() {
        resolve_dispatch_review_class(arg_review_class, second_reviewer)
    } else {
        let task_review_class = crate::tasks::load_by_id(home, task_id_val).and_then(|t| {
            t.metadata
                .get("review_class")
                .and_then(|v| v.as_str())
                .map(String::from)
        });
        resolve_existing_task_review_class(
            task_review_class.as_deref(),
            arg_review_class,
            second_reviewer,
        )
    };
    let armed_review_class = match resolved_review_class {
        Ok(class) => class.as_token(),
        Err(refusal) => {
            // #2745 fail-closed (root pre-review finding 2): REJECT the
            // merge-authority dispatch ATOMICALLY — before any bind / task-create /
            // send side effect — so the caller receives a structured error and no PR
            // work is ever dispatched without a review gate (never a silent Ok with
            // an un-armed watch). `code` distinguishes unspecified vs mismatch; the
            // error carries branch + task remediation.
            tracing::error!(
                %target, %branch, task_id = %task_id_val, code = refusal.code(),
                "#2745 merge-authority dispatch REJECTED — review_class unresolved \
                 (no bind / watch / create / send)"
            );
            let remediation = if task_id_val.is_empty() {
                "declare `review_class: single|dual` on the send (auto-create path), or \
                 create the task with a review_class first, then re-dispatch"
                    .to_string()
            } else {
                format!(
                    "set the review_class metadata (single|dual) on task {task_id_val}, \
                     then re-dispatch"
                )
            };
            return Err(json!({
                "ok": false,
                "error": refusal.diagnostic(branch),
                "code": refusal.code(),
                "remediation": remediation,
                "branch": branch,
                "task_id": task_id_val,
            }));
        }
    };
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
    .map(|_| ())
    .map_err(|e| json!({"ok": false, "error": format!("dispatch rejected: {e}")}))
}

/// Phase 5 — optional auto-create board task after rejectable checks.
fn maybe_auto_create_task(
    home: &Path,
    args: &Value,
    sender: &Sender,
    target: &str,
    plan_ack_required: u64,
) -> (Option<String>, Option<String>) {
    if !args["task_id"].as_str().unwrap_or("").is_empty() || *sender == target {
        return (args["task_id"].as_str().map(String::from), None);
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
        // resolver already validated it via the args fallback in the lease above).
        "review_class": args["review_class"].as_str(),
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

/// Phase 6 — SEND via API with envelope fallback.
fn deliver_delegate(ctx: &DeliveryCtx<'_>) -> Value {
    let env = SendEnvelope {
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
        ..SendEnvelope::directives_from_args(ctx.args)
    };
    match crate::api::call(
        ctx.home,
        &json!({
            "request_id": uuid::Uuid::new_v4().to_string(),
            "method": crate::api::method::SEND,
            "params": env.to_send_params(),
        }),
    ) {
        Ok(resp) if resp["ok"].as_bool() == Some(true) => json!({"target": ctx.target}),
        Ok(resp) => json!({"error": resp["error"].as_str().unwrap_or("send failed")}),
        Err(e) => {
            let inbox_msg = env.to_inbox_message();
            crate::agent_ops::fallback_deliver(
                ctx.home,
                ctx.sender.as_str(),
                ctx.target,
                ctx.msg,
                inbox_msg,
                &e,
            )
        }
    }
}

/// Phase 7 — post-success tracking / UX / auto_created_task_id.
fn track_delegate_success(ctx: &DeliveryCtx<'_>, mut result: Value) -> Value {
    if is_ok_result(&result) {
        let task_id = ctx.task_id.map(str::to_string);
        let status = if ctx.args["no_report_expected"].as_bool().unwrap_or(false) {
            "no_report_expected"
        } else {
            "pending"
        };
        crate::dispatch_tracking::track_dispatch(
            ctx.home,
            crate::dispatch_tracking::DispatchEntry {
                task_id: task_id.clone(),
                from: ctx.sender.as_str().to_string(),
                to: ctx.target.to_string(),
                from_id: crate::agent::resolve_instance(ctx.home, ctx.sender.as_str())
                    .ok()
                    .map(|(id, _)| id.full()),
                to_id: crate::agent::resolve_instance(ctx.home, ctx.target)
                    .ok()
                    .map(|(id, _)| id.full()),
                delegated_at: chrono::Utc::now().to_rfc3339(),
                status: status.to_string(),
            },
        );
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
            task_id,
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
pub(crate) fn handle_delegate_task(home: &Path, args: &Value, sender: &Option<Sender>) -> Value {
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
    if let Err(e) = maybe_auto_bind_lease(home, args, target, composed.second_reviewer) {
        return e;
    }

    let (effective_task_id, auto_created_task_id) =
        maybe_auto_create_task(home, args, sender, target, composed.plan_ack_required);
    let task_id_str = effective_task_id.as_deref();
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
        auto_created_task_id,
    };
    let result = deliver_delegate(&ctx);
    track_delegate_success(&ctx, result)
}

#[cfg(test)]
mod review_class_authority_tests {
    use super::{
        resolve_dispatch_review_class, resolve_existing_task_review_class, ReviewClass,
        ReviewClassRefusal,
    };

    /// #2745 case 1 (durable propagation): the TASK's `review_class` is the
    /// authority. A task marked `dual` resolves `Dual` even when the dispatch
    /// omits the deprecated `second_reviewer` alias.
    #[test]
    fn task_review_class_dual_is_authority_2745() {
        assert_eq!(
            resolve_dispatch_review_class(Some("dual"), false),
            Ok(ReviewClass::Dual),
            "task review_class=dual is the authority regardless of second_reviewer"
        );
        // second_reviewer=true is consistent evidence for a dual task.
        assert_eq!(
            resolve_dispatch_review_class(Some("dual"), true),
            Ok(ReviewClass::Dual),
        );
    }

    /// #2745 case: explicit single resolves single (and dedups don't matter here);
    /// consistency guard so GREEN doesn't over-refuse the ordinary path.
    #[test]
    fn task_review_class_single_resolves_single_2745() {
        assert_eq!(
            resolve_dispatch_review_class(Some("single"), false),
            Ok(ReviewClass::Single),
        );
    }

    /// #2745 case 2 (mismatch refusal): `second_reviewer=true` is EVIDENCE only —
    /// it must NOT override a task that says `single`. A contradiction fails closed
    /// (Mismatch), never a silent pick.
    #[test]
    fn task_single_vs_second_reviewer_true_mismatch_refuses_2745() {
        assert_eq!(
            resolve_dispatch_review_class(Some("single"), true),
            Err(ReviewClassRefusal::Mismatch {
                task_class: "single"
            }),
            "task=single vs second_reviewer=true must Refuse(Mismatch)"
        );
    }

    /// #2745 case 7 (real-entry omission fails loud): a merge-authority dispatch
    /// that OMITS review_class — no task metadata, no second_reviewer — FAILS LOUD
    /// (Unspecified), never silently Single.
    #[test]
    fn merge_authority_omission_fails_loud_2745() {
        assert_eq!(
            resolve_dispatch_review_class(None, false),
            Err(ReviewClassRefusal::Unspecified),
            "omitted review_class on a merge-authority dispatch must Refuse(Unspecified)"
        );
    }

    /// #2745 (codex correction): `second_reviewer=true` is NOT a fallback — a
    /// missing task class with second_reviewer=true STILL refuses (no silent dual).
    #[test]
    fn omission_with_second_reviewer_true_still_refuses_2745() {
        assert_eq!(
            resolve_dispatch_review_class(None, true),
            Err(ReviewClassRefusal::Unspecified),
            "missing class + second_reviewer=true still refuses — no fallback to dual"
        );
        // A typo'd class is likewise unresolvable, second_reviewer notwithstanding.
        assert_eq!(
            resolve_dispatch_review_class(Some("duel"), true),
            Err(ReviewClassRefusal::Unspecified),
        );
    }

    /// #2745 R3 finding 2 (existing-task authority): a REFERENCED task with missing /
    /// typo'd metadata cannot be rescued by a send arg or second_reviewer — the send
    /// value is consistency-evidence only, never a source of durable authority.
    #[test]
    fn existing_task_missing_metadata_send_arg_cannot_fill_2745() {
        assert_eq!(
            resolve_existing_task_review_class(None, Some("single"), false),
            Err(ReviewClassRefusal::Unspecified),
            "send review_class cannot fill an untagged existing task"
        );
        assert_eq!(
            resolve_existing_task_review_class(None, Some("dual"), true),
            Err(ReviewClassRefusal::Unspecified),
        );
        assert_eq!(
            resolve_existing_task_review_class(None, None, false),
            Err(ReviewClassRefusal::Unspecified),
        );
        // typo'd task metadata is likewise unresolvable.
        assert_eq!(
            resolve_existing_task_review_class(Some("duel"), Some("dual"), false),
            Err(ReviewClassRefusal::Unspecified),
        );
    }

    /// #2745 R3 finding 2: a supplied send class that CONTRADICTS the task's durable
    /// class fails closed (Mismatch) — the send is evidence, never an override.
    #[test]
    fn existing_task_contradictory_send_class_rejects_2745() {
        assert_eq!(
            resolve_existing_task_review_class(Some("single"), Some("dual"), false),
            Err(ReviewClassRefusal::Mismatch {
                task_class: "single"
            }),
            "task=single vs send review_class=dual must Refuse(Mismatch)"
        );
        assert_eq!(
            resolve_existing_task_review_class(Some("dual"), Some("single"), false),
            Err(ReviewClassRefusal::Mismatch { task_class: "dual" }),
        );
        // second_reviewer=true (implies dual) contradicts a Single task.
        assert_eq!(
            resolve_existing_task_review_class(Some("single"), None, true),
            Err(ReviewClassRefusal::Mismatch {
                task_class: "single"
            }),
        );
    }

    /// #2745 R3 finding 2 (positive): a consistent or absent send class defers to the
    /// task's durable authority; the task metadata alone resolves the class.
    #[test]
    fn existing_task_authority_with_consistent_send_2745() {
        assert_eq!(
            resolve_existing_task_review_class(Some("dual"), Some("dual"), false),
            Ok(ReviewClass::Dual)
        );
        assert_eq!(
            resolve_existing_task_review_class(Some("dual"), None, true),
            Ok(ReviewClass::Dual)
        );
        assert_eq!(
            resolve_existing_task_review_class(Some("single"), None, false),
            Ok(ReviewClass::Single)
        );
        assert_eq!(
            resolve_existing_task_review_class(Some("single"), Some("single"), false),
            Ok(ReviewClass::Single)
        );
    }
}
