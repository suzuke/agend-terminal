//! Worktree pool — daemon-managed lease/release lifecycle for git worktrees.
//!
//! Builds on existing `worktree.rs` (creation) + `binding.rs` (state).
//! Phase 3: lease/release + daemon-tag + E4.5 enforcement. GC deferred to Phase 4.

use std::path::{Path, PathBuf};

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReleaseTestPhase {
    AfterBindingSnapshot,
    BeforeWorktreeRemove,
    BeforeNoticeEmit,
    CheckoutBoundBeforeCommit,
}

#[cfg(test)]
pub(crate) mod release_test_seam {
    use super::ReleaseTestPhase;
    use std::cell::RefCell;

    type ReleaseHook = Box<dyn Fn(ReleaseTestPhase)>;

    thread_local! {
        static HOOK: RefCell<Option<ReleaseHook>> = RefCell::new(None);
    }

    pub(crate) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            HOOK.with(|slot| *slot.borrow_mut() = None);
        }
    }

    pub(crate) fn install(hook: impl Fn(ReleaseTestPhase) + 'static) -> Guard {
        HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        Guard
    }

    pub(crate) fn hit(phase: ReleaseTestPhase) {
        HOOK.with(|slot| {
            if let Some(hook) = slot.borrow().as_ref() {
                hook(phase);
            }
        });
    }
}

// #1639: the daemon-internal git wrapper lives in `git_helpers::git_bypass`
// (the #781-centralized single source for the `AGEND_GIT_BYPASS=1` contract).
// Call sites below go through it directly rather than a local copy.

/// Marker file placed in daemon-managed worktrees (R14 mitigation).
pub(crate) const MANAGED_MARKER: &str = ".agend-managed";

/// Root directory for daemon-managed worktrees in the new layout.
/// `<home>/worktrees/` — contains `<agent>/<branch>/` subdirectories.
/// Used by lease, gc_candidates, and reconcile_hooks.
pub fn daemon_managed_worktree_root(home: &Path) -> PathBuf {
    home.join("worktrees")
}

/// A lease on a worktree — returned by `lease()`, consumed by `release()`.
#[derive(Debug, Clone)]
pub struct WorktreeLease {
    pub agent: String,
    pub branch: String,
    pub path: PathBuf,
}

/// Typed failure modes of [`lease`] (arch-review finding D+H): replaces the
/// stringly error + the silent `Ok`-on-bind-failure. The dispatch boundary
/// matches these variants instead of `msg.contains("E4.5")`.
#[derive(Debug)]
pub enum LeaseError {
    /// E4.5: `branch` is a protected ref (main/master/…) — never leasable.
    ProtectedBranch(String),
    /// `worktree::create` failed (not a git repo / invalid name / git worktree add / checkout).
    CreateFailed(String),
}
impl LeaseError {
    /// `true` only for the E4.5 protected-branch variant — the dispatch boundary
    /// maps this to a distinct `ErrorCode` (vs the generic lease conflict), via a
    /// TYPED variant check rather than a stringly `msg.contains("E4.5")`.
    pub fn is_protected_branch(&self) -> bool {
        matches!(self, LeaseError::ProtectedBranch(_))
    }
}
impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseError::ProtectedBranch(m) | LeaseError::CreateFailed(m) => write!(f, "{m}"),
        }
    }
}
impl std::error::Error for LeaseError {}

/// Lease a worktree for an agent + branch. Creates if needed, tags as daemon-managed.
/// Rejects `main` branch per E4.5 enforcement.
///
/// Returns the worktree path only — it does NOT write binding.json. The binding
/// (with worktree + source-repo paths) is written by the authoritative caller
/// AFTER leasing (dispatch's checked `bind_full` + reused-aware rollback —
/// arch-review finding D+H). `source_repo` is still required to create the
/// worktree, but is persisted into the binding by the caller, not here.
pub fn lease(
    home: &Path,
    source_repo: &Path,
    agent: &str,
    branch: &str,
) -> Result<WorktreeLease, LeaseError> {
    crate::agent_ops::ensure_not_protected(branch).map_err(LeaseError::ProtectedBranch)?;

    // Create worktree using existing infrastructure. Sprint 57 Wave 4
    // (#546 Item 4): the new external layout requires `home` to
    // resolve the canonical path `$AGEND_HOME/worktrees/<agent>/<branch>/`.
    let info = match crate::worktree::create(home, source_repo, agent, Some(branch)) {
        Some(info) => info,
        None => {
            return Err(LeaseError::CreateFailed(format!(
                "failed to create worktree for {agent}@{branch}"
            )))
        }
    };

    // #1137: marker is now written inside worktree::create() immediately
    // after checkout. Re-write here is idempotent and ensures the marker
    // is present for reused worktrees (which skip the create path).
    let marker = info.path.join(MANAGED_MARKER);
    let _ = std::fs::write(
        &marker,
        format!(
            "agent={agent}\nbranch={branch}\nsource_repo={}\nleased_at={}\n",
            source_repo.display(),
            chrono::Utc::now().to_rfc3339()
        ),
    );

    Ok(WorktreeLease {
        agent: agent.to_string(),
        branch: branch.to_string(),
        path: info.path,
    })
}

/// Check if a worktree is daemon-managed (has .agend-managed marker).
pub fn is_daemon_managed(worktree_path: &Path) -> bool {
    worktree_path.join(MANAGED_MARKER).exists()
}

/// Outcome of a hard release — emitted by `release_full` and serialized
/// directly into the `release_worktree` MCP tool response.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ReleaseProvenance {
    Manual,
    Auto,
    Delete,
}

impl std::fmt::Display for ReleaseProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
            Self::Delete => "delete",
        };
        f.write_str(name)
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ReleaseOutcome {
    pub released: bool,
    /// #1465: true when `released` is a no-op success because the agent had
    /// no binding to begin with (release is idempotent — the target state
    /// was already reached). Distinguishes "nothing to do, success" from an
    /// actual teardown. Never true alongside an `error`.
    pub already_released: bool,
    pub worktree_removed: bool,
    pub binding_removed: bool,
    pub branch_deleted: bool,
    // #807 Item 2: drop optional keys on success so clients
    // don't render `"error": null` as an `<error>` envelope.
    // Real failures still emit `error` (skip_serializing_if
    // drops `None` only, never `Some`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_cleanup_skipped_reason: Option<String>,
    /// #t-21: on a `dry_run=true` release, a human-readable preview of the
    /// destructive effects that were deliberately NOT performed (worktree
    /// removal + binding clear). `None` on a real release. The pre-fix bug ran
    /// `remove_worktree` + `clear_binding_state` unconditionally, so a dry_run
    /// actually destroyed the worktree and binding; now they are previewed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run_preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Internal exact-CAS signal for auto-release. Not part of the MCP response.
    #[serde(skip)]
    pub(crate) stale_fingerprint: bool,
    /// Exact S2 absent-arm metadata cleanup, surfaced only by the force
    /// handler after the transaction; ordinary release responses skip it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent_persist_error: Option<String>,
    #[serde(skip)]
    pub(crate) git_metadata_pruned: usize,
    #[serde(skip)]
    pub(crate) git_metadata_repos: Vec<String>,
}

