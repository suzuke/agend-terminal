//! Worktree pool — daemon-managed lease/release lifecycle for git worktrees.
//!
//! Builds on existing `worktree.rs` (creation) + `binding.rs` (state).
//! Phase 3: lease/release + daemon-tag + E4.5 enforcement. GC deferred to Phase 4.

use std::path::{Path, PathBuf};

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
            "agent={agent}\nbranch={branch}\nleased_at={}\n",
            chrono::Utc::now().to_rfc3339()
        ),
    );

    Ok(WorktreeLease {
        agent: agent.to_string(),
        branch: branch.to_string(),
        path: info.path,
    })
}

/// Release a lease — marks worktree as GC candidate (does NOT delete, Phase 4).
/// Writes `released_at` timestamp for grace period calculation.
pub fn release(home: &Path, lease: &WorktreeLease) {
    // #worktree-git-3: hold the SAME per-agent binding lock that
    // create()/bind_full/gc use, so the unbind + marker read-modify-write is
    // atomic against a concurrent bind or GC pass. Without it, a racing rewrite
    // (or a crash between read and write) can drop `released_at` from the
    // marker, which reclassifies the worktree from the clean-release grace path
    // into the force-reclaim backstop — changing the deletion semantics.
    // Best-effort: an unobtainable lock must not block the release (matches the
    // prior unlocked behaviour, only safer). Scoped so the lock is dropped
    // before the event-log write (no nested flock).
    let lock_path = crate::paths::runtime_dir(home)
        .join(&lease.agent)
        .join(".binding.json.lock");
    {
        let _lock = crate::store::acquire_file_lock(&lock_path);
        // Clear binding (task done).
        crate::binding::unbind(home, &lease.agent);
        // Write released_at into the managed marker for GC grace calculation.
        let marker = lease.path.join(MANAGED_MARKER);
        if let Ok(mut content) = std::fs::read_to_string(&marker) {
            content.push_str(&format!(
                "released_at={}\n",
                chrono::Utc::now().to_rfc3339()
            ));
            if let Err(e) = crate::store::atomic_write(&marker, content.as_bytes()) {
                tracing::warn!(
                    agent = %lease.agent,
                    path = %marker.display(),
                    error = %e,
                    "release: failed to persist released_at into managed marker"
                );
            }
        }
    }
    crate::event_log::log(
        home,
        "worktree_lease_released",
        &lease.agent,
        &format!("branch={} path={}", lease.branch, lease.path.display()),
    );
}

/// Check if a worktree is daemon-managed (has .agend-managed marker).
pub fn is_daemon_managed(worktree_path: &Path) -> bool {
    worktree_path.join(MANAGED_MARKER).exists()
}

/// Outcome of a hard release — emitted by `release_full` and serialized
/// directly into the `release_worktree` MCP tool response.
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
}

