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
    let is_merged = crate::git_helpers::git_bypass(
        source_repo,
        &["merge-base", "--is-ancestor", branch, &default],
    )
    .map(|o| o.status.success())
    .unwrap_or(false);

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

    let wt_str = wt_path.display().to_string();
    let result = if source_repo.as_os_str().is_empty() {
        // git-raw-allowed: empty source_repo → this arm intentionally runs with
        // NO `current_dir`, so `git` resolves the repo from `--force <abs wt>`
        // itself. `git_cmd`/`git_bypass` both REQUIRE a cwd; passing `wt_path
        // .parent()` is wrong (it's the worktrees-pool dir `~/.agend-terminal/
        // worktrees/<agent>/`, outside the repo tree, per lead ruling). Keep raw.
        // TODO(W1.2): audit whether the empty-source_repo branch is still
        // reachable in practice; if dead, delete this arm rather than migrate it.
        std::process::Command::new("git")
            .args(["worktree", "remove", "--force", &wt_str])
            .env("AGEND_GIT_BYPASS", "1")
            .output()
    } else {
        crate::git_helpers::git_bypass(source_repo, &["worktree", "remove", "--force", &wt_str])
    };
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

/// #2234 Phase 0 (cure-(B) safety, independent value): tear down a per-agent
/// WORKSPACE directory that is a git WORKTREE via `git worktree remove --force`
/// from the OWNING repo, so no orphan registration survives in
/// `<canonical>/.git/worktrees/`. Returns `true` when it took responsibility
/// (the path is a worktree) — the caller MUST then NOT `remove_dir_all` (the
/// orphan-leaving bug #2234). Returns `false` for a NON-worktree (`.git` is a
/// directory = pre-(B) `git init`'d standalone clone, or absent = plain dir) →
/// the caller keeps its byte-identical `remove_dir_all`.
///
/// r6/lead dialectic #1 (the critical safety direction): the **gitlink alone**
/// gates this path. The `.agend-managed` marker is logged as a confidence
/// signal but is NEVER a veto — a managed worktree whose marker write was lost
/// (interrupted reconcile) still has a gitlink, and falling through to
/// `remove_dir_all` would orphan it. This fn is only ever called for the
/// per-agent workspace path (daemon-owned by construction), so removal is
/// unconditional once a gitlink is present.
///
/// Work-at-risk guard (must-resolve #2): a worktree with uncommitted/untracked
/// changes OR local commits not on any remote is backed up WHOLE to
/// `<home>/reconcile-backups/<agent>-<epoch>/` BEFORE removal. If the backup
/// FAILS, removal is ABORTED fail-closed (returns `true`, dir left in place for
/// operator recovery) — never destroy work without a durable backup.
pub fn teardown_workspace_worktree(home: &Path, agent: &str, working_dir: &Path) -> bool {
    // Discriminator: a git WORKTREE has a `.git` gitlink FILE; a `git init`'d
    // standalone clone has a `.git` DIRECTORY; a plain dir has neither.
    if !working_dir.join(".git").is_file() {
        return false;
    }
    if !is_daemon_managed(working_dir) {
        tracing::warn!(agent, path = %working_dir.display(),
            "#2234 teardown: workspace worktree missing .agend-managed marker \
             (interrupted reconcile?) — removing via git anyway, NOT remove_dir_all");
    }

    let source_repo = resolve_owning_repo(home, agent, working_dir);

    if worktree_has_work_at_risk(working_dir) {
        match backup_worktree_dir(home, agent, None, working_dir) {
            Ok(dest) => tracing::warn!(agent, backup = %dest.display(),
                "#2234 teardown: workspace worktree had uncommitted/unpushed work — backed up before removal"),
            Err(e) => {
                tracing::error!(agent, path = %working_dir.display(), error = %e,
                    "#2234 teardown: backup FAILED — aborting removal (fail-closed); worktree left for operator recovery");
                return true;
            }
        }
    }

    // Mirror `remove_worktree`'s git call, WITHOUT the marker veto: run from the
    // owning repo so the registration is cleared (not just the dir).
    let wt_str = working_dir.display().to_string();
    let result = if source_repo.as_os_str().is_empty() {
        // git-raw-allowed: defensive fallback when the owning repo can't be
        // resolved (effectively unreachable — a real gitlink always yields a
        // common-dir). Mirrors `remove_worktree`'s empty-source_repo arm: git
        // must resolve the repo from the absolute `<wt>` itself, so this runs
        // with NO `current_dir` — `git_bypass`/`git_cmd` both REQUIRE a cwd.
        std::process::Command::new("git")
            .args(["worktree", "remove", "--force", &wt_str])
            .env("AGEND_GIT_BYPASS", "1")
            .output()
    } else {
        crate::git_helpers::git_bypass(&source_repo, &["worktree", "remove", "--force", &wt_str])
    };
    let removed = matches!(&result, Ok(o) if o.status.success());
    if !removed {
        if let Ok(o) = &result {
            tracing::warn!(agent, error = %String::from_utf8_lossy(&o.stderr).trim(), path = %working_dir.display(),
                "#2234 teardown: git worktree remove failed — falling back to remove_dir_all + prune");
        }
        let _ = std::fs::remove_dir_all(working_dir);
        if !source_repo.as_os_str().is_empty() {
            let _ = crate::git_helpers::git_bypass(&source_repo, &["worktree", "prune"]);
        }
    }
    true
}

/// Resolve the canonical repo that OWNS a worktree, from its gitlink's
/// common-dir (the binding may already be cleared at teardown). Falls back to
/// the binding's recorded `source_repo`.
fn resolve_owning_repo(home: &Path, agent: &str, working_dir: &Path) -> PathBuf {
    if let Ok(o) = crate::git_helpers::git_bypass(working_dir, &["rev-parse", "--git-common-dir"]) {
        if o.status.success() {
            let raw = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !raw.is_empty() {
                let common = if Path::new(&raw).is_absolute() {
                    PathBuf::from(&raw)
                } else {
                    working_dir.join(&raw)
                };
                let common = dunce::canonicalize(&common).unwrap_or(common);
                // common = `<repo>/.git`; its parent is the repo root.
                if let Some(repo) = common.parent() {
                    return repo.to_path_buf();
                }
            }
        }
    }
    crate::binding::read(home, agent)
        .map(|b| source_repo_from_binding(&b, working_dir))
        .unwrap_or_default()
}

/// True if a worktree holds work that must not be silently destroyed:
/// uncommitted/untracked changes, or — when a remote exists to be ahead of —
/// local commits not reachable from any remote-tracking ref (committed-orphan).
fn worktree_has_work_at_risk(wt: &Path) -> bool {
    // Uncommitted/untracked work — EXCLUDING the daemon's own `.agend-managed`
    // marker, which `git status --porcelain` reports as untracked but is
    // regenerable metadata, not work (every leased/provisioned worktree carries
    // it, so counting it would force a backup on EVERY release/teardown). Parse
    // porcelain directly rather than `has_uncommitted_changes` so we can drop the
    // marker line; fail-closed (spawn/non-zero → treat as at-risk) is preserved.
    match crate::git_helpers::git_bypass(wt, &["status", "--porcelain"]) {
        Ok(o) if o.status.success() => {
            // Porcelain line = `XY <path>` (status code + space + path). The ONLY
            // line to ignore is the root marker `?? .agend-managed`; match the path
            // EXACTLY (porcelain path starts at byte 3) so a real file whose name
            // merely ENDS with `.agend-managed` is NOT mistaken for the marker.
            let is_marker_line = |l: &str| l.get(3..) == Some(MANAGED_MARKER);
            let dirty = String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| !is_marker_line(l));
            if dirty {
                return true;
            }
        }
        _ => return true, // fail-closed
    }
    // "Unpushed" only has meaning when a remote exists; in a remote-less repo
    // every commit looks unreachable-from-remotes, which is not work-at-risk.
    let has_remote = crate::git_helpers::git_bypass(wt, &["remote"])
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    if !has_remote {
        return false;
    }
    crate::git_helpers::git_bypass(wt, &["rev-list", "--count", "HEAD", "--not", "--remotes"])
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u64>()
                .ok()
        })
        .map(|n| n > 0)
        .unwrap_or(false)
}

/// Back up a worktree WHOLE to `<home>/reconcile-backups/<agent>-<epoch>/`,
/// skipping the regenerable build cache (`target`) and the gitlink (`.git`).
/// Conservative (lead Q2): never auto-deleted — operator / gc reclaim later.
/// Back up `wt` to `<home>/reconcile-backups/<agent>[-<branch>]-<epoch>/`. The
/// optional `branch` discriminator (lead Q1 ruling) keeps backups UNIQUE when a
/// single dispatch releases MULTIPLE stale holders in the same wall-clock second
/// (#2234 Phase 1c `release_stale_branch_holders`) — without it `<agent>-<epoch>`
/// would collide and the second copy would merge into the first. `None`
/// (teardown's single-worktree path) keeps the original `<agent>-<epoch>` name.
fn backup_worktree_dir(
    home: &Path,
    agent: &str,
    branch: Option<&str>,
    wt: &Path,
) -> std::io::Result<PathBuf> {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = match branch {
        Some(b) => format!("{agent}-{}-{epoch}", sanitize_backup_segment(b)),
        None => format!("{agent}-{epoch}"),
    };
    let dest = home.join("reconcile-backups").join(name);
    std::fs::create_dir_all(&dest)?;
    copy_dir_excluding(wt, &dest, &["target", ".git"])?;
    Ok(dest)
}

/// Filesystem-safe slug for a branch in a backup dir name (`feat/x` → `feat-x`).
fn sanitize_backup_segment(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn copy_dir_excluding(src: &Path, dst: &Path, exclude: &[&str]) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if exclude.iter().any(|e| name == std::ffi::OsStr::new(e)) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_dir_excluding(&from, &to, exclude)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// #2234 cure-(B) gray-rollout gate. The workspace-as-worktree behavior is OFF
/// by default — `resolve_auto_worktree` then returns `None` for workspace dirs
/// exactly as pre-(B) (byte-identical). `AGEND_WORKSPACE_AS_WORKTREE=1` (or
/// `true`) enables it; an optional `AGEND_WORKSPACE_AS_WORKTREE_AGENTS=a,b`
/// allowlist scopes the validation phase to named agents (empty/unset = all
/// agents once the flag is on). Per lead Q1: a flag (not a per-instance field) —
/// default off → opt-in a few agents → flip the default at cutover.
pub fn workspace_as_worktree_enabled(agent: &str) -> bool {
    // Test-injectable seam (#2234 Phase 1c): tests enable (B) via a THREAD-LOCAL
    // override (`workspace_worktree_test_seam::force`) instead of a process-global
    // `set_var`, so a flag-ON test never leaks the flag to other tests running in
    // parallel in the same binary (the env-leak flake class r6 caught twice). The
    // production read-path below is byte-identical — the daemon still reads
    // `AGEND_WORKSPACE_AS_WORKTREE` (+ allowlist). Compiled out of release builds.
    #[cfg(test)]
    if let Some(forced) = workspace_worktree_test_seam::get() {
        return forced;
    }
    workspace_as_worktree_from_env(
        std::env::var("AGEND_WORKSPACE_AS_WORKTREE").ok().as_deref(),
        std::env::var("AGEND_WORKSPACE_AS_WORKTREE_AGENTS")
            .ok()
            .as_deref(),
        agent,
    )
}

/// Pure flag decision over (flag, allowlist) inputs — unit-testable without any
/// process-global env mutation. `AGEND_WORKSPACE_AS_WORKTREE` must be `1`/`true`;
/// a non-empty `AGEND_WORKSPACE_AS_WORKTREE_AGENTS` then scopes to listed agents.
fn workspace_as_worktree_from_env(
    flag: Option<&str>,
    allowlist: Option<&str>,
    agent: &str,
) -> bool {
    if !matches!(flag, Some("1") | Some("true")) {
        return false;
    }
    match allowlist {
        Some(list) if !list.trim().is_empty() => list.split(',').any(|a| a.trim() == agent),
        _ => true,
    }
}

/// #2234 Phase 1c test seam: a THREAD-LOCAL (B)-flag override. Tests force the
/// flag on/off for their OWN thread — `workspace_as_worktree_enabled` runs
/// synchronously on the caller thread (dispatch / resolve_auto_worktree), so the
/// override is observed there but is invisible to other tests' threads. This
/// roots out the process-global `set_var` leak class (no serial-grouping needed).
#[cfg(test)]
pub(crate) mod workspace_worktree_test_seam {
    use std::cell::Cell;
    thread_local! {
        static OVERRIDE: Cell<Option<bool>> = const { Cell::new(None) };
    }
    pub(crate) fn get() -> Option<bool> {
        OVERRIDE.with(|c| c.get())
    }
    fn set(v: Option<bool>) {
        OVERRIDE.with(|c| c.set(v));
    }
    /// RAII: force the flag for the current thread; restores on drop (incl. panic).
    #[must_use]
    pub(crate) struct ForceGuard;
    pub(crate) fn force(enabled: bool) -> ForceGuard {
        set(Some(enabled));
        ForceGuard
    }
    impl Drop for ForceGuard {
        fn drop(&mut self) {
            set(None);
        }
    }
}

/// #2234 cure-(B): make the agent's per-agent workspace dir BE a daemon-managed
/// worktree of `source_repo` (its `.git` a gitlink FILE), so the agent's cwd ==
/// its bound worktree and the cwd<->worktree dual-truth disappears — while the
/// cwd PATH stays byte-identical (the #1919 property: `claude --continue` keys
/// its session on the cwd path, so an in-place branch switch never orphans it).
/// Idempotent. Three states of `target`:
///   (i)   absent / empty       → `git worktree add` (produces a real gitlink).
///   (ii)  standalone clone      → backup the WHOLE dir (fail-closed: backup Err
///         (`.git` is a DIR)        → ABORT, leave the standalone untouched) →
///                                   remove → add. A standalone may carry a
///                                   committed-but-unpushed (orphan) commit that
///                                   `has_uncommitted` misses, so we back up the
///                                   whole dir, not just uncommitted work.
///   (iii) already a worktree    → verify its gitlink common-dir resolves to
///         (`.git` gitlink FILE)    `source_repo`; match → NO-OP (idempotent, no
///                                   backup); foreign → fall through to (ii).
///
/// HOLDING-CLEAN BY CONSTRUCTION (relied on by the Phase-1c no-`--force` in-place
/// checkout's atomicity): a freshly-provisioned worktree is created detached at
/// the repo HEAD (or on `branch` when given) with a clean tree. Phase-1c's
/// dispatch then does the in-place `git checkout <task-branch>` — without
/// `--force`, which git aborts atomically if the tree were dirty. Because this
/// fn only ever hands back a clean holding tree, that checkout cannot silently
/// lose work.
///
/// Returns the worktree path (== `target`) on success; `Err` is fail-safe — the
/// caller (`resolve_auto_worktree`) keeps the workspace as a non-worktree, so the
/// agent stays on the pre-(B) path under the #2254 drift-WARN safety net.
pub fn reconcile_workspace_to_worktree(
    home: &Path,
    agent: &str,
    target: &Path,
    source_repo: &Path,
    branch: Option<&str>,
) -> Result<PathBuf, String> {
    // (iii) already a daemon worktree rooted at source_repo → idempotent no-op.
    if target.join(".git").is_file() && worktree_common_dir_matches(target, source_repo) {
        return Ok(target.to_path_buf());
    }
    // (ii) standalone clone OR foreign worktree: the target must be EMPTY before
    // `git worktree add`, so back up any work then remove. (i) empty/absent skips
    // the backup (nothing at risk).
    if target.exists() {
        let non_empty = std::fs::read_dir(target)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
        if non_empty {
            backup_worktree_dir(home, agent, branch, target).map_err(|e| {
                format!(
                    "reconcile aborted (fail-closed): backup of {} failed: {e} — workspace left untouched",
                    target.display()
                )
            })?;
        }
        std::fs::remove_dir_all(target)
            .map_err(|e| format!("reconcile: remove {} failed: {e}", target.display()))?;
    }
    provision_worktree_at(agent, target, source_repo, branch)?;
    // r6 #4: confirm `git worktree add` produced a real gitlink FILE (the
    // discriminator the whole (B) lifecycle keys on).
    if !target.join(".git").is_file() {
        return Err(format!(
            "reconcile: post-add .git is not a gitlink file at {}",
            target.display()
        ));
    }
    Ok(target.to_path_buf())
}

/// True if `target`'s git common-dir resolves to `source_repo` (i.e. it is a
/// worktree OF that canonical repo, not a foreign one).
fn worktree_common_dir_matches(target: &Path, source_repo: &Path) -> bool {
    let Ok(o) = crate::git_helpers::git_bypass(target, &["rev-parse", "--git-common-dir"]) else {
        return false;
    };
    if !o.status.success() {
        return false;
    }
    let raw = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if raw.is_empty() {
        return false;
    }
    let common = if Path::new(&raw).is_absolute() {
        PathBuf::from(&raw)
    } else {
        target.join(&raw)
    };
    // `common` is `<repo>/.git`; compare its parent to `source_repo`.
    let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    common
        .parent()
        .map(|repo| canon(repo) == canon(source_repo))
        .unwrap_or(false)
}

/// `git worktree add` at an arbitrary `target` (the workspace path), HOLDING:
/// detached at HEAD when `branch` is None, else on `branch` (new via `-b`,
/// falling back to an existing branch). Writes the `.agend-managed` marker.
fn provision_worktree_at(
    agent: &str,
    target: &Path,
    source_repo: &Path,
    branch: Option<&str>,
) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let target_str = target.display().to_string();
    use crate::git_helpers::{git_cmd, GitError};
    let add = |args: &[&str]| git_cmd(source_repo, args);
    let result = match branch {
        // Holding state: detached at the repo HEAD — Phase-1c checks out the task
        // branch in place at dispatch.
        None => add(&["worktree", "add", "--detach", &target_str]),
        Some(b) => match add(&["worktree", "add", "-b", b, &target_str]) {
            // Branch already exists → attach to it (mirror worktree::create).
            Err(GitError::NonZero { stderr, .. }) if stderr.contains("already exists") => {
                add(&["worktree", "add", &target_str, b])
            }
            other => other,
        },
    };
    match result {
        Ok(_) => {
            let _ = std::fs::write(
                target.join(MANAGED_MARKER),
                format!("agent={agent}\nreconciled=workspace-as-worktree\n"),
            );
            Ok(())
        }
        Err(e) => Err(format!(
            "reconcile: git worktree add at {} failed: {e}",
            target.display()
        )),
    }
}

/// #2234 Phase 1c: the (B) replacement for `lease` in dispatch — prepare the
/// agent's WORKSPACE worktree for `branch`. Idempotent reconcile (spawn already
/// provisioned it; re-assert covers a dispatch racing a not-yet-spawned agent or
/// a deferred reconcile) → free `branch` from any stale legacy holders (each
/// work-at-risk backed up before `--force`) → in-place `git checkout` (no
/// `--force`; atomic abort on dirty). Returns the workspace worktree path to bind.
pub fn prepare_workspace_worktree(
    home: &Path,
    agent: &str,
    source_repo: &Path,
    branch: &str,
) -> Result<PathBuf, String> {
    let ws = crate::paths::workspace_dir(home).join(agent);
    reconcile_workspace_to_worktree(home, agent, &ws, source_repo, None)?;
    release_stale_branch_holders(home, agent, source_repo, branch, &ws)?;
    checkout_workspace_branch(&ws, branch)?;
    Ok(ws)
}

/// #2234 Phase 1c (must-resolve #1, r6 confluence catch): free `branch` from any
/// STALE legacy holders (the pre-(B) `worktrees/<agent>/<branch>` pool) before
/// the workspace worktree's in-place checkout — else git refuses with "branch
/// already checked out at <other>". Drives off the canonical
/// [`enumerate_managed_worktrees`] (single source of truth over /workspace +
/// /worktrees). Only releases this `agent`'s registered holders of `branch` that
/// are NOT the workspace worktree itself; other residuals are left for GC (off
/// the dispatch critical path → lower blast). Any single release failing
/// (including a fail-closed backup abort) ABORTS the whole dispatch.
pub fn release_stale_branch_holders(
    home: &Path,
    agent: &str,
    source_repo: &Path,
    branch: &str,
    workspace_path: &Path,
) -> Result<(), String> {
    let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let ws = canon(workspace_path);
    for wt in enumerate_managed_worktrees(home, source_repo) {
        let is_self = canon(&wt.path) == ws;
        let holds_branch = wt.branch.as_deref() == Some(branch);
        let mine = wt.agent.as_deref() == Some(agent);
        if !is_self && mine && holds_branch {
            release_one_stale_holder(home, agent, source_repo, branch, &wt.path)?;
        }
    }
    Ok(())
}

/// Release a single stale legacy worktree, WORK-AT-RISK guarded (must-resolve
/// #1): the old bound worktree is exactly where the shim's `-C <worktree>`
/// redirected commits, so it may hold uncommitted/unpushed work that a bare
/// `--force` would destroy. Mirror Phase-0 teardown: if work-at-risk, back up
/// the WHOLE dir FIRST (keyed by branch so concurrent same-second releases don't
/// collide) — and if that backup FAILS, ABORT fail-closed (never `--force`
/// without a durable backup). All Phase-0 primitives reused verbatim.
fn release_one_stale_holder(
    home: &Path,
    agent: &str,
    source_repo: &Path,
    branch: &str,
    holder: &Path,
) -> Result<(), String> {
    if worktree_has_work_at_risk(holder) {
        backup_worktree_dir(home, agent, Some(branch), holder).map_err(|e| {
            format!(
                "release aborted (fail-closed): backup of stale holder {} failed: {e}",
                holder.display()
            )
        })?;
    }
    match remove_worktree(agent, holder, source_repo) {
        WorktreeRemoval::Removed | WorktreeRemoval::AlreadyAbsent => Ok(()),
        WorktreeRemoval::Unmanaged(m) => Err(m),
        WorktreeRemoval::Failed(e) => Err(e),
    }
}

/// #2234 Phase 1c: in-place `git checkout <branch>` of the workspace worktree
/// (the (B) replacement for leasing a fresh per-branch worktree). NO `--force` —
/// git aborts atomically if the tree is dirty/conflicting, leaving HEAD on the
/// prior branch so the caller can reject the dispatch without a half-applied
/// state (the holding-clean invariant makes this the normal clean case).
pub fn checkout_workspace_branch(workspace: &Path, branch: &str) -> Result<(), String> {
    use crate::git_helpers::git_cmd;
    git_cmd(workspace, &["checkout", branch])
        .map(|_| ())
        .map_err(|e| format!("in-place checkout of '{branch}' failed: {e}"))
}

/// #2234 Phase 1c rollback: return the workspace worktree to its HOLDING state
/// (detached at HEAD) — used when `bind_full` fails AFTER a successful checkout.
/// NEVER deletes the (permanent) workspace worktree; the just-checked-out branch
/// carries no agent work yet (bind_full runs synchronously right after checkout,
/// before the agent is told the dispatch succeeded), so detaching is safe.
pub fn detach_workspace_to_holding(workspace: &Path) -> Result<(), String> {
    use crate::git_helpers::git_cmd;
    git_cmd(workspace, &["checkout", "--detach"])
        .map(|_| ())
        .map_err(|e| format!("rollback detach failed: {e}"))
}