/// Delete the local branch ref after worktree release, IFF:
/// - `managed_verified` is true (caller confirmed .agend-managed marker)
/// - Branch is merged into main (ancestor) OR a squash-merge orphan whose tip
///   clears the age floor (`is_squash_gc_eligible`)
///
/// SAFETY: This function ONLY receives the branch from the daemon's own
/// binding record. User-checkout branches never reach here because
/// release_full early-returns on unmanaged worktrees. The merge-base check
/// below prevents deletion of unmerged branches regardless of the
/// managed_verified flag (#1249). t-...50899-10 (CR-2026-06-14 parity): a
/// remote-tracking ref being gone is NOT by itself proof the branch's work is
/// preserved — a branch pushed once, then its remote deleted by a squash
/// merge, that keeps accruing unpushed local commits afterward is
/// remote-gone yet carries committed-but-unpushed work `git branch -D` would
/// destroy irrecoverably. Mirrors `worktree_cleanup.rs::prune_orphaned_branches`.
///
/// #P3 (branch-residue): pure KEEP/DELETE decision for a released managed
/// branch, split out of `cleanup_merged_branch` so the authoritative-PR-merge
/// fast path and the fail-closed keep reasons are unit-testable without a live
/// `gh`/scm (the RED6 seam). `Ok(())` = delete-eligible; `Err(reason)` = keep,
/// with a reason that no longer blanket-blames "not merged" — the pre-#P3 text
/// was misleading when the real cause was a gh-detection outage or the age floor.
///
/// - `is_merged`: `git merge-base --is-ancestor branch default`.
/// - `pr_merged`: an authoritative merged PR matches the tip → delete NOW,
///   no age gate (monotonic proof).
/// - `squash_aged`: structurally squash-merged AND past the 24h tip-age floor.
/// - `is_squash_structural`: `branch_sweep::is_squash_merged` (offline
///   cherry+tree-diff) — used ONLY to phrase the "under the age floor" keep
///   reason; the caller computes it lazily (keep path only) to avoid an extra
///   git call on the delete path, passing `false` when eligible (never read).
/// - `pr_detect_unknown`: the PR check could not run (no github remote / gh
///   error) → fail-closed keep, retried next sweep.
pub(crate) fn merged_branch_disposition(
    branch: &str,
    default: &str,
    is_merged: bool,
    pr_merged: bool,
    squash_aged: bool,
    is_squash_structural: bool,
    pr_detect_unknown: bool,
) -> Result<(), String> {
    if is_merged || pr_merged || squash_aged {
        return Ok(());
    }
    if pr_detect_unknown && !is_squash_structural {
        return Err(format!(
            "branch '{branch}': PR-merge detection unavailable (no github remote, \
             or gh/scm error) — kept, fail-closed; retried next sweep"
        ));
    }
    if is_squash_structural {
        let hours = crate::worktree_cleanup::SQUASH_GC_MIN_TIP_AGE.as_secs() / 3600;
        return Err(format!(
            "branch '{branch}': squash-merged but tip younger than the {hours}h \
             GC floor — kept; a later sweep will delete it"
        ));
    }
    Err(format!(
        "branch '{branch}' is not merged into '{default}' (no merged PR with a \
         matching head SHA, not a squash-merge orphan) — kept"
    ))
}

/// Returns `(deleted, skip_reason)`:
/// - `(true, None)` — branch was deleted
/// - `(false, Some(reason))` — branch was NOT deleted, reason explains why
fn cleanup_merged_branch(
    source_repo: &Path,
    branch: &str,
    dry_run: bool,
    authority_proven_head: Option<&str>,
) -> (bool, Option<String>) {
    // Never delete protected branches.
    if crate::agent_ops::is_protected_ref(branch) {
        return (false, Some(format!("branch '{branch}' is protected")));
    }

    // Authority-proven review lease → immediate delete with expected-head CAS.
    // The branch never merges into default (review scaffolding), so the normal
    // merged/squash path cannot reap it. An authority-proven lease with a
    // matching expected-head is monotonic proof the branch is disposable.
    if let Some(expected) = authority_proven_head {
        match crate::git_helpers::git_cmd(source_repo, &["rev-parse", branch]) {
            Ok(actual) if actual.trim() == expected => {
                if dry_run {
                    return (
                        false,
                        Some(format!(
                            "dry-run: would delete authority-proven review branch '{branch}' \
                             (head={expected})"
                        )),
                    );
                }
                let del = crate::git_helpers::git_bypass(source_repo, &["branch", "-D", branch]);
                return match del {
                    Ok(o) if o.status.success() => (true, None),
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                        (false, Some(format!("git branch -D failed: {stderr}")))
                    }
                    Err(e) => (false, Some(format!("git branch -D failed: {e}"))),
                };
            }
            Ok(actual) => {
                return (
                    false,
                    Some(format!(
                        "authority-proven review branch '{branch}': expected head {expected} \
                         but actual tip is {} — preserved (fail-closed)",
                        actual.trim()
                    )),
                );
            }
            Err(_) => {
                return (
                    false,
                    Some(format!(
                        "authority-proven review branch '{branch}': cannot read branch tip \
                         — preserved (fail-closed)"
                    )),
                );
            }
        }
    }

    // #t-7 (#1824 follow-up): a `git fetch --prune` MUTATES the source repo's
    // remote-tracking refs (refs/remotes/...), so it must NOT run on a dry-run —
    // a dry-run release must be observation-only. The non-dry-run path keeps the
    // fresh fetch so `is_merged` / `squash` below are accurate; the dry-run
    // preview falls back to the existing local refs (best-effort "would delete").
    if !dry_run {
        let remote = crate::git_helpers::primary_remote(source_repo);
        // #2004: fail-direction is safe (stale remote refs → `is_merged`/`squash`
        // stay false → branch kept, self-heals on the next successful fetch), but a
        // persistently failing fetch accumulates undeletable branches invisibly
        // — surface it. Pure logging, the cleanup proceeds on local refs.
        match crate::git_helpers::git_bypass(source_repo, &["fetch", "--prune", &remote]) {
            Ok(o) if !o.status.success() => {
                tracing::warn!(
                    repo = %source_repo.display(),
                    remote = %remote,
                    stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                    "fetch --prune failed during merged-branch cleanup — merge/gone checks run on possibly-stale local refs (branch kept = safe direction)"
                );
            }
            Err(e) => {
                tracing::warn!(
                    repo = %source_repo.display(),
                    remote = %remote,
                    error = %e,
                    "fetch --prune could not run during merged-branch cleanup — merge/gone checks run on possibly-stale local refs (branch kept = safe direction)"
                );
            }
            Ok(_) => {}
        }
    }

    let default = crate::git_helpers::default_branch(source_repo);
    // W6: converged onto git_helpers::git_ok — byte-identical to the prior
    // git_bypass(...).map(|o| o.status.success()).unwrap_or(false) idiom this
    // absorbs (worktree_cleanup.rs's is_branch_merged already used it; this
    // was the one remaining manual call site).
    let is_merged = crate::git_helpers::git_ok(
        source_repo,
        &["merge-base", "--is-ancestor", branch, &default],
    );

    // #P3 (branch-residue): an authoritative PR-merge (a merged PR whose head
    // SHA == this branch tip, or the tip is an ancestor of it) is MONOTONIC
    // proof the work landed — delete NOW, no age margin. The 24h
    // `SQUASH_GC_MIN_TIP_AGE` floor exists ONLY to stop the cherry/tree-diff
    // HEURISTIC (`is_squash_gc_eligible`) from false-reaping a just-created
    // branch that happens to be tree-equal to main; an authoritative PR needs
    // no such belt. Skip the (network) PR check when already merged.
    let pr_status = if is_merged {
        crate::branch_sweep::PrMergeStatus::NotMerged
    } else {
        crate::branch_sweep::pr_merge_status(source_repo, &default, branch)
    };
    let pr_merged = matches!(pr_status, crate::branch_sweep::PrMergeStatus::Merged);

    // t-...50899-10 / CR-2026-06-14 parity: `is_gone` (remote-tracking ref
    // deleted) is no longer an independent delete trigger — a remote-gone
    // branch carrying commits NOT reachable from `default` (unpushed local
    // work) must be KEPT. Delete only when the branch's work is provably in
    // `default`: merged (ancestor), an authoritative merged PR (above), or
    // squash-merged past the age floor (heuristic), same gate
    // `worktree_cleanup.rs::prune_orphaned_branches` uses.
    let squash_aged = !is_merged
        && !pr_merged
        && crate::worktree_cleanup::is_squash_gc_eligible(source_repo, branch, &default);

    // #P3: split the KEEP/DELETE decision (+ the split skip reason) into a pure,
    // unit-testable free function. Compute the offline structural-squash signal
    // ONLY on the keep path (it's needed just to phrase the age-floor reason) —
    // the delete path passes a dummy `false` the function never reads.
    let eligible = is_merged || pr_merged || squash_aged;
    let is_squash_structural =
        !eligible && crate::branch_sweep::is_squash_merged(source_repo, &default, branch);
    let pr_detect_unknown = matches!(pr_status, crate::branch_sweep::PrMergeStatus::Unknown);
    if let Err(reason) = merged_branch_disposition(
        branch,
        &default,
        is_merged,
        pr_merged,
        squash_aged,
        is_squash_structural,
        pr_detect_unknown,
    ) {
        return (false, Some(reason));
    }

    if dry_run {
        return (
            false,
            Some(format!("dry-run: would delete branch '{branch}'")),
        );
    }

    let del = crate::git_helpers::git_bypass(source_repo, &["branch", "-D", branch]);
    match del {
        Ok(o) if o.status.success() => (true, None),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            (false, Some(format!("git branch -D failed: {stderr}")))
        }
        Err(e) => (false, Some(format!("git branch -D failed: {e}"))),
    }
}

