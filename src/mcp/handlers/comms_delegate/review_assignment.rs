//! t-…-17 reviewer-assignment marker gate + durable-store dispatch.
//!
//! Split out of `mod.rs` (behavior-preserving) so the handler stays under the
//! `file_size_invariant` LOC cap. Both fns are `pub(super)` — only the parent
//! `handle_delegate_task` choreography calls them. They reach the parent-private
//! `ComposedDelegate` / `DeliveryCtx` / `track_delegate_success` via `super::`.

use super::{track_delegate_success, ComposedDelegate, DeliveryCtx};
use crate::daemon::pr_state::ReviewClass;
use crate::identity::Sender;
use crate::mcp::handlers::comms_gates::{DispatchPreChecks, ReviewAuthor};
use crate::mcp::handlers::dispatch_hook;
use serde_json::{json, Value};
use std::path::Path;

/// t-…-17 reviewer-assignment marker gate. Runs BETWEEN `run_dispatch_pre_checks`
/// and `maybe_auto_bind_lease` — i.e. AFTER the generic pre-send gates but BEFORE
/// any bind / task-create / deliver side effect — and ATOMICALLY REJECTS on the
/// FIRST failure so a marker dispatch that fails authority never mutates state.
/// Only invoked when `checks.review_assignment` is true; an ordinary dispatch never
/// enters here (byte-identical legacy path).
///
/// Order is load-bearing (fail-closed):
///   (a) explicit non-empty `task_id` + non-empty `branch` + nonzero `pr_number`
///       + exact full `reviewed_head` (generation identity) — else reject BEFORE
///       resolving anything;
///   (b) provider-neutral repo resolve (fail-closed, no default);
///   (c) source-repo → EXACTLY-ONE team, and sender == that team's SOLE CURRENT
///       orchestrator (live fleet; NO operator-allow);
///   (d) review_author self-review deny — `Agent(author) == reviewer target` is
///       same-namespace self-review; `External(login)` is a distinct principal and
///       is NEVER string-compared to the agent target. (sender == target is already
///       rejected upstream in `resolve_delegate`.)
// #6: pub(crate) so ci/review_workspace_tests can drive bind rejection tests.
pub(crate) fn validate_review_assignment_marker(
    home: &Path,
    sender: &Sender,
    target: &str,
    args: &Value,
    checks: &DispatchPreChecks,
) -> Result<String, Value> {
    // #6: review_assignment must not bind the reviewer to the implementer's branch.
    // Workspace provisioning is a separate concern — reviewers get isolated worktrees
    // via `repo checkout` instead.
    if args.get("bind").and_then(|v| v.as_bool()) == Some(true) {
        return Err(json!({
            "error": "review_assignment must not bind the reviewer to the subject branch \
                      — use an isolated review branch via repo checkout instead",
            "code": "review_assignment_bind_rejected",
        }));
    }
    if args
        .get("worktree_binding_required")
        .and_then(|v| v.as_bool())
        == Some(true)
    {
        return Err(json!({
            "error": "review_assignment does not support worktree_binding_required \
                      — reviewer workspace provisioning is separate",
            "code": "review_assignment_worktree_binding_rejected",
        }));
    }
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
    let reviewed_head = args["reviewed_head"].as_str().unwrap_or("");
    if !crate::review_receipt::is_full_head(reviewed_head) {
        return Err(json!({
            "error": "review_assignment requires `reviewed_head` as an exact full 40/64-hex SHA",
            "code": "review_assignment_missing_exact_head",
        }));
    }
    // `review_author` is a MANDATORY audited principal on a review_assignment (codex
    // ruling): the reviewer must be told WHOSE code they are auditing, and the
    // self-review deny in step (d) is only meaningful when the author is known.
    // Absent ⇒ fail closed HERE, before any repo/ACL/side-effect work — there is NO
    // empty-principal sentinel anywhere.
    if checks.review_author.is_none() {
        return Err(json!({
            "error": "review_assignment requires an explicit `review_author` — the \
                      audited code-author principal (`{\"agent\": <name>}` for a fleet \
                      author, or `{\"external\": <login>}` for an external one)",
            "code": "review_assignment_missing_review_author",
        }));
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
    // Bind the dispatch to the currently observed PR subject before any bind,
    // assignment-store, or inbox side effect. Missing/corrupt state, a stale
    // head, wrong PR, or unresolved/mismatched review class all fail closed.
    let branch = args["branch"].as_str().unwrap_or_default();
    let pr_number = checks.pr_number.unwrap_or_default();
    let state =
        crate::review_receipt::load_pr_state_strict(home, &repo_slug, branch).map_err(|error| {
            json!({
                "error": format!("review_assignment exact subject rejected: {error}"),
                "code": "review_assignment_subject_unavailable",
            })
        })?;
    let task_id = args["task_id"].as_str().unwrap_or_default();
    let task_review_class = crate::tasks::load_routed(home, task_id)
        .ok()
        .and_then(|routed| {
            routed
                .task
                .metadata
                .get("review_class")
                .and_then(|value| value.as_str())
                .map(|value| ReviewClass::parse_fail_closed(Some(value)))
        })
        .unwrap_or(ReviewClass::Unresolved);
    if state.repo != repo_slug
        || state.branch != branch
        || state.pr_number != pr_number
        || state.head_sha != reviewed_head
        || matches!(task_review_class, ReviewClass::Unresolved)
        || state.review_class != task_review_class
    {
        return Err(json!({
            "error": "review_assignment subject must exactly match the active PR and task review class",
            "code": "review_assignment_subject_mismatch",
        }));
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
pub(super) fn dispatch_review_assignment_via_store(
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
    // The marker gate has already proved this exact routed task metadata matches
    // the current PR state; re-read it only to populate the durable assignment.
    let review_class = crate::tasks::load_routed(home, task_id)
        .ok()
        .and_then(|rt| {
            rt.task
                .metadata
                .get("review_class")
                .and_then(|v| v.as_str())
                .map(|s| ReviewClass::parse_fail_closed(Some(s)))
        })
        .expect("marker gate requires resolvable task review_class");
    // `review_author` is MANDATORY on the marker path: the gate
    // (`validate_review_assignment_marker`) fail-closes on an absent author BEFORE
    // this A1 wiring runs, so it is guaranteed present here. NO sentinel principal is
    // ever constructed — the store always records the real audited author.
    let review_author = checks
        .review_author
        .clone()
        .expect("marker gate enforces review_author present before store dispatch");
    let target_instance_id = match crate::fleet::resolve_uuid(home, target) {
        Some(id) => id,
        None => {
            return json!({
                "ok": false,
                "error": "review_assignment target has no stable InstanceId",
                "code": "review_assignment_target_identity_missing",
            })
        }
    };
    let reviewed_head = args["reviewed_head"].as_str().unwrap_or_default();
    let slot = if checks.second_reviewer {
        crate::review_receipt::ReviewSlot::Secondary
    } else {
        crate::review_receipt::ReviewSlot::Primary
    };
    let now = chrono::Utc::now().to_rfc3339();
    let record = crate::daemon::assignment_authority::ActiveAssignment::new_pending_typed(
        repo_slug,
        branch,
        target,
        target_instance_id,
        pr_number,
        reviewed_head,
        slot,
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
        "reviewed_head": reviewed_head,
        "target_instance_id": target_instance_id.full(),
        "review_slot": slot,
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

/// #2782 slice 1: MCP `revoke_review_assignment` — revoke an active reviewer
/// assignment by EXACT `assignment_id` (the complement of
/// [`dispatch_review_assignment_via_store`]). `pub(crate)` (not `pub(super)`) so
/// non-sibling test modules (`daemon::assignment_authority::tests`) can drive it
/// directly via the crate-visible `mcp::handlers::review_assignment` re-export.
///
/// Authorization DIFFERS from dispatch: an ABSENT sender is operator-direct (full
/// authority, no operator-allow needed because there is no fleet identity to
/// check); a PRESENT sender must be the EXACT-CURRENT orchestrator of the team
/// that owns the assignment's `repo` (same authority source as dispatch's marker
/// gate, `crate::teams::resolve_team_by_source_repo`, but revoke additionally
/// allows the operator path that dispatch never does).
///
/// Idempotent by design: a missing / already-revoked / terminal-generation
/// `assignment_id` is `{"ok": true, "already_absent": true}`, never an error — a
/// retried revoke (crash/timeout on the caller side) must never surface as a
/// failure. After a successful retire, `redrive_reserved` recomputes merge
/// readiness for the branch so a now-satisfied gate is not left stale.
pub(crate) fn handle_revoke_review_assignment(
    home: &Path,
    args: &Value,
    sender: &Option<Sender>,
) -> Value {
    let Some(assignment_id) = args["assignment_id"]
        .as_str()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
    else {
        return json!({
            "error": "revoke_review_assignment requires `assignment_id` as a valid UUID",
            "code": "revoke_assignment_invalid_id",
        });
    };

    // Strict store-wide lookup. Distinguish idempotent-safe absence (not found,
    // terminal generation) from store integrity failures (corrupt, unreadable,
    // duplicate UUID) — the latter must fail closed, never report success while
    // a live assignment may remain.
    let record = match crate::daemon::assignment_authority::lookup_by_assignment_id_strict(
        home,
        assignment_id,
    ) {
        Ok(r) => r,
        Err(e) => {
            let msg = e.to_string();
            // stringly-allow: lookup_by_assignment_id_strict returns anyhow::Error with no typed variant; "not found"/"terminal" are idempotent-safe absence
            if msg.contains("not found") || msg.contains("terminal") {
                return json!({"ok": true, "already_absent": true});
            }
            return json!({
                "ok": false,
                "error": format!("revoke_review_assignment: store integrity error — {msg}"),
                "code": "revoke_assignment_store_integrity",
            });
        }
    };

    // Authorization: operator-direct (sender=None) has full authority. A named
    // sender must be the SOLE CURRENT orchestrator of the team owning the
    // assignment's repo — an unresolvable/ambiguous team authority also fails
    // closed as not-authorized (a named sender can never fall back to
    // operator-equivalent trust just because the ACL is unclear).
    if let Some(caller) = sender.as_ref() {
        let authorized = match crate::teams::resolve_team_by_source_repo(home, &record.repo) {
            Ok(team) => team.orchestrator.as_deref() == Some(caller.as_str()),
            Err(_) => false,
        };
        if !authorized {
            return json!({
                "error": format!(
                    "revoke_review_assignment rejected: sender `{}` is not the current \
                     orchestrator of the team owning `{}` — only the owning team's \
                     orchestrator or the operator (no sender) may revoke",
                    caller.as_str(),
                    record.repo
                ),
                "code": "revoke_assignment_not_authorized",
            });
        }
    }

    let now = chrono::Utc::now().to_rfc3339();
    let revoked = match crate::daemon::assignment_authority::retire_if_id_matches(
        home,
        &record.repo,
        &record.branch,
        &record.target,
        assignment_id,
        &now,
    ) {
        Ok(true) => true,
        Ok(false) => {
            // retire returned no-op — verify the record is genuinely absent or
            // replaced (idempotent) vs. corrupt/unreadable (fail closed). The
            // generic retire collapses corruption into Ok(false); the MCP tool
            // must not report success when a live assignment may remain.
            match crate::daemon::assignment_authority::get_strict(
                home,
                &record.repo,
                &record.branch,
                &record.target,
            ) {
                Ok(None) => false,
                Ok(Some(r)) if r.assignment_id != assignment_id => false,
                Ok(Some(_)) => {
                    return json!({
                        "ok": false,
                        "error": "revoke_review_assignment: retire no-op but matching record still active",
                        "code": "revoke_assignment_store_integrity",
                    });
                }
                Err(e) => {
                    return json!({
                        "ok": false,
                        "error": format!(
                            "revoke_review_assignment: store integrity error on post-retire check — {e}"
                        ),
                        "code": "revoke_assignment_store_integrity",
                    });
                }
            }
        }
        Err(e) => {
            return json!({
                "ok": false,
                "error": format!("revoke_review_assignment retire failed: {e}"),
                "code": "revoke_assignment_retire_failed",
            });
        }
    };
    // Recompute merge readiness for the branch now that the assignment is gone.
    crate::daemon::assignment_authority::redrive_reserved(home, &record.repo, &record.branch);

    json!({
        "ok": true,
        "assignment_id": assignment_id.to_string(),
        "target": record.target,
        "repo": record.repo,
        "branch": record.branch,
        "revoked": revoked,
    })
}