/// Delete the local branch ref after worktree release, IFF:
/// - `managed_verified` is true (caller confirmed .agend-managed marker)
/// - Branch is merged into main OR remote tracking ref is gone
///
/// SAFETY: This function ONLY receives the branch from the daemon's own
/// binding record. User-checkout branches never reach here because
/// release_full early-returns on unmanaged worktrees. The merge-base
/// check below prevents deletion of unmerged branches regardless of
/// the managed_verified flag (#1249).
///
/// Returns `(deleted, skip_reason)`:
/// - `(true, None)` — branch was deleted
/// - `(false, Some(reason))` — branch was NOT deleted, reason explains why
fn cleanup_merged_branch(
    source_repo: &Path,
    branch: &str,
    dry_run: bool,
) -> (bool, Option<String>) {
    // Never delete protected branches.
    if crate::agent_ops::is_protected_ref(branch) {
        return (false, Some(format!("branch '{branch}' is protected")));
    }

    // #t-7 (#1824 follow-up): a `git fetch --prune` MUTATES the source repo's
    // remote-tracking refs (refs/remotes/...), so it must NOT run on a dry-run —
    // a dry-run release must be observation-only. The non-dry-run path keeps the
    // fresh fetch so `is_merged` / `is_gone` below are accurate; the dry-run
    // preview falls back to the existing local refs (best-effort "would delete").
    if !dry_run {
        let remote = crate::git_helpers::primary_remote(source_repo);
        // #2004: fail-direction is safe (stale remote refs → `is_gone` stays
        // false → branch kept, self-heals on the next successful fetch), but a
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

    let is_gone = {
        let remote_name = crate::git_helpers::git_bypass(
            source_repo,
            &["config", &format!("branch.{branch}.remote")],
        )
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
        if !remote_name.is_empty() {
            let remote_ref = format!("refs/remotes/{remote_name}/{branch}");
            let exists = crate::git_helpers::git_bypass(
                source_repo,
                &["rev-parse", "--verify", &remote_ref],
            )
            .map(|o| o.status.success())
            .unwrap_or(true);
            !exists
        } else {
            false
        }
    };

    if !is_merged && !is_gone {
        return (false, Some("branch not merged into main".to_string()));
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
    if !wt_path.exists() {
        tracing::info!(agent, path = %wt_path.display(),
            "release: worktree path already absent — pruning registry + clearing binding");
        if !source_repo.as_os_str().is_empty() {
            let _ = crate::git_helpers::git_bypass(source_repo, &["worktree", "prune"]);
        }
        return WorktreeRemoval::AlreadyAbsent;
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
            if !wt_path.exists() {
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

fn clear_binding_state(home: &Path, agent: &str) {
    crate::binding::unbind(home, agent);
    crate::mcp::handlers::dispatch_hook::clear_bind_in_flight(home, agent);
}

fn resolve_branch_cleanup(
    binding: &serde_json::Value,
    managed_verified: bool,
    worktree_absent: bool,
    dry_run: bool,
    out: &mut ReleaseOutcome,
) {
    let branch = binding["branch"].as_str().unwrap_or("");
    let sr_str = binding["source_repo"].as_str().unwrap_or("");
    if !managed_verified && !worktree_absent {
        out.branch_cleanup_skipped_reason =
            Some("cannot verify .agend-managed marker — skipping branch cleanup".to_string());
    } else if !branch.is_empty() && !sr_str.is_empty() {
        let (deleted, skip_reason) = cleanup_merged_branch(Path::new(sr_str), branch, dry_run);
        out.branch_deleted = deleted;
        out.branch_cleanup_skipped_reason = skip_reason;
    } else if branch.is_empty() {
        out.branch_cleanup_skipped_reason = Some("no branch in binding".to_string());
    } else {
        out.branch_cleanup_skipped_reason = Some("no source_repo in binding".to_string());
    }
}

pub fn release_full(home: &Path, agent: &str, dry_run: bool) -> ReleaseOutcome {
    let mut out = ReleaseOutcome::default();

    let Some(binding) = crate::binding::read(home, agent) else {
        // #1465: release is idempotent. "No binding" means the release
        // target state is already reached → report a success no-op rather
        // than an error, so automated dispatch/release can treat release as
        // a safe always-succeeds operation. (A genuine cleanup failure WITH
        // a binding present still returns `released:false` + `error` below —
        // idempotent success applies ONLY to this nothing-to-do path.)
        out.released = true;
        out.already_released = true;
        return out;
    };

    let wt_path_str = binding["worktree"].as_str().unwrap_or("");
    let mut managed_verified = false;
    let mut worktree_absent = false;

    if !wt_path_str.is_empty() {
        let wt_path = Path::new(wt_path_str);
        let source_repo = source_repo_from_binding(&binding, wt_path);

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
                return out;
            } else {
                managed_verified = true;
            }
        } else {
            match remove_worktree(agent, wt_path, &source_repo) {
                WorktreeRemoval::Removed => {
                    managed_verified = true;
                    out.worktree_removed = true;
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
                    clear_binding_state(home, agent);
                    out.binding_removed = true;
                    return out;
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
        clear_binding_state(home, agent);
        out.binding_removed = true;
        // #1465 guardrail: only report `released` when no cleanup step failed.
        // A `WorktreeRemoval::Failed` set `out.error` above — idempotent success
        // must NOT mask a real execution error as success (reviewer contract:
        // "binding present but cleanup failed → released:false + error").
        if out.error.is_none() {
            out.released = true;
        }
    }

    resolve_branch_cleanup(
        &binding,
        managed_verified,
        worktree_absent,
        dry_run,
        &mut out,
    );

    crate::event_log::log(
        home,
        "worktree_released_full",
        agent,
        &format!(
            "wt_removed={} binding_removed={} error={}",
            out.worktree_removed,
            out.binding_removed,
            out.error.as_deref().unwrap_or("")
        ),
    );

    out
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