/// Hard-release an agent's daemon-managed worktree + binding.
///
/// Sprint 53 P0-X: closes the gap left by P0-1's auto-bind/auto-lease.
/// Without this path, every PR-merge transition leaves a stale
/// `.worktrees/<agent>` plus `runtime/<agent>/binding.json` behind, and the
/// next dispatch trips P0-1.6's actual-HEAD check (worktree exists on prior
/// branch). Operator manually `git worktree remove`-d for every transition;
/// this function lets the `release_worktree` MCP tool do it instead.
///
/// Differs from `release()` (Phase 3 soft mark) by actually removing the
/// worktree directory via `git worktree remove --force`.
///
/// Safety: only removes worktrees carrying the `.agend-managed` marker.
/// Operator-created worktrees without the marker are left alone — surfaced
/// as `released: false, error: "...no .agend-managed marker..."`.
///
/// Idempotent (#1465): second call on the same agent sees no binding and
/// returns `released: true, already_released: true` (no error) — the release
/// target state is already reached, so it's a success no-op. A genuine
/// cleanup failure WITH a binding present still returns `released: false` +
/// `error` (idempotent success applies only to the nothing-to-do path).
///
/// Partial cleanup: if the worktree path is missing or `git worktree remove`
/// fails, the binding is still cleared so the agent is not stuck in a
/// half-released state.
/// Result of worktree directory removal attempt.
enum WorktreeRemoval {
    Removed,
    AlreadyAbsent,
    Unmanaged(String),
    Failed(String),
}

fn source_repo_from_binding(binding: &serde_json::Value, wt_path: &Path) -> PathBuf {
    binding["source_repo"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            wt_path
                .parent()
                .filter(|p| p.file_name().and_then(|n| n.to_str()) == Some(".worktrees"))
                .and_then(|p| p.parent())
                .map(PathBuf::from)
        })
        .unwrap_or_default()
}

fn remove_worktree(agent: &str, wt_path: &Path, source_repo: &Path) -> WorktreeRemoval {
    match std::fs::symlink_metadata(wt_path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!(agent, path = %wt_path.display(),
                "release: worktree path already absent — pruning registry + clearing binding");
            if !source_repo.as_os_str().is_empty() {
                let _ = crate::git_helpers::git_bypass(source_repo, &["worktree", "prune"]);
            }
            return WorktreeRemoval::AlreadyAbsent;
        }
        Err(e) => {
            return WorktreeRemoval::Failed(format!(
                "opaque worktree target metadata at {}: {e}",
                wt_path.display()
            ))
        }
        Ok(meta) if !meta.is_dir() => {
            return WorktreeRemoval::Failed(format!(
                "opaque worktree target metadata at {}",
                wt_path.display()
            ))
        }
        Ok(_) => {}
    }
    if !is_daemon_managed(wt_path) {
        tracing::warn!(agent, path = %wt_path.display(),
            "release skipped: no .agend-managed marker — worktree left alone");
        return WorktreeRemoval::Unmanaged(format!(
            "worktree at {} has no .agend-managed marker — refusing to remove (binding NOT cleared)",
            wt_path.display()
        ));
    }

    // #2550 W2: empty source_repo → `git_worktree::remove_force` runs with NO
    // `current_dir` (git resolves the repo from `--force <abs wt>` itself;
    // `git_cmd`/`git_bypass` both REQUIRE a cwd, and `wt_path.parent()` is
    // wrong — it's the worktrees-pool dir, outside the repo tree, per lead
    // ruling). Converged with `worktree_pool/workspace.rs::teardown_workspace_worktree`'s
    // byte-identical dual-cwd arm (see git_worktree.rs module doc).
    // TODO(W1.2): audit whether the empty-source_repo branch is still
    // reachable in practice; if dead, delete this arm rather than migrate it.
    let wt_str = wt_path.display().to_string();
    let result = crate::git_worktree::remove_force(source_repo, &wt_str);
    match result {
        Ok(o) if o.status.success() => WorktreeRemoval::Removed,
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            tracing::warn!(agent, error = %stderr, path = %wt_path.display(),
                "git worktree remove failed — falling back to remove_dir_all");
            let _ = std::fs::remove_dir_all(wt_path);
            if matches!(
                std::fs::symlink_metadata(wt_path),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound
            ) {
                if !source_repo.as_os_str().is_empty() {
                    if let Err(e) =
                        crate::git_helpers::git_bypass(source_repo, &["worktree", "prune"])
                    {
                        tracing::warn!(agent, error = %e, "git worktree prune failed");
                    }
                }
                WorktreeRemoval::Removed
            } else {
                WorktreeRemoval::Failed(format!("git worktree remove failed: {stderr}"))
            }
        }
        Err(e) => {
            tracing::warn!(agent, error = %e, "git command failed for release");
            WorktreeRemoval::Failed(format!("git command failed: {e}"))
        }
    }
}

mod workspace;
pub(crate) use workspace::prepare_workspace_worktree_with_permit;
#[allow(unused_imports)]
pub use workspace::{
    checkout_workspace_branch, detach_workspace_to_holding, prepare_workspace_worktree,
    reconcile_workspace_to_worktree, release_stale_branch_holders, reverse_reconcile,
    teardown_workspace_worktree, workspace_as_worktree_enabled,
};
#[cfg(test)]
pub(crate) use workspace::{
    release_one_stale_holder, workspace_as_worktree_from_env, workspace_worktree_test_seam,
    worktree_common_dir_matches, worktree_has_work_at_risk,
};

fn clear_binding_state(
    home: &Path,
    agent: &str,
    permit: &crate::mcp::handlers::dispatch_hook::LifecyclePermit,
) -> crate::binding::BindingRemoval {
    crate::binding::unbind_with_permit(home, agent, permit)
}

fn record_binding_removal(out: &mut ReleaseOutcome, removal: crate::binding::BindingRemoval) {
    match removal {
        crate::binding::BindingRemoval::Removed => out.binding_removed = true,
        crate::binding::BindingRemoval::Absent => {
            if out.error.is_none() {
                out.error = Some("binding disappeared before removal".to_string());
            }
        }
        crate::binding::BindingRemoval::Failed(error) => {
            if let Some(existing) = &mut out.error {
                existing.push_str("; binding removal failed: ");
                existing.push_str(&error);
            } else {
                out.error = Some(format!("binding removal failed: {error}"));
            }
        }
    }
}

