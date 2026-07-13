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
///       (generation identity; B18) — else reject BEFORE resolving anything;
///   (b) provider-neutral repo resolve (fail-closed, no default);
///   (c) source-repo → EXACTLY-ONE team, and sender == that team's SOLE CURRENT
///       orchestrator (live fleet; NO operator-allow);
///   (d) review_author self-review deny — `Agent(author) == reviewer target` is
///       same-namespace self-review; `External(login)` is a distinct principal and
///       is NEVER string-compared to the agent target. (sender == target is already
///       rejected upstream in `resolve_delegate`.)
pub(super) fn validate_review_assignment_marker(
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
    // The record's `review_class` is authority METADATA (the gate/classifier read the
    // PrState's class, never this) — resolve it from the task's DURABLE metadata (the
    // same authority `maybe_auto_bind_lease` already validated and armed); an untagged
    // task degrades to `Unresolved` (harmless: metadata-only).
    // #2760: read via the STRICT router (project-board aware). A route error degrades
    // to `Unresolved` — harmless here (metadata-only), matching the untagged case.
    let review_class = crate::tasks::load_routed(home, task_id)
        .ok()
        .and_then(|rt| {
            rt.task
                .metadata
                .get("review_class")
                .and_then(|v| v.as_str())
                .map(|s| ReviewClass::parse_fail_closed(Some(s)))
        })
        .unwrap_or(ReviewClass::Unresolved);
    // `review_author` is MANDATORY on the marker path: the gate
    // (`validate_review_assignment_marker`) fail-closes on an absent author BEFORE
    // this A1 wiring runs, so it is guaranteed present here. NO sentinel principal is
    // ever constructed — the store always records the real audited author.
    let review_author = checks
        .review_author
        .clone()
        .expect("marker gate enforces review_author present before store dispatch");
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
