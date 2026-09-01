//! Release-time branch cleanup decision.
//!
//! What happens to a branch when its worktree is released: whether the lease
//! was a disposable review checkout, whether the branch is already merged, and
//! — when it survives with work no merge can ever settle — who is accountable
//! for confirming it is still needed. Split out of `worktree_pool.rs` verbatim
//! under the anti-monolith ceiling (`tests/src_file_size_invariant.rs`).
//!
//! The answer side of the obligation raised here lives in
//! `cleanup_intents::owner_attestation`.

use super::{cleanup_merged_branch, task_active_for_branch, ReleaseOutcome};
use std::path::Path;

fn disposable_review_provisioned_head(
    binding: &serde_json::Value,
) -> Result<Option<&str>, &'static str> {
    let has_disposable_field = binding.get("checkout_purpose").is_some()
        || binding.get("provenance").is_some()
        || binding.get("provisioned_head").is_some();
    if !has_disposable_field {
        return Ok(None);
    }
    if binding.get("checkout_purpose").and_then(|v| v.as_str()) != Some("disposable_review")
        || binding.get("provenance").and_then(|v| v.as_str()) != Some("DaemonProvisionedReview")
    {
        return Err("invalid disposable review provenance");
    }
    let head = binding
        .get("provisioned_head")
        .and_then(|v| v.as_str())
        .filter(|head| {
            (head.len() == 40 || head.len() == 64) && head.chars().all(|c| c.is_ascii_hexdigit())
        })
        .ok_or("missing or invalid disposable review provisioned_head")?;
    Ok(Some(head))
}

fn disposable_review_task_terminal(home: &Path, task_id: &str) -> Option<bool> {
    if task_id.is_empty() {
        return None;
    }
    match crate::tasks::load_routed(home, task_id) {
        Ok(routed) => Some(
            routed.task.status.is_terminal()
                || routed.task.status == crate::task_events::TaskStatus::Verified,
        ),
        Err(crate::tasks::TaskRouteError::NotFound)
        | Err(crate::tasks::TaskRouteError::Unreadable { .. })
        | Err(crate::tasks::TaskRouteError::Ambiguous { .. }) => None,
    }
}

/// t-…-43938-5 (d-20260901132326253721-16): tags carried by an owner-facing
/// branch-retention obligation.
pub(crate) const RETENTION_TAG: &str = "branch-retention";
const RETENTION_ORPHAN_TAG: &str = "branch-retention-orphan";
const RETENTION_DUE_DAYS: i64 = 14;

/// Stable idempotency key over the EXACT (repo, branch, head) triple.
///
/// A FULL SHA-256 digest, not a short hash: this key is persisted in a tag and
/// compared across processes and toolchains, so it must be stable forever.
/// `DefaultHasher` is explicitly not — its output may change between Rust
/// releases, which would silently stop matching and duplicate every obligation
/// after a toolchain upgrade. Fields are separated by a NUL, which can appear in
/// neither a path nor a git ref, so no two distinct triples share material.
///
/// Carried as a TAG rather than task metadata deliberately: `TaskEvent::Created`
/// accepts tags but not metadata, so a metadata key would need a second event and
/// a crash between the two would leave a keyless obligation that the next release
/// could not recognise — producing the duplicate row this key exists to prevent.
pub(crate) fn retention_key(repo: &str, branch: &str, head: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(format!("{repo}\0{branch}\0{head}").as_bytes());
    format!("retention-key:{}", hex::encode(hasher.finalize()))
}