fn resolve_branch_cleanup(
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
    if !managed_verified && !worktree_absent {
        out.branch_cleanup_skipped_reason =
            Some("cannot verify .agend-managed marker — skipping branch cleanup".to_string());
    } else if !branch.is_empty() && !sr_str.is_empty() {
        // Authority-proven review lease: lease_kind + review_assignment_id + expected_head
        // all present → eligible for immediate delete with expected-head CAS.
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
        if was_dirty
            && binding
                .get("lease_kind")
                .and_then(|v| v.as_str())
                .is_some_and(|k| k == "review")
        {
            out.branch_cleanup_skipped_reason = Some(format!(
                "authority-proven review lease on '{branch}' had dirty work — branch preserved"
            ));
            return;
        }
        let (deleted, skip_reason) =
            cleanup_merged_branch(Path::new(sr_str), branch, dry_run, authority_proven_head);
        out.branch_deleted = deleted;
        out.branch_cleanup_skipped_reason = skip_reason.clone();
        // Cleanup intent: clean feature branch released pre-merge → persist
        // intent so it can be settled on pr-merged event or periodic sweep.
        // Dirty branches get no intent (preserved permanently).
        if !deleted && !was_dirty && !dry_run {
            if let Some(tip) =
                crate::git_helpers::git_cmd(Path::new(sr_str), &["rev-parse", branch])
                    .ok()
                    .map(|s| s.trim().to_string())
            {
                let scm_slug = crate::git_helpers::git_cmd(
                    Path::new(sr_str),
                    &["remote", "get-url", "origin"],
                )
                .ok()
                .and_then(|url| crate::branch_sweep::extract_github_repo_for_intent(&url));
                // Derive PR number from task metadata for generation identity.
                let pr_number = if !task_id.is_empty() {
                    crate::tasks::load_routed(home, task_id)
                        .ok()
                        .and_then(|rt| rt.task.metadata.get("pr_number").and_then(|v| v.as_u64()))
                } else {
                    None
                };
                if let Err(e) = crate::cleanup_intents::persist_intent(
                    home,
                    sr_str,
                    branch,
                    &tip,
                    task_id,
                    scm_slug.as_deref(),
                    pr_number,
                ) {
                    tracing::warn!(
                        %branch, error = %e,
                        "cleanup intent persistence failed — branch may leak"
                    );
                    out.intent_persist_error = Some(e);
                }
            }
        }
    } else if branch.is_empty() {
        out.branch_cleanup_skipped_reason = Some("no branch in binding".to_string());
    } else {
        out.branch_cleanup_skipped_reason = Some("no source_repo in binding".to_string());
    }
}

struct LockedRelease {
    out: ReleaseOutcome,
    notices: Vec<crate::worktree::ReleaseNotice>,
    clear_refusal_marker: Option<PathBuf>,
    finish_full_release: bool,
    managed_verified: bool,
    worktree_absent: bool,
    was_dirty: bool,
}

fn idempotent_absent() -> ReleaseOutcome {
    ReleaseOutcome {
        released: true,
        already_released: true,
        ..ReleaseOutcome::default()
    }
}

fn opaque_release(reason: String) -> ReleaseOutcome {
    ReleaseOutcome {
        error: Some(format!(
            "release refused: opaque binding state ({reason}); binding evidence preserved"
        )),
        ..ReleaseOutcome::default()
    }
}

fn stale_release() -> ReleaseOutcome {
    ReleaseOutcome {
        stale_fingerprint: true,
        error: Some(
            "release refused: binding fingerprint changed before destructive authority was reacquired"
                .to_string(),
        ),
        ..ReleaseOutcome::default()
    }
}

fn release_known_locked(
    home: &Path,
    agent: &str,
    binding: &serde_json::Value,
    dry_run: bool,
    permit: &crate::mcp::handlers::dispatch_hook::LifecyclePermit,
) -> LockedRelease {
    let mut out = ReleaseOutcome::default();
    let mut notices = Vec::new();
    let mut clear_refusal_marker = None;
    let wt_path_str = binding["worktree"].as_str().unwrap_or("");
    let mut managed_verified = false;
    let mut worktree_absent = false;
    let mut was_dirty = false;

    if !wt_path_str.is_empty() {
        let wt_path = Path::new(wt_path_str);
        let source_repo = source_repo_from_binding(binding, wt_path);

        if dry_run {
            // #t-21: dry_run is observation-only. Classify the worktree with the
            // SAME checks `remove_worktree` uses (path-exists → absent;
            // missing .agend-managed marker → refuse; managed+present → would
            // remove) but perform NO mutation. This closes the bug where
            // `remove_worktree` deleted the worktree on a dry run before the
            // (dry-run-honoring) branch cleanup ever ran.
            if !wt_path.exists() {
                worktree_absent = true;
            } else if !is_daemon_managed(wt_path) {
                out.error = Some(format!(
                    "worktree at {} has no .agend-managed marker — refusing to remove (binding NOT cleared)",
                    wt_path.display()
                ));
                return LockedRelease {
                    out,
                    notices,
                    clear_refusal_marker,
                    finish_full_release: false,
                    managed_verified,
                    worktree_absent,
                    was_dirty,
                };
            } else {
                managed_verified = true;
            }
        } else {
            // #2158-adjacent: a MANUAL release must not silently drop
            // uncommitted WIP. Snapshot it to a durable recovery ref BEFORE the
            // destructive remove (a clean worktree is a no-op → zero behaviour
            // change). `is_daemon_managed` is false for an absent OR unmanaged
            // path, so this fires only for the managed-worktree case
            // `remove_worktree` would actually delete — the workspace-teardown
            // callers of `remove_worktree` are intentionally left untouched.
            if is_daemon_managed(wt_path) {
                let branch = binding["branch"].as_str().unwrap_or("");
                // No caller identity on the background pool-sweep path → None
                // routes the WIP-preserved notice to the agent's team orchestrator
                // (fallback: operator inbox) rather than a hardcoded recipient.
                let (preservation, collected) = crate::worktree::preserve_dirty_worktree_collect(
                    home, agent, wt_path, branch, None,
                );
                notices.extend(collected);
                was_dirty = matches!(preservation, crate::worktree::WipPreservation::Preserved);
                if let Some(reason) = preservation.blocked_reason() {
                    // reviewer4 #2672 fix — FAIL-CLOSED: there IS uncommitted WIP
                    // but it could not be snapshotted (e.g. a contended
                    // `index.lock`). Refuse to remove the worktree AND keep the
                    // binding so the operator can recover the WIP in place. Report
                    // `released:false` + the reason; the branch cleanup + binding
                    // clear below are intentionally skipped.
                    out.error = Some(format!(
                        "release refused: worktree has uncommitted WIP that could not be \
                         preserved ({reason}); not removing it so the WIP can be recovered. \
                         Commit or stash the changes, then release again."
                    ));
                    return LockedRelease {
                        out,
                        notices,
                        clear_refusal_marker,
                        finish_full_release: false,
                        managed_verified,
                        worktree_absent,
                        was_dirty,
                    };
                }
            }
            #[cfg(test)]
            release_test_seam::hit(ReleaseTestPhase::BeforeWorktreeRemove);
            match remove_worktree(agent, wt_path, &source_repo) {
                WorktreeRemoval::Removed => {
                    managed_verified = true;
                    out.worktree_removed = true;
                    // Success path: a prior refused release may have left a
                    // per-worktree unpreservable-nested-dirt notice marker; clear
                    // it (+ its lock) so a future re-lease of this path re-notifies
                    // from a clean slate. Best-effort.
                    clear_refusal_marker = Some(wt_path.to_path_buf());
                }
                WorktreeRemoval::AlreadyAbsent => {
                    worktree_absent = true;
                }
                WorktreeRemoval::Unmanaged(err) => {
                    out.error = Some(err);
                    // #1879 (WT-LEAK-2): refusing to delete an UNMANAGED
                    // (operator-created) worktree protects operator data, but the
                    // daemon's stale binding to it must STILL be cleared — leaving
                    // it leaks the binding + blocks a same-agent re-bind. The
                    // worktree-protection refusal must not also skip binding
                    // cleanup. (This arm is already the non-dry_run path; the
                    // dry_run classifier above mutates nothing.)
                    let removal = clear_binding_state(home, agent, permit);
                    record_binding_removal(&mut out, removal);
                    return LockedRelease {
                        out,
                        notices,
                        clear_refusal_marker,
                        finish_full_release: false,
                        managed_verified,
                        worktree_absent,
                        was_dirty,
                    };
                }
                WorktreeRemoval::Failed(err) => {
                    managed_verified = true;
                    out.error = Some(err);
                }
            }
        }
    }

    if dry_run {
        // #t-21: preserve the binding + worktree; report what WOULD happen.
        // `released` is an observation-success (matches the dry-run branch
        // cleanup contract), but nothing was actually removed.
        let wt_preview = if worktree_absent {
            format!("worktree {wt_path_str} already absent")
        } else if managed_verified {
            format!("would remove worktree {wt_path_str}")
        } else {
            "no worktree to remove".to_string()
        };
        out.dry_run_preview = Some(format!(
            "dry-run: {wt_preview}; would clear binding for '{agent}'"
        ));
        out.released = true;
    } else {
        let removal = clear_binding_state(home, agent, permit);
        record_binding_removal(&mut out, removal);
        // #1465 guardrail: only report `released` when no cleanup step failed.
        // A `WorktreeRemoval::Failed` set `out.error` above — idempotent success
        // must NOT mask a real execution error as success (reviewer contract:
        // "binding present but cleanup failed → released:false + error").
        if out.error.is_none() && out.binding_removed {
            out.released = true;
        }
    }

    let finish_full_release = out.released || dry_run;
    LockedRelease {
        out,
        notices,
        clear_refusal_marker,
        finish_full_release,
        managed_verified,
        worktree_absent,
        was_dirty,
    }
}