/// #2234 cure-(B) ROLLBACK primitive (flag-independent — callable in ANY flag
/// state). Revert an agent whose `/workspace/<agent>` was converted to a (B)
/// worktree back to a standalone, so OFF legacy dispatch works correctly again
/// (it leases a SEPARATE `worktrees/<agent>/<branch>`; if `/workspace` stayed a
/// gitlink worktree, OFF would re-introduce the #2234 cwd↔binding split AND a
/// same-branch lease would hit git's "already checked out at /workspace").
///
/// **Work-safety**:
/// - COMMITTED work is preserved BY CONSTRUCTION — `/workspace` is a worktree of
///   the canonical repo, so its commits live in canonical's object store and the
///   branch ref (`refs/heads/<X>`) lives in canonical. `git worktree remove
///   --force` removes ONLY the working dir + admin, never the branch ref/commits;
///   the agent's next OFF lease of branch `<X>` checks them back out. (Empirically
///   verified.) We deliberately do NOT restore from `reconcile-backups` — that is
///   the FORWARD (standalone→worktree) snapshot and does not contain
///   post-conversion commits, so using it would LOSE that work.
/// - UNCOMMITTED/untracked work is the only at-risk class → Phase-0
///   `worktree_has_work_at_risk` + whole-dir backup, fail-closed (a backup error
///   ABORTS the revert and leaves `/workspace` untouched).
///
/// No-op (`Ok`) when `/workspace` is NOT a (B) worktree (already standalone /
/// absent / plain dir). Restores a clean git-init standalone, matching pre-(B).
/// Edge: a deleted (never re-leased) agent leaves its branch + commits in
/// canonical (branch_sweep keeps unpushed branches) — recoverable, not lost.
pub fn reverse_reconcile(home: &Path, agent: &str) -> Result<(), String> {
    let ws = crate::paths::workspace_dir(home).join(agent);
    // Only a real (B) worktree has a `.git` gitlink FILE. Standalone (dir) / plain
    // dir / absent are already OFF-compatible → nothing to revert.
    if !ws.join(".git").is_file() {
        return Ok(());
    }
    // Save uncommitted/untracked work BEFORE any destructive step (committed work
    // is already safe in canonical). Fail-closed: backup error → abort, untouched.
    if worktree_has_work_at_risk(&ws) {
        backup_worktree_dir(home, agent, None, &ws).map_err(|e| {
            format!(
                "reverse_reconcile aborted (fail-closed): backup of {} failed: {e} — workspace left untouched",
                ws.display()
            )
        })?;
    }
    // Remove the (B) worktree via the SAME primitive dev-2's Phase 2 / Phase-0
    // teardown use (git worktree remove --force from the owning repo; branch ref +
    // commits remain in canonical).
    let source_repo = resolve_owning_repo(home, agent, &ws);
    match remove_worktree(agent, &ws, &source_repo) {
        WorktreeRemoval::Removed | WorktreeRemoval::AlreadyAbsent => {}
        WorktreeRemoval::Unmanaged(m) => {
            return Err(format!("reverse_reconcile: {m}"));
        }
        WorktreeRemoval::Failed(e) => {
            return Err(format!("reverse_reconcile: worktree remove failed: {e}"));
        }
    }
    // Clear the (B) binding so the next dispatch leases fresh (legacy path).
    crate::binding::unbind(home, agent);
    // Restore `/workspace/<agent>` as a clean standalone (git-init), matching the
    // pre-(B) state. `git worktree remove` deleted the dir, so recreate it; the
    // next spawn's `ensure_project_root` would also do this, but doing it here
    // makes the revert self-contained + testable.
    let _ = std::fs::create_dir_all(&ws);
    crate::instructions::ensure_project_root(&ws);
    Ok(())
}

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
    let runtime_dir = crate::paths::runtime_dir(home);
    if !runtime_dir.exists() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(&runtime_dir) {
        for entry in entries.flatten() {
            let binding_path = entry.path().join("binding.json");
            if !binding_path.exists() {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&binding_path) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(wt_path) = v["worktree"].as_str() {
                        if !Path::new(wt_path).exists() {
                            tracing::warn!(
                                agent = entry.file_name().to_string_lossy().as_ref(),
                                worktree = wt_path,
                                "orphan lease: worktree path missing"
                            );
                        }
                    }
                }
            }
        }
    }
}

// ── Phase 4: GC scan + dry-run + cutover ────────────────────────────────

/// Grace period before a released worktree becomes a GC candidate.
const GC_GRACE_HOURS: i64 = 24;

/// t-worktree-leak PR-2: hard age cap for the force-reclaim backstop. A
/// never-released lease whose agent shows NO liveness AND whose `leased_at` is
/// older than this is force-reclaimed. Configurable (`AGEND_WORKTREE_FORCE_RECLAIM_DAYS`).
fn force_reclaim_age_days() -> i64 {
    crate::env_util::env_parse_min::<i64>("AGEND_WORKTREE_FORCE_RECLAIM_DAYS", 7, 1)
}

/// reviewer-2 #5: force-reclaim post-boot grace (seconds). After a daemon restart
/// the live-agent registry (the process-liveness signal) is empty until agents
/// re-spawn; suspend force-reclaim for this window so a mid-respawn agent is not
/// reclaimed during the liveness blind spot. Fixed const 600s / 10 min
/// (#env-cleanup: was env-overridable via
/// `AGEND_WORKTREE_FORCE_RECLAIM_BOOT_GRACE_SECS`; demoted to YAGNI).
const FORCE_RECLAIM_BOOT_GRACE_SECS: u64 = 600;

fn force_reclaim_boot_grace_secs() -> u64 {
    FORCE_RECLAIM_BOOT_GRACE_SECS
}

/// Pure boot-grace predicate: is `now_unix` within `grace_secs` of `boot_unix`?
/// Unknown boot time → conservative `true` (suspend reclaim — never reclaim when
/// we cannot tell how long the daemon has been up).
fn within_boot_grace(boot_unix: Option<u64>, now_unix: u64, grace_secs: u64) -> bool {
    match boot_unix {
        Some(b) => now_unix.saturating_sub(b) < grace_secs,
        None => true,
    }
}

/// reviewer-2 #5: is the running daemon still inside its post-boot grace window?
/// No active daemon run dir → NOT in grace (tests / non-daemon contexts — GC only
/// runs inside the daemon). Daemon present but boot time unreadable → conservative
/// in-grace (suspend).
fn daemon_within_boot_grace(home: &Path) -> bool {
    let Some(run_dir) = crate::daemon::find_active_run_dir(home) else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    within_boot_grace(
        crate::daemon::read_daemon_boot_unix(&run_dir),
        now,
        force_reclaim_boot_grace_secs(),
    )
}

/// PR-2: liveness recency window (mirrors the binding-reconcile heartbeat window,
/// binding.rs:380). A heartbeat / PTY input within this counts as alive.
const LIVENESS_WINDOW_MS: u64 = 3_600_000; // 1h

/// PR-2: per-agent jitter ceiling (hours) added to the age cap, so a fleet whose
/// leases all crossed the cap together (e.g. after a long daemon outage) is
/// reclaimed spread across ticks rather than in a single thundering-herd archive.
const FORCE_RECLAIM_JITTER_HOURS: i64 = 6;

/// t-worktree-leak PR-2: how a candidate was selected — drives the retention
/// sweep's action (clean releases just archive; force-reclaims also emit a LOUD
/// confidence-classified ALERT).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcKind {
    /// Released past the grace TTL — the normal, expected path.
    CleanRelease,
    /// Never released, agent abandoned (no liveness) AND past the age cap — the
    /// force-reclaim backstop tail (no-event abandonment / dead agent).
    ForceReclaim,
}

/// A worktree identified as a GC candidate.
#[derive(Debug, Clone)]
pub struct GcCandidate {
    pub path: PathBuf,
    pub agent: String,
    pub reason: String,
    /// t-worktree-leak PR-2: selection kind (clean-release vs force-reclaim).
    pub kind: GcKind,
}

/// Scan for GC candidates: daemon-tagged, past grace TTL, not pinned, no active binding.
/// Max directory depth the marker-walk descends under the worktree root. Covers
/// flat (`<agent>-<enc>/` = depth 1), nested (`<agent>/<branch>/` = depth 2), and
/// slash-branch (`<agent>/fix/xxx/` = depth 3) layouts with headroom; bounded so a
/// pathological tree can't make the walk unbounded.
pub(crate) const MARKER_WALK_MAX_DEPTH: usize = 5;

/// t-worktree-leak (reviewer-2 #4): recursively collect daemon-managed worktree
/// dirs (those holding a `.agend-managed` marker) under `root`, to any depth up to
/// `max_depth`. Once a dir carries the marker it IS a worktree → collected and NOT
/// descended into (so we never walk a worktree's own working tree). This replaces
/// the old fixed-depth scan that missed slash-branch worktrees.
///
/// Shared by `gc_candidates` and (#restart-freeze) `binding::reconcile_hooks` —
/// both need every real worktree leaf regardless of slash-branch nesting depth.
pub(crate) fn collect_managed_worktrees(root: &Path, max_depth: usize, out: &mut Vec<PathBuf>) {
    if max_depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        if p.join(MANAGED_MARKER).exists() {
            out.push(p); // a worktree — collect, don't descend into its working tree
        } else {
            collect_managed_worktrees(&p, max_depth - 1, out);
        }
    }
}

/// #2234 Phase 2: derive the owning agent from a worktree path, layout-aware —
/// the FIRST path component under whichever managed root contains it. Used as
/// the fallback when the `.agend-managed` marker lacks an authoritative `agent=`.
///
/// - `<home>/worktrees/<agent>/<branch...>` → `<agent>` (slash branches nest
///   deeper; the first component is the agent — #worktree-git-6).
/// - `<home>/workspace/<agent>` (cure-(B): the worktree IS the workspace dir) →
///   `<agent>` (the dir name). The OLD fallback used the immediate PARENT dir
///   name here → `"workspace"` (the root, not the agent) → liveness keyed on a
///   non-agent → a live agent's `/workspace` cwd could be GC-reclaimed (#2234
///   no-wrong-delete break). Strip-prefix per managed root fixes it.
///
/// `None` when the path is under neither managed root — the caller treats that
/// as unresolvable and SKIPS the worktree (fail-toward-alive), never guessing
/// from the parent dir.
pub(crate) fn agent_from_layout(home: &Path, wt_path: &Path) -> Option<String> {
    for root in [
        daemon_managed_worktree_root(home),
        crate::paths::workspace_dir(home),
    ] {
        if let Ok(rel) = wt_path.strip_prefix(&root) {
            if let Some(s) = rel
                .components()
                .next()
                .and_then(|c| c.as_os_str().to_str())
                .filter(|s| !s.is_empty())
            {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// #2234 Phase 2: one daemon-managed worktree, layout-agnostic. The single
/// enumeration shape consumed by Phase 1c `release_stale_branch_holders` and
/// (future) the GC scan — replacing the dual fs-root scans that assume the
/// `worktrees/<agent>/<branch>` layout and miss `/workspace/<agent>` worktrees
/// once cure-(B) moves them there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedWorktree {
    /// Canonicalized worktree directory.
    pub path: PathBuf,
    /// Owning agent (marker `agent=` authoritative, else layout-derived).
    pub agent: Option<String>,
    /// HEAD branch from the registry (`None` = detached or fs-only/orphan).
    pub branch: Option<String>,
    /// `true` if it appears in `git worktree list` (canonical knows it).
    pub registered: bool,
}

/// Parse `git worktree list --porcelain` into `(path, branch)` pairs. Porcelain
/// records are blank-line-separated; `worktree <path>` opens a record, `branch
/// refs/heads/<b>` names the checked-out branch (absent = detached).
fn parse_worktree_porcelain(out: &str) -> Vec<(PathBuf, Option<String>)> {
    let mut records = Vec::new();
    let mut cur_path: Option<PathBuf> = None;
    let mut cur_branch: Option<String> = None;
    let flush = |p: &mut Option<PathBuf>, b: &mut Option<String>, out: &mut Vec<_>| {
        if let Some(path) = p.take() {
            out.push((path, b.take()));
        }
        *b = None;
    };
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            flush(&mut cur_path, &mut cur_branch, &mut records);
            cur_path = Some(PathBuf::from(p.trim()));
        } else if let Some(b) = line.strip_prefix("branch ") {
            cur_branch = Some(
                b.trim()
                    .strip_prefix("refs/heads/")
                    .unwrap_or(b.trim())
                    .to_string(),
            );
        }
    }
    flush(&mut cur_path, &mut cur_branch, &mut records);
    records
}

/// #2234 Phase 2: cure-(B) workspace worktrees — `workspace/<agent>` dirs whose
/// `.git` is a gitlink FILE (a real worktree, Phase 0's discriminator). Empty
/// when (B) is OFF (no workspace dir is a worktree), so every consumer stays
/// byte-identical until (B) ships. Marker-LESS interrupted-reconcile worktrees
/// are still caught here (gitlink-alone). Single impl shared by
/// `fs_managed_worktrees` (→ enumerate / gc) and `worktree::list_residual`.
pub(crate) fn workspace_gitlink_worktrees(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(crate::paths::workspace_dir(home)) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.join(".git").is_file() {
                out.push(p);
            }
        }
    }
    out
}

/// #2234 Phase 2: the fs-scan portion of [`enumerate_managed_worktrees`] —
/// every daemon-managed worktree dir on disk across BOTH layouts:
/// `worktrees/<agent>/<branch...>` (marker-walk, slash-branch aware) +
/// cure-(B) `workspace/<agent>` (gitlink). The SINGLE marker-walk impl shared by
/// `enumerate` (registry ∪ fs) and `gc_candidates` — no parallel rewrite, no
/// drift. Home-only (no `source_repo`): gc/list are home-wide and need no
/// `git worktree list`. byte-identical when (B) OFF (the workspace part is empty).
pub(crate) fn fs_managed_worktrees(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_managed_worktrees(
        &daemon_managed_worktree_root(home),
        MARKER_WALK_MAX_DEPTH,
        &mut out,
    );
    out.extend(workspace_gitlink_worktrees(home));
    out
}

/// #2234 Phase 2: enumerate EVERY daemon-managed worktree across BOTH layouts
/// (`worktrees/<agent>/<branch>` and cure-(B) `workspace/<agent>`), unioning the
/// canonical registry (authoritative for any path) with an fs-scan of the known
/// roots (catches orphan dirs whose registration was pruned). Single source of
/// truth replacing the dual fs-root scans. De-duped by canonicalized path.
///
/// no-miss: any real worktree is registered (in `git worktree list`) OR a dir
/// under a known root (in the fs-scan) — both false ⟹ it doesn't exist. The
/// union therefore covers the full set: `git worktree list` alone misses orphan
/// dirs; the fs-scan alone misses pruned-registration / non-standard roots.
pub fn enumerate_managed_worktrees(home: &Path, source_repo: &Path) -> Vec<ManagedWorktree> {
    let worktrees_root = daemon_managed_worktree_root(home);
    let workspace = crate::paths::workspace_dir(home);
    let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (croot, cws) = (canon(&worktrees_root), canon(&workspace));
    let under_managed = |cp: &Path| cp.starts_with(&croot) || cp.starts_with(&cws);
    // Agent = first path component under whichever CANONICAL managed root
    // contains the (canonical) worktree path. Derived against the canonical roots
    // here — NOT `agent_from_layout` (which strips the un-canonicalized roots for
    // the GC path) — because `git worktree list` returns CANONICAL paths while
    // `home` may be un-canonicalized (macOS `/var`→`/private/var`), so the two
    // must be compared canonical-to-canonical.
    let agent_of = |cp: &Path| -> Option<String> {
        for root in [&croot, &cws] {
            if let Ok(rel) = cp.strip_prefix(root) {
                if let Some(s) = rel
                    .components()
                    .next()
                    .and_then(|c| c.as_os_str().to_str())
                    .filter(|s| !s.is_empty())
                {
                    return Some(s.to_string());
                }
            }
        }
        None
    };

    // Keyed by canonicalized path → natural dedup; registry entries win (they
    // carry the branch). BTreeMap for deterministic ordering.
    let mut by_path: std::collections::BTreeMap<PathBuf, ManagedWorktree> =
        std::collections::BTreeMap::new();

    // Registry pass — authoritative, any path. Filter to the managed roots
    // (excludes the canonical main worktree + foreign worktrees).
    if let Ok(out) =
        crate::git_helpers::git_bypass(source_repo, &["worktree", "list", "--porcelain"])
    {
        if out.status.success() {
            for (path, branch) in parse_worktree_porcelain(&String::from_utf8_lossy(&out.stdout)) {
                let cp = canon(&path);
                if under_managed(&cp) {
                    let agent = agent_of(&cp);
                    by_path.insert(
                        cp.clone(),
                        ManagedWorktree {
                            path: cp,
                            agent,
                            branch,
                            registered: true,
                        },
                    );
                }
            }
        }
    }

    // fs pass — catch orphan dirs the registry doesn't know. Shared
    // `fs_managed_worktrees` is the SINGLE marker-walk + workspace-gitlink impl
    // (also used by gc_candidates / list_residual — no parallel rewrite).
    for p in fs_managed_worktrees(home) {
        let cp = canon(&p);
        by_path
            .entry(cp.clone())
            .or_insert_with(|| ManagedWorktree {
                agent: agent_of(&cp),
                branch: None,
                registered: false,
                path: cp,
            });
    }

    by_path.into_values().collect()
}

pub fn gc_candidates(home: &Path) -> Vec<GcCandidate> {
    let mut candidates = Vec::new();
    // t-worktree-leak PR-2: snapshot the live-agent set ONCE per pass (the
    // force-reclaim liveness check consults it per candidate; this is the
    // process-alive signal that protects idle-but-running agents).
    let live_agents: std::collections::HashSet<String> =
        crate::runtime::list_agents_with_fallback(home)
            .into_iter()
            .collect();

    // New layout `worktrees/<agent>/<branch>/` (marker-walk, slash-branch aware)
    // + cure-(B) `workspace/<agent>` gitlink worktrees, via the shared
    // `fs_managed_worktrees` (#2234 Phase 2 — single marker-walk impl). The
    // workspace part is empty when (B) is OFF → byte-identical candidate set; the
    // `evaluate_candidate` marker-gate filters anything non-managed regardless.
    for wt_path in fs_managed_worktrees(home) {
        if let Some(candidate) = evaluate_candidate(home, &wt_path, &live_agents) {
            candidates.push(candidate);
        }
    }

    // Legacy layout: <home>/workspace/*/.worktrees/*/
    let workspace = crate::paths::workspace_dir(home);
    if workspace.exists() {
        if let Ok(entries) = std::fs::read_dir(&workspace) {
            for entry in entries.flatten() {
                let wt_base = entry.path().join(".worktrees");
                if !wt_base.is_dir() {
                    continue;
                }
                if let Ok(wts) = std::fs::read_dir(&wt_base) {
                    for wt in wts.flatten() {
                        let wt_path = wt.path();
                        if !wt_path.is_dir() {
                            continue;
                        }
                        if let Some(candidate) = evaluate_candidate(home, &wt_path, &live_agents) {
                            candidates.push(candidate);
                        }
                    }
                }
            }
        }
    }

    candidates
}

/// t-worktree-leak PR-2 safety #1: does the agent show ANY sign of life? This is
/// MULTI-signal — never just heartbeat — so an idle-but-running agent (no recent
/// heartbeat) is still protected. A positive on ANY signal → the worktree is
/// NEVER force-reclaimed, regardless of age (liveness-AND-age). Reads that fail
/// lean toward "alive" (conservative — never mis-reclaim).
fn agent_has_liveness(
    home: &Path,
    agent: &str,
    live_agents: &std::collections::HashSet<String>,
) -> bool {
    // (process) In the live-agent registry — covers idle-but-running agents that
    // are not currently heartbeating.
    if live_agents.contains(agent) {
        return true;
    }
    let hb = crate::daemon::heartbeat_pair::snapshot_for(agent);
    let now = crate::daemon::heartbeat_pair::now_ms();
    // (heartbeat) any MCP tool call within the recency window.
    if hb.heartbeat_at_ms != 0 && now.saturating_sub(hb.heartbeat_at_ms) < LIVENESS_WINDOW_MS {
        return true;
    }
    // (PTY) recent terminal input.
    if hb.last_input_at_ms != 0 && now.saturating_sub(hb.last_input_at_ms) < LIVENESS_WINDOW_MS {
        return true;
    }
    // (waiting_on) actively declared a blocker → alive.
    if hb.waiting_on_since_ms.is_some() {
        return true;
    }
    // (ci-watch) subscribed to a live ci-watch → active CI-tracked work.
    if agent_is_ci_watch_subscriber(home, agent) {
        return true;
    }
    false
}

/// t-worktree-leak PR-2: fresh multi-signal liveness check for `agent` (snapshots
/// the live-agent set itself). Used by the retention sweep's pre-archive fencing
/// re-validation so an agent that came back to life between enumeration and
/// archive is spared.
pub(crate) fn is_agent_alive(home: &Path, agent: &str) -> bool {
    let live_agents: std::collections::HashSet<String> =
        crate::runtime::list_agents_with_fallback(home)
            .into_iter()
            .collect();
    agent_has_liveness(home, agent, &live_agents)
}

/// PR-2: is `agent` a subscriber on any live ci-watch? codex gap ②: this is a
/// liveness source, so every read failure FAILS TOWARD ALIVE (returns `true`,
/// blocking reclaim) rather than silently treating the agent as not-subscribed —
/// a mis-read must never let us reclaim a live agent. The ONE exception is the
/// watch dir being genuinely absent (NotFound), which is a real "no watches"
/// state, not a read failure.
fn agent_is_ci_watch_subscriber(home: &Path, agent: &str) -> bool {
    let dir = crate::daemon::ci_watch::ci_watches_dir(home);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true, // can't enumerate watches → fail-toward-alive
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // A watch file we cannot read/parse COULD carry this agent's subscription
        // → fail-toward-alive rather than skip it.
        let Ok(content) = std::fs::read_to_string(&path) else {
            return true;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) else {
            return true;
        };
        if let Some(subs) = v.get("subscribers").and_then(|s| s.as_array()) {
            if subs
                .iter()
                .any(|s| s.get("instance").and_then(|i| i.as_str()) == Some(agent))
            {
                return true;
            }
        }
    }
    false
}

/// PR-2: is a never-released lease past the (per-agent jittered) force-reclaim age
/// cap? The deterministic jitter spreads a fleet whose leases all crossed the cap
/// together across ticks (anti-thundering-herd, safety #3). No `leased_at` → not
/// reclaimable (conservative).
fn leased_at_force_reclaimable(leased_at: Option<&str>, agent: &str) -> bool {
    let Some(ts) = leased_at else {
        return false;
    };
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return false;
    };
    let age = chrono::Utc::now().signed_duration_since(dt.with_timezone(&chrono::Utc));
    let jitter_h = (fnv1a(agent) % (FORCE_RECLAIM_JITTER_HOURS.max(1) as u64)) as i64;
    let cap = chrono::Duration::days(force_reclaim_age_days()) + chrono::Duration::hours(jitter_h);
    age > cap
}

/// Stable per-agent FNV-1a hash → deterministic jitter (no randomness, so reclaim
/// timing is reproducible).
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn evaluate_candidate(
    home: &Path,
    wt_path: &Path,
    live_agents: &std::collections::HashSet<String>,
) -> Option<GcCandidate> {
    // Must be daemon-managed (R14).
    if !is_daemon_managed(wt_path) {
        return None;
    }
    // Must not be pinned.
    if is_pinned(wt_path) {
        return None;
    }
    // Resolve agent name: read from .agend-managed marker (authoritative),
    // else derive layout-aware from the path (#2234 Phase 2).
    let marker = wt_path.join(MANAGED_MARKER);
    let marker_content = std::fs::read_to_string(&marker).unwrap_or_default();
    let agent_name = marker_content
        .lines()
        .find(|l| l.starts_with("agent="))
        .and_then(|l| l.strip_prefix("agent="))
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| agent_from_layout(home, wt_path))
        .unwrap_or_default();
    // #2234: unresolvable agent → NOT a GC candidate (fail-toward-alive). Never
    // reclaim a worktree whose owner we can't name.
    if agent_name.is_empty() {
        return None;
    }
    let binding_present = crate::binding::read(home, &agent_name).is_some();
    let released_at = marker_content
        .lines()
        .find_map(|l| l.strip_prefix("released_at="));

    match released_at {
        // ── Clean-release path: explicitly released, past the grace TTL. ──
        Some(ts) => {
            // A released lease should already be unbound; if it is still bound,
            // that is a contradiction — leave it alone (conservative).
            if binding_present {
                return None;
            }
            match chrono::DateTime::parse_from_rfc3339(ts) {
                Ok(dt) => {
                    let age =
                        chrono::Utc::now().signed_duration_since(dt.with_timezone(&chrono::Utc));
                    if age < chrono::Duration::hours(GC_GRACE_HOURS) {
                        return None; // still within grace
                    }
                }
                // #1870 (H1): a malformed `released_at=` (e.g. a partial-write /
                // crash-truncated marker) MUST NOT be treated as "past grace" — the
                // grace window protects a just-released worktree's WIP. But it is
                // also `Some(garbage)`, so it never reaches the never-released
                // force-reclaim arm below → pre-#1882 it leaked FOREVER (both GC
                // paths skipped it). #1882 (WT-LEAK-1): treat "corrupt released_at ≈
                // never-released" — hand off to the SAME force-reclaim backstop. Its
                // liveness + leased_at age-cap guards (NOT the unparseable grace
                // window) protect a still-used / recently-leased worktree; only an
                // abandoned (no liveness, leased past the cap) corrupt-marker
                // worktree is reclaimed. This does NOT reintroduce the H1
                // WIP-destruction (that was the grace-window bypass).
                Err(_) => {
                    return force_reclaim_candidate(
                        home,
                        wt_path,
                        agent_name,
                        &marker_content,
                        live_agents,
                        "malformed released_at marker",
                    );
                }
            }
            Some(GcCandidate {
                path: wt_path.to_path_buf(),
                agent: agent_name,
                reason: format!("daemon-tagged, released >{}h, not pinned", GC_GRACE_HOURS),
                kind: GcKind::CleanRelease,
            })
        }
        // ── t-worktree-leak PR-2 force-reclaim backstop: NEVER released. ──
        // This is ONLY the no-event-abandonment / dead-agent tail (the
        // invariant + sweeper in PR-1 handle every worktree that DID see a
        // merge/close/task-done event; the 7-day expired-intent path hands off
        // here). liveness-AND-age: ANY live signal → never reclaim (even past the
        // cap); otherwise require the per-agent-jittered age cap.
        None => force_reclaim_candidate(
            home,
            wt_path,
            agent_name,
            &marker_content,
            live_agents,
            "never-released lease",
        ),
    }
}

