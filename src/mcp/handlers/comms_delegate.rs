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

use super::super::comms_gates::{self, DispatchPreChecks, ReviewAuthor};
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

/// t-…-17 reviewer-assignment marker gate. Runs BETWEEN `run_dispatch_pre_checks`
/// and `maybe_auto_bind_lease` — i.e. AFTER the generic pre-send gates but BEFORE
/// any bind / task-create / deliver side effect — and ATOMICALLY REJECTS on the
/// FIRST failure so a marker dispatch that fails authority never mutates state.
/// Only invoked when `checks.review_assignment` is true; an ordinary dispatch never
/// enters here (byte-identical legacy path).
///
/// Order is load-bearing (fail-closed):
///   (a) explicit non-empty `task_id` + non-empty `branch` + nonzero `pr_number`
///       (generation identity; B18) — else reject BEFORE resolving anything;
///   (b) provider-neutral repo resolve (fail-closed, no default);
///   (c) source-repo → EXACTLY-ONE team, and sender == that team's SOLE CURRENT
///       orchestrator (live fleet; NO operator-allow);
///   (d) review_author self-review deny — `Agent(author) == reviewer target` is
///       same-namespace self-review; `External(login)` is a distinct principal and
///       is NEVER string-compared to the agent target. (sender == target is already
///       rejected upstream in `resolve_delegate`.)
fn validate_review_assignment_marker(
    home: &Path,
    sender: &Sender,
    target: &str,
    args: &Value,
    checks: &DispatchPreChecks,
) -> Result<String, Value> {
    // (a) mandatory explicit generation-bound identifiers.
    if args["task_id"].as_str().unwrap_or("").is_empty() {
        return Err(json!({
            "error": "review_assignment requires an explicit non-empty `task_id` \
                      (the marker path never auto-creates a task)",
            "code": "review_assignment_missing_task_id",
        }));
    }
    if args["branch"].as_str().unwrap_or("").is_empty() {
        return Err(json!({
            "error": "review_assignment requires a non-empty `branch`",
            "code": "review_assignment_missing_branch",
        }));
    }
    match checks.pr_number {
        Some(n) if n != 0 => {}
        _ => {
            return Err(json!({
                "error": "review_assignment requires a nonzero `pr_number` \
                          (mandatory generation identity — B18)",
                "code": "review_assignment_missing_pr_number",
            }))
        }
    }

    // (b) fail-closed provider-neutral repo resolve (the ACL key).
    let repo_slug = dispatch_hook::resolve_review_assignment_repo(home, args, target)?;

    // (c) source-repo → exactly-one team + sole-current-orchestrator authority.
    let team =
        crate::teams::resolve_team_by_source_repo(home, &repo_slug).map_err(|e| match e {
            crate::teams::TeamAuthorityError::NoMatch => json!({
                "error": format!(
                    "review_assignment rejected: no team's `source_repo` matches `{repo_slug}` \
                     — operator must set the owning team's source_repo (fail-closed, no default)"
                ),
                "code": "review_assignment_no_team",
            }),
            crate::teams::TeamAuthorityError::Ambiguous(n) => json!({
                "error": format!(
                    "review_assignment rejected: {n} teams share `source_repo` `{repo_slug}` \
                     — ambiguous authority, operator must disambiguate (fail-closed)"
                ),
                "code": "review_assignment_ambiguous_team",
            }),
        })?;
    if team.orchestrator.as_deref() != Some(sender.as_str()) {
        return Err(json!({
            "error": format!(
                "review_assignment rejected: sender `{}` is not the sole current \
                 orchestrator of team `{}` (source-repo `{repo_slug}`) — authority is \
                 the owning team's orchestrator only, no operator-allow",
                sender.as_str(),
                team.name
            ),
            "code": "review_assignment_not_authorized",
        }));
    }

    // (d) review_author self-review deny (typed; External never compared).
    if let Some(ReviewAuthor::Agent(author)) = &checks.review_author {
        if author == target {
            return Err(json!({
                "error": format!(
                    "review_assignment rejected: reviewer target `{target}` is the code \
                     author (same-namespace self-review)"
                ),
                "code": "review_assignment_self_review",
            }));
        }
    }
    // The validated, canonical `owner/repo` slug is returned so the store dispatch
    // (A1) keys the record on the SAME lockstep form the ACL matched (I25) — no
    // second resolve (which could redrift or re-run a git subprocess).
    Ok(repo_slug)
}