fn release_full_guarded(
    home: &Path,
    agent: &str,
    dry_run: bool,
    permit: &crate::mcp::handlers::dispatch_hook::LifecyclePermit,
    expected: Option<&crate::binding::BindingFingerprint>,
    provenance: ReleaseProvenance,
    validate_marker: bool,
) -> ReleaseOutcome {
    use crate::binding::GuardedBinding;

    if !permit.authorizes(home, agent) {
        return ReleaseOutcome {
            error: Some(format!(
                "release refused: lifecycle permit is not valid for agent '{agent}'"
            )),
            ..ReleaseOutcome::default()
        };
    }

    let (snapshot, fingerprint) = match crate::binding::snapshot_guarded_binding(home, agent) {
        Err(e) => return opaque_release(e),
        Ok(GuardedBinding::Absent) => return idempotent_absent(),
        Ok(GuardedBinding::Opaque(reason)) => return opaque_release(reason),
        Ok(GuardedBinding::Known { value, fingerprint }) => (value, fingerprint),
    };
    if expected.is_some_and(|expected| expected != &fingerprint) {
        return stale_release();
    }
    #[cfg(test)]
    release_test_seam::hit(ReleaseTestPhase::AfterBindingSnapshot);

    let branch = snapshot["branch"].as_str().unwrap_or("");
    if branch.is_empty() {
        return opaque_release("known binding has no branch lease identity".to_string());
    }
    let wt = snapshot["worktree"].as_str().unwrap_or("");
    let source_repo = source_repo_from_binding(&snapshot, Path::new(wt));
    let source_repo_str = source_repo.display().to_string();
    let _branch_lock =
        match crate::binding::acquire_branch_lease_lock(home, &source_repo_str, branch) {
            Ok(lock) => lock,
            Err(e) => return opaque_release(format!("branch lease lock failed: {e}")),
        };
    let _agent_lock = match crate::binding::acquire_agent_mutation_lock(home, agent) {
        Ok(lock) => lock,
        Err(e) => return opaque_release(e),
    };
    let _binding_lock = match crate::binding::acquire_binding_file_lock(home, agent) {
        Ok(lock) => lock,
        Err(e) => return opaque_release(e),
    };
    let current = match crate::binding::guarded_binding_disk_fresh(home, agent) {
        GuardedBinding::Absent => return idempotent_absent(),
        GuardedBinding::Opaque(reason) => return opaque_release(reason),
        GuardedBinding::Known {
            value,
            fingerprint: live,
        } if live == fingerprint => value,
        GuardedBinding::Known { .. } => return stale_release(),
    };
    let wt_path = current["worktree"].as_str().unwrap_or("");
    let wt_exists = !wt_path.is_empty() && Path::new(wt_path).exists();
    if validate_marker && wt_exists {
        let marker_path = Path::new(wt_path).join(MANAGED_MARKER);
        let marker_content = match std::fs::read_to_string(&marker_path) {
            Ok(c) => c,
            Err(_) => {
                return ReleaseOutcome {
                    error: Some(
                        "managed marker absent or unreadable under lock — refusing (fail-closed)"
                            .into(),
                    ),
                    ..ReleaseOutcome::default()
                };
            }
        };
        let mk_get = |prefix: &str| {
            marker_content
                .lines()
                .find_map(|l| l.strip_prefix(prefix))
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };
        let mk_agent = mk_get("agent=");
        let mk_branch = mk_get("branch=");
        let mk_source = mk_get("source_repo=");
        if mk_agent.is_empty() || mk_branch.is_empty() || mk_source.is_empty() {
            return ReleaseOutcome {
                error: Some(format!(
                    "managed marker has empty identity under lock (agent={mk_agent:?} \
                     branch={mk_branch:?} source={mk_source:?}) — refusing (fail-closed)"
                )),
                ..ReleaseOutcome::default()
            };
        }
        if mk_agent != agent {
            return ReleaseOutcome {
                error: Some(format!(
                    "marker agent '{mk_agent}' does not match release agent '{agent}' — refusing"
                )),
                ..ReleaseOutcome::default()
            };
        }
        let bound_branch = current["branch"].as_str().unwrap_or("");
        if mk_branch != bound_branch {
            return ReleaseOutcome {
                error: Some(format!(
                    "marker branch '{mk_branch}' does not match binding branch '{bound_branch}' — refusing"
                )),
                ..ReleaseOutcome::default()
            };
        }
        let bound_source_str = current["source_repo"].as_str().unwrap_or("");
        if bound_source_str.is_empty() {
            return ReleaseOutcome {
                error: Some("binding source_repo is empty — refusing (fail-closed)".into()),
                ..ReleaseOutcome::default()
            };
        }
        let Ok(mk_source_canonical) = std::fs::canonicalize(&mk_source) else {
            return ReleaseOutcome {
                error: Some(format!(
                    "marker source_repo '{mk_source}' cannot be canonicalized — refusing"
                )),
                ..ReleaseOutcome::default()
            };
        };
        let Ok(bound_source_canonical) = std::fs::canonicalize(bound_source_str) else {
            return ReleaseOutcome {
                error: Some(format!(
                    "binding source_repo '{bound_source_str}' cannot be canonicalized — refusing"
                )),
                ..ReleaseOutcome::default()
            };
        };
        if mk_source_canonical != bound_source_canonical {
            return ReleaseOutcome {
                error: Some(format!(
                    "marker source_repo '{}' does not match binding source_repo '{}' — refusing",
                    mk_source, bound_source_str
                )),
                ..ReleaseOutcome::default()
            };
        }
        if !target_source_repo_matches(Path::new(wt_path), &bound_source_canonical) {
            return ReleaseOutcome {
                error: Some(format!(
                    "worktree git pointer does not match source_repo '{}' — refusing",
                    bound_source_canonical.display()
                )),
                ..ReleaseOutcome::default()
            };
        }
    }
    let mut locked = release_known_locked(home, agent, &current, dry_run, permit);
    // Explicit drops document the lock boundary: no notice, marker cleanup,
    // branch cleanup, or release event runs with a flock held.
    drop(_binding_lock);
    drop(_agent_lock);
    drop(_branch_lock);

    for notice in locked.notices.drain(..) {
        notice.emit(home);
    }
    if let Some(path) = locked.clear_refusal_marker.take() {
        crate::worktree::clear_nested_refusal_marker(home, &path);
    }
    if locked.finish_full_release {
        resolve_branch_cleanup(
            home,
            &current,
            locked.managed_verified,
            locked.worktree_absent,
            dry_run,
            locked.was_dirty,
            &mut locked.out,
        );
        crate::event_log::log(
            home,
            "worktree_released_full",
            agent,
            &format!(
                "operation={:?} provenance={provenance} wt_removed={} binding_removed={} error={}",
                permit.operation,
                locked.out.worktree_removed,
                locked.out.binding_removed,
                locked.out.error.as_deref().unwrap_or("")
            ),
        );
    }
    locked.out
}