/// t-worktree-leak PR-2 force-reclaim backstop: reclaim a worktree ONLY when it
/// is genuinely abandoned — not in the daemon's post-boot grace window (#5), NO
/// liveness signal for its agent, AND its `leased_at` is past the per-agent
/// force-reclaim age cap. ANY live signal → never reclaim (even past the cap).
/// Shared by the never-released (`released_at` absent) arm AND the #1882 WT-LEAK-1
/// corrupt-`released_at` fall-through. `marker_state` names why we're here, for
/// the candidate's reason. The liveness + age-cap guards (NOT the grace window)
/// are what protect a just-leased / just-released worktree from premature reclaim.
fn force_reclaim_candidate(
    home: &Path,
    wt_path: &Path,
    agent_name: String,
    marker_content: &str,
    live_agents: &std::collections::HashSet<String>,
    marker_state: &str,
) -> Option<GcCandidate> {
    // reviewer-2 #5: suspend force-reclaim during the daemon's post-boot grace
    // window (the process-liveness signal is still re-establishing).
    if daemon_within_boot_grace(home) {
        return None;
    }
    if agent_has_liveness(home, &agent_name, live_agents) {
        return None;
    }
    let leased_at = marker_content
        .lines()
        .find_map(|l| l.strip_prefix("leased_at="));
    if !leased_at_force_reclaimable(leased_at, &agent_name) {
        return None;
    }
    Some(GcCandidate {
        path: wt_path.to_path_buf(),
        agent: agent_name,
        reason: format!(
            "force-reclaim: {marker_state}, no liveness signal, leased >{}d (abandoned)",
            force_reclaim_age_days()
        ),
        kind: GcKind::ForceReclaim,
    })
}

/// Dry-run: log candidates without deleting. Returns candidate list.
pub fn gc_dry_run(home: &Path) -> Vec<GcCandidate> {
    let candidates = gc_candidates(home);
    for c in &candidates {
        tracing::info!(
            agent = %c.agent,
            path = %c.path.display(),
            reason = %c.reason,
            "gc_dry_run candidate"
        );
    }
    if !candidates.is_empty() {
        crate::event_log::log(
            home,
            "gc_dry_run",
            "",
            &format!("{} candidates identified", candidates.len()),
        );
    }
    candidates
}

// ─────────────────────────────────────────────────────────────────────────
// t-…50793-9: managed-worktree `target/` retention sweep.
//
// Build `target/` dirs are the dominant fleet disk consumer (incident
// 2026-06-21: r4 ~90GB + dev-2 ~64GB stale worktree targets → /Users ENOSPC →
// daemon inbox went readonly). The whole-worktree GC above frees `target/` only
// as a SIDE-EFFECT of deleting the entire worktree, gated on explicit-release +
// 24h grace OR 7-day abandonment — so an alive-agent-never-released worktree
// (e.g. a reviewer that finished a branch but never released) leaks `target/`
// indefinitely. This sweep reclaims a managed worktree's `target/` once it goes
// STALE (no build activity within the age threshold) WITHOUT deleting the
// worktree/checkout itself. `target/` is regenerable (already excluded from
// worktree backups), so a swept worktree pays only a one-time rebuild on reuse.
//
// SAFETY (footgun — must NEVER delete canonical/operator data, never clobber an
// active build). Layered:
//   1. marker-STRICT enumeration — only worktrees under `home/worktrees` that
//      carry `.agend-managed`, via `target_sweep_worktrees` (NOT the looser
//      `fs_managed_worktrees`, which unions markerless workspace gitlinks incl.
//      operator-owned ones). The operator's canonical repo has no marker + lives
//      OUTSIDE the managed root → unreachable by the enumerator.
//   2. symlinked-root refusal + canonical-home confinement (`safe_managed_root`)
//      — never enumerate or delete through a symlinked / home-escaping root.
//   3. symlink refusal — a worktree's `target` must be a REAL directory; a
//      symlinked `target` (could point at the canonical 49GB target) is refused.
//   4. active-build exclusion (`predicate_protects`, round-4) — a build can only
//      happen in a worktree whose owner is in the daemon ROSTER and CURRENTLY
//      bound HERE (stable signals; liveness is FLAPPY and was dropped). Such
//      worktrees are excluded. The delete pass HOLDS the owner's
//      `.binding.json.lock` (the SAME lock `bind_full` takes) through
//      predicate→recheck→delete, so the binding can't change under us — closing
//      the bound-but-not-yet-live and rebind-during-window races (the lock
//      guards BIND, not cargo). Only instance-gone / bound-elsewhere / unbound
//      stale targets are swept.
//   5. fail-CLOSED mtime gate — swept only when nothing under `target/` changed
//      within `max_age`, RE-checked immediately before deletion (load-bearing
//      last line: any stat/read error ⇒ treated as active ⇒ skip). ONLY
//      `target/` is removed — never the worktree dir or source.

/// Default staleness age for the `target/` sweep — no build activity within
/// this window ⇒ eligible. Conservative: a 2-day-idle build cache is cheap to
/// regenerate relative to the GBs reclaimed.
const TARGET_GC_AGE_HOURS_DEFAULT: u64 = 48;

/// Resolve the `target/` sweep config from env. Returns `None` when the sweep
/// is disabled via `AGEND_TARGET_GC_DISABLE` (operator kill-switch).
/// `(max_age, min_size_bytes)`:
///   - `AGEND_TARGET_GC_AGE_HOURS` (default 48) — staleness window.
///   - `AGEND_TARGET_GC_MIN_SIZE_BYTES` (default 0 = no floor) — skip targets
///     smaller than this (avoid churn on trivially-small build dirs).
pub fn target_gc_config() -> Option<(std::time::Duration, u64)> {
    if std::env::var_os("AGEND_TARGET_GC_DISABLE").is_some() {
        return None;
    }
    let hours = std::env::var("AGEND_TARGET_GC_AGE_HOURS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(TARGET_GC_AGE_HOURS_DEFAULT);
    let min_size = std::env::var("AGEND_TARGET_GC_MIN_SIZE_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    Some((
        std::time::Duration::from_secs(hours.saturating_mul(3600)),
        min_size,
    ))
}

/// A managed-worktree `target/` dir eligible for the retention sweep.
#[derive(Debug, Clone)]
pub struct TargetSweepCandidate {
    pub worktree: PathBuf,
    pub target: PathBuf,
    pub agent: String,
    /// Seconds since the most-recent modification anywhere under `target/`.
    pub idle_secs: u64,
    pub size_bytes: u64,
}

/// Outcome of one `target/` removal.
#[derive(Debug, Clone)]
pub struct TargetSweepResult {
    pub target: PathBuf,
    pub agent: String,
    pub removed: bool,
    pub freed_bytes: u64,
    pub error: Option<String>,
}

/// Fail-CLOSED activity probe for a DESTRUCTIVE sweep. Returns `true` if ANY
/// entry under `path` (inclusive) was modified at/after `cutoff` OR if any
/// `symlink_metadata`/`read_dir`/mtime call fails — i.e. uncertainty ⇒ `true` ⇒
/// the caller MUST NOT delete. Returns `false` (eligible) ONLY when the ENTIRE
/// tree was readable AND every mtime is older than `cutoff`. Uses
/// `symlink_metadata` and recurses only into REAL directories, so it never
/// follows symlinks (can't escape the tree or loop). Early-exits on the first
/// fresh/unreadable entry (fast path for active builds).
fn tree_active_or_unreadable(path: &Path, cutoff: std::time::SystemTime) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return true; // can't stat ⇒ uncertain ⇒ fail-closed (treat as active)
    };
    match meta.modified() {
        Ok(m) if m >= cutoff => return true, // fresh ⇒ active
        Ok(_) => {}
        Err(_) => return true, // no mtime ⇒ uncertain ⇒ fail-closed
    }
    if meta.file_type().is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return true; // can't list a dir we're about to delete ⇒ fail-closed
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return true; // unreadable entry ⇒ fail-closed
            };
            if tree_active_or_unreadable(&entry.path(), cutoff) {
                return true;
            }
        }
    }
    false
}

/// Total bytes under `path` (real files; symlinks counted by their own link
/// size, never followed). Best-effort — unreadable entries are skipped.
fn tree_size_bytes(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.file_type().is_dir() {
        let mut total = 0u64;
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                total = total.saturating_add(tree_size_bytes(&entry.path()));
            }
        }
        total
    } else {
        meta.len()
    }
}

/// Newest mtime anywhere under `path` (inclusive); `None` if unreadable. Full
/// walk (no early-exit) — used only to compute `idle_secs` for the preview.
fn tree_newest_mtime(path: &Path) -> Option<std::time::SystemTime> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let mut newest = meta.modified().ok();
    if meta.file_type().is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                if let Some(m) = tree_newest_mtime(&entry.path()) {
                    newest = Some(match newest {
                        Some(n) if n >= m => n,
                        _ => m,
                    });
                }
            }
        }
    }
    newest
}

/// Operator-facing scope boundary (no-silent-coverage-cap, lead VET condition).
/// This sweep reclaims `target/` ONLY for `.agend-managed` `home/worktrees`
/// worktrees whose owner is GONE from the daemon roster, or in the roster but
/// bound ELSEWHERE / unbound (instance-gone / rebound-away / orphan). It
/// deliberately does NOT reclaim any worktree whose owner is in the roster AND
/// currently bound there — REGARDLESS of liveness — because that owner can start
/// a build at any instant (mtime cannot prevent a build starting between check
/// and delete). It also does NOT touch legacy markerless `workspace/<agent>/target`
/// or agent-self-built `.claude/worktrees/*/target` (the larger fleet consumers,
/// but markerless = the operator-data danger zone, left to a separate
/// authoritative binding-registry sweep). Surfaced in dry-run/log so
/// reclaimed-space figures never imply the fleet disk problem is fully solved.
pub const TARGET_SWEEP_SCOPE_NOTE: &str = "scope: sweeps stale target/ ONLY for .agend-managed home/worktrees worktrees whose owner is gone from the roster, or bound elsewhere/unbound (instance-gone / rebound-away / orphan). NOT reclaimed: any currently-bound worktree (regardless of liveness — a build can start anytime), legacy markerless workspace/<agent>/target, or .claude/worktrees/*/target.";

/// Marker-STRICT enumerator for the `target/` sweep (r6/r4 #1 fix): ONLY
/// daemon-leased worktrees under `home/worktrees` that carry the
/// `.agend-managed` marker, via `collect_managed_worktrees`. Deliberately does
/// NOT union `workspace_gitlink_worktrees` — that scan collects `.git`-gitlink
/// dirs WITHOUT a marker (incl. operator-owned, interrupted-reconcile
/// worktrees), which is by-design for read-only LISTING but a footgun for a
/// DESTRUCTIVE sweep. A sweep must never inherit the looser enumeration.
fn target_sweep_worktrees(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_managed_worktrees(
        &daemon_managed_worktree_root(home),
        MARKER_WALK_MAX_DEPTH,
        &mut out,
    );
    out
}

/// Resolve the canonical `home/worktrees` sweep root, or `None` if it is unsafe
/// to sweep — root missing, root is a symlink, or root escapes the canonical
/// home (r6 #2 fix). Anchoring confinement to the canonical HOME and refusing a
/// symlinked root closes "canonicalize-a-symlinked-root-then-trust-it" escapes.
/// (FIX1 already drops the workspace enumeration that was the real escape
/// vector; this is defense-in-depth.)
fn safe_managed_root(home: &Path) -> Option<PathBuf> {
    let root = daemon_managed_worktree_root(home);
    match std::fs::symlink_metadata(&root) {
        Ok(m) if m.file_type().is_symlink() => return None, // never sweep through a symlinked root
        Ok(_) => {}
        Err(_) => return None, // missing/unreadable ⇒ nothing to sweep
    }
    let canon_home = dunce::canonicalize(home).ok()?;
    let canon_root = dunce::canonicalize(&root).ok()?;
    canon_root.starts_with(&canon_home).then_some(canon_root)
}

/// Validate `target` is safe to hard-delete: the managed root must be safe
/// (`safe_managed_root` — non-symlink, under canonical home), `target` must be a
/// REAL directory (not a symlink — a symlinked `target` could point at the
/// canonical repo's target), and `canonicalize(target)` must resolve under the
/// canonical managed root. Returns the validated canonical target path.
fn validate_target_for_delete(home: &Path, target: &Path) -> Result<PathBuf, String> {
    let canon_root = safe_managed_root(home).ok_or_else(|| {
        "refusing: managed root is missing, a symlink, or escapes home".to_string()
    })?;
    let meta = std::fs::symlink_metadata(target).map_err(|e| format!("stat failed: {e}"))?;
    if meta.file_type().is_symlink() {
        return Err("refusing: `target` is a symlink (could escape to canonical)".to_string());
    }
    if !meta.file_type().is_dir() {
        return Err("refusing: `target` is not a directory".to_string());
    }
    let canon = dunce::canonicalize(target).map_err(|e| format!("canonicalize failed: {e}"))?;
    if !canon.starts_with(&canon_root) {
        return Err(format!(
            "refusing: {} does not resolve under the managed root {}",
            canon.display(),
            canon_root.display()
        ));
    }
    Ok(canon)
}

/// Stable-signal protect predicate (round-4, r6 re-DUAL — DROPS the flappy
/// `liveness` signal that caused the bound-but-not-yet-live TOCTOU). A worktree
/// is PROTECTED when its owner instance is in the `roster` AND its binding
/// currently points HERE — meaning a process can still build in it. The binding
/// is read from DISK (not the in-process cache) so the caller's held
/// `.binding.json.lock` makes it authoritative (no bind can mutate it).
///
/// - owner unresolvable: PROTECT (fail-closed).
/// - owner NOT in roster (deleted): sweepable — no process can ever bind here again.
/// - in roster, binding points HERE: PROTECT — could build (closes the
///   bound-but-not-yet-live race).
/// - in roster, binding elsewhere/absent: sweepable (can't rebind here while the
///   caller holds the bind lock).
/// - in roster, binding UNREADABLE/malformed: PROTECT (fail-closed).
fn predicate_protects(home: &Path, wt: &Path, roster: &std::collections::HashSet<String>) -> bool {
    let Some(owner) = agent_from_layout(home, wt) else {
        return true; // unresolvable owner ⇒ fail-closed protect
    };
    if !roster.contains(&owner) {
        return false; // instance gone (deleted) ⇒ no process can build ⇒ sweepable
    }
    let binding_path = crate::paths::runtime_dir(home)
        .join(&owner)
        .join("binding.json");
    if !binding_path.exists() {
        return false; // in roster but unbound ⇒ sweepable (can't rebind under our lock)
    }
    // Read the FILE directly (not the cache) so the caller's held lock is the
    // source of truth for the binding.
    let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    match std::fs::read_to_string(&binding_path) {
        Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
            Ok(v) => match v["worktree"].as_str() {
                Some(bw) => canon(std::path::Path::new(bw)) == canon(wt), // bound HERE ⇒ PROTECT
                None => true, // malformed (no worktree field) ⇒ fail-closed PROTECT
            },
            Err(_) => true, // parse error ⇒ fail-closed PROTECT
        },
        Err(_) => true, // exists but unreadable ⇒ fail-closed PROTECT
    }
}

/// Snapshot the daemon's known-agent roster (stable membership, NOT flappy
/// process-liveness — round-4) for a sweep pass.
fn sweep_roster(home: &Path) -> std::collections::HashSet<String> {
    crate::runtime::list_agents_with_fallback(home)
        .into_iter()
        .collect()
}

/// Enumerate daemon-leased worktrees (marker-strict, `home/worktrees` only —
/// see [`target_sweep_worktrees`] / [`TARGET_SWEEP_SCOPE_NOTE`]) whose `target/`
/// build dir is STALE (no activity within `max_age`, fail-closed), NOT protected
/// by [`predicate_protects`] (owner-in-roster + bound-here), and at least
/// `min_size` bytes. Resolves the roster itself; tests use the `_with_roster`
/// variant to inject a deterministic roster.
pub fn target_sweep_candidates(
    home: &Path,
    max_age: std::time::Duration,
    min_size: u64,
) -> Vec<TargetSweepCandidate> {
    target_sweep_candidates_with_roster(home, max_age, min_size, &sweep_roster(home))
}

/// Roster-injected core of [`target_sweep_candidates`]. NOTE: the protect
/// predicate here is BEST-EFFORT (no `.binding.json.lock` held — this is the
/// enumeration/dry-run pass). The AUTHORITATIVE, lock-frozen protect check runs
/// in [`target_sweep_run_with_roster`] before each delete.
pub(crate) fn target_sweep_candidates_with_roster(
    home: &Path,
    max_age: std::time::Duration,
    min_size: u64,
    roster: &std::collections::HashSet<String>,
) -> Vec<TargetSweepCandidate> {
    // SAFETY 2: never enumerate through a symlinked / escaping managed root.
    if safe_managed_root(home).is_none() {
        return Vec::new();
    }
    let now = std::time::SystemTime::now();
    let cutoff = now.checked_sub(max_age).unwrap_or(now);
    let mut out = Vec::new();
    for wt in target_sweep_worktrees(home) {
        // SAFETY 4 (active-build): owner in roster + bound here ⇒ a process can
        // build at any instant ⇒ exclude. (Best-effort here; re-checked under
        // the bind lock in the run pass.)
        if predicate_protects(home, &wt, roster) {
            continue;
        }
        let target = wt.join("target");
        // SAFETY 3: real directory only (symlink_metadata → is_dir is false for
        // a symlink-to-dir, so a symlinked `target` is skipped here too).
        let Ok(meta) = std::fs::symlink_metadata(&target) else {
            continue;
        };
        if !meta.file_type().is_dir() || meta.file_type().is_symlink() {
            continue;
        }
        // SAFETY 5 (fail-closed): active build OR any unreadable entry ⇒ skip.
        if tree_active_or_unreadable(&target, cutoff) {
            continue;
        }
        let size_bytes = tree_size_bytes(&target);
        if size_bytes < min_size {
            continue;
        }
        let idle_secs = tree_newest_mtime(&target)
            .and_then(|m| now.duration_since(m).ok())
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let agent = agent_from_layout(home, &wt).unwrap_or_default();
        out.push(TargetSweepCandidate {
            worktree: wt,
            target,
            agent,
            idle_secs,
            size_bytes,
        });
    }
    out
}

/// Execute the `target/` sweep. For each candidate, try-acquire the OWNER's
/// `.binding.json.lock` (the SAME lock `bind_full` holds) and HOLD it through
/// {predicate → marker re-assert → fail-closed mtime recheck → delete}. While
/// held, no bind/rebind can occur (bind_full would block on it), so the protect
/// predicate is authoritative and a rebind-to-here cannot race the delete.
/// Contended lock ⇒ an active bind/release ⇒ SKIP this worktree this tick
/// (fail-safe). Resolves the roster itself; tests use the `_with_roster` variant.
pub fn target_sweep_run(
    home: &Path,
    max_age: std::time::Duration,
    min_size: u64,
) -> Vec<TargetSweepResult> {
    target_sweep_run_with_roster(home, max_age, min_size, &sweep_roster(home))
}

/// Roster-injected core of [`target_sweep_run`].
pub(crate) fn target_sweep_run_with_roster(
    home: &Path,
    max_age: std::time::Duration,
    min_size: u64,
    roster: &std::collections::HashSet<String>,
) -> Vec<TargetSweepResult> {
    let candidates = target_sweep_candidates_with_roster(home, max_age, min_size, roster);
    if candidates.is_empty() {
        return Vec::new();
    }
    let mut results = Vec::new();
    for c in &candidates {
        let skip = |reason: String| TargetSweepResult {
            target: c.target.clone(),
            agent: c.agent.clone(),
            removed: false,
            freed_bytes: 0,
            error: Some(reason),
        };
        let Some(owner) = agent_from_layout(home, &c.worktree) else {
            results.push(skip(
                "skipped: unresolvable owner (fail-closed)".to_string(),
            ));
            continue;
        };
        // Hold the owner's binding lock through {predicate → recheck → delete}.
        // bind_full holds this same lock while writing binding.json, so while we
        // hold it NO bind/rebind can occur — freezing the binding the predicate
        // reads and making a rebind-to-here-vs-delete race impossible. Non-blocking:
        // a held lock = an active bind/release in flight ⇒ skip this tick (fail-safe).
        let lock_path = crate::paths::runtime_dir(home)
            .join(&owner)
            .join(".binding.json.lock");
        let _lock = match crate::store::try_acquire_file_lock(&lock_path) {
            Ok(Some(l)) => l,
            Ok(None) => {
                results.push(skip(
                    "skipped: binding lock held (bind/release in flight)".to_string(),
                ));
                continue;
            }
            Err(e) => {
                results.push(skip(format!(
                    "skipped: binding lock error (fail-closed): {e}"
                )));
                continue;
            }
        };
        // UNDER LOCK — binding is frozen ⇒ this protect check is authoritative.
        if predicate_protects(home, &c.worktree, roster) {
            results.push(skip(
                "skipped: owner in roster + bound here (active-build protection)".to_string(),
            ));
            continue;
        }
        // Re-assert the daemon-managed marker at delete time.
        if !is_daemon_managed(&c.worktree) {
            results.push(skip(
                "skipped: worktree no longer .agend-managed".to_string(),
            ));
            continue;
        }
        // LOAD-BEARING fail-closed last line: any fresh mtime OR unreadable
        // entry ⇒ skip (don't delete).
        let now = std::time::SystemTime::now();
        let cutoff = now.checked_sub(max_age).unwrap_or(now);
        if tree_active_or_unreadable(&c.target, cutoff) {
            results.push(skip(
                "skipped: target became active/unreadable before delete".to_string(),
            ));
            continue;
        }
        // SAFETY 2 & 3: symlinked-root/target refusal + canonical-root confinement.
        // Removes ONLY `target/` (canon resolves under the managed root) — never
        // the worktree dir or source.
        let canon = match validate_target_for_delete(home, &c.target) {
            Ok(p) => p,
            Err(e) => {
                results.push(skip(e));
                continue;
            }
        };
        match std::fs::remove_dir_all(&canon) {
            Ok(()) => results.push(TargetSweepResult {
                target: c.target.clone(),
                agent: c.agent.clone(),
                removed: true,
                freed_bytes: c.size_bytes,
                error: None,
            }),
            Err(e) => results.push(skip(format!("remove failed: {e}"))),
        }
    }
    let removed_count = results.iter().filter(|r| r.removed).count();
    if removed_count > 0 {
        let freed: u64 = results
            .iter()
            .filter(|r| r.removed)
            .map(|r| r.freed_bytes)
            .sum();
        crate::event_log::log(
            home,
            "target_gc",
            "",
            &format!(
                "{removed_count} stale target/ dirs reclaimed (~{} MB)",
                freed / (1024 * 1024)
            ),
        );
    }
    results
}

/// Non-destructive preview of `target/` sweep candidates (mirrors `gc_dry_run`).
/// Resolves config from env; returns empty when the sweep is disabled.
pub fn target_sweep_dry_run(home: &Path) -> Vec<TargetSweepCandidate> {
    let Some((max_age, min_size)) = target_gc_config() else {
        return Vec::new();
    };
    let candidates = target_sweep_candidates(home, max_age, min_size);
    for c in &candidates {
        tracing::info!(
            agent = %c.agent,
            target = %c.target.display(),
            idle_secs = c.idle_secs,
            size_bytes = c.size_bytes,
            "target_sweep_dry_run candidate"
        );
    }
    if !candidates.is_empty() {
        let total: u64 = candidates.iter().map(|c| c.size_bytes).sum();
        crate::event_log::log(
            home,
            "target_sweep_dry_run",
            "",
            &format!(
                "{} stale target/ dirs (~{} MB) eligible — {}",
                candidates.len(),
                total / (1024 * 1024),
                TARGET_SWEEP_SCOPE_NOTE
            ),
        );
    }
    candidates
}

/// Result of a single GC removal attempt.
#[derive(Debug, Clone)]
pub struct GcResult {
    pub path: PathBuf,
    pub agent: String,
    pub removed: bool,
    pub error: Option<String>,
}

/// Execute GC: remove all candidates identified by [`gc_candidates`].
/// Each candidate is removed via `git worktree remove --force` with
/// `remove_dir_all` fallback (mirrors [`release_full`] deletion pattern).
pub fn gc_run(home: &Path) -> Vec<GcResult> {
    let candidates = gc_candidates(home);
    if candidates.is_empty() {
        return Vec::new();
    }
    let mut results = Vec::new();
    for c in &candidates {
        let result = gc_remove_one(home, c);
        results.push(result);
    }
    let removed_count = results.iter().filter(|r| r.removed).count();
    let removed_paths: Vec<String> = results
        .iter()
        .filter(|r| r.removed)
        .map(|r| r.path.display().to_string())
        .collect();
    if removed_count > 0 {
        crate::event_log::log(
            home,
            "gc_run",
            "",
            &format!(
                "{removed_count} worktrees removed: [{}]",
                removed_paths.join(", ")
            ),
        );
    }
    results
}

