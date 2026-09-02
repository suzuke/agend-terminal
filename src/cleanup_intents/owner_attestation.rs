//! The owner-attested settlement authority for a cleanup intent no merge can
//! settle — t-20260901084057482124-43938-5 B3/B4 (d-20260901132326253721-16).
//!
//! Split out of `cleanup_intents.rs` verbatim under the anti-monolith ceiling
//! (`tests/src_file_size_invariant.rs`). The obligation this consumes is raised
//! by `worktree_pool::branch_cleanup`.

use super::{intent_key, intents_dir, is_branch_checked_out, CleanupIntent};
use std::path::Path;

/// The two answers a branch-retention obligation accepts, matched exactly:
/// `Delete:` or `deleted` is not an authorization.
const ATTESTED_DELETE: &str = "delete:";
const ATTESTED_KEEP: &str = "keep:";
/// A `keep:` is renewable but never permanent, or the mechanism degrades into
/// ceremony. The term runs from the answer, not from the release.
pub(super) const KEEP_TTL_DAYS: i64 = 14;
const RETENTION_ACTOR: &str = "system:branch-retention";

/// t-20260901084057482124-43938-5 B3/B4 (d-20260901132326253721-16): the
/// recorded owner's answer, the SECOND settlement authority for a cleanup
/// intent that no merge can ever settle.
///
/// A sibling of [`settle_intent`] at the seam where merge evidence is absent —
/// the merge authority is neither called nor widened here, so `merged == true`
/// remains the only thing that path accepts.
///
/// `delete:` authorizes deletion only when ALL of these hold: exactly one
/// obligation carries the exact (repo, branch, head) key; it is terminal; it
/// names an accountable assignee; the branch still stands at the attested head;
/// no worktree holds it; and its tip is archived first. `can_mutate_record`
/// (tasks/acl.rs:112-139) returns true for ANY caller when the owner is `None`,
/// so an unassigned obligation is refused rather than treated as merely
/// unattributed — anybody at all could have written that answer.
///
/// `keep:` reopens the SAME obligation once its term runs out, measured from
/// the LATEST terminal answer so a renewal starts a new one. Nothing is written
/// to the ledger for a keep: the obligation is the only durable state.
///
/// Every other shape — no obligation, an ambiguous one, a non-terminal one, a
/// malformed answer, head drift, a held branch, a failed archive, any IO error
/// — preserves branch, intent and obligation alike.
pub(super) fn settle_by_owner_attestation(
    home: &Path,
    intent: &CleanupIntent,
    now: chrono::DateTime<chrono::Utc>,
    obligations: &[crate::task_events::TaskRecord],
) {
    let Some(obligation) = exact_retention_obligation(obligations, intent) else {
        return;
    };
    if obligation.status != crate::task_events::TaskStatus::Done {
        return;
    }
    // Leading whitespace is not a malformed answer; the prefix and its case are
    // matched exactly.
    let answer = obligation
        .result
        .as_deref()
        .unwrap_or_default()
        .trim_start();
    if answer.starts_with(ATTESTED_DELETE) {
        reap_owner_attested(home, intent, obligation);
    } else if answer.starts_with(ATTESTED_KEEP) {
        reopen_expired_keep(home, intent, obligation, now);
    }
}

/// Every branch-retention obligation on the board.
///
/// Read from the SAME board `record_retention_obligation` writes to; a
/// different board would make every answer invisible. The tag narrows WHAT is
/// cloned; the exact retention key, not this tag, is what authorizes anything
/// downstream.
pub(super) fn retention_obligations(
    home: &Path,
    origin_task_id: &str,
) -> Vec<crate::task_events::TaskRecord> {
    let project = if origin_task_id.is_empty() {
        crate::task_events::DEFAULT_PROJECT.to_string()
    } else {
        let Ok(routed) = crate::tasks::load_routed(home, origin_task_id) else {
            tracing::warn!(
                %origin_task_id,
                "branch-retention origin could not be routed — answers preserved fail-closed"
            );
            return Vec::new();
        };
        routed.project().to_string()
    };
    let board = crate::task_events::board_root(home, &project);
    let Ok(state) = crate::task_events::projected_state_at(&board) else {
        return Vec::new();
    };
    state
        .tasks
        .values()
        .filter(|task| {
            task.tags
                .iter()
                .any(|tag| tag == crate::worktree_pool::RETENTION_TAG)
        })
        .cloned()
        .collect()
}

/// The obligation raised for this exact (repo, branch, head) triple, or `None`
/// when there is not exactly one. Two rows carrying one key cannot say which
/// answer the owner gave, so ambiguity authorizes nothing.
fn exact_retention_obligation<'a>(
    obligations: &'a [crate::task_events::TaskRecord],
    intent: &CleanupIntent,
) -> Option<&'a crate::task_events::TaskRecord> {
    let key_tag =
        crate::worktree_pool::retention_key(&intent.repo, &intent.branch, &intent.expected_head);
    let mut matched = obligations
        .iter()
        .filter(|task| task.tags.contains(&key_tag));
    let obligation = matched.next()?;
    if matched.next().is_some() {
        tracing::warn!(
            branch = %intent.branch,
            "several branch-retention obligations carry one exact key — preserved"
        );
        return None;
    }
    Some(obligation)
}