pub fn release_full(home: &Path, agent: &str, dry_run: bool) -> ReleaseOutcome {
    let permit = match crate::mcp::handlers::dispatch_hook::LifecyclePermit::acquire(
        home,
        agent,
        crate::mcp::handlers::dispatch_hook::LifecycleOperation::Release,
    ) {
        Ok(permit) => permit,
        Err(error) => {
            return ReleaseOutcome {
                error: Some(format!("release refused: {error}")),
                ..ReleaseOutcome::default()
            };
        }
    };
    release_full_guarded(
        home,
        agent,
        dry_run,
        &permit,
        None,
        ReleaseProvenance::Manual,
        false,
    )
}

pub(crate) fn release_full_with_permit(
    home: &Path,
    agent: &str,
    dry_run: bool,
    permit: &crate::mcp::handlers::dispatch_hook::LifecyclePermit,
) -> ReleaseOutcome {
    release_full_with_permit_origin(home, agent, dry_run, permit, ReleaseProvenance::Manual)
}

pub(crate) fn release_full_with_permit_origin(
    home: &Path,
    agent: &str,
    dry_run: bool,
    permit: &crate::mcp::handlers::dispatch_hook::LifecyclePermit,
    provenance: ReleaseProvenance,
) -> ReleaseOutcome {
    release_full_guarded(home, agent, dry_run, permit, None, provenance, false)
}

pub(crate) fn release_full_exact(
    home: &Path,
    agent: &str,
    expected: &crate::binding::BindingFingerprint,
    validate_marker: bool,
) -> ReleaseOutcome {
    let permit = match crate::mcp::handlers::dispatch_hook::LifecyclePermit::acquire(
        home,
        agent,
        crate::mcp::handlers::dispatch_hook::LifecycleOperation::Release,
    ) {
        Ok(permit) => permit,
        Err(error) => {
            return ReleaseOutcome {
                error: Some(format!("release refused: {error}")),
                ..ReleaseOutcome::default()
            }
        }
    };
    release_full_guarded(
        home,
        agent,
        false,
        &permit,
        Some(expected),
        ReleaseProvenance::Auto,
        validate_marker,
    )
}

#[allow(clippy::too_many_arguments)]
fn release_bound_target_exact_impl(
    home: &Path,
    agent: &str,
    expected: &crate::binding::BindingFingerprint,
    target: &Path,
    source_repo: &Path,
    caller_holds_branch_lease: bool,
    clear_binding_on_remove_failure: bool,
    require_force_identity: bool,
    sender: Option<&str>,
    permit: &crate::mcp::handlers::dispatch_hook::LifecyclePermit,
) -> ReleaseOutcome {
    use crate::binding::GuardedBinding;

    if !permit.authorizes(home, agent) {
        return ReleaseOutcome {
            error: Some(format!(
                "release refused: lifecycle permit is not valid for agent '{agent}'"
            )),
            ..ReleaseOutcome::default()
        };
    }

    let snapshot = match crate::binding::snapshot_guarded_binding(home, agent) {
        Err(e) => return opaque_release(e),
        Ok(GuardedBinding::Absent) => return idempotent_absent(),
        Ok(GuardedBinding::Opaque(reason)) => return opaque_release(reason),
        Ok(GuardedBinding::Known { value, fingerprint }) if &fingerprint == expected => value,
        Ok(GuardedBinding::Known { .. }) => return stale_release(),
    };
    #[cfg(test)]
    release_test_seam::hit(ReleaseTestPhase::AfterBindingSnapshot);
    let branch = snapshot["branch"].as_str().unwrap_or("");
    if branch.is_empty() {
        return opaque_release("known binding has no branch lease identity".to_string());
    }
    let branch_lock = if caller_holds_branch_lease {
        None
    } else {
        match crate::binding::acquire_branch_lease_lock(
            home,
            &source_repo.display().to_string(),
            branch,
        ) {
            Ok(lock) => Some(lock),
            Err(e) => return opaque_release(format!("branch lease lock failed: {e}")),
        }
    };
    let _agent_lock = match crate::binding::acquire_agent_mutation_lock(home, agent) {
        Ok(lock) => lock,
        Err(e) => return opaque_release(e),
    };
    let _binding_lock = match crate::binding::acquire_binding_file_lock(home, agent) {
        Ok(lock) => lock,
        Err(e) => return opaque_release(e),
    };
    let current = match crate::binding::guarded_binding_disk_fresh(home, agent) {
        GuardedBinding::Absent => return idempotent_absent(),
        GuardedBinding::Opaque(reason) => return opaque_release(reason),
        GuardedBinding::Known { value, fingerprint } if &fingerprint == expected => value,
        GuardedBinding::Known { .. } => return stale_release(),
    };
    let target_str = target.display().to_string();
    if current.get("worktree").and_then(|v| v.as_str()) != Some(target_str.as_str()) {
        return stale_release();
    }
    let force_target_state = if require_force_identity {
        match crate::mcp::handlers::classify_target(target) {
            Ok(state) => Some(state),
            Err(reason) => return opaque_release(reason),
        }
    } else {
        None
    };
    if require_force_identity
        && matches!(
            force_target_state,
            Some(crate::mcp::handlers::TargetState::Present)
        )
    {
        if crate::binding::managed_marker_agent(target).as_deref() != Some(agent) {
            return opaque_release(format!(
                "managed marker agent does not exactly equal target '{agent}'"
            ));
        }
        if marker_branch(target).as_deref() != current.get("branch").and_then(|v| v.as_str()) {
            return opaque_release("managed marker branch does not match binding".to_string());
        }
        if !target_source_repo_matches(target, source_repo) {
            return opaque_release(
                "managed target owning repository changed or is ambiguous".to_string(),
            );
        }
    }

    if require_force_identity
        && matches!(
            force_target_state,
            Some(crate::mcp::handlers::TargetState::Absent)
        )
    {
        let metadata = crate::mcp::handlers::prune_exact_git_metadata(
            source_repo,
            target,
            agent,
            current["branch"].as_str().unwrap_or(""),
        );
        let mut out = ReleaseOutcome {
            git_metadata_pruned: metadata.pruned_count,
            git_metadata_repos: metadata.repos_touched,
            ..ReleaseOutcome::default()
        };
        if let crate::mcp::handlers::ExactMetadataState::Opaque(reason) = &metadata.state {
            out.error = Some(format!(
                "exact git metadata enumeration opaque; binding preserved: {reason}"
            ));
            drop(_binding_lock);
            drop(_agent_lock);
            drop(branch_lock);
            return out;
        }
        if metadata.matched && metadata.pruned_count == 0 {
            out.error = Some(
                "exact git worktree metadata matched but could not be removed; binding preserved"
                    .to_string(),
            );
            drop(_binding_lock);
            drop(_agent_lock);
            drop(branch_lock);
            return out;
        }
        let removal = clear_binding_state(home, agent, permit);
        record_binding_removal(&mut out, removal);
        out.released = out.error.is_none() && out.binding_removed;
        drop(_binding_lock);
        drop(_agent_lock);
        drop(branch_lock);
        return out;
    }

    #[cfg(test)]
    release_test_seam::hit(ReleaseTestPhase::BeforeWorktreeRemove);
    let mut notices = Vec::new();
    if require_force_identity
        && matches!(
            force_target_state,
            Some(crate::mcp::handlers::TargetState::Present)
        )
    {
        let (preservation, collected) = crate::worktree::preserve_dirty_worktree_collect(
            home,
            agent,
            target,
            current["branch"].as_str().unwrap_or(""),
            sender,
        );
        notices = collected;
        if let Some(reason) = preservation.blocked_reason() {
            drop(_binding_lock);
            drop(_agent_lock);
            drop(branch_lock);
            for notice in notices {
                notice.emit(home);
            }
            return ReleaseOutcome {
                error: Some(format!(
                    "force release refused: worktree WIP could not be preserved ({reason}); not removing it"
                )),
                ..ReleaseOutcome::default()
            };
        }
    }
    let mut out = ReleaseOutcome::default();
    let mut clear_marker = false;
    let remove = remove_worktree(agent, target, source_repo);
    match remove {
        WorktreeRemoval::Removed => {
            out.worktree_removed = true;
            clear_marker = true;
            let removal = clear_binding_state(home, agent, permit);
            record_binding_removal(&mut out, removal);
            out.released = out.error.is_none() && out.binding_removed;
        }
        WorktreeRemoval::AlreadyAbsent => {
            let removal = clear_binding_state(home, agent, permit);
            record_binding_removal(&mut out, removal);
            out.released = out.error.is_none() && out.binding_removed;
        }
        WorktreeRemoval::Unmanaged(error) => out.error = Some(error),
        WorktreeRemoval::Failed(error) => {
            out.error = Some(error);
            if clear_binding_on_remove_failure {
                let removal = clear_binding_state(home, agent, permit);
                record_binding_removal(&mut out, removal);
            }
        }
    }
    drop(_binding_lock);
    drop(_agent_lock);
    drop(branch_lock);
    if clear_marker {
        crate::worktree::clear_nested_refusal_marker(home, target);
    }
    for notice in notices {
        notice.emit(home);
    }
    out
}