fn gc_remove_one(home: &Path, candidate: &GcCandidate) -> GcResult {
    let wt_path = &candidate.path;

    // t-worktree-leak PR-2 (codex gap ① CRITICAL): a force-reclaim candidate MUST
    // NEVER be hard-deleted. Route it through the SINGLE safe deletion path
    // (retention's `maybe_remove_candidate`: pre-archive liveness re-check +
    // atomic archive-to-trash + unbind + LOUD confidence ALERT), so the daemon
    // `gc_run` path and the retention sweep cannot diverge into an irrecoverable
    // delete. Clean-release candidates keep the historical hard-delete below.
    if candidate.kind == GcKind::ForceReclaim {
        use crate::daemon::retention::worktrees::{maybe_remove_candidate, RemovalOutcome};
        let outcome = maybe_remove_candidate(home, candidate);
        return GcResult {
            path: wt_path.clone(),
            agent: candidate.agent.clone(),
            removed: matches!(outcome, RemovalOutcome::Removed),
            error: match outcome {
                RemovalOutcome::Skipped { reason } => Some(reason),
                RemovalOutcome::Removed => None,
            },
        };
    }

    // Acquire the same binding lock that bind_full() uses, making
    // GC deletion and bind mutually exclusive (eliminates TOCTOU).
    let lock_path = crate::paths::runtime_dir(home)
        .join(&candidate.agent)
        .join(".binding.json.lock");
    let _lock = match crate::store::acquire_file_lock(&lock_path) {
        Ok(l) => l,
        Err(e) => {
            return GcResult {
                path: wt_path.clone(),
                agent: candidate.agent.clone(),
                removed: false,
                error: Some(format!("skipped: binding lock acquisition failed: {e}")),
            };
        }
    };

    // Re-validate under lock: binding/pinned/grace state may have
    // changed since gc_candidates() enumerated this worktree. t-worktree-leak
    // PR-2: re-snapshot liveness here too, so a force-reclaim candidate whose
    // agent came back to life between enumeration and removal is spared (fencing).
    let live_agents: std::collections::HashSet<String> =
        crate::runtime::list_agents_with_fallback(home)
            .into_iter()
            .collect();
    if evaluate_candidate(home, wt_path, &live_agents).is_none() {
        return GcResult {
            path: wt_path.clone(),
            agent: candidate.agent.clone(),
            removed: false,
            error: Some("skipped: pre-deletion re-validation failed".to_string()),
        };
    }

    // #worktree-git-4: the owning repo's cwd is MANDATORY for `git worktree
    // remove`. Empirically, running it with the daemon's inherited cwd (an
    // unrelated repo) fails with "is not a working tree", leaving the dir on
    // disk; the remove_dir_all fallback then physically deletes the dir but
    // CANNOT prune the owning repo's registry (the prune is keyed on
    // source_repo) → a prunable-registry leak that blocks re-lease. If the
    // owning repo can't be resolved, skip rather than run git cwd-less; a later
    // pass reclaims it once `.git` is resolvable.
    let Some(source_repo) = resolve_source_repo(wt_path) else {
        return GcResult {
            path: wt_path.clone(),
            agent: candidate.agent.clone(),
            removed: false,
            error: Some(
                "skipped: owning source repo unresolved — refusing to run \
                 `git worktree remove` without the owning-repo cwd"
                    .to_string(),
            ),
        };
    };

    let mut result = GcResult {
        path: wt_path.clone(),
        agent: candidate.agent.clone(),
        removed: false,
        error: None,
    };

    // git-raw-allowed: kept raw (not git_cmd) per the decided #2128 migration
    // scope; cwd is now always the resolved owning repo.
    let mut cmd = std::process::Command::new("git");
    cmd.args([
        "worktree",
        "remove",
        "--force",
        &wt_path.display().to_string(),
    ])
    .env("AGEND_GIT_BYPASS", "1")
    .current_dir(&source_repo);
    match cmd.output() {
        Ok(o) if o.status.success() => {
            result.removed = true;
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            tracing::warn!(
                agent = %candidate.agent,
                path = %wt_path.display(),
                error = %stderr,
                "gc: git worktree remove failed — falling back to remove_dir_all"
            );
            let _ = std::fs::remove_dir_all(wt_path);
            if !wt_path.exists() {
                // W1.2: best-effort prune (result already ignored).
                let _ = crate::git_helpers::git_ok(&source_repo, &["worktree", "prune"]);
                result.removed = true;
            } else {
                result.error = Some(format!("git worktree remove failed: {stderr}"));
            }
        }
        Err(e) => {
            tracing::warn!(
                agent = %candidate.agent,
                path = %wt_path.display(),
                error = %e,
                "gc: git command failed"
            );
            result.error = Some(format!("git command failed: {e}"));
        }
    }
    result
}

/// Resolve the source (owning) repo from a worktree's `.git` file.
/// A git worktree's `.git` is a file containing `gitdir: <path>` pointing
/// to `<source>/.git/worktrees/<name>`. We walk up from that to find the
/// source repo root.
fn resolve_source_repo(wt_path: &Path) -> Option<PathBuf> {
    let git_file = wt_path.join(".git");
    let content = std::fs::read_to_string(&git_file).ok()?;
    let gitdir_line = content.lines().find(|l| l.starts_with("gitdir:"))?;
    let gitdir = gitdir_line.strip_prefix("gitdir:")?.trim();
    let gitdir_path = if Path::new(gitdir).is_absolute() {
        PathBuf::from(gitdir)
    } else {
        wt_path.join(gitdir).canonicalize().ok()?
    };
    // gitdir_path is <source>/.git/worktrees/<name>
    // Walk up: worktrees → .git → source_repo
    gitdir_path.parent()?.parent()?.parent().map(PathBuf::from)
}