/// Delete the branch on the owner's authority, through the same primitives the
/// merge path uses: exact-head CAS and archive-before-delete. Every refusal
/// leaves branch, intent and obligation exactly as they were, so the next sweep
/// retries once the obstacle clears.
fn reap_owner_attested(
    home: &Path,
    intent: &CleanupIntent,
    obligation: &crate::task_events::TaskRecord,
) {
    let branch = intent.branch.as_str();
    let Some(owner) = obligation
        .owner
        .as_ref()
        .map(|owner| owner.0.trim())
        .filter(|owner| !owner.is_empty())
    else {
        tracing::warn!(
            %branch, task = %obligation.id.0,
            "branch-retention delete: carries no accountable owner — preserved"
        );
        return;
    };
    let repo = Path::new(&intent.repo);
    if !repo.is_dir() {
        return;
    }
    // Typed branch-existence check (show-ref exit 0/1/other): only a confirmed
    // present branch is a deletion candidate; anything unreadable preserves.
    let full_ref = format!("refs/heads/{branch}");
    match crate::git_helpers::git_bypass(repo, &["show-ref", "--verify", "--quiet", &full_ref]) {
        Ok(out) if out.status.code() == Some(0) => {}
        _ => return,
    }
    let tip = match crate::git_helpers::git_cmd(repo, &["rev-parse", branch]) {
        Ok(tip) if !tip.trim().is_empty() => tip.trim().to_string(),
        _ => return,
    };
    // Fail fast on drift — which is also what keeps a branch that will not be
    // deleted from being archived. The `update-ref -d <ref> <old>` below is the
    // atomic half of the same guard: it closes the window between this rev-parse
    // and the delete, which no in-process test can observe.
    if tip != intent.expected_head {
        tracing::warn!(
            %branch, attested = %intent.expected_head, actual = %tip,
            "branch moved since the owner answered — preserved (fail-closed)"
        );
        return;
    }
    if is_branch_checked_out(repo, branch) {
        tracing::warn!(%branch, "branch is still checked out — preserved");
        return;
    }
    let recovery = match crate::branch_sweep::prepare_branch_recovery(
        Some(home),
        repo,
        branch,
        &tip,
        "owner-attested delete",
    ) {
        Ok(recovery) => recovery,
        Err(error) => {
            tracing::warn!(%branch, %error, "branch could not be archived — preserved");
            return;
        }
    };
    // Atomic CAS delete: the ref goes only if it still holds the attested head.
    match crate::git_helpers::git_bypass(
        repo,
        &["update-ref", "-d", &full_ref, &intent.expected_head],
    ) {
        Ok(out) if out.status.success() => {
            let key = intent_key(&intent.repo, branch);
            let _ = std::fs::remove_file(intents_dir(home).join(format!("{key}.json")));
            crate::event_log::log(
                home,
                "branch_retention_settled",
                RETENTION_ACTOR,
                &format!(
                    "repo={} branch={branch} head={tip} owner={owner} \
                     obligation={} recovery_ref={recovery}",
                    intent.repo, obligation.id.0
                ),
            );
        }
        _ => tracing::warn!(%branch, "owner-attested delete failed — preserved for retry"),
    }
}

/// Reopen the obligation once the `keep:` term has run out. The term starts at
/// the LATEST terminal transition, so a renewal buys a fresh one; the original
/// `due_at` is by then in the past, which is what surfaces the row as overdue.
fn reopen_expired_keep(
    home: &Path,
    intent: &CleanupIntent,
    obligation: &crate::task_events::TaskRecord,
    now: chrono::DateTime<chrono::Utc>,
) {
    let Some(attested_at) = obligation
        .history
        .iter()
        .rev()
        .find(|entry| entry.kind == "done")
        .and_then(|entry| chrono::DateTime::parse_from_rfc3339(&entry.timestamp).ok())
        .map(|at| at.with_timezone(&chrono::Utc))
    else {
        return;
    };
    if now.signed_duration_since(attested_at) < chrono::Duration::days(KEEP_TTL_DAYS) {
        return;
    }
    let event = crate::task_events::TaskEvent::Reopened {
        task_id: obligation.id.clone(),
        reason: format!(
            "keep: attestation for '{}' expired after {KEEP_TTL_DAYS}d — confirm again",
            intent.branch
        ),
        source_evidence: format!(
            "repo={} branch={} head={} attested_at={}",
            intent.repo,
            intent.branch,
            intent.expected_head,
            attested_at.to_rfc3339()
        ),
    };
    if let Err(error) = crate::task_events::append(
        home,
        &crate::task_events::InstanceName(RETENTION_ACTOR.to_string()),
        event,
    ) {
        tracing::warn!(
            branch = %intent.branch, %error,
            "expired branch-retention keep could not be reopened"
        );
    }
}