/// Exact Known-target release for reverse reconciliation. It acquires the
/// branch lease itself and leaves the binding intact if removal fails.
pub(crate) fn release_bound_target_exact(
    home: &Path,
    agent: &str,
    expected: &crate::binding::BindingFingerprint,
    target: &Path,
    source_repo: &Path,
) -> ReleaseOutcome {
    let permit = match crate::mcp::handlers::dispatch_hook::LifecyclePermit::acquire(
        home,
        agent,
        crate::mcp::handlers::dispatch_hook::LifecycleOperation::Release,
    ) {
        Ok(permit) => permit,
        Err(error) => {
            return ReleaseOutcome {
                error: Some(format!("release refused: {error}")),
                ..ReleaseOutcome::default()
            }
        }
    };
    release_bound_target_exact_impl(
        home,
        agent,
        expected,
        target,
        source_repo,
        false,
        false,
        false,
        None,
        &permit,
    )
}

/// Exact Known-target release when an outer lifecycle transaction already owns
/// the permit (workspace reverse reconciliation and other composite callers).
pub(crate) fn release_bound_target_exact_with_permit(
    home: &Path,
    agent: &str,
    expected: &crate::binding::BindingFingerprint,
    target: &Path,
    source_repo: &Path,
    permit: &crate::mcp::handlers::dispatch_hook::LifecyclePermit,
) -> ReleaseOutcome {
    release_bound_target_exact_impl(
        home,
        agent,
        expected,
        target,
        source_repo,
        false,
        false,
        false,
        None,
        permit,
    )
}

/// Exact Known-target release for checkout rollback, whose caller already holds
/// L(repo,branch). Preserves the rollback path's historical partial-unbind
/// behavior if the worktree removal itself fails.
pub(crate) fn release_bound_target_exact_under_branch_lock(
    home: &Path,
    agent: &str,
    expected: &crate::binding::BindingFingerprint,
    target: &Path,
    source_repo: &Path,
) -> ReleaseOutcome {
    let permit = match crate::mcp::handlers::dispatch_hook::LifecyclePermit::acquire(
        home,
        agent,
        crate::mcp::handlers::dispatch_hook::LifecycleOperation::Release,
    ) {
        Ok(permit) => permit,
        Err(error) => {
            return ReleaseOutcome {
                error: Some(format!("release refused: {error}")),
                ..ReleaseOutcome::default()
            };
        }
    };
    release_bound_target_exact_impl(
        home,
        agent,
        expected,
        target,
        source_repo,
        true,
        true,
        false,
        None,
        &permit,
    )
}

/// Exact Known-target release for checkout rollback when the outer bind
/// transaction already owns the lifecycle permit and branch lease.
pub(crate) fn release_bound_target_exact_under_branch_lock_with_permit(
    home: &Path,
    agent: &str,
    expected: &crate::binding::BindingFingerprint,
    target: &Path,
    source_repo: &Path,
    permit: &crate::mcp::handlers::dispatch_hook::LifecyclePermit,
) -> ReleaseOutcome {
    release_bound_target_exact_impl(
        home,
        agent,
        expected,
        target,
        source_repo,
        true,
        true,
        false,
        None,
        permit,
    )
}

/// Exact Known-target release used by S2 force/rebase.  The caller owns
/// `L(repo,branch)`; this path re-reads A/B and revalidates the marker and
/// owning-repository evidence before removing anything.
pub(crate) fn release_bound_target_exact_under_branch_lock_for_force(
    home: &Path,
    agent: &str,
    expected: &crate::binding::BindingFingerprint,
    target: &Path,
    source_repo: &Path,
    sender: Option<&str>,
    permit: &crate::mcp::handlers::dispatch_hook::LifecyclePermit,
) -> ReleaseOutcome {
    release_bound_target_exact_impl(
        home,
        agent,
        expected,
        target,
        source_repo,
        true,
        false,
        true,
        sender,
        permit,
    )
}