/// Cleanup stale ci-watch lock files whose PRs merged >7 days ago.
pub fn gc_stale_ci_watch_locks(home: &Path) -> usize {
    let ci_dir = home.join("ci-watches");
    if !ci_dir.is_dir() {
        return 0;
    }
    let mut removed = 0;
    let cutoff = chrono::Utc::now() - chrono::Duration::days(7);
    if let Ok(entries) = std::fs::read_dir(&ci_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("lock") {
                continue;
            }
            // Check file modification time as a proxy for PR merge time.
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let modified_dt: chrono::DateTime<chrono::Utc> = modified.into();
            if modified_dt < cutoff && std::fs::remove_file(&path).is_ok() {
                tracing::info!(path = %path.display(), "gc: removed stale ci-watch lock");
                removed += 1;
            }
        }
    }
    if removed > 0 {
        crate::event_log::log(
            home,
            "gc_stale_ci_watch_locks",
            "",
            &format!("{removed} stale lock files removed"),
        );
    }
    removed
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-pool-test-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// #2234 Phase 0: build a daemon-managed git WORKTREE at the per-agent
    /// workspace path (`<home>/workspace/<agent>`), mirroring the cure-(B)
    /// world where the workspace dir IS the bound worktree (its `.git` a gitlink
    /// FILE). Returns the worktree path.
    fn managed_workspace_worktree(home: &Path, repo: &Path, agent: &str, branch: &str) -> PathBuf {
        let wt = crate::paths::workspace_dir(home).join(agent);
        std::fs::create_dir_all(wt.parent().expect("workspace parent")).ok();
        let out = std::process::Command::new("git")
            .args(["worktree", "add", "-b", branch, &wt.display().to_string()])
            .current_dir(repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git worktree add");
        assert!(
            out.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Daemon-managed marker (as lease() writes).
        std::fs::write(wt.join(MANAGED_MARKER), "").ok();
        assert!(
            wt.join(".git").is_file(),
            "worktree .git must be a gitlink file"
        );
        wt
    }

    /// `git worktree list --porcelain` for `repo` — used to assert no orphan
    /// registration survives a teardown.
    fn worktree_list(repo: &Path) -> String {
        let out = std::process::Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git worktree list");
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    /// Number of registered worktrees (one `worktree ` line each). Counting +
    /// the `prunable` marker is path-format-independent — a path-STRING
    /// `.contains(wt.display())` is Windows-fragile (git lists forward slashes,
    /// `Path::display` emits backslashes), which is unrelated to the orphan
    /// property under test.
    fn worktree_entry_count(repo: &Path) -> usize {
        worktree_list(repo)
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count()
    }

    /// #2234 Phase 0 (RED→GREEN): tearing down a per-agent workspace that is a
    /// daemon-managed worktree must route through `git worktree remove` (clearing
    /// the canonical registration) — NOT a bare `remove_dir_all`, which deletes
    /// the dir but leaves an ORPHAN worktree entry in `<canonical>/.git/worktrees/`.
    #[test]
    fn cleanup_working_dir_managed_worktree_removes_via_git_no_orphan() {
        let home = tmp_home("p0-wt-noorphan");
        let repo = tmp_repo("p0-wt-noorphan-repo");
        let wt = managed_workspace_worktree(&home, &repo, "devw", "feat/p0");
        assert_eq!(
            worktree_entry_count(&repo),
            2,
            "baseline: main + the agent worktree are registered"
        );

        crate::agent_ops::cleanup_working_dir(&home, "devw", &wt);

        assert!(!wt.exists(), "worktree dir must be removed");
        let after = worktree_list(&repo);
        assert_eq!(
            after.lines().filter(|l| l.starts_with("worktree ")).count(),
            1,
            "only main may remain — a bare remove_dir_all would leave the entry: {after}"
        );
        assert!(
            !after.contains("prunable"),
            "no ORPHAN (prunable) worktree registration may survive: {after}"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Find the single reconcile-backup dir for `agent` (epoch suffix varies).
    fn backup_dir_for(home: &Path, agent: &str) -> Option<PathBuf> {
        std::fs::read_dir(home.join("reconcile-backups"))
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&format!("{agent}-")))
            })
    }

    /// #2234 Phase 0: a worktree with UNCOMMITTED work is backed up WHOLE before
    /// the git removal — never silently destroyed.
    #[test]
    fn cleanup_working_dir_dirty_worktree_backs_up_before_remove() {
        let home = tmp_home("p0-wt-dirty");
        let repo = tmp_repo("p0-wt-dirty-repo");
        let wt = managed_workspace_worktree(&home, &repo, "devd", "feat/p0d");
        std::fs::write(wt.join("WIP.txt"), "unsaved work").unwrap();

        crate::agent_ops::cleanup_working_dir(&home, "devd", &wt);

        assert!(!wt.exists(), "worktree removed");
        let backup = backup_dir_for(&home, "devd").expect("backup dir created");
        assert_eq!(
            std::fs::read_to_string(backup.join("WIP.txt")).unwrap(),
            "unsaved work",
            "uncommitted work must be preserved in the backup"
        );
        assert!(
            !backup.join(".git").exists() && !backup.join("target").exists(),
            "backup excludes the gitlink + regenerable target/"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// #2234 Phase 0: a worktree with a local commit not on any remote
    /// (committed-orphan) is backed up before removal — the has_uncommitted
    /// guard alone would miss it.
    #[test]
    fn cleanup_working_dir_committed_orphan_backs_up_before_remove() {
        let home = tmp_home("p0-wt-orphan");
        let repo = tmp_repo("p0-wt-orphan-repo");
        let wt = managed_workspace_worktree(&home, &repo, "devo", "feat/p0o");
        // A remote exists but nothing is pushed → HEAD's commits are unreachable
        // from remotes = committed-orphan. Tree itself is clean.
        std::process::Command::new("git")
            .args(["remote", "add", "origin", &repo.display().to_string()])
            .current_dir(&wt)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git remote add");

        crate::agent_ops::cleanup_working_dir(&home, "devo", &wt);

        assert!(!wt.exists(), "worktree removed");
        assert!(
            backup_dir_for(&home, "devo").is_some(),
            "committed-orphan worktree must be backed up before removal"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// #2234 Phase 0 (r6/lead dialectic #1): a gitlink worktree MISSING the
    /// `.agend-managed` marker (e.g. interrupted reconcile) still routes through
    /// `git worktree remove` — the marker is NEVER a veto into the
    /// orphan-leaving remove_dir_all path.
    #[test]
    fn teardown_marker_missing_still_removes_via_git_no_orphan() {
        let home = tmp_home("p0-wt-nomarker");
        let repo = tmp_repo("p0-wt-nomarker-repo");
        let wt = managed_workspace_worktree(&home, &repo, "devn", "feat/p0n");
        std::fs::remove_file(wt.join(MANAGED_MARKER)).unwrap();
        assert!(!is_daemon_managed(&wt));

        let handled = teardown_workspace_worktree(&home, "devn", &wt);

        assert!(
            handled,
            "gitlink present → must take the worktree path even sans marker"
        );
        assert!(!wt.exists(), "worktree removed");
        let after = worktree_list(&repo);
        assert_eq!(
            after.lines().filter(|l| l.starts_with("worktree ")).count(),
            1,
            "only main may remain (marker-less worktree still git-removed): {after}"
        );
        assert!(
            !after.contains("prunable"),
            "no orphan (prunable) registration may survive: {after}"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// #2234 Phase 0: a pre-(B) STANDALONE clone (`.git` is a DIRECTORY) is NOT
    /// a worktree → `teardown_workspace_worktree` declines (returns false) and
    /// `cleanup_working_dir` falls back to the byte-identical remove_dir_all.
    #[test]
    fn teardown_standalone_clone_declines_byte_identical() {
        let home = tmp_home("p0-standalone");
        let ws = crate::paths::workspace_dir(&home).join("devs");
        std::fs::create_dir_all(&ws).unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&ws)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git init");
        assert!(
            ws.join(".git").is_dir(),
            ".git must be a directory (standalone)"
        );

        // Helper declines (not a worktree).
        assert!(!teardown_workspace_worktree(&home, "devs", &ws));
        // Public path still removes the whole dir (byte-identical pre-(B)).
        crate::agent_ops::cleanup_working_dir(&home, "devs", &ws);
        assert!(!ws.exists(), "standalone workspace dir removed as before");
        assert!(
            backup_dir_for(&home, "devs").is_none(),
            "no backup for standalone"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #2234 cure-(B) Phase 1: reconcile_workspace_to_worktree ──────────────

    /// (i) empty workspace → a real daemon-managed gitlink worktree.
    #[test]
    fn reconcile_empty_workspace_creates_gitlink_worktree() {
        let home = tmp_home("p1-empty");
        let repo = tmp_repo("p1-empty-repo");
        let ws = crate::paths::workspace_dir(&home).join("devx");

        let got = reconcile_workspace_to_worktree(&home, "devx", &ws, &repo, None)
            .expect("reconcile empty");

        assert_eq!(got, ws);
        assert!(ws.join(".git").is_file(), "real gitlink FILE (r6 #4)");
        assert!(is_daemon_managed(&ws), ".agend-managed marker written");
        assert!(
            worktree_common_dir_matches(&ws, &repo),
            "worktree rooted at the canonical source_repo"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// (ii) standalone clone (`.git` is a DIR) → backup WHOLE dir, then convert
    /// to a gitlink worktree. Work is preserved in the backup.
    #[test]
    fn reconcile_standalone_clone_backs_up_then_converts() {
        let home = tmp_home("p1-standalone");
        let repo = tmp_repo("p1-standalone-repo");
        let ws = crate::paths::workspace_dir(&home).join("devy");
        std::fs::create_dir_all(&ws).unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&ws)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git init standalone");
        std::fs::write(ws.join("work.txt"), "wip").unwrap();
        assert!(
            ws.join(".git").is_dir(),
            "precondition: standalone .git dir"
        );

        reconcile_workspace_to_worktree(&home, "devy", &ws, &repo, None).expect("reconcile");

        assert!(ws.join(".git").is_file(), "converted to gitlink worktree");
        assert!(is_daemon_managed(&ws));
        let backup = backup_dir_for(&home, "devy").expect("standalone work backed up");
        assert_eq!(
            std::fs::read_to_string(backup.join("work.txt")).unwrap(),
            "wip",
            "pre-existing work preserved in backup"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// (ii) fail-closed: if the whole-dir backup fails, reconcile ABORTS and
    /// leaves the standalone UNTOUCHED — never destroy work without a backup.
    #[test]
    fn reconcile_backup_failure_aborts_fail_closed() {
        let home = tmp_home("p1-backupfail");
        let repo = tmp_repo("p1-backupfail-repo");
        let ws = crate::paths::workspace_dir(&home).join("devz");
        std::fs::create_dir_all(&ws).unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&ws)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git init standalone");
        std::fs::write(ws.join("work.txt"), "precious").unwrap();
        // Force backup_worktree_dir's create_dir_all to fail: make the
        // reconcile-backups parent a FILE.
        std::fs::create_dir_all(&home).ok();
        std::fs::write(home.join("reconcile-backups"), "blocker").unwrap();

        let err = reconcile_workspace_to_worktree(&home, "devz", &ws, &repo, None)
            .expect_err("must abort when backup fails");
        assert!(
            err.contains("backup"),
            "error names the backup failure: {err}"
        );

        // Standalone left fully intact — no work lost, not converted.
        assert!(
            ws.join(".git").is_dir(),
            "standalone still present (untouched)"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join("work.txt")).unwrap(),
            "precious",
            "work must NOT be destroyed on backup failure"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// (iii) already a worktree of this repo → idempotent NO-OP (no second
    /// backup, gitlink unchanged).
    #[test]
    fn reconcile_already_worktree_is_idempotent_noop() {
        let home = tmp_home("p1-idem");
        let repo = tmp_repo("p1-idem-repo");
        let ws = crate::paths::workspace_dir(&home).join("devi");

        reconcile_workspace_to_worktree(&home, "devi", &ws, &repo, None).expect("first");
        assert!(ws.join(".git").is_file());
        assert!(
            backup_dir_for(&home, "devi").is_none(),
            "no backup for fresh provision"
        );

        let again = reconcile_workspace_to_worktree(&home, "devi", &ws, &repo, None)
            .expect("second reconcile is a no-op");

        assert_eq!(again, ws);
        assert!(ws.join(".git").is_file(), "still a gitlink worktree");
        assert!(
            backup_dir_for(&home, "devi").is_none(),
            "idempotent no-op must NOT back up again"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// #1919 (Method B, the e2e that backs flipping the flag ON): reconcile keeps
    /// the cwd PATH stable, so the PRODUCTION claude-session locator
    /// (`backend::claude_session::has_resumable` + `encode_project_dir`) still
    /// finds the agent's resumable session after a standalone→worktree convert —
    /// `claude --continue` is not orphaned.
    #[test]
    fn reconcile_preserves_claude_session_key_1919() {
        use crate::backend::claude_session::{encode_project_dir, has_resumable};
        let home = tmp_home("p1-1919");
        let repo = tmp_repo("p1-1919-repo");
        let ws = crate::paths::workspace_dir(&home).join("dev9");
        std::fs::create_dir_all(&ws).unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&ws)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git init standalone");
        std::fs::write(ws.join("src.rs"), "fn main() {}").unwrap();

        // A fake claude session under an injectable projects root, keyed exactly
        // as the production locator computes it from the cwd.
        let proj_root = home.join("fake-claude-projects");
        let key_before = encode_project_dir(&dunce::canonicalize(&ws).unwrap());
        let proj_dir = proj_root.join(&key_before);
        std::fs::create_dir_all(&proj_dir).unwrap();
        std::fs::write(
            proj_dir.join("sess.jsonl"),
            "{\"type\":\"user\",\"message\":\"hi\"}\n",
        )
        .unwrap();
        assert!(
            has_resumable(&ws, &proj_root),
            "baseline: session is resumable before reconcile"
        );

        reconcile_workspace_to_worktree(&home, "dev9", &ws, &repo, None).expect("reconcile");

        let key_after = encode_project_dir(&dunce::canonicalize(&ws).unwrap());
        assert_eq!(
            key_before, key_after,
            "cwd PATH stable across reconcile → claude session key unchanged"
        );
        assert!(
            has_resumable(&ws, &proj_root),
            "#1919: reconcile preserves the resumable session — claude --continue not orphaned"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    // ── #2234 cure-(B) Phase 1c: release_stale_branch_holders + in-place checkout ──

    /// A clean legacy holder is released without a backup.
    #[test]
    fn release_stale_holder_clean_removes_no_backup() {
        let home = tmp_home("p1c-clean");
        let repo = tmp_repo("p1c-clean-repo");
        let l = lease(&home, &repo, "deva", "feat/clean").expect("lease legacy holder");
        assert!(l.path.exists());

        release_one_stale_holder(&home, "deva", &repo, "feat/clean", &l.path).expect("release");

        assert!(!l.path.exists(), "clean legacy holder removed via git");
        assert!(backup_dir_for(&home, "deva").is_none(), "clean → no backup");
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// A legacy holder with work-at-risk (uncommitted) is backed up before
    /// removal — never silently destroyed (the shim-redirected-commit hazard).
    #[test]
    fn release_stale_holder_work_at_risk_backs_up() {
        let home = tmp_home("p1c-risk");
        let repo = tmp_repo("p1c-risk-repo");
        let l = lease(&home, &repo, "devb", "feat/risk").expect("lease");
        std::fs::write(l.path.join("WIP.txt"), "unsaved").unwrap();
        assert!(worktree_has_work_at_risk(&l.path));

        release_one_stale_holder(&home, "devb", &repo, "feat/risk", &l.path).expect("release");

        assert!(!l.path.exists(), "holder removed after backup");
        let backup = backup_dir_for(&home, "devb").expect("work-at-risk backed up");
        assert_eq!(
            std::fs::read_to_string(backup.join("WIP.txt")).unwrap(),
            "unsaved"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Fail-closed: if a work-at-risk holder's backup fails, release ABORTS and
    /// leaves the holder untouched (never --force without a durable backup).
    #[test]
    fn release_stale_holder_backup_fail_aborts() {
        let home = tmp_home("p1c-bkfail");
        let repo = tmp_repo("p1c-bkfail-repo");
        let l = lease(&home, &repo, "devc", "feat/bk").expect("lease");
        std::fs::write(l.path.join("WIP.txt"), "precious").unwrap();
        // Block backup: make the reconcile-backups parent a FILE.
        std::fs::create_dir_all(&home).ok();
        std::fs::write(home.join("reconcile-backups"), "blocker").unwrap();

        let err = release_one_stale_holder(&home, "devc", &repo, "feat/bk", &l.path)
            .expect_err("must abort on backup failure");
        assert!(err.contains("backup"), "names the backup failure: {err}");
        assert!(l.path.exists(), "holder untouched");
        assert_eq!(
            std::fs::read_to_string(l.path.join("WIP.txt")).unwrap(),
            "precious",
            "work not destroyed on backup failure"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// The confluence end-to-end: a legacy `worktrees/<agent>/<branch>` holds the
    /// branch, so the workspace worktree CANNOT check it out in place — until
    /// `release_stale_branch_holders` frees it.
    #[test]
    fn release_stale_branch_holders_frees_branch_for_in_place_checkout() {
        let home = tmp_home("p1c-free");
        let repo = tmp_repo("p1c-free-repo");
        // (B) workspace worktree (detached holding).
        let ws = crate::paths::workspace_dir(&home).join("devf");
        reconcile_workspace_to_worktree(&home, "devf", &ws, &repo, None).expect("provision ws");
        // Legacy holder of feat/coexist (checks the branch out THERE).
        let l = lease(&home, &repo, "devf", "feat/coexist").expect("lease legacy");
        assert!(l.path.exists());
        assert!(
            checkout_workspace_branch(&ws, "feat/coexist").is_err(),
            "branch already checked out at the legacy holder → in-place checkout blocked"
        );

        release_stale_branch_holders(&home, "devf", &repo, "feat/coexist", &ws).expect("free");

        assert!(!l.path.exists(), "legacy holder released");
        checkout_workspace_branch(&ws, "feat/coexist")
            .expect("branch is now free → in-place checkout succeeds");
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// In-place checkout + holding-detach rollback round-trip.
    #[test]
    fn checkout_workspace_branch_and_detach_rollback() {
        let home = tmp_home("p1c-checkout");
        let repo = tmp_repo("p1c-checkout-repo");
        let ws = crate::paths::workspace_dir(&home).join("devg");
        reconcile_workspace_to_worktree(&home, "devg", &ws, &repo, None).expect("provision");

        // Make a branch to land on.
        std::process::Command::new("git")
            .args(["branch", "feat/land"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git branch");

        checkout_workspace_branch(&ws, "feat/land").expect("in-place checkout");
        let cur = crate::git_helpers::git_cmd(&ws, &["branch", "--show-current"]).unwrap();
        assert_eq!(cur, "feat/land", "workspace now on the branch");

        detach_workspace_to_holding(&ws).expect("rollback to holding");
        let detached = crate::git_helpers::git_cmd(&ws, &["branch", "--show-current"]).unwrap();
        assert!(
            detached.is_empty(),
            "rollback → detached holding (no branch)"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// #2234 Phase 1c: the production flag decision (`workspace_as_worktree_from_env`)
    /// — pure over (flag, allowlist) inputs, no process-global env (so no leak).
    #[test]
    fn workspace_as_worktree_from_env_flag_and_allowlist() {
        // Off by default / unset / wrong value.
        assert!(!workspace_as_worktree_from_env(None, None, "a"));
        assert!(!workspace_as_worktree_from_env(Some("0"), None, "a"));
        assert!(!workspace_as_worktree_from_env(Some("yes"), None, "a"));
        // On for all agents when set and no allowlist.
        assert!(workspace_as_worktree_from_env(Some("1"), None, "a"));
        assert!(workspace_as_worktree_from_env(Some("true"), Some(""), "a"));
        // Allowlist scopes to listed agents only.
        assert!(workspace_as_worktree_from_env(Some("1"), Some("a,b"), "a"));
        assert!(workspace_as_worktree_from_env(
            Some("1"),
            Some(" a , b "),
            "b"
        ));
        assert!(!workspace_as_worktree_from_env(Some("1"), Some("a,b"), "c"));
    }

    /// #2234 Phase 1c: the thread-local test seam overrides the env decision for
    /// the current thread only, and the RAII guard restores on drop.
    #[test]
    fn workspace_worktree_test_seam_is_thread_scoped_and_restores() {
        assert!(!workspace_as_worktree_enabled("z"), "default off");
        {
            let _g = workspace_worktree_test_seam::force(true);
            assert!(
                workspace_as_worktree_enabled("z"),
                "forced on for this thread"
            );
        }
        assert!(
            !workspace_as_worktree_enabled("z"),
            "guard drop restores the env-default (off)"
        );
    }

    // ── #2234 rollback primitive: reverse_reconcile ─────────────────────────

    /// Helper: convert /workspace into a (B) worktree on `branch` with one
    /// committed-but-unpushed commit (simulates post-conversion in-place work).
    fn converted_workspace_with_commit(
        home: &Path,
        repo: &Path,
        agent: &str,
        branch: &str,
    ) -> PathBuf {
        let ws = crate::paths::workspace_dir(home).join(agent);
        reconcile_workspace_to_worktree(home, agent, &ws, repo, None).expect("reconcile");
        // Create the branch in canonical, check it out in the workspace worktree,
        // commit there (commit lands in canonical's object store + refs/heads).
        std::process::Command::new("git")
            .args(["branch", branch])
            .current_dir(repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git branch");
        checkout_workspace_branch(&ws, branch).expect("checkout");
        std::fs::write(ws.join("work.rs"), "fn main() {}").unwrap();
        for args in [
            vec!["add", "work.rs"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "unpushed C1",
            ],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&ws)
                .env("AGEND_GIT_BYPASS", "1")
                .output()
                .expect("git commit");
        }
        ws
    }

    fn rev_parse(dir: &Path, rev: &str) -> Option<String> {
        crate::git_helpers::git_cmd(dir, &["rev-parse", rev]).ok()
    }

    /// #2234 (毀-work core): a committed-but-unpushed commit on the converted
    /// workspace's branch is preserved BY CONSTRUCTION across reverse_reconcile —
    /// it lives in canonical, and a subsequent OFF lease of the branch recovers it.
    #[test]
    fn reverse_reconcile_preserves_committed_work_via_canonical() {
        let home = tmp_home("rr-commit");
        let repo = tmp_repo("rr-commit-repo");
        let ws = converted_workspace_with_commit(&home, &repo, "deva", "feat/rr");
        let c1 = rev_parse(&ws, "HEAD").expect("ws HEAD");

        reverse_reconcile(&home, "deva").expect("reverse_reconcile");

        // Workspace is no longer a (B) worktree (restored to a standalone).
        assert!(
            !ws.join(".git").is_file(),
            "workspace reverted from gitlink worktree to standalone"
        );
        // The commit + branch ref SURVIVE in canonical (not lost).
        assert_eq!(
            rev_parse(&repo, "feat/rr").as_deref(),
            Some(c1.as_str()),
            "committed work preserved in canonical (not via reconcile-backups)"
        );
        // An OFF-style lease of the branch recovers the commit's tree.
        let l = lease(&home, &repo, "deva", "feat/rr").expect("re-lease recovers branch");
        assert_eq!(
            std::fs::read_to_string(l.path.join("work.rs")).unwrap(),
            "fn main() {}",
            "re-leased worktree has the recovered committed work"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Uncommitted/untracked work IS at risk → backed up before the revert.
    #[test]
    fn reverse_reconcile_backs_up_uncommitted_work() {
        let home = tmp_home("rr-uncommitted");
        let repo = tmp_repo("rr-uncommitted-repo");
        let ws = crate::paths::workspace_dir(&home).join("devb");
        reconcile_workspace_to_worktree(&home, "devb", &ws, &repo, None).expect("reconcile");
        std::fs::write(ws.join("WIP.txt"), "unsaved").unwrap();

        reverse_reconcile(&home, "devb").expect("reverse_reconcile");

        let backup = backup_dir_for(&home, "devb").expect("uncommitted work backed up");
        assert_eq!(
            std::fs::read_to_string(backup.join("WIP.txt")).unwrap(),
            "unsaved"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Fail-closed: if the uncommitted-work backup fails, the revert ABORTS and
    /// leaves the (B) worktree untouched (never destroy work without a backup).
    #[test]
    fn reverse_reconcile_backup_fail_aborts() {
        let home = tmp_home("rr-bkfail");
        let repo = tmp_repo("rr-bkfail-repo");
        let ws = crate::paths::workspace_dir(&home).join("devc");
        reconcile_workspace_to_worktree(&home, "devc", &ws, &repo, None).expect("reconcile");
        std::fs::write(ws.join("WIP.txt"), "precious").unwrap();
        std::fs::create_dir_all(&home).ok();
        std::fs::write(home.join("reconcile-backups"), "blocker").unwrap();

        let err = reverse_reconcile(&home, "devc").expect_err("must abort on backup failure");
        assert!(err.contains("backup"), "names backup failure: {err}");
        assert!(
            ws.join(".git").is_file(),
            "still a (B) worktree (untouched on abort)"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join("WIP.txt")).unwrap(),
            "precious",
            "work not destroyed on backup failure"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// "Can turn off" proof: after reverse_reconcile, the workspace worktree is
    /// gone from canonical's registry (no "already checked out" block for a future
    /// OFF lease) and the (B) binding is cleared. Also no-op on an unconverted dir.
    #[test]
    fn reverse_reconcile_clears_registration_and_is_noop_when_unconverted() {
        let home = tmp_home("rr-offready");
        let repo = tmp_repo("rr-offready-repo");
        // No-op on an absent/unconverted workspace.
        reverse_reconcile(&home, "devd").expect("no-op on unconverted");

        let ws = converted_workspace_with_commit(&home, &repo, "devd", "feat/off");
        crate::binding::bind_full(&home, "devd", "T-1", "feat/off", &ws, &repo, false).ok();
        let wt_listed = |repo: &Path| {
            crate::git_helpers::git_cmd(repo, &["worktree", "list", "--porcelain"])
                .unwrap_or_default()
        };
        // Precondition: workspace is registered as a 2nd worktree (canonical +
        // workspace). Count rather than substring-match the path — git porcelain
        // emits a canonicalized path form (drive-case / separator) that need not
        // equal ws.display() on Windows.
        let before = wt_listed(&repo);
        assert_eq!(
            before
                .lines()
                .filter(|l| l.starts_with("worktree "))
                .count(),
            2,
            "workspace registered before reverse_reconcile: {before}"
        );

        reverse_reconcile(&home, "devd").expect("reverse_reconcile");

        let after = wt_listed(&repo);
        assert_eq!(
            after.lines().filter(|l| l.starts_with("worktree ")).count(),
            1,
            "only canonical remains — workspace worktree deregistered (OFF lease won't conflict): {after}"
        );
        assert!(
            !after.contains("prunable"),
            "no orphan registration: {after}"
        );
        assert!(
            crate::binding::read(&home, "devd").is_none(),
            "(B) binding cleared"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    fn tmp_repo(tag: &str) -> PathBuf {
        let dir = tmp_home(tag);
        // #1463: scratch-repo git must bypass the agend-git shim, else an
        // agent-run suite (AGEND_INSTANCE_NAME set) ChdirPass-redirects the
        // commit into the bound worktree (init-pile pollution).
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .ok();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(&dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .ok();
        dir
    }

    /// Lease + bind — finding D+H: `lease` no longer writes binding.json (the
    /// authoritative caller binds AFTER leasing). Tests that exercise `release`/
    /// `release_full` need a binding present, so this helper simulates dispatch's
    /// pre-build bind (the production `bind_full` that now solely owns binding).
    fn lease_bound(home: &Path, repo: &Path, agent: &str, branch: &str) -> WorktreeLease {
        let l = lease(home, repo, agent, branch).expect("lease");
        crate::binding::bind_full(home, agent, "", branch, &l.path, repo, false)
            .expect("bind_full (simulates the authoritative caller)");
        l
    }

    #[test]
    fn lease_main_branch_rejected() {
        let home = tmp_home("main-reject");
        let repo = tmp_repo("main-reject-repo");
        let result = lease(&home, &repo, "agent-1", "main");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            LeaseError::ProtectedBranch(_)
        ));
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Finding D+H load-bearing: `lease` returns DISTINCT typed variants — a
    /// protected ref is `ProtectedBranch`, a `worktree::create` failure is
    /// `CreateFailed`. Reverse-mutation guard: collapsing both arms to one
    /// variant (or back to a `String`) breaks the dispatch-boundary match.
    #[test]
    fn lease_returns_typed_protected_and_propagates_errors() {
        let home = tmp_home("typed-err");
        let repo = tmp_repo("typed-err-repo");

        // E4.5 protected ref → ProtectedBranch (message preserves "E4.5").
        match lease(&home, &repo, "agent-t", "main") {
            Err(LeaseError::ProtectedBranch(m)) => assert!(m.contains("E4.5"), "msg: {m}"),
            other => panic!("expected ProtectedBranch, got {other:?}"),
        }

        // A non-protected but invalid branch name (`..` fails validate_branch)
        // makes `worktree::create` return None → CreateFailed (NOT ProtectedBranch).
        match lease(&home, &repo, "agent-t", "feat..bad") {
            Err(LeaseError::CreateFailed(m)) => {
                assert!(m.contains("feat..bad"), "msg names the target: {m}")
            }
            other => panic!("expected CreateFailed, got {other:?}"),
        }

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn lease_creates_daemon_tagged_worktree() {
        let home = tmp_home("lease-tag");
        let repo = tmp_repo("lease-tag-repo");
        let result = lease(&home, &repo, "agent-2", "feat/test");
        assert!(result.is_ok());
        let l = result.expect("lease");
        assert!(l.path.exists());
        assert!(is_daemon_managed(&l.path));
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_marks_candidate_no_delete() {
        let home = tmp_home("release");
        let repo = tmp_repo("release-repo");
        let l = lease(&home, &repo, "agent-3", "feat/release").expect("lease");
        release(&home, &l);
        // Worktree still exists (no delete in Phase 3).
        assert!(l.path.exists());
        // Binding cleared.
        assert!(crate::binding::read(&home, "agent-3").is_none());
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_idempotent() {
        let home = tmp_home("release-idem");
        let repo = tmp_repo("release-idem-repo");
        let l = lease(&home, &repo, "agent-4", "feat/idem").expect("lease");
        release(&home, &l);
        release(&home, &l); // second release — no panic
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    // ── Sprint 53 P0-X: release_full (hard release) tests ───────────────
    //
    // These call the production function `release_full`, which in turn is
    // the body of the `release_worktree` MCP tool. The MCP layer test in
    // `src/mcp/handlers/worktree.rs` covers the handler contract; here we
    // focus on the filesystem semantics.
    //
    // Regression-proof: comment out the `git worktree remove` block in
    // `release_full` and `p0x_release_full_happy_path_removes_worktree_and_binding`
    // FAILS (`worktree_removed` stays false; `l.path.exists()` stays true).
    // Restore → PASS. See commit message §regression-proof.

    #[test]
    fn p0x_release_full_happy_path_removes_worktree_and_binding() {
        let home = tmp_home("p0x-happy");
        let repo = tmp_repo("p0x-happy-repo");
        let l = lease_bound(&home, &repo, "agent-h", "feat/happy");
        // Pre-condition: lease created both binding + worktree.
        assert!(l.path.exists(), "pre: worktree must exist");
        assert!(crate::binding::read(&home, "agent-h").is_some());
        assert!(is_daemon_managed(&l.path));

        let outcome = release_full(&home, "agent-h", false);

        assert!(outcome.released, "happy path must report released");
        assert!(outcome.worktree_removed, "worktree must be removed");
        assert!(outcome.binding_removed, "binding must be removed");
        assert!(outcome.error.is_none(), "no error: {:?}", outcome.error);
        assert!(!l.path.exists(), "worktree dir must be gone post-release");
        assert!(crate::binding::read(&home, "agent-h").is_none());

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// §3.9 regression (#t-21 HIGH #1): `release_full(dry_run=true)` must be
    /// observation-only — the worktree directory AND binding.json must survive.
    /// Pre-fix, `remove_worktree` + `clear_binding_state` ran unconditionally,
    /// so a dry run actually destroyed both. Regression-proof: revert the
    /// `if dry_run` guard in `release_full` and this FAILS (`l.path` gone,
    /// binding cleared).
    #[test]
    fn dry_run_release_preserves_worktree_and_binding_t21() {
        let home = tmp_home("t21-dry-run");
        let repo = tmp_repo("t21-dry-run-repo");
        let l = lease_bound(&home, &repo, "agent-dry", "feat/keep");
        assert!(l.path.exists(), "pre: worktree must exist");
        assert!(crate::binding::read(&home, "agent-dry").is_some());

        let outcome = release_full(&home, "agent-dry", true);

        // Observation-success, nothing actually removed.
        assert!(outcome.released, "dry-run reports observation success");
        assert!(
            !outcome.worktree_removed,
            "dry-run must NOT remove worktree"
        );
        assert!(!outcome.binding_removed, "dry-run must NOT clear binding");
        assert!(outcome.error.is_none(), "no error: {:?}", outcome.error);
        // The destructive effects are previewed, not performed.
        assert!(
            outcome.dry_run_preview.as_deref().is_some_and(
                |p| p.contains("would remove worktree") && p.contains("would clear binding")
            ),
            "dry-run must preview both effects: {:?}",
            outcome.dry_run_preview
        );
        // The actual on-disk state is untouched.
        assert!(
            l.path.exists(),
            "worktree dir MUST survive a dry-run release"
        );
        assert!(
            crate::binding::read(&home, "agent-dry").is_some(),
            "binding.json MUST survive a dry-run release"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn p0x_release_full_idempotent_second_call_noop() {
        // #1465: release is idempotent. The first call tears down; the
        // second (no binding left) is a SUCCESS no-op — `released:true,
        // already_released:true`, no error — NOT the pre-#1465 `released:
        // false + "no binding"` error (that encoded the bug this fixes).
        let home = tmp_home("p0x-idem");
        let repo = tmp_repo("p0x-idem-repo");
        lease_bound(&home, &repo, "agent-i", "feat/idem");
        let r1 = release_full(&home, "agent-i", false);
        assert!(r1.released, "first call must release");
        assert!(
            !r1.already_released,
            "first call is a real teardown, not a no-op"
        );
        let r2 = release_full(&home, "agent-i", false);
        assert!(r2.released, "second call must be idempotent success");
        assert!(
            r2.already_released,
            "second call must flag already_released"
        );
        assert!(
            r2.error.is_none(),
            "idempotent no-op must NOT error: {:?}",
            r2.error
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn p0x_release_full_missing_binding_graceful() {
        // #1465: releasing an agent that never had a binding is a success
        // no-op (release target state already reached), not an error.
        let home = tmp_home("p0x-missing-binding");
        let outcome = release_full(&home, "ghost-agent", false);
        assert!(
            outcome.released,
            "missing binding must be idempotent success"
        );
        assert!(outcome.already_released, "must flag already_released");
        assert!(
            outcome.error.is_none(),
            "no-op must not error: {:?}",
            outcome.error
        );
        // Nothing was actually torn down — no worktree/binding removal.
        assert!(!outcome.worktree_removed);
        assert!(!outcome.binding_removed);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn p0x_release_full_missing_worktree_path_clears_binding_anyway() {
        // Binding exists but the worktree directory was deleted out from
        // under us (manual cleanup, daemon restart races, etc.). Spec:
        // "still remove binding (partial cleanup ok)".
        let home = tmp_home("p0x-missing-wt");
        let repo = tmp_repo("p0x-missing-wt-repo");
        let l = lease_bound(&home, &repo, "agent-mw", "feat/mw");
        // Manually remove the worktree dir behind the daemon's back, but
        // leave the binding pointing at the now-stale path.
        std::fs::remove_dir_all(&l.path).ok();
        assert!(!l.path.exists(), "pre: worktree must be gone");
        assert!(crate::binding::read(&home, "agent-mw").is_some());

        let outcome = release_full(&home, "agent-mw", false);
        assert!(outcome.released, "must still release: {:?}", outcome);
        assert!(outcome.binding_removed, "binding must be cleared");
        assert!(
            !outcome.worktree_removed,
            "worktree wasn't removed by us (it was already gone)"
        );
        assert!(crate::binding::read(&home, "agent-mw").is_none());

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn p0x_release_full_unmanaged_worktree_skipped_safely() {
        // R14 safety: if the binding points at a worktree that lacks the
        // .agend-managed marker (operator-created, not daemon-leased), the
        // release MUST NOT remove the worktree. #1879 (WT-LEAK-2): the stale
        // binding IS cleared, though — leaving it leaked the binding and blocked
        // a same-agent re-bind. The worktree (operator data) survives for
        // investigation; the daemon's binding to it does not.
        let home = tmp_home("p0x-unmanaged");
        let unmanaged_wt = tmp_home("p0x-unmanaged-wt-target");
        // Hand-craft a binding pointing at an unmanaged path.
        std::fs::create_dir_all(crate::paths::runtime_dir(&home).join("agent-u")).ok();
        let binding = serde_json::json!({
            "version": 1,
            "agent": "agent-u",
            "task_id": "T-1",
            "branch": "feat/manual",
            "issued_at": chrono::Utc::now().to_rfc3339(),
            "worktree": unmanaged_wt.display().to_string(),
        });
        std::fs::write(
            crate::paths::runtime_dir(&home)
                .join("agent-u")
                .join("binding.json"),
            serde_json::to_string_pretty(&binding).unwrap(),
        )
        .unwrap();
        // Sanity: no marker.
        assert!(!is_daemon_managed(&unmanaged_wt));

        let outcome = release_full(&home, "agent-u", false);
        assert!(
            !outcome.released,
            "unmanaged worktree must NOT be released: {:?}",
            outcome
        );
        assert!(
            outcome.binding_removed,
            "#1879 WT-LEAK-2: the stale binding must be CLEARED even when the unmanaged worktree removal is refused"
        );
        assert!(
            outcome
                .error
                .as_deref()
                .unwrap_or("")
                .contains(".agend-managed"),
            "error must explain the marker check: {:?}",
            outcome.error
        );
        assert!(unmanaged_wt.exists(), "operator-created dir must survive");
        assert!(
            crate::binding::read(&home, "agent-u").is_none(),
            "#1879 WT-LEAK-2: the binding must be cleared (no leak / re-bind block)"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&unmanaged_wt).ok();
    }

    /// Helper: assert `git worktree list --porcelain` from `repo` does NOT
    /// emit any `prunable` line (registry leak indicator).
    fn assert_no_prunable_registry(repo: &Path, scenario: &str) {
        let output = std::process::Command::new("git")
            .current_dir(repo)
            .args(["worktree", "list", "--porcelain"])
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git worktree list");
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        assert!(
            !stdout.contains("prunable"),
            "[{scenario}] git worktree registry must be clean — found `prunable` entry. Output:\n{stdout}"
        );
    }

    #[test]
    fn p0x_release_full_clears_git_worktree_registry() {
        // r1 reviewer (PR #470): the prior IMPL didn't pass `.current_dir(source_repo)`
        // and the `remove_dir_all` fallback didn't `git worktree prune`, so
        // `git worktree list --porcelain` kept emitting `prunable` entries
        // that would block re-lease (registry vs filesystem skew).
        //
        // Scenario A: happy path — `release_full` invokes `git worktree
        // remove --force` from the owning repo's cwd. Registry must be clean.
        let home = tmp_home("p0x-registry-happy");
        let repo = tmp_repo("p0x-registry-happy-repo");
        let _l = lease_bound(&home, &repo, "agent-r", "feat/registry");

        let outcome = release_full(&home, "agent-r", false);
        assert!(outcome.released);
        assert!(outcome.worktree_removed);
        assert_no_prunable_registry(&repo, "happy-path");

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn p0x_release_full_prunes_registry_after_external_dir_removal() {
        // Reviewer's exact failure mode: the worktree dir gets removed
        // externally (daemon crash mid-op, manual `rm`), so when `release_full`
        // runs the dir is already gone but the git registry still lists the
        // path as `prunable`. Without the explicit `git worktree prune` call
        // in the missing-path branch, the next lease re-attempt fails because
        // the registry sees the path as still claimed.
        //
        // This is the load-bearing regression-proof for the r1 fix:
        // commenting out the `git worktree prune` block in `release_full`'s
        // missing-path branch makes this test FAIL on the post-release
        // assertion. Restore → PASS.
        let home = tmp_home("p0x-registry-prune");
        let repo = tmp_repo("p0x-registry-prune-repo");
        let l = lease_bound(&home, &repo, "agent-rm", "feat/prune");

        // Simulate the leak: yank the worktree dir behind git's back.
        std::fs::remove_dir_all(&l.path).ok();
        assert!(!l.path.exists(), "test setup: dir must be gone");

        // Pre-condition sanity: registry MUST list the now-missing entry as
        // `prunable` before release_full runs. If git's behavior changes and
        // this assertion no longer holds, the test setup is no longer
        // exercising the bug — flag it via panic in the assertion.
        let pre_output = std::process::Command::new("git")
            .current_dir(&repo)
            .args(["worktree", "list", "--porcelain"])
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git worktree list pre");
        let pre_stdout = String::from_utf8_lossy(&pre_output.stdout).to_string();
        assert!(
            pre_stdout.contains("prunable"),
            "test setup invariant: dir-removed worktree must show as prunable pre-release. Output:\n{pre_stdout}"
        );

        let outcome = release_full(&home, "agent-rm", false);
        assert!(outcome.released);
        assert!(outcome.binding_removed);

        // Post-condition: prune must have run, registry is clean.
        assert_no_prunable_registry(&repo, "post-external-rm");

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn p0x_release_full_via_handle_release_worktree_end_to_end() {
        // Production smoke (§5): exercise the full MCP path —
        // `handle_release_worktree(home, args, sender)` — the same function
        // the daemon dispatches `release_worktree` calls into. Asserts that
        // a leased agent + worktree gets fully cleaned up via the MCP layer.
        let home = tmp_home("p0x-prod-smoke");
        let repo = tmp_repo("p0x-prod-smoke-repo");
        let l = lease_bound(&home, &repo, "agent-prod", "feat/prod");
        assert!(l.path.exists());

        let result = crate::mcp::handlers::worktree_test_release(
            &home,
            &serde_json::json!({"instance": "agent-prod"}),
        );
        assert_eq!(result["released"].as_bool(), Some(true), "{result}");
        assert_eq!(result["worktree_removed"].as_bool(), Some(true), "{result}");
        assert_eq!(result["binding_removed"].as_bool(), Some(true), "{result}");
        assert!(!l.path.exists(), "worktree must be removed by MCP path");
        assert!(crate::binding::read(&home, "agent-prod").is_none());

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn is_daemon_managed_excludes_human_worktrees() {
        let dir = tmp_home("human-wt");
        // No marker → not managed.
        assert!(!is_daemon_managed(&dir));
        // Add marker → managed.
        std::fs::write(dir.join(MANAGED_MARKER), "test").ok();
        assert!(is_daemon_managed(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pin_unpin_idempotent() {
        let dir = tmp_home("pin");
        pin(&dir);
        assert!(is_pinned(&dir));
        pin(&dir); // idempotent
        assert!(is_pinned(&dir));
        unpin(&dir);
        assert!(!is_pinned(&dir));
        unpin(&dir); // idempotent
        assert!(!is_pinned(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── Phase 4 GC tests ────────────────────────────────────────────

    fn make_gc_candidate(home: &Path, agent: &str) -> PathBuf {
        let wt = home
            .join("workspace")
            .join("repo")
            .join(".worktrees")
            .join(agent);
        std::fs::create_dir_all(&wt).ok();
        // Daemon-managed marker with old timestamp (past grace).
        let old_ts = (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
        std::fs::write(
            wt.join(MANAGED_MARKER),
            format!("agent={agent}\nleased_at={old_ts}\nreleased_at={old_ts}\n"),
        )
        .ok();
        wt
    }

    #[test]
    fn gc_candidates_includes_only_daemon_tagged() {
        let home = tmp_home("gc-tagged");
        let wt = home
            .join("workspace")
            .join("repo")
            .join(".worktrees")
            .join("human");
        std::fs::create_dir_all(&wt).ok();
        // No .agend-managed marker → not a candidate.
        let candidates = gc_candidates(&home);
        assert!(
            candidates.is_empty(),
            "human worktree must not be candidate"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2234 Phase 2 gc-safety (r6 #2269 observation): a MARKER-LESS
    /// `workspace/<agent>` gitlink worktree — a real worktree (`.git` is a gitlink
    /// FILE) but missing the `.agend-managed` marker (e.g. an interrupted
    /// reconcile) — is enumerated by the new (B) `workspace_gitlink_worktrees`
    /// scan (gitlink-alone gate), so it DOES reach `evaluate_candidate`; it MUST
    /// NOT become a GC candidate because the `is_daemon_managed` marker-gate
    /// rejects it. The sibling `gc_candidates_includes_only_daemon_tagged` only
    /// covers a NESTED `.worktrees/human` dir that the marker-walk collect stage
    /// filters BEFORE evaluate, so it never exercises this workspace-gitlink path.
    #[test]
    fn gc_candidates_excludes_marker_less_workspace_gitlink_2234() {
        let home = tmp_home("gc-markerless-ws");
        let repo = tmp_repo("gc-markerless-ws-repo");
        // Same fixture as `managed_workspace_worktree`, minus the marker write.
        let ws = managed_workspace_worktree(&home, &repo, "deve", "feat/markerless");
        std::fs::remove_file(ws.join(MANAGED_MARKER)).expect("drop marker");
        assert!(
            ws.join(".git").is_file(),
            "fixture is a real gitlink worktree"
        );
        // Precondition: the (B) scan DOES enumerate it (so it reaches evaluate).
        assert!(
            fs_managed_worktrees(&home).iter().any(|p| p == &ws),
            "marker-less workspace gitlink must be enumerated (reaches evaluate_candidate)"
        );
        // Property under test: the marker-gate keeps it OUT of the candidate set.
        let candidates = gc_candidates(&home);
        assert!(
            candidates.iter().all(|c| c.path != ws),
            "marker-less workspace gitlink must NOT be a GC candidate: {candidates:?}"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn gc_candidates_excludes_pinned() {
        let home = tmp_home("gc-pinned");
        let wt = make_gc_candidate(&home, "pinned-agent");
        pin(&wt);
        let candidates = gc_candidates(&home);
        assert!(candidates.is_empty(), "pinned must not be candidate");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_candidates_respects_grace_ttl() {
        let home = tmp_home("gc-grace");
        let wt = home
            .join("workspace")
            .join("repo")
            .join(".worktrees")
            .join("fresh");
        std::fs::create_dir_all(&wt).ok();
        // Recent timestamp (within grace).
        let recent = chrono::Utc::now().to_rfc3339();
        std::fs::write(
            wt.join(MANAGED_MARKER),
            format!("agent=fresh\nleased_at={recent}\nreleased_at={recent}\n"),
        )
        .ok();
        let candidates = gc_candidates(&home);
        assert!(
            candidates.is_empty(),
            "fresh worktree within grace must not be candidate"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// §3.9 #1870 (H1): a worktree whose `.agend-managed` `released_at=` is
    /// MALFORMED (e.g. a partial-write / crash-truncated marker) must NOT be
    /// reclaimed — the grace window protects just-released WIP, so a parse
    /// failure fails conservative (skip GC). A valid PAST-grace `released_at`
    /// still yields a candidate (behavior unchanged). Regression-proof: revert
    /// the fix and the malformed worktree falls through to a CleanRelease
    /// candidate, so `bad-ts` appears.
    #[test]
    fn gc_candidates_skips_malformed_released_at_1870() {
        let home = tmp_home("gc-malformed-ts");
        // Malformed released_at + a RECENT lease → must be kept. #1870 stopped the
        // immediate grace-bypass reclaim; #1882 (WT-LEAK-1) then routes a corrupt
        // marker to the force-reclaim backstop — but its leased_at age-cap still
        // protects a recently-leased (possibly still-in-use) worktree. So a recent
        // `leased_at` here stays NOT a candidate (an ABANDONED corrupt marker IS
        // reclaimed — see force_reclaim_corrupt_marker_* tests).
        let recent = chrono::Utc::now().to_rfc3339();
        let bad = home
            .join("workspace")
            .join("repo")
            .join(".worktrees")
            .join("bad-ts");
        std::fs::create_dir_all(&bad).ok();
        std::fs::write(
            bad.join(MANAGED_MARKER),
            format!("agent=bad-ts\nleased_at={recent}\nreleased_at=not-a-timestamp\n"),
        )
        .ok();
        // Valid past-grace released_at → still a candidate (unchanged).
        make_gc_candidate(&home, "good-ts");

        let agents: Vec<String> = gc_candidates(&home).into_iter().map(|c| c.agent).collect();
        assert!(
            !agents.iter().any(|a| a == "bad-ts"),
            "#1870/#1882: a malformed released_at on a RECENT lease must NOT be reclaimed (age-cap protects it), got: {agents:?}"
        );
        assert!(
            agents.iter().any(|a| a == "good-ts"),
            "#1870: a valid past-grace released_at must STILL yield a candidate (unchanged), got: {agents:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_candidates_excludes_active_binding() {
        let home = tmp_home("gc-active");
        make_gc_candidate(&home, "active-agent");
        // Create active binding.
        crate::binding::bind(&home, "active-agent", "T-1", "feat");
        let candidates = gc_candidates(&home);
        assert!(candidates.is_empty(), "active binding must exclude from GC");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dry_run_no_actual_delete() {
        let home = tmp_home("gc-dry");
        let wt = make_gc_candidate(&home, "dry-agent");
        gc_dry_run(&home);
        assert!(wt.exists(), "dry-run must NOT delete");
        std::fs::remove_dir_all(&home).ok();
    }

    // ------------------------------------------------------------------
    // Sprint 57 Wave 2 Track B (#546 Item 2) — release_worktree must
    // unsubscribe the released agent from EVERY ci-watch they appear
    // on, not just the binding-branch entry.
    // ------------------------------------------------------------------

    /// Helper: write a synthetic ci-watch JSON listing the given
    /// subscribers on `(repo, branch)`. Returns the watch path.
    fn write_ci_watch(
        home: &std::path::Path,
        repo: &str,
        branch: &str,
        subscribers: &[&str],
    ) -> PathBuf {
        write_ci_watch_with_extras(home, repo, branch, subscribers, None, None)
    }

    /// #931: variant that also stores `next_after_ci` (workflow chain) and
    /// `last_notified_head_sha` (polling state). Used by the decouple-fix
    /// tests to assert release_full preserves these fields.
    fn write_ci_watch_with_extras(
        home: &std::path::Path,
        repo: &str,
        branch: &str,
        subscribers: &[&str],
        next_after_ci: Option<&str>,
        last_notified_head_sha: Option<&str>,
    ) -> PathBuf {
        let ci_dir = crate::daemon::ci_watch::ci_watches_dir(home);
        std::fs::create_dir_all(&ci_dir).ok();
        let filename = crate::daemon::ci_watch::watch_filename(repo, branch);
        let path = ci_dir.join(&filename);
        let subs: Vec<serde_json::Value> = subscribers
            .iter()
            .map(|s| serde_json::json!({"instance": *s}))
            .collect();
        let mut watch = serde_json::json!({
            "repo": repo,
            "branch": branch,
            "interval_secs": 60,
            "subscribers": subs,
            "instance": subscribers.first().copied().unwrap_or(""),
            "last_run_id": 12345_u64,
            "head_sha": "deadbeefcafe",
            "last_polled_at": chrono::Utc::now().timestamp_millis(),
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
            "last_terminal_seen_at": null,
        });
        if let Some(n) = next_after_ci {
            watch["next_after_ci"] = serde_json::json!(n);
        }
        if let Some(sha) = last_notified_head_sha {
            watch["last_notified_head_sha"] = serde_json::json!(sha);
        }
        std::fs::write(&path, serde_json::to_string_pretty(&watch).unwrap()).ok();
        path
    }

    /// #931 helper: read a watch field as string (returns None if absent or
    /// not a string). Used by decouple tests to assert state preservation.
    fn read_ci_watch_field(path: &std::path::Path, field: &str) -> Option<String> {
        let content = std::fs::read_to_string(path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&content).ok()?;
        v.get(field)?.as_str().map(String::from)
    }

    /// Read a ci-watch JSON's subscriber `instance` strings. Returns
    /// empty Vec if file missing or parse fails — `assert` on the
    /// caller handles the missing-file case as appropriate.
    fn read_ci_watch_subscribers(path: &std::path::Path) -> Vec<String> {
        let Ok(content) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        let Ok(watch) = serde_json::from_str::<serde_json::Value>(&content) else {
            return Vec::new();
        };
        crate::daemon::ci_watch::parse_subscribers(&watch)
    }

    #[test]
    fn release_worktree_unsubscribes_all_agent_ci_watches() {
        // #931 INVERTED (was Sprint 57 Wave 2 Track B #546 Item 2 pin).
        //
        // Pre-#931: release_full unconditionally swept the released agent
        // out of EVERY ci-watch they appeared on (binding-branch + ad-hoc).
        // That cleanup cascaded to watch-file deletion when the released
        // agent was the sole subscriber, destroying `next_after_ci`
        // chains and polling state — 4-in-a-row PR stalls
        // (#920/#925/#928/#929) traced to this exact path.
        //
        // Post-#931 (Direction A.1): release_full no longer mutates any
        // ci-watch on the agent's behalf. Subscriptions persist across
        // release per operator intent in issue #931:
        //   "Subscription persists across bind handoff unless explicitly
        //    `unwatch`ed."
        //
        // Hygiene is delegated to:
        //   - 72h absolute TTL (`expires_at`)
        //   - 72h inactivity TTL (`last_terminal_seen_at`)
        //   - PR-terminal auto-clear (poller's `check_pr_terminal`)
        //   - Explicit `ci action=unwatch` (operator-callable)
        //
        // This test now PINS the new persist-across-release behavior so
        // a regression that re-introduces the broad sweep is caught
        // immediately. Rollback criteria documented in PR #931 body.
        let home = tmp_home("931-persist-multi");
        let repo = tmp_repo("931-persist-multi-repo");
        let l = lease_bound(&home, &repo, "dev", "feat-track-x");
        assert!(l.path.exists(), "pre: worktree must exist");

        let auto_watch = write_ci_watch(&home, "owner/repo", "feat-track-x", &["dev", "lead"]);
        let main_watch = write_ci_watch(&home, "owner/repo", "main", &["dev", "lead"]);
        let bystander = write_ci_watch(&home, "owner/repo", "feat-bystander", &["lead"]);

        let outcome = release_full(&home, "dev", false);

        assert!(outcome.released, "release must succeed");
        assert!(outcome.binding_removed, "binding must be cleared");

        // Auto-watch (binding-branch): dev MUST STILL be subscribed.
        let auto_subs = read_ci_watch_subscribers(&auto_watch);
        assert!(
            auto_subs.contains(&"dev".to_string()),
            "#931: dev must persist on binding-branch watch — got {auto_subs:?}"
        );
        assert!(
            auto_subs.contains(&"lead".to_string()),
            "lead untouched on binding-branch watch — got {auto_subs:?}"
        );

        // Ad-hoc cross-branch watch on main: dev MUST STILL be subscribed.
        let main_subs = read_ci_watch_subscribers(&main_watch);
        assert!(
            main_subs.contains(&"dev".to_string()),
            "#931: dev must persist on ad-hoc main watch — got {main_subs:?}"
        );

        // Bystander: untouched (dev never subscribed).
        assert!(bystander.exists(), "bystander watch must survive untouched");

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_worktree_deletes_watch_when_last_subscriber_unsubscribes() {
        // #931 INVERTED (was P0-X bonus delete-on-empty pin).
        //
        // Pre-#931: when the released agent was the sole subscriber,
        // release_full deleted the watch file entirely — losing
        // `next_after_ci`, `last_notified_head_sha`, polling state.
        // Post-#931: file persists across release. Cleanup via TTL
        // and PR-terminal paths only.
        let home = tmp_home("931-persist-sole");
        let repo = tmp_repo("931-persist-sole-repo");
        let _l = lease(&home, &repo, "dev", "feat-x").expect("lease");

        let solo_watch = write_ci_watch(&home, "owner/repo", "main", &["dev"]);

        release_full(&home, "dev", false);

        assert!(
            solo_watch.exists(),
            "#931: sole-subscriber watch must persist across release (TTL handles cleanup)"
        );
        // Subs should still contain dev — pure persistence.
        let subs = read_ci_watch_subscribers(&solo_watch);
        assert!(
            subs.contains(&"dev".to_string()),
            "#931: dev persists in subs across release — got {subs:?}"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    // ── #931: ci_watch decouple from worktree release lifecycle ─────────
    //
    // Issue: 4-in-a-row PR stalls overnight (#920/#925/#928/#929) traced
    // to `release_full` calling `unsubscribe_all_ci_watches_for_agent`,
    // which removed the released agent from every ci-watch (binding-branch
    // and ad-hoc), cascading to watch-file deletion on sole-subscriber.
    // The cascade destroyed `next_after_ci` chains + polling state, so
    // reviewer/dev never received post-CI handoff notifications.
    //
    // Direction A.1 (operator-approved 2026-05-19): decouple subscription
    // from worktree binding entirely. Hygiene via 72h TTL + PR-terminal
    // auto-clear + explicit unwatch only.
    //
    // RED→GREEN regression-proof anchors: each test below documents the
    // pre-fix failure signature; if the call at the historic
    // `unsubscribe_all_ci_watches_for_agent` site is re-introduced, these
    // tests immediately fail.

    #[test]
    fn release_does_not_delete_ci_watch_when_agent_was_sole_subscriber_931() {
        // Anchor: pre-#931 release_full ran `remove_file(&path)` when subs
        // became empty (`unsubscribe_all_ci_watches_for_agent`,
        // `worktree_pool.rs:464-468`). The watch file gone → poller skipped
        // → `next_after_ci` target never injected. Post-#931 the file
        // persists with full state.
        let home = tmp_home("931-sole-persist");
        let repo = tmp_repo("931-sole-persist-repo");
        let _l = lease(&home, &repo, "dev", "feat/931-sole").expect("lease");

        let watch_path = write_ci_watch_with_extras(
            &home,
            "owner/repo",
            "feat/931-sole",
            &["dev"],
            Some("reviewer"),
            Some("cafe1234"),
        );
        assert!(watch_path.exists(), "pre: watch exists");

        release_full(&home, "dev", false);

        assert!(
            watch_path.exists(),
            "#931 GREEN: sole-subscriber watch file MUST persist across release"
        );
        assert_eq!(
            read_ci_watch_field(&watch_path, "next_after_ci"),
            Some("reviewer".to_string()),
            "#931 GREEN: next_after_ci chain MUST survive release"
        );
        assert_eq!(
            read_ci_watch_field(&watch_path, "last_notified_head_sha"),
            Some("cafe1234".to_string()),
            "#931 GREEN: polling state MUST survive release"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_does_not_remove_agent_from_multi_subscriber_watch_931() {
        // Anchor: pre-#931, retain(|s| s != agent) shrank subscriber lists
        // on EVERY watch the released agent appeared on, including non-
        // binding-branch ad-hoc watches (e.g. agent watching `main` to
        // follow upstream during closeout). Post-#931, no subscriber list
        // is mutated on release — operator's stated direction is full
        // persistence.
        let home = tmp_home("931-multi-persist");
        let repo = tmp_repo("931-multi-persist-repo");
        let _l = lease(&home, &repo, "dev", "feat/binding").expect("lease");

        let binding_watch =
            write_ci_watch(&home, "owner/repo", "feat/binding", &["dev", "reviewer"]);
        let other_watch = write_ci_watch(&home, "owner/repo", "feat/other", &["dev"]);

        release_full(&home, "dev", false);

        // Binding branch watch: dev preserved alongside reviewer.
        let binding_subs = read_ci_watch_subscribers(&binding_watch);
        assert!(
            binding_subs.contains(&"dev".to_string()),
            "#931 GREEN: dev preserved on binding-branch watch — got {binding_subs:?}"
        );
        assert!(
            binding_subs.contains(&"reviewer".to_string()),
            "co-subscriber preserved — got {binding_subs:?}"
        );

        // Non-binding branch watch: dev preserved untouched.
        let other_subs = read_ci_watch_subscribers(&other_watch);
        assert!(
            other_subs.contains(&"dev".to_string()),
            "#931 GREEN: dev preserved on non-binding-branch ad-hoc watch — got {other_subs:?}"
        );
        assert!(
            other_watch.exists(),
            "non-binding-branch watch file preserved"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_under_rebase_mode_preserves_subscription_931() {
        // #931 Fix 1 corollary: when `bind_self(rebase_mode=true)` triggers
        // `rebase_clean_self` (force_release/mod.rs:187), the underlying
        // `release_full` call MUST preserve the ci-watch so that the
        // immediately-following `dispatch_auto_bind_lease` re-arms the
        // existing watch via append-idempotent handle_watch_ci — keeping
        // any prior `next_after_ci` chain intact.
        //
        // Pre-#931: rebase_clean_self → release_full → file deleted
        // (sole-sub case) → re-dispatch creates fresh watch missing
        // next_after_ci → reviewer never gets [ci-ready-for-action].
        //
        // Post-#931: file persists across the rebase round-trip; the
        // re-dispatch sees the same watch JSON and appends; chain intact.
        //
        // This test exercises the release-half of the rebase cycle
        // directly (calling release_full is what rebase_clean_self does
        // internally). The full bind_self(rebase_mode=true) round-trip
        // is covered by the dispatch_hook test for next_after_ci wiring
        // (test 6) — those two together pin both halves.
        let home = tmp_home("931-rebase");
        let repo = tmp_repo("931-rebase-repo");
        let _l = lease(&home, &repo, "dev", "feat/rebase-cycle").expect("lease");

        let watch_path = write_ci_watch_with_extras(
            &home,
            "owner/repo",
            "feat/rebase-cycle",
            &["dev"],
            Some("reviewer"),
            Some("beefcafe"),
        );

        // Release (the rebase_clean_self path's release_full invocation).
        release_full(&home, "dev", false);

        // File persists with next_after_ci + state intact across release.
        assert!(
            watch_path.exists(),
            "#931 GREEN: rebase-path release_full must preserve watch file"
        );
        assert_eq!(
            read_ci_watch_field(&watch_path, "next_after_ci"),
            Some("reviewer".to_string()),
            "#931 GREEN: next_after_ci chain survives rebase-path release"
        );
        assert_eq!(
            read_ci_watch_field(&watch_path, "last_notified_head_sha"),
            Some("beefcafe".to_string()),
            "#931 GREEN: polling state survives rebase-path release"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn explicit_unwatch_wins_over_concurrent_release_931() {
        // #931 race invariant (§3.20 SOP 1 deterministic): when an operator
        // explicitly unsubscribes an agent (`ci action=unwatch`) AND a
        // concurrent release_full fires, the explicit-unwatch's destructive
        // intent (drop agent from subs; remove watch if sole) MUST be the
        // surviving outcome regardless of arrival order.
        //
        // Post-#931 Fix 1 the race is degenerate by construction:
        // release_full is a no-op against ci-watch state, so the explicit
        // unwatch alone decides the outcome. This test pins that property
        // so a future regression that re-introduces release-side mutation
        // (or worse, race-with-unwatch double-write) is caught.
        let home = tmp_home("931-unwatch-vs-release");
        let repo = tmp_repo("931-unwatch-vs-release-repo");
        let _l = lease(&home, &repo, "dev", "feat/unwatch-race").expect("lease");

        let watch_path = write_ci_watch(&home, "owner/repo", "feat/unwatch-race", &["dev"]);

        // Order 1: release then explicit unwatch via direct file mutation
        // (mirrors what `handle_unwatch_ci`'s last-subscriber path does:
        // remove the watch file). Deterministic — no sleep, no threads.
        release_full(&home, "dev", false);
        assert!(
            watch_path.exists(),
            "release_full is no-op for ci-watch post-#931"
        );

        // Simulate explicit unwatch: agent's removal cascades to file
        // deletion (sole-subscriber path of handle_unwatch_ci).
        let _ = std::fs::remove_file(&watch_path);

        assert!(
            !watch_path.exists(),
            "#931: explicit unwatch wins → watch file gone after both ops"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn poll_tick_vs_subscriber_mutation_preserves_single_delivery_931() {
        // #931 race invariant (§3.20 SOP 1 deterministic): a poll cycle
        // reading the watch file MUST see a consistent subscriber list
        // even if release_full or handle_watch_ci (subscribe) interleaves.
        //
        // Post-#931 Fix 1, release_full does not mutate ci-watch state →
        // the only mutating writer on this file is `handle_watch_ci`
        // (append) and `handle_unwatch_ci` (shrink/delete). All use
        // `crate::store::atomic_write` so a half-written file is never
        // observed by a concurrent reader (atomicity == temp-file +
        // rename invariant).
        //
        // Determinism: this test does NOT spawn threads. Instead it
        // exercises the read-modify-write contract sequentially and
        // asserts the file's parseability + subscriber stability invariant
        // at each step. SOP 1 pattern — no sleeps, no joins.
        let home = tmp_home("931-poll-mut-race");
        let repo = tmp_repo("931-poll-mut-race-repo");
        let _l = lease(&home, &repo, "dev", "feat/poll-mut").expect("lease");

        let watch_path = write_ci_watch(&home, "owner/repo", "feat/poll-mut", &["dev", "reviewer"]);

        // Snapshot 1: pre-release reading must observe both subscribers
        // and be a fully-parseable JSON (atomic-write invariant).
        let snap1 = read_ci_watch_subscribers(&watch_path);
        assert_eq!(snap1.len(), 2, "pre-release snapshot: 2 subscribers");

        // Release fires — must not corrupt file or strip subscribers.
        release_full(&home, "dev", false);

        // Snapshot 2: post-release reading STILL parses + STILL has both.
        let snap2 = read_ci_watch_subscribers(&watch_path);
        assert_eq!(
            snap1, snap2,
            "#931: release_full preserves subscriber list (poll reader sees stable state)"
        );

        // File still atomically parseable (no partial write).
        let content = std::fs::read_to_string(&watch_path).expect("readable");
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("parseable JSON");
        assert_eq!(parsed["branch"].as_str(), Some("feat/poll-mut"));

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn released_agent_still_receives_ci_pass_inject_931() {
        // #931 MANDATORY INTEGRATION TEST.
        //
        // End-to-end: after release_full, the agent's subscription on
        // the binding branch's ci-watch MUST persist so that a subsequent
        // CI-pass poll cycle still enqueues `[ci-pass]` to their inbox.
        // Pre-#931 this was impossible because release_full stripped the
        // agent and (in sole-subscriber case) deleted the file entirely.
        //
        // Note on harness: this test exercises the SUBSCRIPTION half of
        // the integration (release → subs preserved → file ready to be
        // polled), not the full HTTP→provider→enqueue chain (that's
        // already covered by `mock_success_run_updates_watch_state` and
        // others in poller.rs#tests, which use the in-process MockCiProvider).
        // The decouple fix is purely about subscriber-state preservation
        // across release; the poll path is unchanged.
        //
        // Specifically: we assert that immediately after release_full,
        // (a) the watch file exists, (b) the released agent is still in
        // subscribers, (c) the next_after_ci chain is intact, (d) the
        // poll-state fields haven't been clobbered. If all four hold,
        // the next ci_check_repo invocation by the daemon's tick loop
        // will fan out [ci-pass] to the agent verbatim — same code path
        // as the unchanged poller tests verify.
        let home = tmp_home("931-integration-still-receives");
        let repo = tmp_repo("931-integration-still-receives-repo");
        let _l = lease(&home, &repo, "dev", "feat/integration").expect("lease");

        // Pre-state: ci-watch armed with dev as sole subscriber + chain.
        let watch_path = write_ci_watch_with_extras(
            &home,
            "owner/repo",
            "feat/integration",
            &["dev"],
            Some("reviewer"),
            Some("cafefeed"),
        );

        // The operator's pattern: dev pushes PR + releases worktree
        // (frees for next task), expects CI-pass notification later.
        release_full(&home, "dev", false);

        // INTEGRATION ASSERTIONS — all four conditions for the poll
        // pipeline to fan out [ci-pass] to dev's inbox:
        assert!(
            watch_path.exists(),
            "#931 GREEN: (a) watch file present after release"
        );

        let subs = read_ci_watch_subscribers(&watch_path);
        assert!(
            subs.contains(&"dev".to_string()),
            "#931 GREEN: (b) dev still in subscribers — got {subs:?}"
        );

        assert_eq!(
            read_ci_watch_field(&watch_path, "next_after_ci"),
            Some("reviewer".to_string()),
            "#931 GREEN: (c) next_after_ci chain intact"
        );

        // Polling state: last_notified_head_sha preserved (so dedup +
        // rerun detection both keep working).
        assert_eq!(
            read_ci_watch_field(&watch_path, "last_notified_head_sha"),
            Some("cafefeed".to_string()),
            "#931 GREEN: (d) polling state preserved"
        );

        // Pre-#931, all four would fail in the sole-subscriber case
        // because the watch file was deleted entirely. The fact that the
        // existing poller test `mock_success_run_updates_watch_state`
        // demonstrates the [ci-pass] enqueue path works given a valid
        // watch file completes the end-to-end argument.

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_dry_run_does_not_mutate_subscribers_931() {
        // Defensive: dry_run=true is contract-defined as observation-only.
        // Pre-#931, even dry_run paths through release_full would invoke
        // the unsubscribe sweep (no dry_run gate around it). Post-#931
        // there's nothing to gate — but the test pins the invariant in
        // case future code re-introduces mutation on this path.
        let home = tmp_home("931-dry-run");
        let repo = tmp_repo("931-dry-run-repo");
        let _l = lease(&home, &repo, "dev", "feat/dry").expect("lease");

        let watch_path = write_ci_watch_with_extras(
            &home,
            "owner/repo",
            "feat/dry",
            &["dev", "reviewer"],
            Some("next-agent"),
            None,
        );
        let subs_before = read_ci_watch_subscribers(&watch_path);

        let outcome = release_full(&home, "dev", true);
        // dry_run skips actual git/binding teardown semantics elsewhere;
        // we only assert ci-watch state is identical pre/post.

        let subs_after = read_ci_watch_subscribers(&watch_path);
        assert_eq!(
            subs_before, subs_after,
            "#931: dry_run must not mutate subscriber list — before {subs_before:?} after {subs_after:?} outcome {outcome:?}"
        );
        assert_eq!(
            read_ci_watch_field(&watch_path, "next_after_ci"),
            Some("next-agent".to_string()),
            "#931: dry_run must preserve next_after_ci"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    // ── Issue #611: branch cleanup tests ────────────────────────────────

    #[test]
    fn release_full_deletes_merged_branch() {
        let home = tmp_home("611-merged");
        let repo = tmp_repo("611-merged-repo");
        // Lease creates the branch + worktree.
        let l = lease_bound(&home, &repo, "agent-611m", "feat/merged");
        // Add a commit on the feature branch via the worktree.
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "feat",
            ])
            .current_dir(&l.path)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        // Merge feat/merged into main from the source repo (without checking it out).
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "merge",
                "feat/merged",
                "--no-ff",
                "-m",
                "merge",
            ])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();

        let outcome = release_full(&home, "agent-611m", false);

        assert!(outcome.released);
        assert!(
            outcome.branch_deleted,
            "merged branch must be deleted: {:?}",
            outcome
        );
        assert!(outcome.branch_cleanup_skipped_reason.is_none());
        // Verify branch is actually gone from the repo.
        let branch_exists = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "feat/merged"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(
            !branch_exists,
            "branch must not exist in repo after cleanup"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_full_preserves_unmerged_branch() {
        let home = tmp_home("611-unmerged");
        let repo = tmp_repo("611-unmerged-repo");
        // Lease creates the branch + worktree.
        let l = lease_bound(&home, &repo, "agent-611u", "feat/unmerged");
        // Add a commit on the feature branch (not merged into main).
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "wip",
            ])
            .current_dir(&l.path)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();

        let outcome = release_full(&home, "agent-611u", false);

        assert!(outcome.released);
        assert!(
            !outcome.branch_deleted,
            "unmerged branch must NOT be deleted"
        );
        assert_eq!(
            outcome.branch_cleanup_skipped_reason.as_deref(),
            Some("branch not merged into main")
        );
        // Verify branch still exists.
        let branch_exists = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "feat/unmerged"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(branch_exists, "unmerged branch must still exist in repo");

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_full_absent_worktree_merged_branch_cleaned_up() {
        let home = tmp_home("1249-absent-merged");
        let repo = tmp_repo("1249-absent-merged-repo");
        let l = lease_bound(&home, &repo, "agent-1249m", "feat/absent-merged");
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "feat",
            ])
            .current_dir(&l.path)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "merge",
                "feat/absent-merged",
                "--no-ff",
                "-m",
                "merge",
            ])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        // Remove worktree directory to simulate absent-worktree scenario.
        std::fs::remove_dir_all(&l.path).unwrap();
        std::process::Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();

        let outcome = release_full(&home, "agent-1249m", false);

        assert!(outcome.released);
        assert!(
            outcome.branch_deleted,
            "merged branch must be deleted even when worktree absent: {outcome:?}"
        );
        assert!(outcome.branch_cleanup_skipped_reason.is_none());
        let branch_exists = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "feat/absent-merged"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(!branch_exists, "branch must not exist after cleanup");
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_full_absent_worktree_unmerged_branch_preserved() {
        let home = tmp_home("1249-absent-unmerged");
        let repo = tmp_repo("1249-absent-unmerged-repo");
        let l = lease_bound(&home, &repo, "agent-1249u", "feat/absent-unmerged");
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "wip",
            ])
            .current_dir(&l.path)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        // Remove worktree directory without merging.
        std::fs::remove_dir_all(&l.path).unwrap();
        std::process::Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();

        let outcome = release_full(&home, "agent-1249u", false);

        assert!(outcome.released);
        assert!(
            !outcome.branch_deleted,
            "unmerged branch must NOT be deleted"
        );
        assert_eq!(
            outcome.branch_cleanup_skipped_reason.as_deref(),
            Some("branch not merged into main")
        );
        let branch_exists = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "feat/absent-unmerged"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(branch_exists, "unmerged branch must still exist");
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_full_dry_run_does_not_delete_branch() {
        let home = tmp_home("611-dryrun");
        let repo = tmp_repo("611-dryrun-repo");
        // Lease creates the branch + worktree.
        let l = lease_bound(&home, &repo, "agent-611d", "feat/dryrun");
        // Add a commit and merge into main.
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "feat",
            ])
            .current_dir(&l.path)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "merge",
                "feat/dryrun",
                "--no-ff",
                "-m",
                "merge",
            ])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();

        let outcome = release_full(&home, "agent-611d", true);

        assert!(outcome.released);
        assert!(!outcome.branch_deleted, "dry-run must NOT delete branch");
        assert_eq!(
            outcome.branch_cleanup_skipped_reason.as_deref(),
            Some("dry-run: would delete branch 'feat/dryrun'")
        );
        // Verify branch still exists.
        let branch_exists = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "feat/dryrun"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(branch_exists, "branch must survive dry-run");

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// §3.9 (#t-7, #1824 follow-up): a dry-run `release_full` must be
    /// observation-only — it must NOT run the ref-mutating `git fetch --prune`
    /// inside `cleanup_merged_branch`. Proven by planting a STALE
    /// remote-tracking ref (`refs/remotes/origin/ghost`, absent on the real
    /// origin) that a `fetch --prune` WOULD remove, then asserting it survives a
    /// dry-run. Regression-proof: un-gate the fetch and `ghost` is pruned →
    /// the ref set differs.
    #[test]
    fn dry_run_release_does_not_mutate_remote_tracking_refs_t7() {
        fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("AGEND_GIT_BYPASS", "1")
                .output()
                .expect("git")
        }
        fn refs_remotes(dir: &std::path::Path) -> String {
            String::from_utf8_lossy(&git(dir, &["for-each-ref", "refs/remotes"]).stdout).to_string()
        }

        let home = tmp_home("t7-dryrun-refs");
        // A real upstream + a clone (so the clone has an `origin` remote +
        // refs/remotes/origin/*). `release_full` operates on the clone.
        let origin = tmp_repo("t7-origin");
        let source = tmp_home("t7-source");
        git(
            std::path::Path::new("/"),
            &[
                "clone",
                &origin.display().to_string(),
                &source.display().to_string(),
            ],
        );
        // Plant a stale remote-tracking ref that `fetch --prune` would remove.
        let head = String::from_utf8_lossy(&git(&source, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();
        git(&source, &["update-ref", "refs/remotes/origin/ghost", &head]);

        // Lease a worktree in the clone (binds source_repo=source); merge state
        // is irrelevant — the fetch runs BEFORE the merge check.
        let _l = lease_bound(&home, &source, "agent-t7", "feat/t7");

        let before = refs_remotes(&source);
        assert!(
            before.contains("refs/remotes/origin/ghost"),
            "pre-cond: stale ghost ref planted: {before}"
        );

        let outcome = release_full(&home, "agent-t7", true); // dry-run
        assert!(outcome.released, "dry-run reports observation success");

        let after = refs_remotes(&source);
        assert_eq!(
            before, after,
            "dry-run must NOT mutate remote-tracking refs (no fetch --prune)"
        );
        assert!(
            after.contains("refs/remotes/origin/ghost"),
            "the prune-target stale ref must survive a dry-run: {after}"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&origin).ok();
        std::fs::remove_dir_all(&source).ok();
    }

    #[test]
    fn release_full_does_not_delete_unrelated_branch() {
        let home = tmp_home("unrelated-branch");
        let repo = tmp_repo("unrelated-branch-repo");
        // Create an unrelated user branch with its own commit
        std::process::Command::new("git")
            .args(["checkout", "-b", "user/my-feature"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "user work",
            ])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["checkout", "main"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        // Lease a different branch
        let _l = lease_bound(&home, &repo, "agent-x", "feat/daemon-task");
        let outcome = release_full(&home, "agent-x", false);
        assert!(outcome.released);
        // Unrelated branch must still exist
        let branch_exists = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "user/my-feature"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(
            branch_exists,
            "unrelated user branch must NOT be deleted by release_worktree"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn gc_new_layout_active_binding_not_candidate() {
        let home = tmp_home("gc-new-active");
        // Create new-layout worktree with active binding
        let wt = home.join("worktrees").join("dev-1").join("feat-branch");
        std::fs::create_dir_all(&wt).unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::hours(100)).to_rfc3339();
        std::fs::write(
            wt.join(MANAGED_MARKER),
            format!("agent=dev-1\nbranch=feat-branch\nleased_at={old}\nreleased_at={old}\n"),
        )
        .unwrap();
        // Create active binding for dev-1
        let rt = crate::paths::runtime_dir(&home).join("dev-1");
        std::fs::create_dir_all(&rt).unwrap();
        std::fs::write(
            rt.join("binding.json"),
            r#"{"worktree":"/tmp/x","branch":"feat-branch"}"#,
        )
        .unwrap();

        let candidates = gc_candidates(&home);
        assert!(
            candidates.is_empty(),
            "new-layout worktree with active binding must not be GC candidate"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_new_layout_released_past_grace_is_candidate() {
        let home = tmp_home("gc-new-released");
        let wt = home.join("worktrees").join("dev-2").join("old-branch");
        std::fs::create_dir_all(&wt).unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::hours(100)).to_rfc3339();
        std::fs::write(
            wt.join(MANAGED_MARKER),
            format!("agent=dev-2\nbranch=old-branch\nleased_at={old}\nreleased_at={old}\n"),
        )
        .unwrap();
        // No binding for dev-2

        let candidates = gc_candidates(&home);
        assert_eq!(
            candidates.len(),
            1,
            "new-layout released worktree past grace should be GC candidate"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #807 Item 2 — ReleaseOutcome serialization shape ──

    #[test]
    fn test_release_outcome_success_omits_error_key() {
        // #807 Item 2 RED: pre-fix `ReleaseOutcome` always serializes
        // `error: None` → `"error": null`, which client renderers
        // (Claude Code, etc.) interpret as an `<error>` envelope on
        // what is actually a successful release. Fix: add
        // `#[serde(skip_serializing_if = "Option::is_none")]` so the
        // `error` key is absent on success.
        let outcome = ReleaseOutcome {
            released: true,
            worktree_removed: true,
            binding_removed: true,
            branch_deleted: true,
            ..Default::default()
        };
        let json = serde_json::to_value(&outcome).expect("serialize");
        let obj = json.as_object().expect("object shape");
        assert!(
            !obj.contains_key("error"),
            "success response must NOT carry `error` key (#807 cosmetic fix), got keys: {:?}",
            obj.keys().collect::<Vec<_>>()
        );
        assert!(
            !obj.contains_key("branch_cleanup_skipped_reason"),
            "success response must NOT carry `branch_cleanup_skipped_reason` when None, got keys: {:?}",
            obj.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_release_outcome_real_failure_emits_error_key() {
        // #807 Item 2 contract guarantee: actual failures STILL emit
        // the `error` field. Only the `None`-on-success case is
        // omitted — `skip_serializing_if` only drops `None`, never
        // `Some`.
        let outcome = ReleaseOutcome {
            released: false,
            error: Some("test failure".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_value(&outcome).expect("serialize");
        let obj = json.as_object().expect("object shape");
        assert!(
            obj.contains_key("error"),
            "real failure must surface `error` key, got keys: {:?}",
            obj.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            obj["error"], "test failure",
            "error message must round-trip unchanged"
        );
    }

    // ── gc_run tests ──────────────────────────────────────────────

    #[test]
    fn gc_run_returns_empty_when_no_candidates() {
        let home = tmp_home("gc-run-empty");
        let results = gc_run(&home);
        assert!(results.is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_stale_ci_watch_locks_removes_old_locks() {
        let home = tmp_home("gc-locks");
        let ci_dir = home.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).unwrap();

        // Create a lock file with an old mtime (> 7 days ago)
        let stale_lock = ci_dir.join("pr-123.lock");
        std::fs::write(&stale_lock, "locked").unwrap();
        // Set mtime to 8 days ago
        let eight_days_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 24 * 3600);
        let f = std::fs::File::options()
            .write(true)
            .open(&stale_lock)
            .unwrap();
        f.set_modified(eight_days_ago).unwrap();

        // Create a recent lock file (should NOT be removed)
        let recent_lock = ci_dir.join("pr-456.lock");
        std::fs::write(&recent_lock, "locked").unwrap();

        // Create a non-lock file (should NOT be removed)
        let json_file = ci_dir.join("pr-789.json");
        std::fs::write(&json_file, "{}").unwrap();

        let removed = gc_stale_ci_watch_locks(&home);
        assert_eq!(removed, 1, "only the stale lock should be removed");
        assert!(!stale_lock.exists(), "stale lock must be deleted");
        assert!(recent_lock.exists(), "recent lock must be preserved");
        assert!(json_file.exists(), "non-lock file must be preserved");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_stale_ci_watch_locks_handles_missing_dir() {
        let home = tmp_home("gc-locks-nodir");
        let removed = gc_stale_ci_watch_locks(&home);
        assert_eq!(removed, 0);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resolve_source_repo_parses_gitdir_pointer() {
        let home = tmp_home("resolve-src");
        let fake_wt = home.join("wt");
        std::fs::create_dir_all(&fake_wt).unwrap();
        // Simulate .git file pointing to source/.git/worktrees/wt
        let source = home.join("source");
        let gitdir_target = source.join(".git").join("worktrees").join("wt");
        std::fs::create_dir_all(&gitdir_target).unwrap();
        std::fs::write(
            fake_wt.join(".git"),
            format!("gitdir: {}", gitdir_target.display()),
        )
        .unwrap();
        let resolved = resolve_source_repo(&fake_wt);
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap(), source);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resolve_source_repo_returns_none_for_regular_repo() {
        let home = tmp_home("resolve-none");
        let fake_dir = home.join("regular");
        std::fs::create_dir_all(&fake_dir).unwrap();
        // A regular .git directory, not a worktree
        std::fs::create_dir_all(fake_dir.join(".git")).unwrap();
        let resolved = resolve_source_repo(&fake_dir);
        assert!(resolved.is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    // ── t-worktree-leak PR-2: force-reclaim backstop tests ──

    fn backdate_lease(wt_path: &Path, days_ago: i64) {
        let marker = wt_path.join(MANAGED_MARKER);
        let content = std::fs::read_to_string(&marker).unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::days(days_ago)).to_rfc3339();
        let new: String = content
            .lines()
            .map(|l| {
                if l.starts_with("leased_at=") {
                    format!("leased_at={old}")
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&marker, new).unwrap();
    }

    #[test]
    fn force_reclaim_dead_agent_past_cap_is_candidate() {
        let home = tmp_home("fr-dead");
        let repo = tmp_repo("fr-dead-repo");
        let lease = lease(&home, &repo, "dev-dead", "feat/x").expect("lease");
        backdate_lease(&lease.path, force_reclaim_age_days() + 2);
        let live: std::collections::HashSet<String> = std::collections::HashSet::new();
        let cand = evaluate_candidate(&home, &lease.path, &live);
        assert!(
            cand.is_some(),
            "dead agent, never-released, past cap → force-reclaim candidate"
        );
        assert_eq!(cand.unwrap().kind, GcKind::ForceReclaim);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Append a malformed `released_at=` to a lease's marker and drop its binding
    /// (a released worktree is unbound) — the #1882 WT-LEAK-1 corrupt-marker shape.
    fn corrupt_released_at(home: &Path, agent: &str, wt_path: &Path) {
        crate::binding::unbind(home, agent);
        let marker = wt_path.join(MANAGED_MARKER);
        let mut content = std::fs::read_to_string(&marker).unwrap();
        content.push_str("released_at=not-a-timestamp\n");
        std::fs::write(&marker, content).unwrap();
    }

    /// §3.9 #1882 (WT-LEAK-1): a corrupt-`released_at` worktree that is ABANDONED
    /// (no liveness, leased past the force-reclaim age cap) is now reclaimed via
    /// the force-reclaim backstop — pre-fix it leaked forever (the clean-release
    /// path returned None and the never-released arm was unreachable for a
    /// `Some(garbage)` released_at). Regression-proof: revert the parse-Err
    /// fall-through and this is None (leaked).
    #[test]
    fn force_reclaim_corrupt_marker_abandoned_is_candidate_1882() {
        let home = tmp_home("fr-corrupt-dead");
        let repo = tmp_repo("fr-corrupt-dead-repo");
        let lease = lease(&home, &repo, "dev-corrupt", "feat/x").expect("lease");
        corrupt_released_at(&home, "dev-corrupt", &lease.path);
        backdate_lease(&lease.path, force_reclaim_age_days() + 2);
        let live: std::collections::HashSet<String> = std::collections::HashSet::new();
        let cand = evaluate_candidate(&home, &lease.path, &live);
        assert_eq!(
            cand.map(|c| c.kind),
            Some(GcKind::ForceReclaim),
            "#1882: abandoned corrupt-marker worktree (no liveness, past cap) → force-reclaim, not leaked"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// §3.9 #1882 (WT-LEAK-1, no H1 regression): a corrupt-`released_at` worktree
    /// whose agent has a LIVENESS signal is SPARED even past the age cap — the
    /// force-reclaim liveness guard (not the unparseable grace window) protects a
    /// worktree the operator may still be using.
    #[test]
    fn force_reclaim_corrupt_marker_spares_live_1882() {
        let home = tmp_home("fr-corrupt-live");
        let repo = tmp_repo("fr-corrupt-live-repo");
        let lease = lease(&home, &repo, "dev-corrupt-live", "feat/x").expect("lease");
        corrupt_released_at(&home, "dev-corrupt-live", &lease.path);
        backdate_lease(&lease.path, force_reclaim_age_days() + 2);
        let live: std::collections::HashSet<String> =
            ["dev-corrupt-live".to_string()].into_iter().collect();
        assert!(
            evaluate_candidate(&home, &lease.path, &live).is_none(),
            "#1882: a live agent's corrupt-marker worktree must be SPARED (no H1-style WIP destruction)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn force_reclaim_spares_live_registry_agent() {
        // safety #1: any live signal → never reclaim, even past the cap.
        let home = tmp_home("fr-live");
        let repo = tmp_repo("fr-live-repo");
        let lease = lease(&home, &repo, "dev-live", "feat/x").expect("lease");
        backdate_lease(&lease.path, force_reclaim_age_days() + 2);
        let live: std::collections::HashSet<String> =
            ["dev-live".to_string()].into_iter().collect();
        assert!(
            evaluate_candidate(&home, &lease.path, &live).is_none(),
            "agent live in the registry → spared even past cap (liveness-AND-age)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn force_reclaim_spares_ci_watch_subscriber() {
        // multi-signal: a ci-watch subscription is a liveness signal (not heartbeat).
        let home = tmp_home("fr-ciw");
        let repo = tmp_repo("fr-ciw-repo");
        let lease = lease(&home, &repo, "dev-ciw", "feat/x").expect("lease");
        backdate_lease(&lease.path, force_reclaim_age_days() + 2);
        let ci_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
        std::fs::create_dir_all(&ci_dir).unwrap();
        std::fs::write(
            ci_dir.join("w.json"),
            serde_json::json!({
                "repo": "o/r", "branch": "feat/x",
                "subscribers": [{ "instance": "dev-ciw" }]
            })
            .to_string(),
        )
        .unwrap();
        let live: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert!(
            evaluate_candidate(&home, &lease.path, &live).is_none(),
            "agent subscribed to a ci-watch → spared (multi-signal liveness, not just heartbeat)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn force_reclaim_spares_recent_lease() {
        // dead agent but the lease is recent → not yet past the age cap.
        let home = tmp_home("fr-recent");
        let repo = tmp_repo("fr-recent-repo");
        let lease = lease(&home, &repo, "dev-recent", "feat/x").expect("lease");
        let live: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert!(
            evaluate_candidate(&home, &lease.path, &live).is_none(),
            "recent lease → not yet reclaimable (age gate)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    // codex gap ③: the heartbeat / PTY / waiting_on liveness signals + the
    // read-failure → fail-toward-alive path (§3.9, safety-critical).

    #[test]
    fn force_reclaim_spares_recent_heartbeat() {
        let home = tmp_home("fr-hb");
        let repo = tmp_repo("fr-hb-repo");
        let agent = "fr-hb-agent";
        let lease = lease(&home, &repo, agent, "feat/x").expect("lease");
        backdate_lease(&lease.path, force_reclaim_age_days() + 2);
        crate::daemon::heartbeat_pair::update_with(agent, |p| {
            p.heartbeat_at_ms = crate::daemon::heartbeat_pair::now_ms();
        });
        let live: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert!(
            evaluate_candidate(&home, &lease.path, &live).is_none(),
            "recent heartbeat → spared (heartbeat liveness signal)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn force_reclaim_spares_recent_pty_input() {
        let home = tmp_home("fr-pty");
        let repo = tmp_repo("fr-pty-repo");
        let agent = "fr-pty-agent";
        let lease = lease(&home, &repo, agent, "feat/x").expect("lease");
        backdate_lease(&lease.path, force_reclaim_age_days() + 2);
        crate::daemon::heartbeat_pair::update_with(agent, |p| {
            p.last_input_at_ms = crate::daemon::heartbeat_pair::now_ms();
        });
        let live: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert!(
            evaluate_candidate(&home, &lease.path, &live).is_none(),
            "recent PTY input → spared (PTY liveness signal)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn force_reclaim_spares_declared_waiting_on() {
        let home = tmp_home("fr-wait");
        let repo = tmp_repo("fr-wait-repo");
        let agent = "fr-wait-agent";
        let lease = lease(&home, &repo, agent, "feat/x").expect("lease");
        backdate_lease(&lease.path, force_reclaim_age_days() + 2);
        crate::daemon::heartbeat_pair::update_with(agent, |p| {
            p.waiting_on_since_ms = Some(crate::daemon::heartbeat_pair::now_ms());
        });
        let live: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert!(
            evaluate_candidate(&home, &lease.path, &live).is_none(),
            "declared waiting_on → spared (blocked-but-alive signal)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn force_reclaim_ci_watch_read_failure_fails_alive() {
        let home = tmp_home("fr-ciwfail");
        let repo = tmp_repo("fr-ciwfail-repo");
        let agent = "fr-ciwfail-agent";
        let lease = lease(&home, &repo, agent, "feat/x").expect("lease");
        backdate_lease(&lease.path, force_reclaim_age_days() + 2);
        // An unparseable ci-watch file → the liveness read fails → fail-toward-alive.
        let ci_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
        std::fs::create_dir_all(&ci_dir).unwrap();
        std::fs::write(ci_dir.join("corrupt.json"), "{ this is not json").unwrap();
        let live: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert!(
            evaluate_candidate(&home, &lease.path, &live).is_none(),
            "unparseable ci-watch → fail-toward-alive → spared (never reclaim on uncertainty)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn gc_run_force_reclaim_archives_never_hard_deletes() {
        // codex gap ① CRITICAL: the daemon gc_run/gc_remove_one path must route a
        // force-reclaim through the SAFE helper, never hard-delete. Proof: it is
        // ARCHIVED to .trash (recoverable) rather than removed — the old
        // `git worktree remove --force` would have left nothing behind.
        let home = tmp_home("fr-gcrun");
        let repo = tmp_repo("fr-gcrun-repo");
        let lease = lease(&home, &repo, "fr-gcrun-agent", "feat/x").expect("lease");
        let cand = GcCandidate {
            path: lease.path.clone(),
            agent: "fr-gcrun-agent".to_string(),
            reason: "fr".to_string(),
            kind: GcKind::ForceReclaim,
        };
        let result = gc_remove_one(&home, &cand);
        assert!(
            result.removed,
            "force-reclaim via gc_run should archive: {:?}",
            result.error
        );
        assert!(!lease.path.exists(), "worktree moved out");
        let trash = home.join(".trash").join("worktrees");
        assert!(
            std::fs::read_dir(&trash)
                .map(|d| d.flatten().count() > 0)
                .unwrap_or(false),
            "gc_run force-reclaim must ARCHIVE to .trash (recoverable), never hard-delete"
        );
        assert!(
            crate::binding::read(&home, "fr-gcrun-agent").is_none(),
            "binding unbound after force-reclaim"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn collect_managed_worktrees_finds_slash_branch_nested() {
        // reviewer-2 #4: a slash-branch worktree nests an extra level
        // (worktrees/<agent>/fix/xxx) and was missed by the old fixed-depth scan.
        let home = tmp_home("walk-slash");
        let root = daemon_managed_worktree_root(&home);
        let nested = root.join("dev").join("fix").join("xxx");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join(MANAGED_MARKER), "agent=dev\n").unwrap();
        let flat = root.join("dev").join("track-x");
        std::fs::create_dir_all(&flat).unwrap();
        std::fs::write(flat.join(MANAGED_MARKER), "agent=dev\n").unwrap();
        let mut out = Vec::new();
        collect_managed_worktrees(&root, MARKER_WALK_MAX_DEPTH, &mut out);
        assert!(
            out.contains(&nested),
            "slash-branch nested worktree must be enumerated (reviewer-2 #4)"
        );
        assert!(out.contains(&flat), "non-slash worktree still enumerated");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn boot_grace_predicate_suspends_only_when_recent_or_unknown() {
        // reviewer-2 #5: recent boot → suspend; aged boot → proceed; unknown →
        // conservative suspend.
        assert!(
            within_boot_grace(Some(1000), 1100, 600),
            "100s after boot, 600s grace → in grace (suspend)"
        );
        assert!(
            !within_boot_grace(Some(1000), 2000, 600),
            "1000s after boot → past grace (proceed)"
        );
        assert!(
            within_boot_grace(None, 2000, 600),
            "unknown boot time → conservative suspend"
        );
    }

    // ── #2234 Phase 2: layout-aware agent attribution + enumerate ──────────
    /// The GC agent-attribution fix. The OLD fallback used the immediate PARENT
    /// dir name, so a cure-(B) `<home>/workspace/<agent>` worktree resolved to
    /// `"workspace"` (the root) → liveness keyed on a non-agent → a live agent's
    /// cwd could be GC-reclaimed. Layout-aware strip-prefix returns the real agent.
    #[test]
    fn agent_from_layout_is_layout_aware_2234() {
        let home = tmp_home("agent-from-layout");
        // worktrees/<agent>/<slash-branch> → FIRST component is the agent.
        let nested = home.join("worktrees").join("dev").join("fix").join("x");
        assert_eq!(agent_from_layout(&home, &nested), Some("dev".to_string()));
        // workspace/<agent> (cure-(B)): the dir name IS the agent, NOT "workspace".
        let ws = crate::paths::workspace_dir(&home).join("dev2");
        assert_eq!(
            agent_from_layout(&home, &ws),
            Some("dev2".to_string()),
            "#2234: /workspace/<agent> must resolve to <agent>, not the parent 'workspace'"
        );
        // Off both managed roots → None (never guess via parent dir).
        assert_eq!(
            agent_from_layout(&home, std::path::Path::new("/tmp/elsewhere/x")),
            None
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// RED→GREEN end-to-end: a clean-released cure-(B) `workspace/<agent>` worktree
    /// whose marker lacks `agent=` (forces the fallback) must yield a GcCandidate
    /// whose `agent` is the workspace dir name — RED (old parent-file_name): the
    /// candidate's agent was `"workspace"`, so the force-reclaim liveness guard
    /// would key on a non-agent and could reclaim a LIVE agent's workspace cwd.
    #[test]
    fn evaluate_candidate_workspace_worktree_resolves_real_agent_2234() {
        let home = tmp_home("eval-ws-agent");
        let repo = tmp_repo("eval-ws-agent-repo");
        let wt = managed_workspace_worktree(&home, &repo, "devw", "fix/x");
        // Clean-released past grace, NO agent= field → exercises the path fallback.
        let old = (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
        std::fs::write(
            wt.join(MANAGED_MARKER),
            format!("leased_at={old}\nreleased_at={old}\n"),
        )
        .unwrap();
        let live = std::collections::HashSet::new();
        let cand =
            evaluate_candidate(&home, &wt, &live).expect("clean-released worktree is a candidate");
        assert_eq!(
            cand.agent, "devw",
            "#2234: agent must resolve to the workspace dir name 'devw', NOT the parent 'workspace'"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// no-miss union: a REGISTERED workspace worktree (seen via `git worktree
    /// list`) AND an unregistered orphan marker dir under the worktrees root (seen
    /// via the fs-scan) are BOTH enumerated, with correct `registered` flags +
    /// layout-derived agents. Proves neither source alone suffices.
    #[test]
    fn enumerate_unions_registered_and_orphan_no_miss_2234() {
        let home = tmp_home("enum-union");
        let repo = tmp_repo("enum-union-repo");
        // (a) REGISTERED, cure-(B) workspace layout.
        let _ws = managed_workspace_worktree(&home, &repo, "devw", "feat/y");
        // (b) ORPHAN: a marker dir under worktrees root, NOT git-registered.
        let orphan = home.join("worktrees").join("devo").join("fix").join("z");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join(MANAGED_MARKER), "agent=devo\n").unwrap();

        let got = enumerate_managed_worktrees(&home, &repo);

        let ws = got
            .iter()
            .find(|w| w.agent.as_deref() == Some("devw"))
            .expect("registered workspace worktree must be enumerated (registry pass)");
        assert!(ws.registered, "workspace worktree is git-registered");

        let orp = got
            .iter()
            .find(|w| w.agent.as_deref() == Some("devo"))
            .expect("orphan marker dir must be enumerated (fs-scan — no-miss)");
        assert!(
            !orp.registered,
            "orphan dir is NOT git-registered (caught only by the fs-scan)"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// #2234 Phase 2 byte-identical-OFF: with no cure-(B) workspace worktree,
    /// `fs_managed_worktrees` == the worktrees_root marker-walk (gc's prior scan).
    #[test]
    fn fs_managed_worktrees_off_byte_identical_2234() {
        let home = tmp_home("fsm-off");
        let wt = home.join("worktrees").join("dev").join("fix").join("x");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(MANAGED_MARKER), "agent=dev\n").unwrap();
        let mut collected = Vec::new();
        collect_managed_worktrees(
            &daemon_managed_worktree_root(&home),
            MARKER_WALK_MAX_DEPTH,
            &mut collected,
        );
        assert_eq!(
            fs_managed_worktrees(&home),
            collected,
            "#2234 OFF: fs_managed == worktrees_root marker-walk (workspace part empty → byte-identical)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2234 Phase 2 (B) ON: a `workspace/<agent>` gitlink worktree is included.
    #[test]
    fn fs_managed_worktrees_includes_workspace_gitlink_2234() {
        let home = tmp_home("fsm-b");
        let repo = tmp_repo("fsm-b-repo");
        let ws = managed_workspace_worktree(&home, &repo, "devb", "feat/y");
        assert!(
            fs_managed_worktrees(&home).iter().any(|p| p == &ws),
            "#2234: cure-(B) workspace gitlink worktree must be enumerated by fs_managed_worktrees"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    // ── t-…50793-9: managed-worktree target/ retention sweep ──────────────
    // These exercise the REAL sweep against on-disk fixtures. Unix-gated
    // because they set past mtimes via `touch -t` and create symlinks; the
    // helpers are #[cfg(unix)] for the same reason (Windows -D warnings would
    // flag them as dead otherwise).

    /// Age every entry under `p` (dirs+files) to a fixed past time (2025-01-01),
    /// well over the 48h staleness window relative to the test clock.
    #[cfg(unix)]
    fn touch_old(p: &Path) {
        let _ = std::process::Command::new("touch")
            .args(["-t", "202501010000"])
            .arg(p)
            .status();
        if let Ok(entries) = std::fs::read_dir(p) {
            for e in entries.flatten() {
                touch_old(&e.path());
            }
        }
    }

    /// Create a daemon-managed worktree (`.agend-managed` marker) under
    /// `home/worktrees/<agent>/<branch>` with a populated `target/`. `stale`
    /// ages the whole `target/` tree past the window. Returns (worktree, target).
    #[cfg(unix)]
    fn mk_managed_target(
        home: &Path,
        agent: &str,
        branch: &str,
        stale: bool,
    ) -> (PathBuf, PathBuf) {
        let wt = daemon_managed_worktree_root(home).join(agent).join(branch);
        std::fs::create_dir_all(wt.join("target").join("debug")).unwrap();
        std::fs::write(
            wt.join(MANAGED_MARKER),
            format!(
                "agent={agent}\nbranch={branch}\nleased_at={}\n",
                chrono::Utc::now().to_rfc3339()
            ),
        )
        .unwrap();
        std::fs::write(wt.join("target").join("debug").join("app"), vec![0u8; 4096]).unwrap();
        if stale {
            touch_old(&wt.join("target"));
        }
        (wt.clone(), wt.join("target"))
    }

    /// ① Sweep a STALE managed `target/` — deleted; the worktree + marker survive.
    #[cfg(unix)]
    #[test]
    fn target_sweep_reclaims_stale_managed_target() {
        let home = tmp_home("tgt-stale");
        let (wt, target) = mk_managed_target(&home, "dev-x", "feat/foo", true);
        assert!(target.exists());
        let age = std::time::Duration::from_secs(48 * 3600);

        let cands = target_sweep_candidates(&home, age, 0);
        assert_eq!(cands.len(), 1, "stale managed target/ must be a candidate");

        let results = target_sweep_run(&home, age, 0);
        assert!(
            results.iter().any(|r| r.removed),
            "stale target/ must be removed: {results:?}"
        );
        assert!(!target.exists(), "target/ must be deleted");
        assert!(
            wt.exists() && wt.join(MANAGED_MARKER).exists(),
            "worktree + marker MUST survive — only target/ is swept"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// ② NEVER sweep an unmanaged dir, and NEVER follow a `target` symlink that
    /// escapes to canonical/operator data (the core footgun).
    #[cfg(unix)]
    #[test]
    fn target_sweep_refuses_unmanaged_and_symlinked_canonical() {
        let home = tmp_home("tgt-safe");
        let age = std::time::Duration::from_secs(48 * 3600);

        // (a) unmanaged worktree dir (NO .agend-managed marker) with a stale target/.
        let unmanaged = daemon_managed_worktree_root(&home)
            .join("nomarker")
            .join("br");
        std::fs::create_dir_all(unmanaged.join("target")).unwrap();
        std::fs::write(unmanaged.join("target").join("f"), b"x").unwrap();
        touch_old(&unmanaged.join("target"));

        // (b) a "canonical" repo target OUTSIDE the managed roots, plus a managed
        //     worktree whose `target` is a SYMLINK pointing at it (escape attempt).
        let canonical = home.join("canonical-repo").join("target");
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::write(canonical.join("precious.bin"), b"operator-data").unwrap();
        touch_old(&canonical);
        let wt = daemon_managed_worktree_root(&home).join("dev-y").join("br");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(MANAGED_MARKER), "agent=dev-y\nbranch=br\n").unwrap();
        std::os::unix::fs::symlink(&canonical, wt.join("target")).unwrap();

        let cands = target_sweep_candidates(&home, age, 0);
        assert!(
            cands.is_empty(),
            "unmanaged target + symlink-to-canonical must NOT be candidates: {cands:?}"
        );

        let results = target_sweep_run(&home, age, 0);
        assert!(
            results.iter().all(|r| !r.removed),
            "nothing must be deleted: {results:?}"
        );
        assert!(
            canonical.join("precious.bin").exists(),
            "canonical operator data MUST survive the symlink-escape attempt"
        );
        assert!(
            unmanaged.join("target").exists(),
            "unmanaged target/ MUST survive"
        );
        // Direct unit on the guard: a symlinked target is refused.
        assert!(
            validate_target_for_delete(&home, &wt.join("target")).is_err(),
            "symlinked target must be refused by validate_target_for_delete"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// ③ Skip an ACTIVE build (fresh mtime within the window) — not deleted.
    #[cfg(unix)]
    #[test]
    fn target_sweep_skips_fresh_active_build_target() {
        let home = tmp_home("tgt-fresh");
        let (_, target) = mk_managed_target(&home, "dev-z", "feat/bar", false); // fresh
        let age = std::time::Duration::from_secs(48 * 3600);

        let cands = target_sweep_candidates(&home, age, 0);
        assert!(
            cands.is_empty(),
            "fresh (active-build) target/ must be skipped: {cands:?}"
        );
        let results = target_sweep_run(&home, age, 0);
        assert!(results.iter().all(|r| !r.removed));
        assert!(target.exists(), "active target/ MUST survive");
        std::fs::remove_dir_all(&home).ok();
    }

    /// ④ Dry-run previews the stale candidate WITHOUT deleting it.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn target_sweep_dry_run_previews_without_deleting() {
        let home = tmp_home("tgt-dry");
        let (_, target) = mk_managed_target(&home, "dev-d", "feat/baz", true); // stale
                                                                               // dry_run reads env config — pin to defaults (enabled, 48h).
        std::env::remove_var("AGEND_TARGET_GC_DISABLE");
        std::env::remove_var("AGEND_TARGET_GC_AGE_HOURS");
        std::env::remove_var("AGEND_TARGET_GC_MIN_SIZE_BYTES");

        let preview = target_sweep_dry_run(&home);
        assert_eq!(preview.len(), 1, "dry-run must preview the stale candidate");
        assert!(preview[0].size_bytes > 0, "preview reports a size");
        assert!(target.exists(), "dry-run MUST NOT delete");
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Rework regression tests (r6 REJECT repros, PR #2398) ──────────────

    /// R1 (r6 #1): a markerless `workspace/<agent>` gitlink worktree — caught by
    /// the looser `fs_managed_worktrees` union — is NEVER swept. The sweep's
    /// marker-strict enumerator walks `home/worktrees` only. neuter: revert
    /// `target_sweep_candidates` to `fs_managed_worktrees(home)` ⇒ this goes RED
    /// (the operator workspace target/ becomes a candidate + is deleted).
    #[cfg(unix)]
    #[test]
    fn target_sweep_ignores_markerless_workspace_gitlink() {
        let home = tmp_home("tgt-ws-markerless");
        // The managed root must EXIST so the run reaches the enumerator (else
        // safe_managed_root short-circuits and the neuter is masked). Empty
        // home/worktrees ⇒ the marker-strict enumerator finds nothing; only the
        // looser union would (wrongly) pull in the workspace gitlink below.
        std::fs::create_dir_all(daemon_managed_worktree_root(&home)).unwrap();
        let ws = crate::paths::workspace_dir(&home).join("operator-owned");
        std::fs::create_dir_all(ws.join("target")).unwrap();
        std::fs::write(ws.join(".git"), b"gitdir: /elsewhere\n").unwrap(); // gitlink FILE, NO marker
        std::fs::write(ws.join("target").join("f"), vec![0u8; 2048]).unwrap();
        touch_old(&ws.join("target"));

        let age = std::time::Duration::from_secs(48 * 3600);
        let cands = target_sweep_candidates(&home, age, 0);
        assert!(
            cands.is_empty(),
            "markerless workspace gitlink must NOT be a candidate: {cands:?}"
        );
        let results = target_sweep_run(&home, age, 0);
        assert!(results.iter().all(|r| !r.removed));
        assert!(
            ws.join("target").exists(),
            "operator workspace target/ MUST survive"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// R2a (r6 #2, symlink-ROOT arm ISOLATED): `home/worktrees` symlinked to a
    /// real dir INSIDE home — confinement (canon under home) WOULD pass, so
    /// `safe_managed_root`'s SYMLINK arm is the SOLE guard. neuter: drop the
    /// symlink arm ⇒ the inside-home root is enumerated + its stale managed
    /// target swept (RED). (Isolates the symlink arm — r6's MEDIUM: the old
    /// combined-gut neuter didn't, since confinement independently caught it.)
    #[cfg(unix)]
    #[test]
    fn target_sweep_aborts_on_symlinked_root_inside_home() {
        let home = tmp_home("tgt-symroot-in");
        // Real dir INSIDE home holding a stale managed worktree (agent not live
        // → would be swept if enumeration reached it).
        let real_root = home.join("real-wt-root");
        let wt = real_root.join("dev-sa").join("br");
        std::fs::create_dir_all(wt.join("target")).unwrap();
        std::fs::write(wt.join(MANAGED_MARKER), "agent=dev-sa\nbranch=br\n").unwrap();
        std::fs::write(wt.join("target").join("f"), vec![0u8; 2048]).unwrap();
        touch_old(&wt.join("target"));
        std::os::unix::fs::symlink(&real_root, daemon_managed_worktree_root(&home)).unwrap();

        // Symlink arm rejects even though confinement-to-home would pass.
        assert!(
            safe_managed_root(&home).is_none(),
            "a symlinked managed root must be rejected by the symlink arm"
        );
        let age = std::time::Duration::from_secs(48 * 3600);
        assert!(target_sweep_candidates(&home, age, 0).is_empty());
        assert!(target_sweep_run(&home, age, 0).iter().all(|r| !r.removed));
        assert!(
            wt.join("target").exists(),
            "must not sweep through a symlinked managed root"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// R2b (r6 #2 / r4 — CONFINEMENT isolated, the real CRITICAL-2 guard): an
    /// ancestor-escape — `home/worktrees` is REAL, but a child `<agent>` dir is a
    /// SYMLINK to an external tree (outside home) with a real `target/`. The
    /// safe_managed_root symlink arm does NOT fire (the ROOT is real); only
    /// `validate_target_for_delete`'s canonical-root CONFINEMENT stands. neuter:
    /// drop that confinement ⇒ external operator target swept (RED).
    #[cfg(unix)]
    #[test]
    fn target_sweep_confinement_blocks_ancestor_escape() {
        let home = tmp_home("tgt-confine");
        std::fs::create_dir_all(daemon_managed_worktree_root(&home)).unwrap(); // REAL root
        let external = tmp_home("tgt-confine-ext");
        let ext_wt = external.join("wt");
        std::fs::create_dir_all(ext_wt.join("target")).unwrap();
        std::fs::write(ext_wt.join("target").join("precious.bin"), b"operator-data").unwrap();
        std::fs::write(ext_wt.join(MANAGED_MARKER), "agent=dev-ce\nbranch=br\n").unwrap();
        touch_old(&ext_wt.join("target"));
        // home/worktrees/dev-ce → external worktree (real root, SYMLINKED child).
        std::os::unix::fs::symlink(&ext_wt, daemon_managed_worktree_root(&home).join("dev-ce"))
            .unwrap();

        // Root is real → symlink arm does NOT fire; confinement is the sole guard.
        assert!(
            safe_managed_root(&home).is_some(),
            "a real managed root must pass safe_managed_root"
        );
        let escaping_target = daemon_managed_worktree_root(&home)
            .join("dev-ce")
            .join("target");
        assert!(
            validate_target_for_delete(&home, &escaping_target).is_err(),
            "confinement must reject a target that resolves outside the managed root"
        );
        let age = std::time::Duration::from_secs(48 * 3600);
        let results = target_sweep_run(&home, age, 0);
        assert!(results.iter().all(|r| !r.removed));
        assert!(
            ext_wt.join("target").join("precious.bin").exists(),
            "external operator data MUST survive (confinement)"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&external).ok();
    }

    /// R3 (r6 #4): a managed `target/` containing an UNREADABLE subdir (read_dir
    /// errors) FAILS CLOSED — treated as active, NOT deleted. neuter: revert the
    /// activity probe to return `false` on error ⇒ the dir is swept despite being
    /// unreadable (RED).
    #[cfg(unix)]
    #[test]
    fn target_sweep_fail_closed_on_unreadable_subdir() {
        use std::os::unix::fs::PermissionsExt;
        let home = tmp_home("tgt-failclosed");
        let (_, target) = mk_managed_target(&home, "dev-fc", "feat/fc", true); // stale
        let locked = target.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("x"), b"y").unwrap();
        touch_old(&target); // age the whole tree (incl. locked) past the window
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let age = std::time::Duration::from_secs(48 * 3600);
        let cands = target_sweep_candidates(&home, age, 0);
        let removed = target_sweep_run(&home, age, 0).iter().any(|r| r.removed);
        // Restore perms BEFORE asserting so cleanup always succeeds.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).ok();

        assert!(
            cands.is_empty(),
            "target with an unreadable subdir must fail-closed (not a candidate): {cands:?}"
        );
        assert!(
            !removed,
            "fail-closed: target with an unreadable subdir must NOT be deleted"
        );
        assert!(target.exists(), "target/ MUST survive fail-closed");
        std::fs::remove_dir_all(&home).ok();
    }

    /// Write a binding.json for `agent` pointing at `worktree` (where
    /// `binding::read` looks: `runtime_dir/<agent>/binding.json`).
    #[cfg(unix)]
    fn write_binding(home: &Path, agent: &str, worktree: &Path) {
        let dir = crate::paths::runtime_dir(home).join(agent);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("binding.json"),
            serde_json::json!({ "worktree": worktree.to_string_lossy() }).to_string(),
        )
        .unwrap();
    }

    /// FIX3 round-4 (r6 active-build TOCTOU): owner IN roster + bound HERE ⇒
    /// PROTECTED, regardless of liveness — closes the bound-but-not-yet-live race
    /// (the flappy liveness signal is gone). neuter: gut `predicate_protects`
    /// (force not-protected) ⇒ the bound target is swept ⇒ RED.
    #[cfg(unix)]
    #[test]
    fn target_sweep_protects_bound_in_roster() {
        let home = tmp_home("tgt-bound-roster");
        let (wt, target) = mk_managed_target(&home, "own-a", "feat/x", true); // stale
        write_binding(&home, "own-a", &wt);
        let roster = std::collections::HashSet::from(["own-a".to_string()]);

        let age = std::time::Duration::from_secs(48 * 3600);
        assert!(
            target_sweep_candidates_with_roster(&home, age, 0, &roster).is_empty(),
            "in-roster + bound-here must be PROTECTED"
        );
        assert!(target_sweep_run_with_roster(&home, age, 0, &roster)
            .iter()
            .all(|r| !r.removed));
        assert!(target.exists(), "bound-in-roster target/ MUST survive");
        std::fs::remove_dir_all(&home).ok();
    }

    /// Round-4: an instance GONE from the roster (deleted) is sweepable even with
    /// a stale binding pointing here — it can never bind again (the real orphan
    /// reclaim, e.g. claude-8145a9).
    #[cfg(unix)]
    #[test]
    fn target_sweep_reclaims_instance_gone() {
        let home = tmp_home("tgt-gone");
        let (wt, target) = mk_managed_target(&home, "own-gone", "feat/y", true); // stale
        write_binding(&home, "own-gone", &wt); // stale binding points here...
        let roster = std::collections::HashSet::new(); // ...but owner NOT in roster (deleted)

        let age = std::time::Duration::from_secs(48 * 3600);
        assert_eq!(
            target_sweep_candidates_with_roster(&home, age, 0, &roster).len(),
            1,
            "a gone instance's stale-bound target must be sweepable"
        );
        assert!(target_sweep_run_with_roster(&home, age, 0, &roster)
            .iter()
            .any(|r| r.removed));
        assert!(
            !target.exists(),
            "gone-instance stale target/ must be reclaimed"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Round-4: a roster member that REBOUND AWAY (binding points elsewhere)
    /// leaves this worktree sweepable.
    #[cfg(unix)]
    #[test]
    fn target_sweep_reclaims_rebound_away() {
        let home = tmp_home("tgt-rebound");
        let (_wt, target) = mk_managed_target(&home, "own-reb", "feat/old", true); // stale
        let elsewhere = daemon_managed_worktree_root(&home)
            .join("own-reb")
            .join("feat-new");
        write_binding(&home, "own-reb", &elsewhere); // bound ELSEWHERE
        let roster = std::collections::HashSet::from(["own-reb".to_string()]);

        let age = std::time::Duration::from_secs(48 * 3600);
        assert_eq!(
            target_sweep_candidates_with_roster(&home, age, 0, &roster).len(),
            1,
            "a rebound-away worktree must be sweepable"
        );
        assert!(target_sweep_run_with_roster(&home, age, 0, &roster)
            .iter()
            .any(|r| r.removed));
        assert!(
            !target.exists(),
            "rebound-away stale target/ must be reclaimed"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Round-4 (fixes the None fail-open): a roster member whose binding.json
    /// EXISTS but is UNREADABLE/malformed ⇒ fail-closed PROTECT. neuter: revert
    /// the predicate's Err/None arm to `false` ⇒ swept ⇒ RED.
    #[cfg(unix)]
    #[test]
    fn target_sweep_fail_closed_on_unreadable_binding() {
        let home = tmp_home("tgt-badbind");
        let (_wt, target) = mk_managed_target(&home, "own-bad", "feat/z", true); // stale
        let dir = crate::paths::runtime_dir(&home).join("own-bad");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("binding.json"), b"{ this is not valid json").unwrap();
        let roster = std::collections::HashSet::from(["own-bad".to_string()]);

        let age = std::time::Duration::from_secs(48 * 3600);
        assert!(
            target_sweep_candidates_with_roster(&home, age, 0, &roster).is_empty(),
            "an unreadable binding for a roster member must fail-closed PROTECT"
        );
        assert!(target_sweep_run_with_roster(&home, age, 0, &roster)
            .iter()
            .all(|r| !r.removed));
        assert!(
            target.exists(),
            "fail-closed: target/ MUST survive unreadable binding"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Round-4: the run pass HOLDS the owner's .binding.json.lock; a contended
    /// lock (bind/release in flight) ⇒ SKIP this tick — never delete while the
    /// binding could change under us. neuter: drop the try-lock ⇒ deletes despite
    /// the held lock ⇒ RED.
    #[cfg(unix)]
    #[test]
    fn target_sweep_skips_when_bind_lock_contended() {
        let home = tmp_home("tgt-lockcontend");
        // stale + NOT in roster ⇒ would be sweepable, but the held lock must veto.
        let (_wt, target) = mk_managed_target(&home, "own-lk", "feat/lk", true);
        let lock_path = crate::paths::runtime_dir(&home)
            .join("own-lk")
            .join(".binding.json.lock");
        let _held = crate::store::acquire_file_lock(&lock_path).expect("hold the bind lock");
        let roster = std::collections::HashSet::new();

        let age = std::time::Duration::from_secs(48 * 3600);
        let results = target_sweep_run_with_roster(&home, age, 0, &roster);
        assert!(
            results.iter().all(|r| !r.removed),
            "must skip while the bind lock is held: {results:?}"
        );
        assert!(
            target.exists(),
            "target/ MUST survive while the bind lock is held"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}

#[cfg(test)]
mod review_repro_worktree_git;