/// Record ONE owner-facing obligation for a branch that survived release with
/// work the ledger can never settle by merge. Idempotent on (repo, branch,
/// head): the bounded `branch-retention` tag scan plus the exact key tag makes a
/// second release at the same head a no-op.
///
/// Escheatment, because agents are ephemeral: the recorded owner takes it when
/// live; otherwise the originating task's `created_by` orchestrator; otherwise
/// nobody, tagged for board hygiene. Liveness that cannot be determined at all
/// (empty live set — daemon unreachable) is treated as "owner is live": the
/// operator's rule makes the branch opener accountable, and reassigning on
/// ignorance would silently move that accountability.
fn record_retention_obligation(
    home: &Path,
    repo: &str,
    branch: &str,
    head: &str,
    origin_task_id: &str,
    owner: &str,
) {
    if owner.is_empty() || head.is_empty() {
        return;
    }
    let key_tag = retention_key(repo, branch, head);
    // Bounded by the `branch-retention` tag, and read from the SAME board
    // `task_events::append` writes to below — a different board would make the
    // key invisible and duplicate the row on every release.
    let board = crate::task_events::board_root(home, crate::task_events::DEFAULT_PROJECT);
    let existing = crate::tasks::list_all_at_checked(home, &board)
        .map(|tasks| {
            tasks
                .iter()
                .any(|t| t.tags.iter().any(|tag| tag == RETENTION_TAG) && t.tags.contains(&key_tag))
        })
        .unwrap_or(false);
    if existing {
        return;
    }

    let live = crate::runtime::list_agents_with_fallback(home);
    let is_live = |name: &str| live.is_empty() || live.iter().any(|a| a == name);
    let created_by = (!origin_task_id.is_empty())
        .then(|| crate::tasks::load_routed(home, origin_task_id).ok())
        .flatten()
        .map(|routed| routed.task.created_by);
    let assignee = if is_live(owner) {
        Some(owner.to_string())
    } else {
        created_by.filter(|orchestrator| is_live(orchestrator))
    };

    let mut tags = vec![RETENTION_TAG.to_string(), key_tag];
    if assignee.is_none() {
        tags.push(RETENTION_ORPHAN_TAG.to_string());
    }

    use std::sync::atomic::{AtomicU64, Ordering};
    static RETENTION_SEQ: AtomicU64 = AtomicU64::new(0);
    let ts = chrono::Utc::now().format("%Y%m%d%H%M%S%6f");
    let seq = RETENTION_SEQ.fetch_add(1, Ordering::Relaxed);
    let id = format!("t-{ts}-{}-{seq}", std::process::id());
    let event = crate::task_events::TaskEvent::Created {
        task_id: crate::task_events::TaskId(id),
        title: format!("branch-retention: confirm '{branch}' is still needed"),
        description: format!(
            "Released with unmerged work and no merge authority is possible on this lane, \
             so the cleanup intent can never settle by merge.\n\n\
             repo: {repo}\nbranch: {branch}\noriginating task: {origin_task_id}\nhead: {head}\n\n\
             Answer with `task action=done` and a `result` beginning `keep:` or `delete:`."
        ),
        priority: "normal".to_string(),
        owner: assignee.map(crate::task_events::InstanceName),
        due_at: Some(
            (chrono::Utc::now() + chrono::Duration::days(RETENTION_DUE_DAYS)).to_rfc3339(),
        ),
        depends_on: Vec::new(),
        routed_to: None,
        // Deliberately branchless. `assignee_completion_guard` gates a
        // BRANCH-carrying task on the assignee holding an exact live binding —
        // which the owner no longer has once the worktree is released, so a
        // branch-carrying obligation would be unanswerable by the very agent it
        // holds accountable. The branch is still carried in the description and
        // in the retention key.
        branch: None,
        bind: None,
        eta_secs: None,
        tags,
        parent_id: None,
        governing_decision_id: None,
        review_class: None,
    };
    if let Err(error) = crate::task_events::append(
        home,
        &crate::task_events::InstanceName("system:branch-retention".to_string()),
        event,
    ) {
        tracing::warn!(%repo, %branch, %error, "branch-retention obligation could not be recorded");
    }
}