/// t-…-17 C11 (A1→A2→A3): deliver a validated reviewer-assignment marker dispatch
/// through the DURABLE outbox store instead of `deliver_delegate`. Called from
/// [`handle_delegate_task`] AFTER `maybe_auto_bind_lease` (bind) has succeeded and
/// the marker gate has resolved the canonical `repo_slug`.
///
/// A1 `persist` the PENDING record (mint assignment_id + delivery_nonce; store the
/// mandatory `pr_number` — the generation identity). A2 `durable_enqueue` the
/// reviewer's actionable inbox row (the store owns delivery — this is NOT a
/// `deliver_delegate`/API send). A3 emit a best-effort self-IPC WAKE pointer OUTSIDE
/// all flocks (`durable_enqueue` has already released its lock). A store failure
/// AFTER the bind FAILS LOUD (structured error) rather than silently proceeding
/// (I23) — the bind is already durable, so a swallowed store error would strand the
/// assignment with no record.
#[allow(clippy::too_many_arguments)]
fn dispatch_review_assignment_via_store(
    home: &Path,
    sender: &Sender,
    target: &str,
    task: &str,
    args: &Value,
    checks: &DispatchPreChecks,
    composed: &ComposedDelegate,
    repo_slug: &str,
) -> Value {
    // All three are gate-validated: branch + task_id non-empty, pr_number nonzero.
    let branch = args["branch"].as_str().unwrap_or_default();
    let task_id = args["task_id"].as_str().unwrap_or_default();
    let pr_number = checks.pr_number.unwrap_or(0);
    // The record's `review_class` is authority METADATA (the gate/classifier read the
    // PrState's class, never this) — resolve it from the task's DURABLE metadata (the
    // same authority `maybe_auto_bind_lease` already validated and armed); an untagged
    // task degrades to `Unresolved` (harmless: metadata-only).
    let review_class = crate::tasks::load_by_id(home, task_id)
        .and_then(|t| {
            t.metadata
                .get("review_class")
                .and_then(|v| v.as_str())
                .map(|s| ReviewClass::parse_fail_closed(Some(s)))
        })
        .unwrap_or(ReviewClass::Unresolved);
    // `review_author` is OPTIONAL at dispatch (the gate only self-review-denies an
    // Agent author); the store record carries a non-Option principal, so an absent
    // author is stored as an empty `External` sentinel — display-only, never
    // string-compared to the agent target (I7), and the merge gate keys on reserved
    // PRESENCE, not authorship.
    let review_author = checks
        .review_author
        .clone()
        .unwrap_or(ReviewAuthor::External(String::new()));
    let now = chrono::Utc::now().to_rfc3339();
    let record = crate::daemon::assignment_authority::ActiveAssignment::new_pending(
        repo_slug,
        branch,
        target,
        pr_number,
        sender.as_str(),
        task_id,
        review_class,
        review_author,
        composed.msg.clone(),
        args["thread_id"].as_str().map(String::from),
        args["parent_id"].as_str().map(String::from),
        &now,
    );
    // A1 — persist the PENDING record (no PrState/inbox row seeded — I9).
    if let Err(e) = crate::daemon::assignment_authority::persist(home, &record) {
        return json!({
            "ok": false,
            "error": format!("review_assignment store persist failed after bind: {e}"),
            "code": "review_assignment_store_persist_failed",
        });
    }
    // A2 — durable enqueue of the reviewer's actionable row (store owns delivery).
    if let Err(e) =
        crate::daemon::assignment_authority::durable_enqueue(home, repo_slug, branch, target, &now)
    {
        return json!({
            "ok": false,
            "error": format!("review_assignment store enqueue failed after bind: {e}"),
            "code": "review_assignment_store_enqueue_failed",
        });
    }
    // A3 — best-effort self-IPC WAKE pointer, OUTSIDE all flocks.
    crate::inbox::notify::wake_review_assignment(home, target);
    let result = json!({
        "target": target,
        "review_assignment": true,
        "assignment_id": record.assignment_id.to_string(),
        "pr_number": pr_number,
    });
    // Dispatch tracking / UX parity with the legacy path (task_id is explicit).
    let ctx = DeliveryCtx {
        home,
        args,
        sender,
        target,
        task,
        msg: &composed.msg,
        task_id: Some(task_id),
        force_meta_json: None,
        auto_created_task_id: None,
    };
    track_delegate_success(&ctx, result)
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

    // t-…-17 reviewer-assignment marker gate — fail-closed, BEFORE any bind/create/
    // deliver side effect. A dispatch WITHOUT the marker skips this entirely and is
    // byte-identical to the legacy path. On success it yields the validated canonical
    // repo slug, which the store dispatch (A1) keys the record on.
    let review_assignment_repo = if checks.review_assignment {
        match validate_review_assignment_marker(home, sender, target, args, &checks) {
            Ok(slug) => Some(slug),
            Err(e) => return e,
        }
    } else {
        None
    };

    if let Err(e) = maybe_auto_bind_lease(home, args, target, composed.second_reviewer) {
        return e;
    }

    // t-…-17 C11: the marker path delivers via the DURABLE outbox store (A1→A2→A3),
    // NOT `deliver_delegate`, and bypasses `maybe_auto_create_task` (the store owns
    // the assignment record). Returns here; the legacy path below is untouched
    // (byte-identical) for a non-marker dispatch.
    if let Some(repo_slug) = review_assignment_repo {
        return dispatch_review_assignment_via_store(
            home, sender, target, task, args, &checks, &composed, &repo_slug,
        );
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

// ─────────────────────────────────────────────────────────────────
// t-…-17 reviewer-assignment marker gate (C4/C5/C6 + reject wiring).
// Real-entry: the validation-layer cases drive `validate_review_assignment_marker`
// directly (no bind/deliver side effects); the reject cases drive the full
// `handle_delegate_task` to prove ZERO side effects (no auto-create) on rejection.
// ─────────────────────────────────────────────────────────────────
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod review_assignment_marker_tests {
    use super::{handle_delegate_task, validate_review_assignment_marker, DispatchPreChecks};
    use crate::identity::Sender;
    use crate::mcp::handlers::comms_gates::ReviewAuthor;
    use serde_json::{json, Value};

    fn tmp_home(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-ra-marker-{}-{label}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Seed a fleet.yaml with the given raw `teams:` body. `source_repo` is a bare
    /// `owner/repo` slug so the provider-neutral canonicalizer resolves it with NO
    /// git subprocess (lockstep with the dispatch side, which uses the same
    /// canonicalizer on the explicit `repository` arg).
    fn seed_fleet(home: &std::path::Path, teams_yaml: &str) {
        let yaml = format!("instances:\n  lead:\n    backend: claude\n{teams_yaml}");
        std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
    }

    fn marker_checks(
        review_author: Option<ReviewAuthor>,
        pr_number: Option<u64>,
    ) -> DispatchPreChecks {
        DispatchPreChecks {
            force: false,
            force_reason: None,
            second_reviewer: false,
            plan_ack_required: 0,
            review_assignment: true,
            review_author,
            pr_number,
        }
    }

    fn marker_args(repo: &str, pr_number: u64) -> Value {
        json!({
            "instance": "reviewer",
            "task": "review the PR",
            "task_id": "t-rev-1",
            "branch": "feat/x",
            "repository": repo,
            "pr_number": pr_number,
        })
    }

    /// T2: sender is the SOLE CURRENT orchestrator of the team owning the repo ⇒ allow.
    #[test]
    fn t2_sole_orchestrator_authority_allows() {
        let home = tmp_home("t2-allow");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        let sender = Sender::new("lead").unwrap();
        validate_review_assignment_marker(
            &home,
            &sender,
            "reviewer",
            &marker_args("owner/repo", 42),
            &marker_checks(None, Some(42)),
        )
        .expect("sole-orchestrator dispatch must pass the marker gate");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T3: sender is NOT the team's orchestrator ⇒ deny (no operator-allow).
    #[test]
    fn t3_non_authority_denied() {
        let home = tmp_home("t3-deny");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        let sender = Sender::new("intruder").unwrap();
        let err = validate_review_assignment_marker(
            &home,
            &sender,
            "reviewer",
            &marker_args("owner/repo", 42),
            &marker_checks(None, Some(42)),
        )
        .expect_err("non-orchestrator must be denied");
        assert_eq!(err["code"], "review_assignment_not_authorized", "{err}");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T4a: no team's source_repo matches the dispatch repo ⇒ operator-repair reject.
    #[test]
    fn t4_zero_team_match_rejected() {
        let home = tmp_home("t4-zero");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        let sender = Sender::new("lead").unwrap();
        let err = validate_review_assignment_marker(
            &home,
            &sender,
            "reviewer",
            &marker_args("other/repo", 42),
            &marker_checks(None, Some(42)),
        )
        .expect_err("no team owning other/repo ⇒ reject");
        assert_eq!(err["code"], "review_assignment_no_team", "{err}");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T4b: ≥2 teams share the same source_repo ⇒ ambiguous-authority reject.
    #[test]
    fn t4_ambiguous_team_match_rejected() {
        let home = tmp_home("t4-ambig");
        seed_fleet(
            &home,
            "teams:\n  \
               edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n  \
               edge2:\n    orchestrator: lead\n    members:\n      - lead2\n    source_repo: Owner/Repo\n",
        );
        let sender = Sender::new("lead").unwrap();
        let err = validate_review_assignment_marker(
            &home,
            &sender,
            "reviewer",
            &marker_args("owner/repo", 42),
            &marker_checks(None, Some(42)),
        )
        .expect_err("two teams owning the same canonical repo ⇒ reject");
        assert_eq!(err["code"], "review_assignment_ambiguous_team", "{err}");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T5 / T8-Agent: review_author Agent(name) == reviewer target ⇒ self-review deny.
    #[test]
    fn t5_review_author_agent_self_review_denied() {
        let home = tmp_home("t5-self");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        let sender = Sender::new("lead").unwrap();
        let err = validate_review_assignment_marker(
            &home,
            &sender,
            "reviewer",
            &marker_args("owner/repo", 42),
            &marker_checks(Some(ReviewAuthor::Agent("reviewer".to_string())), Some(42)),
        )
        .expect_err("agent reviewing own code must be denied");
        assert_eq!(err["code"], "review_assignment_self_review", "{err}");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T8-External: External(login) equal-string to the target is a DISTINCT
    /// principal — an agent reviewing external-authored code is allowed.
    #[test]
    fn t8_review_author_external_matching_target_allowed() {
        let home = tmp_home("t8-ext");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        let sender = Sender::new("lead").unwrap();
        validate_review_assignment_marker(
            &home,
            &sender,
            "reviewer",
            &marker_args("owner/repo", 42),
            &marker_checks(
                Some(ReviewAuthor::External("reviewer".to_string())),
                Some(42),
            ),
        )
        .expect("external author string-equal to target is a distinct principal ⇒ allow");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T6: authority is derived from a LIVE fleet load — reassigning the team's
    /// orchestrator flips the verdict without any restart/cache.
    #[test]
    fn t6_authority_uses_live_fleet_load() {
        let home = tmp_home("t6-live");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n      - lead2\n    source_repo: owner/repo\n",
        );
        let lead = Sender::new("lead").unwrap();
        let lead2 = Sender::new("lead2").unwrap();
        let args = marker_args("owner/repo", 42);
        let checks = marker_checks(None, Some(42));
        // lead is orchestrator ⇒ allowed; lead2 is not ⇒ denied.
        validate_review_assignment_marker(&home, &lead, "reviewer", &args, &checks)
            .expect("current orchestrator allowed");
        assert!(
            validate_review_assignment_marker(&home, &lead2, "reviewer", &args, &checks).is_err()
        );
        // Reassign orchestrator to lead2 via the real teams API (rewrites fleet.yaml).
        let updated =
            crate::teams::update(&home, &json!({"name": "edge", "orchestrator": "lead2"}));
        assert_eq!(updated["status"], "updated", "{updated}");
        // The verdict flips on the very next call — proving a live read.
        assert!(
            validate_review_assignment_marker(&home, &lead, "reviewer", &args, &checks).is_err(),
            "former orchestrator must lose authority after reassignment"
        );
        validate_review_assignment_marker(&home, &lead2, "reviewer", &args, &checks)
            .expect("new orchestrator must gain authority from the live fleet");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T1: an unresolvable repo (malformed slug, no team/instance source_repo)
    /// ⇒ fail-closed reject with NO default.
    #[test]
    fn t1_unresolvable_repo_rejected() {
        let home = tmp_home("t1-repo");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        let sender = Sender::new("lead").unwrap();
        // "single" is a one-component slug the provider-neutral canonicalizer rejects.
        let err = validate_review_assignment_marker(
            &home,
            &sender,
            "reviewer",
            &marker_args("single", 42),
            &marker_checks(None, Some(42)),
        )
        .expect_err("unresolvable repo must fail closed");
        assert_eq!(err["code"], "review_assignment_repo_unresolved", "{err}");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T7: a marker dispatch MISSING task_id (or branch) is atomically rejected at
    /// the full entry point, and `maybe_auto_create_task` is NEVER invoked (zero
    /// board side effects).
    #[test]
    fn t7_missing_task_id_rejects_no_auto_create() {
        let home = tmp_home("t7-taskid");
        seed_fleet(
            &home,
            "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
        );
        let sender = Some(Sender::new("lead").unwrap());
        // review_assignment=true, branch + repo + pr_number present, task_id MISSING.
        let out = handle_delegate_task(
            &home,
            &json!({
                "instance": "reviewer",
                "task": "review the PR",
                "review_assignment": true,
                "branch": "feat/x",
                "repository": "owner/repo",
                "pr_number": 42,
            }),
            &sender,
        );
        assert_eq!(out["code"], "review_assignment_missing_task_id", "{out}");
        // ZERO side effects: no task auto-created on the board.
        let board = crate::tasks::handle(&home, "lead", &json!({"action": "list"}));
        assert!(
            board["tasks"]
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(true),
            "marker reject must NOT auto-create a task: {board}"
        );
        assert!(out.get("auto_created_task_id").is_none(), "{out}");
        std::fs::remove_dir_all(&home).ok();
    }

    /// T17 (B18): a marker dispatch with missing OR zero pr_number is atomically
    /// rejected BEFORE any side effect (no bind/create).
    #[test]
    fn t17_missing_or_zero_pr_number_rejects_no_side_effects() {
        for (label, pr) in [("zero", json!(0)), ("absent", Value::Null)] {
            let home = tmp_home(&format!("t17-{label}"));
            seed_fleet(
                &home,
                "teams:\n  edge:\n    orchestrator: lead\n    members:\n      - lead\n    source_repo: owner/repo\n",
            );
            let sender = Some(Sender::new("lead").unwrap());
            let mut args = json!({
                "instance": "reviewer",
                "task": "review the PR",
                "review_assignment": true,
                "task_id": "t-rev-1",
                "branch": "feat/x",
                "repository": "owner/repo",
            });
            if !pr.is_null() {
                args["pr_number"] = pr;
            }
            let out = handle_delegate_task(&home, &args, &sender);
            assert_eq!(
                out["code"], "review_assignment_missing_pr_number",
                "pr_number {label} must atomically reject: {out}"
            );
            let board = crate::tasks::handle(&home, "lead", &json!({"action": "list"}));
            assert!(
                board["tasks"]
                    .as_array()
                    .map(|a| a.is_empty())
                    .unwrap_or(true),
                "pr_number reject must NOT create a task ({label}): {board}"
            );
            std::fs::remove_dir_all(&home).ok();
        }
    }

    /// C11 (A1→A2): a validated marker dispatch delivers through the DURABLE outbox
    /// store — A1 persists a generation-bound record (assignment_id + nonce +
    /// mandatory pr_number), A2 durable-enqueues the reviewer's ACTIONABLE row (NOT a
    /// `deliver_delegate` send). Drives the store-dispatch stage directly (the bind
    /// stage is orthogonal and covered elsewhere).
    #[test]
    fn c11_marker_path_persists_record_and_enqueues_row() {
        use super::{dispatch_review_assignment_via_store, ComposedDelegate};
        let home = tmp_home("c11-store");
        let sender = Sender::new("lead").unwrap();
        let args = marker_args("owner/repo", 42); // task_id t-rev-1, branch feat/x
        let checks = marker_checks(Some(ReviewAuthor::External("octocat".into())), Some(42));
        let composed = ComposedDelegate {
            msg: "[delegate_task] review the PR".to_string(),
            force_meta_json: None,
            second_reviewer: false,
            plan_ack_required: 0,
        };

        let out = dispatch_review_assignment_via_store(
            &home,
            &sender,
            "reviewer",
            "review the PR",
            &args,
            &checks,
            &composed,
            "owner/repo",
        );

        // A1: a durable, generation-bound record was persisted.
        let rec =
            crate::daemon::assignment_authority::get(&home, "owner/repo", "feat/x", "reviewer")
                .expect("A1 persisted a record");
        assert_eq!(rec.pr_number, 42);
        assert_eq!(rec.sender, "lead");
        assert_eq!(rec.task_id, "t-rev-1");
        assert_eq!(rec.review_author, ReviewAuthor::External("octocat".into()));
        // A2: the reviewer's actionable outbox row carries the record's nonce, and the
        // record advanced to Persisted (delivered by the store, not deliver_delegate).
        assert!(
            crate::inbox::storage::nonce_present_actionable(&home, "reviewer", &rec.delivery_nonce),
            "A2 durable_enqueue delivered the reviewer's actionable row"
        );
        assert_eq!(
            rec.row,
            crate::daemon::assignment_authority::RowState::Persisted
        );
        assert_eq!(out["review_assignment"], true, "{out}");
        assert_eq!(out["pr_number"], 42, "{out}");
        std::fs::remove_dir_all(&home).ok();
    }
}