/// Absent-binding arm of the S2 force transaction.  The branch lease is held
/// by the caller; A/B are acquired here and the binding is re-read as truly
/// absent before the managed target is removed.
pub(crate) fn release_absent_target_under_branch_lock(
    home: &Path,
    agent: &str,
    branch: &str,
    target: &Path,
    source_repo: &Path,
    sender: Option<&str>,
    permit: &crate::mcp::handlers::dispatch_hook::LifecyclePermit,
) -> ReleaseOutcome {
    if !permit.authorizes(home, agent) {
        return ReleaseOutcome {
            error: Some(format!(
                "release refused: lifecycle permit is not valid for agent '{agent}'"
            )),
            ..ReleaseOutcome::default()
        };
    }
    let _agent_lock = match crate::binding::acquire_agent_mutation_lock(home, agent) {
        Ok(lock) => lock,
        Err(e) => return opaque_release(e),
    };
    let _binding_lock = match crate::binding::acquire_binding_file_lock(home, agent) {
        Ok(lock) => lock,
        Err(e) => return opaque_release(e),
    };
    match crate::binding::guarded_binding_disk_fresh(home, agent) {
        crate::binding::GuardedBinding::Absent => {}
        crate::binding::GuardedBinding::Opaque(reason) => return opaque_release(reason),
        crate::binding::GuardedBinding::Known { .. } => return stale_release(),
    }
    #[cfg(test)]
    release_test_seam::hit(ReleaseTestPhase::AfterBindingSnapshot);
    match crate::binding::guarded_binding_disk_fresh(home, agent) {
        crate::binding::GuardedBinding::Absent => {}
        crate::binding::GuardedBinding::Opaque(reason) => return opaque_release(reason),
        crate::binding::GuardedBinding::Known { .. } => return stale_release(),
    }
    let target_state = match crate::mcp::handlers::classify_target(target) {
        Ok(state) => state,
        Err(reason) => return opaque_release(reason),
    };
    if matches!(target_state, crate::mcp::handlers::TargetState::Present)
        && (crate::binding::managed_marker_agent(target).as_deref() != Some(agent)
            || marker_branch(target).as_deref() != Some(branch)
            || !target_source_repo_matches(target, source_repo))
    {
        return opaque_release(
            "managed target identity changed, marker branch drifted, or owning repository is ambiguous"
                .to_string(),
        );
    }
    let mut out = ReleaseOutcome::default();
    let mut notices = Vec::new();
    if matches!(target_state, crate::mcp::handlers::TargetState::Absent) {
        let metadata =
            crate::mcp::handlers::prune_exact_git_metadata(source_repo, target, agent, branch);
        out.git_metadata_pruned = metadata.pruned_count;
        out.git_metadata_repos = metadata.repos_touched;
        if let crate::mcp::handlers::ExactMetadataState::Opaque(reason) = &metadata.state {
            out.error = Some(format!(
                "exact git metadata enumeration opaque; state preserved: {reason}"
            ));
            drop(_binding_lock);
            drop(_agent_lock);
            return out;
        }
        if metadata.matched && metadata.pruned_count == 0 {
            out.error = Some(
                "exact git worktree metadata matched but could not be removed; state preserved"
                    .to_string(),
            );
            drop(_binding_lock);
            drop(_agent_lock);
            return out;
        }
        out.released = true;
        out.already_released = true;
        drop(_binding_lock);
        drop(_agent_lock);
        return out;
    }
    if matches!(target_state, crate::mcp::handlers::TargetState::Present) {
        let (preservation, collected) = crate::worktree::preserve_dirty_worktree_collect(
            home,
            agent,
            target,
            marker_branch(target).as_deref().unwrap_or(""),
            sender,
        );
        notices = collected;
        if let Some(reason) = preservation.blocked_reason() {
            drop(_binding_lock);
            drop(_agent_lock);
            for notice in notices {
                notice.emit(home);
            }
            return ReleaseOutcome {
                error: Some(format!(
                    "force release refused: worktree WIP could not be preserved ({reason}); not removing it"
                )),
                ..ReleaseOutcome::default()
            };
        }
    }
    match remove_worktree(agent, target, source_repo) {
        WorktreeRemoval::Removed => {
            out.released = true;
            out.already_released = true;
            out.worktree_removed = true;
        }
        WorktreeRemoval::AlreadyAbsent => {
            out.released = true;
            out.already_released = true;
        }
        WorktreeRemoval::Unmanaged(error) | WorktreeRemoval::Failed(error) => {
            out.error = Some(error)
        }
    }
    drop(_binding_lock);
    drop(_agent_lock);
    for notice in notices {
        notice.emit(home);
    }
    out
}

fn marker_branch(worktree: &Path) -> Option<String> {
    std::fs::read_to_string(worktree.join(MANAGED_MARKER))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("branch="))
        .map(|s| s.trim().to_string())
}

fn marker_source_repo(worktree: &Path) -> Option<PathBuf> {
    std::fs::read_to_string(worktree.join(MANAGED_MARKER))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("source_repo="))
        .map(|s| PathBuf::from(s.trim()))
}

fn git_pointer_source_repo(worktree: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(worktree.join(".git")).ok()?;
    let gitdir = content
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:").map(str::trim))?;
    let gitdir = PathBuf::from(gitdir);
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        worktree.join(gitdir)
    };
    let canonical = gitdir.canonicalize().ok()?;
    let worktrees = canonical.parent()?;
    if worktrees.file_name().and_then(|n| n.to_str()) != Some("worktrees") {
        return None;
    }
    Some(worktrees.parent()?.parent()?.to_path_buf())
}

fn target_source_repo_matches(worktree: &Path, source_repo: &Path) -> bool {
    let source = source_repo.canonicalize().ok();
    let Some(source) = source else { return false };
    let marker = marker_source_repo(worktree).and_then(|p| p.canonicalize().ok());
    let pointer = git_pointer_source_repo(worktree).and_then(|p| p.canonicalize().ok());
    match (marker, pointer) {
        (Some(marker), Some(pointer)) => marker == source && pointer == source,
        (Some(marker), None) => marker == source,
        (None, Some(pointer)) => pointer == source,
        (None, None) => false,
    }
}

/// Pin a worktree (operator override — prevents GC in Phase 4).
pub fn pin(worktree_path: &Path) {
    let pin_file = worktree_path.join(".agend-pinned");
    let _ = std::fs::write(&pin_file, chrono::Utc::now().to_rfc3339());
}

/// Unpin a worktree (allow GC again).
pub fn unpin(worktree_path: &Path) {
    let pin_file = worktree_path.join(".agend-pinned");
    let _ = std::fs::remove_file(pin_file);
}

/// Check if a worktree is pinned.
pub fn is_pinned(worktree_path: &Path) -> bool {
    worktree_path.join(".agend-pinned").exists()
}

/// Reconcile orphan leases at daemon startup (log only, no delete in Phase 3).
pub fn reconcile_orphan_leases(home: &Path) {
    for (agent_name, v) in crate::binding::binding_scan_all(home) {
        if let Some(wt_path) = v["worktree"].as_str() {
            if !Path::new(wt_path).exists() {
                tracing::warn!(
                    agent = agent_name.as_str(),
                    worktree = wt_path,
                    "orphan lease: worktree path missing"
                );
            }
        }
    }
}

// ── Phase 4: GC scan + dry-run + cutover ────────────────────────────────

mod gc;
pub(crate) use gc::{
    agent_from_layout, collect_managed_worktrees, fs_managed_worktrees, is_agent_alive,
    workspace_gitlink_worktrees, MARKER_WALK_MAX_DEPTH,
};
#[allow(unused_imports)]
pub use gc::{
    enumerate_managed_worktrees, gc_candidates, gc_dry_run, gc_run, gc_stale_ci_watch_locks,
    GcCandidate, GcKind, GcResult, ManagedWorktree,
};
#[cfg(test)]
pub(crate) use gc::{
    evaluate_candidate, force_reclaim_age_days, gc_remove_one, resolve_source_repo,
    within_boot_grace,
};

mod target_sweep;
#[cfg(test)]
#[allow(unused_imports)] // Unix-only target_sweep tests use these seams; Windows may not.
pub(crate) use target_sweep::{
    safe_managed_root, target_sweep_candidates_with_roster, target_sweep_run_with_roster,
    validate_target_for_delete,
};
#[allow(unused_imports)]
pub use target_sweep::{
    target_gc_config, target_sweep_candidates, target_sweep_dry_run, target_sweep_run,
    TargetSweepCandidate, TargetSweepResult, TARGET_SWEEP_SCOPE_NOTE,
};

#[cfg(test)]
mod review_repro_worktree_git;
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