pub(super) fn resolve_branch_cleanup(
    home: &Path,
    binding: &serde_json::Value,
    managed_verified: bool,
    worktree_absent: bool,
    dry_run: bool,
    was_dirty: bool,
    out: &mut ReleaseOutcome,
) {
    let branch = binding["branch"].as_str().unwrap_or("");
    let sr_str = binding["source_repo"].as_str().unwrap_or("");
    let task_id = binding["task_id"].as_str().unwrap_or("");
    let disposable_head = match disposable_review_provisioned_head(binding) {
        Ok(head) => head,
        Err(reason) => {
            out.branch_cleanup_skipped_reason = Some(format!(
                "disposable review provenance {reason} — preserved (fail-closed)"
            ));
            return;
        }
    };
    if !managed_verified && !worktree_absent {
        out.branch_cleanup_skipped_reason =
            Some("cannot verify .agend-managed marker — skipping branch cleanup".to_string());
    } else if !branch.is_empty() && !sr_str.is_empty() {
        // Authority-proven review lease: lease_kind + review_assignment_id + expected_head
        // all present → eligible for immediate delete with expected-head CAS. A
        // daemon-provisioned disposable review uses the same strict lifecycle
        // classifier, but its provenance is independent of assignment_authority.
        // Dirty work → never auto-delete regardless of provenance.
        let authority_proven_head = if was_dirty {
            None
        } else {
            binding
                .get("lease_kind")
                .and_then(|v| v.as_str())
                .filter(|&k| k == "review")
                .and(
                    binding
                        .get("review_assignment_id")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty()),
                )
                .and(
                    binding
                        .get("expected_head")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty()),
                )
        };
        let review_head = disposable_head.or(authority_proven_head);
        if was_dirty
            && (disposable_head.is_some()
                || binding
                    .get("lease_kind")
                    .and_then(|v| v.as_str())
                    .is_some_and(|k| k == "review"))
        {
            out.branch_cleanup_skipped_reason = Some(format!(
                "authority-proven review lease on '{branch}' had dirty work — branch preserved"
            ));
            return;
        }
        if let Some(expected) = review_head {
            let default = crate::git_helpers::default_branch(Path::new(sr_str));
            let task_active = if disposable_head.is_some() {
                disposable_review_task_terminal(home, task_id).map(|terminal| !terminal)
            } else {
                task_active_for_branch(home, task_id, branch)
            };
            let open_pr =
                match crate::branch_sweep::open_pr_status(Path::new(sr_str), &default, branch) {
                    crate::branch_sweep::OpenPrStatus::Open => Some(true),
                    crate::branch_sweep::OpenPrStatus::NotOpen => Some(false),
                    crate::branch_sweep::OpenPrStatus::Unknown => None,
                };
            let unique_unpreserved_work =
                crate::git_helpers::git_cmd(Path::new(sr_str), &["rev-parse", branch])
                    .ok()
                    .map(|tip| tip.trim() != expected);
            let active_holder = crate::worktree_cleanup::branch_has_other_active_binding(
                home,
                Path::new(sr_str),
                branch,
                binding["worktree"].as_str(),
            );
            let lifecycle = crate::worktree::disposition::branch_lifecycle_disposition(
                &crate::worktree::disposition::BranchLifecycleInput {
                    provenance: crate::worktree::disposition::BranchProvenance::ManagedReview,
                    terminal: true,
                    active_holder,
                    task_active,
                    open_pr,
                    unique_unpreserved_work,
                },
            );
            if !matches!(
                lifecycle,
                crate::worktree::disposition::BranchLifecycleDisposition::Delete
            ) {
                // #3090: a LIVE task is a temporary blocker — this is managed
                // review residue that becomes reapable the moment the task goes
                // terminal, and the release attempt is the only automatic one,
                // firing at the one moment the evidence cannot yet be terminal.
                // Record the intent so the existing terminal-review reconcile
                // sweep gets that second chance. Every other blocker still
                // preserves with no intent, exactly as before.
                if task_active == Some(true) && !dry_run {
                    out.intent_persist_error = crate::cleanup_intents::persist_release_intent(
                        home, sr_str, branch, task_id,
                    );
                }
                out.branch_cleanup_skipped_reason = Some(format!(
                    "authority-proven review branch '{branch}' lifecycle evidence is not terminal — preserved (fail-closed)"
                ));
                return;
            }
            if !dry_run {
                if let Err(error) = crate::branch_sweep::prepare_branch_recovery(
                    Some(home),
                    Path::new(sr_str),
                    branch,
                    expected,
                    "authority-proven review release",
                ) {
                    out.branch_cleanup_skipped_reason = Some(error);
                    return;
                }
            }
        }
        let (deleted, skip_reason) = cleanup_merged_branch(
            Path::new(sr_str),
            branch,
            dry_run,
            // A daemon-provisioned disposable review has the same strict
            // expected-head CAS as an assignment-authority review lease. Once
            // its lifecycle gates pass, pass that provenance head through to
            // the deletion primitive so the branch is reaped without a PR
            // merge signal (review scaffolding is intentionally unmerged).
            review_head,
        );
        out.branch_deleted = deleted;
        out.branch_cleanup_skipped_reason = skip_reason.clone();
        // Cleanup intent: clean feature branch released pre-merge → persist
        // intent so it can be settled on pr-merged event or periodic sweep.
        // Dirty branches get no intent (preserved permanently).
        if !deleted && !was_dirty && !dry_run {
            out.intent_persist_error =
                crate::cleanup_intents::persist_release_intent(home, sr_str, branch, task_id);
            // t-…-43938-5: the intent above can only ever be settled by `merged`,
            // which a local-only lane never produces — so ask the recorded owner.
            // Runs in the post-lock notice phase (every flock is already dropped
            // above), so the task-board write takes no worktree/branch/binding lock.
            let head = crate::git_helpers::git_cmd(Path::new(sr_str), &["rev-parse", branch])
                .map(|tip| tip.trim().to_string())
                .unwrap_or_default();
            record_retention_obligation(
                home,
                sr_str,
                branch,
                &head,
                task_id,
                binding["agent"].as_str().unwrap_or(""),
            );
        }
    } else if branch.is_empty() {
        out.branch_cleanup_skipped_reason = Some("no branch in binding".to_string());
    } else {
        out.branch_cleanup_skipped_reason = Some("no source_repo in binding".to_string());
    }
}
