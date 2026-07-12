//! Git worktree management — create, reuse, prune.
//!
//! Rule: if working_directory is set and is a git repo, create a worktree.
//!
//! Sprint 57 Wave 4 (#546 Item 4) — worktrees live external to the
//! source repo per operator-approved Option A. Canonical layout:
//!   `$AGEND_HOME/worktrees/<agent>/<branch>/`
//! (e.g. `~/.agend/worktrees/dev/feat/track-x/`). `worktree_path` is
//! the single source of truth for this layout; all production code
//! paths (lease, create, release, gc, list_residual) route through it.

use crate::agent_ops::validate_branch;
use std::path::{Path, PathBuf};

/// PR-D · D1: the unified `terminal_disposition` classifier seam (spike §1).
/// Additive — no production caller yet; D2–D4 wire the call sites.
pub(crate) mod disposition;

/// Sprint 57 Wave 4 (#546 Item 4) canonical worktree path:
/// `$AGEND_HOME/worktrees/<agent>/<branch>/`. Single source of truth
/// — every site that needs to know "where does agent X's branch Y
/// worktree live?" routes through this helper. Branch names with `/`
/// (e.g. `feat/foo`) become nested dirs naturally; `validate_branch`
/// already rejects path-traversal characters at the daemon API
/// boundary.
pub fn worktree_path(home: &Path, agent: &str, branch: &str) -> PathBuf {
    home.join("worktrees").join(agent).join(branch)
}

/// Info about a created worktree.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct WorktreeInfo {
    /// Actual working directory (the worktree path).
    pub path: PathBuf,
    /// Original repo root.
    pub source_repo: PathBuf,
    /// Branch name.
    pub branch: String,
}

/// Check if a directory is a git repo (has .git).
pub fn is_git_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Check if a git repo has at least one commit (valid HEAD).
fn has_commits(repo_dir: &Path) -> bool {
    // W1.2: LOCAL bool check via git_ok (was bypass+`.map(success).unwrap_or(false)`
    // — exactly what git_ok absorbs, plus the LOCAL_GIT_TIMEOUT bound).
    crate::git_helpers::git_ok(repo_dir, &["rev-parse", "HEAD"])
}

/// Create a worktree for an instance. Returns WorktreeInfo if created,
/// None if not a git repo.
///
/// - If worktree already exists, reuses it.
/// - Branch name: custom_branch or "agend/{instance_name}".
/// - Worktree path (Sprint 57 Wave 4 #546 Item 4):
///   `{home}/worktrees/{instance_name}/{branch}/` — external to
///   source_repo per operator-approved Option A. The pre-Wave-4
///   layout `{repo}/.worktrees/{instance_name}/` is no longer
///   created; existing worktrees there are left alone for the
///   operator to clean up manually (a startup migration sweep
///   surfaces them via warning).
pub fn create(
    home: &Path,
    repo_dir: &Path,
    instance_name: &str,
    custom_branch: Option<&str>,
) -> Option<WorktreeInfo> {
    if !is_git_repo(repo_dir) {
        return None;
    }

    // Defense-in-depth (xcut-concurrency F4): validate the AGENT segment too, not
    // just the branch. `worktree_path` joins `instance_name` into the pool path,
    // so an unvalidated `..` agent segment paired with a VALID custom branch would
    // traverse OUT of `<home>/worktrees/`. Mirror the `validate_branch` guard
    // below; `agent::validate_name` rejects `/` and `.` (hence `..`).
    if crate::agent::validate_name(instance_name).is_err() {
        tracing::warn!(
            instance = %instance_name,
            "invalid instance name, rejecting worktree creation"
        );
        return None;
    }

    // Empty repo (git init without any commits) → HEAD is invalid.
    // Worktree creation requires at least one commit.
    if !has_commits(repo_dir) {
        tracing::info!(repo = %repo_dir.display(), "empty repo, creating initial commit for worktree support");
        // W1.2: LOCAL bool result via git_ok (was bypass+`.map(success).unwrap_or(false)`).
        let ok = crate::git_helpers::git_ok(
            repo_dir,
            &[
                "-c",
                "user.name=agend-terminal",
                "-c",
                "user.email=agend@localhost",
                "commit",
                "--allow-empty",
                "-m",
                "init (agend-terminal)",
            ],
        );
        if !ok {
            tracing::warn!(repo = %repo_dir.display(), "failed to create initial commit in empty repo");
            return None;
        }
    }

    let branch = custom_branch
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("agend/{instance_name}"));

    if !validate_branch(&branch) {
        tracing::warn!(branch = %branch, "invalid branch name, rejecting worktree creation");
        return None;
    }

    // Sprint 57 Wave 4 (#546 Item 4): canonical path is now external
    // to source_repo at `$AGEND_HOME/worktrees/<agent>/<branch>/`.
    let wt_dir = worktree_path(home, instance_name, &branch);

    // Already exists — verify actual HEAD before reuse.
    // P0-1.6: pre-fix this branch echoed `branch` back without verifying the
    // worktree's actual HEAD. dispatch_auto_bind_lease therefore could not
    // distinguish "reuse on same branch" (idempotent) from "reuse on different
    // branch" (lease conflict). Smoke test 2 caught it: a second dispatch with
    // a different branch silently passed and the message was delivered.
    if wt_dir.exists() {
        // W1.2: LOCAL value via git_cmd. git_cmd returns trimmed stdout on success
        // (matching the prior `.trim().to_string()`) and Err on spawn/non-zero
        // (matching both prior None branches). Detached HEAD → exit 0 + empty
        // stdout → Ok("") → Some("") (byte-identical to the prior Some("")).
        let actual = crate::git_helpers::git_cmd(&wt_dir, &["branch", "--show-current"]).ok();
        if actual.as_deref() != Some(branch.as_str()) {
            // #2010 2b: the worktree exists at this branch-keyed path but its HEAD
            // drifted off the requested branch — most commonly a DETACHED HEAD,
            // where `git branch --show-current` yields `Some("")` (not `None`),
            // e.g. a reviewer that did a detached `repo checkout` for inspection.
            // Pre-fix this ALWAYS returned None → LeaseConflict, forcing a manual
            // release before the lead could re-dispatch to the same branch.
            // Clean-guarded reattach: when the worktree has no uncommitted changes
            // we check the requested branch back out and REUSE the worktree; a
            // DIRTY drift still conflicts (protects in-flight review WIP — the
            // reviewer's normal detached form is only reattached at the moment a
            // NEW lease for this branch is requested, i.e. right here).
            if has_uncommitted_changes(&wt_dir) {
                tracing::warn!(
                    instance = instance_name,
                    requested = %branch,
                    actual = ?actual,
                    path = %wt_dir.display(),
                    "lease conflict: worktree drifted off the requested branch and is dirty — rejecting (protecting WIP)"
                );
                return None;
            }
            match checkout_branch(&wt_dir, &branch) {
                Ok(()) => {
                    tracing::info!(
                        instance = instance_name,
                        requested = %branch,
                        previous = ?actual,
                        path = %wt_dir.display(),
                        "reattached clean drifted/detached worktree to requested branch — reusing (#2010 2b)"
                    );
                    // #2115: `checkout_branch` (git switch) already lands the
                    // tree on the branch tip from a clean drift, but force-sync
                    // for uniformity with the same-branch path below — a clean
                    // tree is a no-op (see sync_worktree_to_head early return).
                    sync_worktree_to_head(&wt_dir);
                    return Some(WorktreeInfo {
                        path: wt_dir,
                        source_repo: repo_dir.to_path_buf(),
                        branch,
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        instance = instance_name,
                        requested = %branch,
                        actual = ?actual,
                        path = %wt_dir.display(),
                        error = %e,
                        "lease conflict: clean reattach to requested branch failed — rejecting"
                    );
                    return None;
                }
            }
        }
        tracing::info!(
            instance = instance_name,
            path = %wt_dir.display(),
            branch = %branch,
            "reusing existing worktree (branch verified)"
        );
        // #2115: the branch ref may have been fast-forwarded (#869 update-ref)
        // since this worktree was last synced, leaving a stale (dirty) tree on
        // hand-off. Force-sync to HEAD before returning so the new occupant gets
        // a clean tree at the current SHA.
        sync_worktree_to_head(&wt_dir);
        return Some(WorktreeInfo {
            path: wt_dir,
            source_repo: repo_dir.to_path_buf(),
            branch,
        });
    }

    // Worktree's parent dir must exist before `git worktree add`
    // runs against it. Branches with `/` (e.g. `feat/foo`) become
    // nested dirs naturally via create_dir_all.
    if let Some(parent) = wt_dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Try creating worktree: first with -b (new branch), fallback without -b (existing branch).
    // #2128: both attempts route through the bounded `git_cmd` (AGEND_GIT_BYPASS +
    // LOCAL_GIT_TIMEOUT + process-group-kill) so a `worktree add` wedged on
    // `.git/index.lock` contention fails in 60s instead of hanging forever. The
    // nested stderr-substring dispatch ("already exists" / "is already checked out")
    // is byte-identical on `git_cmd`'s `Err(NonZero { stderr, .. })`: its stderr is
    // `.trim()`ed but the interior substring survives trim. The ONLY behavioural
    // change is the timeout bound — a wedged add surfaces as `Err(Spawn(TimedOut))`
    // → the lock-contention warn below (DP2; see #2128 / #1897).
    use crate::git_helpers::{git_cmd, GitError};
    match git_cmd(
        repo_dir,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            &wt_dir.display().to_string(),
        ],
    ) {
        Ok(_) => {
            tracing::info!(
                instance = instance_name,
                path = %wt_dir.display(),
                branch = %branch,
                "created worktree"
            );
            // #1137: write .agend-managed marker immediately after successful
            // checkout to prevent orphan dirs if process dies before caller writes it.
            let _ = std::fs::write(
                wt_dir.join(".agend-managed"),
                format!(
                    "agent={instance_name}\nbranch={branch}\nleased_at={}\n",
                    chrono::Utc::now().to_rfc3339()
                ),
            );
            // Fresh worktree: `git worktree add` copies gitlinks + .gitmodules
            // but does NOT populate submodule content — init recursively here.
            init_submodules_after_create(&wt_dir);
            Some(WorktreeInfo {
                path: wt_dir,
                source_repo: repo_dir.to_path_buf(),
                branch,
            })
        }
        // #781 Piece 2 (Bug B): the prior `o.status.code() == Some(128)` gate was
        // too strict. `git worktree add -b <existing-branch>` can exit 255 (not 128)
        // when the failure surfaces after the "Preparing worktree (new branch …)"
        // progress line (macOS git 2.42+, #781 spike: exit 255, stderr "fatal: a
        // branch named '…' already exists"). Exit codes from `worktree add` are not
        // contracted in any released git manpage; the stderr substring is the
        // load-bearing semantic signal — dispatch on it alone. Across git versions /
        // locales the wording stays stable (English) for the duplicate-branch
        // ("already exists") and cross-worktree-checkout ("is already checked out")
        // cases; the exit-code drift is version-specific.
        Err(GitError::NonZero { stderr, .. })
            if stderr.contains("already exists") || stderr.contains("is already checked out") =>
        {
            // Existing-branch fallback (no -b): same bounded `git_cmd`.
            match git_cmd(
                repo_dir,
                &["worktree", "add", &wt_dir.display().to_string(), &branch],
            ) {
                Ok(_) => {
                    tracing::info!(
                        instance = instance_name,
                        %branch,
                        "created worktree on existing branch"
                    );
                    // #1137: write marker immediately (same as primary path above).
                    let _ = std::fs::write(
                        wt_dir.join(".agend-managed"),
                        format!(
                            "agent={instance_name}\nbranch={branch}\nleased_at={}\n",
                            chrono::Utc::now().to_rfc3339()
                        ),
                    );
                    init_submodules_after_create(&wt_dir);
                    Some(WorktreeInfo {
                        path: wt_dir,
                        source_repo: repo_dir.to_path_buf(),
                        branch,
                    })
                }
                Err(GitError::NonZero { stderr, .. }) => {
                    tracing::warn!(
                        instance = instance_name,
                        error = %stderr,
                        "worktree creation failed"
                    );
                    None
                }
                Err(GitError::Spawn(e)) => {
                    warn_worktree_add_spawn_err(&e);
                    None
                }
            }
        }
        Err(GitError::NonZero { stderr, .. }) => {
            tracing::warn!(instance = instance_name, error = %stderr, "worktree creation failed");
            None
        }
        Err(GitError::Spawn(e)) => {
            warn_worktree_add_spawn_err(&e);
            None
        }
    }
}

/// #2128 DP2: log a `git worktree add` spawn error, distinguishing a
/// `LOCAL_GIT_TIMEOUT` wedge (index.lock contention — the hang this migration
/// bounds) from git genuinely missing, so a 60s-timed-out add is observable in
/// logs instead of looking like an anonymous "git not available". Non-timeout
/// spawn failures keep the prior "git not available" wording (byte-identical).
fn warn_worktree_add_spawn_err(e: &std::io::Error) {
    if e.kind() == std::io::ErrorKind::TimedOut {
        tracing::warn!(error = %e, "worktree add timed out (lock contention?)");
    } else {
        tracing::warn!(error = %e, "git not available");
    }
}

/// After a **fresh** `git worktree add`, populate configured submodules.
///
/// `git worktree add` materializes the superproject tree (including
/// `.gitmodules` and gitlink entries) but leaves submodule directories empty.
/// Without `--init --recursive`, builds that depend on nested content
/// (e.g. `vendor/agentic-git`) fail on every fresh daemon-managed worktree.
///
/// Soft-warn policy: optional/private submodules may be unavailable offline —
/// a non-zero update must not turn a previously successful lease into a hard
/// failure. One actionable warn carries worktree path + stderr; caller still
/// returns `WorktreeInfo`.
///
/// Uses [`crate::git_helpers::git_cmd`] (AGEND_GIT_BYPASS + LOCAL_GIT_TIMEOUT).
/// No-op when `.gitmodules` is absent. Reuse-path and deployments are out of scope.
///
/// `-c protocol.file.allow=always` is intentional: git's submodule clone helper
/// ignores the superproject's local `protocol.file.allow` config (security
/// default post-2.38), so hermetic file-path submodule fixtures — and rare
/// local-path submodules — need the command-line override. https/ssh remotes
/// are unaffected (file protocol only).
fn init_submodules_after_create(wt_dir: &Path) {
    if !wt_dir.join(".gitmodules").is_file() {
        return;
    }
    match crate::git_helpers::git_cmd(
        wt_dir,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "--recursive",
        ],
    ) {
        Ok(_) => {
            tracing::info!(
                path = %wt_dir.display(),
                "initialized submodules after worktree create"
            );
        }
        Err(e) => {
            tracing::warn!(
                path = %wt_dir.display(),
                error = %e,
                "submodule update --init --recursive failed after worktree create \
                 (soft-warn: lease still succeeds; nested content may be missing — \
                 run: git -C <worktree> submodule update --init --recursive)"
            );
        }
    }
}

/// #888 affirmative-signal predicate: does this instance opt into a per-agent
/// worktree? `worktree: false` is a hard veto; otherwise `source_repo` OR
/// `git_branch` is the opt-in. Pure (no filesystem) so the truth table can be
/// pinned directly. The base-dir / is_git_repo decision lives in
/// [`resolve_auto_worktree`] (the full gate).
pub fn wants_auto_worktree(resolved: &crate::fleet::ResolvedInstance) -> bool {
    if resolved.worktree == Some(false) {
        return false;
    }
    resolved.source_repo.is_some() || resolved.git_branch.is_some()
}

/// #1858: single source of truth for the per-agent auto-worktree decision,
/// shared by the boot/reload path (`bootstrap::agent_resolve::resolve_one`) and
/// the live-spawn path (`app::pane_factory`). Returns the redirected
/// working directory (a freshly-created worktree path) when the instance wants
/// one, or `None` to keep `resolved.working_directory` as-is.
///
/// The gate is the PERSISTED intent only:
/// - [`wants_auto_worktree`] (the `worktree:false` veto + `source_repo` /
///   `git_branch` opt-in signal).
/// - the base dir must be a real git repo **that the operator/deploy explicitly
///   pointed `working_directory` at** — the daemon-managed default
///   `workspace_dir(home)/<name>` is NEVER auto-worktree'd.
///
/// That last clause kills the #1858 drift: `instructions::ensure_project_root`
/// `git init`s the workspace dir tail-side of this decision, which used to flip
/// the old `is_git_repo(base_dir)` gate between launch 1 (plain) and launch 2
/// (worktree) — silently redirecting the dir into a worktree of the *empty,
/// git-init'd workspace* (not the real `source_repo`) on every restart/reboot.
/// Pinning the gate to "explicit non-default working_directory" makes the
/// decision launch-idempotent: an agent that started plain stays plain, and an
/// agent the operator pointed at a real repo (or a branch-mode deploy whose
/// `working_directory` is already a repo) still gets its worktree.
pub fn resolve_auto_worktree(
    home: &Path,
    name: &str,
    resolved: &crate::fleet::ResolvedInstance,
) -> Option<PathBuf> {
    if !wants_auto_worktree(resolved) {
        return None;
    }
    let base_dir = resolved.working_directory.as_ref()?;
    // #1858: the daemon-managed default workspace dir is never a legitimate
    // worktree source — it only becomes a "repo" via ensure_project_root's
    // git-init. Skip it regardless of is_git_repo so the decision can't drift
    // across launches.
    // #1919: PREFIX match, not exact. A team-deploy nests the per-instance default
    // under a team subdir (`<home>/workspace/<team>/<instance>`), which the old
    // exact `== workspace_dir/<name>` check missed → the git-init'd default fell
    // through to auto-worktree and broke `claude --continue` session resume on
    // restart. Everything under `<home>/workspace/` is daemon-managed default (a
    // real user working_directory is explicit-config'd OUTSIDE it) — the same
    // invariant `agent_ops::cleanup_working_dir` already encodes via
    // `starts_with(&workspaces)`.
    if base_dir.starts_with(crate::paths::workspace_dir(home)) {
        // #2234 cure-(B): under the gray-rollout flag, the workspace dir BECOMES
        // a canonical worktree (cwd == worktree; cwd PATH stays stable so #1919
        // `claude --continue` session resume survives). #1858's three reasons
        // (git-init drift / launch-idempotency / session resume) become invariants
        // here, not violations: reconcile produces a PROPER canonical worktree
        // (not a git-init standalone), so `ensure_project_root` no-ops and the
        // decision is launch-idempotent. Default OFF → the `None` below →
        // byte-identical to pre-(B). Reconcile failure is fail-safe (→ None → the
        // agent stays a non-worktree under the #2254 drift-WARN net).
        if crate::worktree_pool::workspace_as_worktree_enabled(name) {
            if let Some(src) = resolved.source_repo.as_ref() {
                match crate::worktree_pool::reconcile_workspace_to_worktree(
                    home,
                    name,
                    base_dir,
                    src,
                    resolved.git_branch.as_deref(),
                ) {
                    Ok(path) => return Some(path),
                    Err(e) => {
                        tracing::error!(agent = name, error = %e,
                            "#2234 (B) reconcile failed — falling back to non-worktree workspace (drift-WARN net)");
                        return None;
                    }
                }
            }
        }
        return None;
    }
    if !is_git_repo(base_dir) {
        return None;
    }
    create(home, base_dir, name, resolved.git_branch.as_deref()).map(|info| info.path)
}

/// Run `git worktree prune` on a repo to clean stale worktree entries.
pub fn prune(repo_dir: &Path) {
    if !is_git_repo(repo_dir) {
        return;
    }
    // W1.2: LOCAL prune via git_cmd. The prior 3-way match maps exactly onto
    // git_cmd's Ok / Err(NonZero) / Err(Spawn): Ok(success)→info; Ok(non-zero) with
    // non-empty stderr→warn (git_cmd's NonZero.stderr is already trimmed, matching
    // the prior `stderr.trim()`); spawn-failure→warn. Migrating also adds the
    // LOCAL_GIT_TIMEOUT bound — prune can wedge on a contended `.git/index.lock`,
    // so the 60s bound is a real reliability win (a wedged timeout surfaces in the
    // Err(Spawn) arm as the same "git worktree prune failed" warn).
    use crate::git_helpers::{git_cmd, GitError};
    match git_cmd(repo_dir, &["worktree", "prune"]) {
        Ok(_) => {
            tracing::info!(repo = %repo_dir.display(), "pruned stale worktree entries");
        }
        Err(GitError::NonZero { stderr, .. }) => {
            if !stderr.is_empty() {
                tracing::warn!(warning = %stderr, "worktree prune warning");
            }
        }
        Err(GitError::Spawn(e)) => {
            tracing::warn!(repo = %repo_dir.display(), error = %e, "git worktree prune failed");
        }
    }
}

/// Check if a worktree directory has uncommitted changes.
/// Returns true if `git status --porcelain` produces non-empty output.
pub fn has_uncommitted_changes(worktree_dir: &Path) -> bool {
    // #2128: bounded `git_cmd` (AGEND_GIT_BYPASS + LOCAL_GIT_TIMEOUT) so this
    // safety-critical WIP guard (lease-conflict reattach) can't wedge on a
    // contended `.git/index.lock`. Fail-closed (`Err => true`) is preserved AND
    // strengthened: a spawn failure OR a 60s timeout → assume dirty (don't risk
    // discarding WIP). Byte-identical for a present worktree — porcelain `status`
    // exits 0 there, and trimming can't turn non-empty porcelain output empty (a
    // theoretical non-zero exit, unreachable for a valid worktree, now also maps to
    // fail-closed `true` rather than the prior raw-bytes `false`).
    crate::git_helpers::git_cmd(worktree_dir, &["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(true)
}

// ── #2158-adjacent: dirty-WIP preservation on MANUAL worktree release ──────
//
// The daemon's AUTO-release already refuses to remove a dirty worktree
// (auto_release.rs SkipDirtyWorktree). The two MANUAL release paths —
// `worktree_pool::release_full` (release_worktree force:false) and
// `mcp::handlers::force_release::rebase_clean_self` (force:true) — removed a
// dirty worktree UNCONDITIONALLY, silently losing uncommitted WIP. Before that
// destructive removal we snapshot the WIP into a durable git ref that outlives
// the worktree dir, and notify the operator with a one-line recovery command.
// A clean worktree is a no-op → zero behaviour change.

/// Namespace for WIP-recovery refs. Each ref is
/// `refs/agend/recovery/<branch>/<UTC YYYYMMDDTHHMMSSZ>` and points at a commit
/// whose tree is the WORKING-tree snapshot (tracked + untracked), parented on
/// `HEAD`. When the STAGED index differed from BOTH `HEAD` and the working tree,
/// that staged tree is captured as a distinct second parent, recoverable at
/// `<ref>^2` (see [`preserve_dirty_worktree`]). NEVER captures submodule-internal
/// dirt — that case refuses removal (`UnpreservableNestedDirty`) instead of
/// minting a false-"preserved" ref.
pub(crate) const RECOVERY_REF_PREFIX: &str = "refs/agend/recovery";

/// Recovery-ref retention (lead-vetted 2026-07-07: 14-day TTL + at most 3 per
/// branch). Enforced at creation time against the branch's own refs — see
/// [`prune_recovery_refs`].
const RECOVERY_TTL_DAYS: i64 = 14;
const RECOVERY_MAX_PER_BRANCH: usize = 3;

/// Is there WIP in this worktree worth preserving? Any `git status --porcelain`
/// entry that is NOT the daemon's own `.agend-managed` marker counts (porcelain
/// without `--ignored` already excludes gitignored build artifacts). This
/// deliberately differs from [`has_uncommitted_changes`]: a freshly-leased
/// worktree ALWAYS carries the untracked marker, which must NOT trigger a
/// recovery snapshot on an otherwise-clean release (mirrors the marker-exempt
/// rule in `retention::worktrees::maybe_remove_candidate`). Fail-closed
/// (`Err => true`): a status failure attempts preservation rather than risk
/// dropping WIP — a broken git then fails the snapshot gracefully (`None`).
fn worktree_has_preservable_wip(wt_path: &Path) -> bool {
    match crate::git_helpers::git_cmd(wt_path, &["status", "--porcelain"]) {
        Ok(s) => s.lines().any(|line| {
            let path = line.get(3..).map(str::trim).unwrap_or("");
            !path.is_empty() && path != crate::worktree_pool::MANAGED_MARKER
        }),
        Err(_) => true,
    }
}

/// Outcome of a pre-removal WIP-preservation attempt. `#[must_use]` so a release
/// path cannot silently drop it and destroy the worktree regardless — the
/// fail-OPEN bug reviewer4 caught in the first cut: a contended `index.lock`
/// made `git add -A` fail, the ignored `None` let `release_full` proceed, and the
/// dirty untracked WIP evaporated with zero recovery ref. A caller MUST refuse to
/// remove the worktree on [`WipPreservation::Blocked`].
#[must_use]
pub(crate) enum WipPreservation {
    /// No preservable WIP (clean, or only the daemon `.agend-managed` marker) —
    /// safe to remove; zero behaviour change vs a pre-guard release.
    Clean,
    /// WIP was snapshotted to a recovery ref (name logged + surfaced to the
    /// operator inside `preserve_dirty_worktree`) — safe to remove.
    Preserved,
    /// Preservable WIP EXISTS but could NOT be snapshotted (git failure or a
    /// contended index) — the caller MUST NOT remove the worktree, so the operator
    /// can recover the WIP in place. Carries a human-readable reason.
    Blocked(String),
    /// `git status` is dirty, but a non-destructive parent snapshot proves BOTH
    /// the live-index tree AND the working-tree tree equal `HEAD^{tree}` — i.e. the
    /// ONLY residual dirt is a submodule's INTERNAL working tree (unchanged
    /// gitlinks), which a parent recovery ref cannot capture. Minting a ref would
    /// be a false "preserved". Fail-closed: the caller MUST NOT remove the worktree
    /// (the nested WIP lives only in the worktree dir); a serialized, de-duped
    /// actionable notice is emitted so the operator resolves the nested repo in
    /// place. Distinct from `Blocked` (a genuine snapshot FAILURE).
    UnpreservableNestedDirty(String),
}

impl WipPreservation {
    /// The reason the caller MUST refuse to remove the worktree (fail-closed):
    /// preservation FAILED (`Blocked`) OR the dirt is nested-submodule-internal and
    /// unpreservable by a parent snapshot (`UnpreservableNestedDirty`). `None` for
    /// `Clean`/`Preserved` (safe to remove).
    pub(crate) fn blocked_reason(&self) -> Option<&str> {
        match self {
            WipPreservation::Blocked(reason)
            | WipPreservation::UnpreservableNestedDirty(reason) => Some(reason),
            WipPreservation::Clean | WipPreservation::Preserved => None,
        }
    }
}

/// Snapshot a dirty worktree's uncommitted WIP into a durable recovery ref BEFORE
/// a manual release removes the worktree dir. Returns [`WipPreservation`]:
/// `Clean` (nothing to preserve → safe to remove), `Preserved` (WIP captured →
/// safe to remove), `Blocked(reason)` (WIP present but the snapshot genuinely
/// FAILED → the caller MUST NOT remove; fail-closed), or
/// `UnpreservableNestedDirty(reason)` (the ONLY dirt is submodule-internal, which
/// a parent ref cannot capture → refuse + notify so the operator resolves it in
/// place; also fail-closed).
///
/// Mechanism (race-free, untracked-complete, bypass-only — no raw subprocess, and
/// **the LIVE index is never mutated**):
///  - `head_tree` = `HEAD^{tree}`; `index_tree` = `write-tree` of the LIVE index
///    (READ-ONLY — never `add`/`read-tree`/`reset` the live index); `worktree_tree`
///    = the working tree snapshotted through a TEMP index
///    ([`snapshot_worktree_tree`]) so the live index stays byte-identical.
///  - Classification: a dirty `status` where BOTH `index_tree == head_tree` AND
///    `worktree_tree == head_tree` ⟺ the only residual dirt is a submodule's
///    INTERNAL working tree (gitlinks unchanged) — a parent recovery commit would
///    capture nothing, so minting one is a false "preserved" that silently loses
///    the nested WIP on removal. Refuse (`UnpreservableNestedDirty`) instead.
///  - Otherwise PRESERVE: anchor a commit whose tree = `worktree_tree` to a ref in
///    the SHARED object/ref store (survives the worktree-dir removal). When the
///    STAGED tree differs from BOTH HEAD and the working tree, it is captured as a
///    second parent (`ref^2`) so staged-only WIP stays recoverable. Unlike
///    `git stash create` this captures untracked files; unlike `git stash push` it
///    never touches the shared `refs/stash` stack, so two concurrent dirty releases
///    can't cross-contaminate.
pub(crate) fn preserve_dirty_worktree(
    home: &Path,
    agent: &str,
    wt_path: &Path,
    branch: &str,
    sender: Option<&str>,
) -> WipPreservation {
    use crate::git_helpers::git_cmd;
    if branch.is_empty() {
        return WipPreservation::Clean; // unknown branch → nothing to key a recovery ref on
    }
    // Not a LIVE git worktree (a pruned/dangling stale dir — its `.git` gitlink
    // points at a removed gitdir, or there is none) → there is no git WIP to
    // snapshot, so removal is safe. Gate here so the fail-closed WIP path below
    // fires ONLY for a real worktree whose preservation genuinely failed (e.g. a
    // contended `index.lock`), NOT for a stale dir git can't read at all — which
    // would wrongly block the force_release stale-dir cleanup this backs. Our
    // call sites pass `home/worktrees/...` (outside any repo), so rev-parse can't
    // resolve a spurious ancestor `.git`.
    if git_cmd(wt_path, &["rev-parse", "--git-dir"]).is_err() {
        return WipPreservation::Clean;
    }
    if !worktree_has_preservable_wip(wt_path) {
        return WipPreservation::Clean; // clean / marker-only → zero behaviour change
    }
    // Branch tip tree. Err → Blocked (fail-closed): a repo we can't read HEAD^{tree}
    // from is one we can't safely snapshot, so refuse removal.
    let head_tree = match git_cmd(wt_path, &["rev-parse", "HEAD^{tree}"]) {
        Ok(t) if !t.is_empty() => t,
        other => {
            tracing::warn!(agent, branch, ?other,
                "preserve dirty WIP: HEAD^{{tree}} resolve failed — refusing to remove (fail-closed)");
            return WipPreservation::Blocked(format!(
                "`git rev-parse HEAD^{{tree}}` failed: {other:?}"
            ));
        }
    };
    // LIVE index tree — READ-ONLY. `write-tree` reads the current index and writes
    // tree objects WITHOUT mutating the index file (no `add`/`read-tree`/`reset`),
    // so the live index stays byte-identical on every path below. An UNMERGED index
    // makes `write-tree` fail → Blocked (fail-closed).
    let index_tree = match git_cmd(wt_path, &["write-tree"]) {
        Ok(t) if !t.is_empty() => t,
        other => {
            tracing::warn!(agent, branch, ?other,
                "preserve dirty WIP: `write-tree` (live index) failed — refusing to remove (fail-closed)");
            return WipPreservation::Blocked(format!(
                "`git write-tree` (live index) failed: {other:?}"
            ));
        }
    };
    // Working-tree tree via a TEMP index so the LIVE index is byte-untouched.
    let worktree_tree = match snapshot_worktree_tree(wt_path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(agent, branch, error = %e,
                "preserve dirty WIP: working-tree snapshot failed — refusing to remove (fail-closed)");
            return WipPreservation::Blocked(e);
        }
    };
    // Classification: dirty `status`, yet BOTH the live-index tree AND the working
    // tree snapshot equal HEAD ⟺ the only dirt is submodule-internal (gitlinks
    // unchanged) — UNPRESERVABLE by a parent ref. Refuse + emit a serialized,
    // de-duped actionable notice; the caller MUST NOT remove the worktree.
    if index_tree == head_tree && worktree_tree == head_tree {
        let nested = enumerate_nested_dirty(wt_path);
        notify_unpreservable_nested_dirty(home, agent, branch, wt_path, &nested, sender);
        tracing::warn!(agent, branch,
            "preserve dirty WIP: only submodule-internal dirt (unpreservable by a parent ref) — refusing removal (fail-closed)");
        return WipPreservation::UnpreservableNestedDirty(
            "dirty worktree but both live-index and working-tree snapshots == HEAD \
             (only submodule-internal dirt); refused removal to preserve nested WIP in place"
                .into(),
        );
    }
    // PRESERVE. commit-tree needs a committer identity supplied via `-c` so the
    // daemon never depends on ambient user.name/email. Parent = HEAD (branch tip).
    // When the STAGED tree differs from BOTH HEAD and the working tree, snapshot it
    // as a distinct reachable second parent (`ref^2`) so staged-only WIP survives.
    let parent2 = if index_tree != head_tree && index_tree != worktree_tree {
        match git_cmd(
            wt_path,
            &[
                "-c",
                "user.name=agend-recovery",
                "-c",
                "user.email=recovery@agend.local",
                "commit-tree",
                index_tree.as_str(),
                "-p",
                "HEAD",
                "-m",
                "agend recovery: staged index snapshot",
            ],
        ) {
            Ok(c) if !c.is_empty() => Some(c),
            other => {
                tracing::warn!(agent, branch, ?other,
                    "preserve dirty WIP: staged-snapshot `commit-tree` failed — refusing to remove (fail-closed)");
                return WipPreservation::Blocked(format!(
                    "`git commit-tree` (staged snapshot) failed: {other:?}"
                ));
            }
        }
    } else {
        None
    };
    let msg = format!("agend recovery: dirty WIP for {branch} preserved on release");
    let mut commit_args: Vec<&str> = vec![
        "-c",
        "user.name=agend-recovery",
        "-c",
        "user.email=recovery@agend.local",
        "commit-tree",
        worktree_tree.as_str(),
        "-p",
        "HEAD",
    ];
    if let Some(p2) = parent2.as_deref() {
        commit_args.push("-p");
        commit_args.push(p2);
    }
    commit_args.push("-m");
    commit_args.push(&msg);
    let commit = match git_cmd(wt_path, &commit_args) {
        Ok(c) if !c.is_empty() => c,
        other => {
            tracing::warn!(
                agent,
                branch,
                ?other,
                "preserve dirty WIP: `commit-tree` failed — refusing to remove (fail-closed)"
            );
            return WipPreservation::Blocked(format!("`git commit-tree` failed: {other:?}"));
        }
    };
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let ref_name = format!("{RECOVERY_REF_PREFIX}/{branch}/{ts}");
    if let Err(e) = git_cmd(wt_path, &["update-ref", &ref_name, &commit]) {
        tracing::warn!(agent, branch, error = %e,
            "preserve dirty WIP: `update-ref` failed — refusing to remove (fail-closed)");
        return WipPreservation::Blocked(format!("`git update-ref {ref_name}` failed: {e}"));
    }
    tracing::info!(agent, branch, %ref_name, dual_parent = parent2.is_some(),
        "preserve dirty WIP: uncommitted worktree changes snapshotted before manual release");
    prune_recovery_refs(wt_path, branch);
    notify_wip_preserved(home, agent, branch, &ref_name, parent2.is_some(), sender);
    WipPreservation::Preserved
}

/// Bound the recovery-ref set for `branch`: keep at most
/// [`RECOVERY_MAX_PER_BRANCH`] newest and drop any older than
/// [`RECOVERY_TTL_DAYS`]. Enforced at CREATION time against THIS branch's own
/// refs, in the repo the worktree shares — deliberately NOT a periodic
/// retention-tick sweep: recovery refs live in the canonical repo's ref store,
/// which `retention::worktrees` (a `.trash`-directory mtime GC) does not
/// enumerate, and per-branch growth is already bounded to the cap.
///
/// Known gap (accepted, lead-vetted 2026-07-07): a branch dirty-released exactly
/// once keeps its single ref indefinitely — the 14d TTL only re-fires on that
/// branch's NEXT dirty release. The footprint is ≤ cap tiny refs per such branch
/// (each a single small commit object), so this is negligible. If orphan
/// recovery refs ever accumulate in practice, escalate to a periodic
/// repo-registry sweep (enumerate managed repos → prune `refs/agend/recovery/*`
/// by TTL) rather than widening this per-branch creation-time prune.
///
/// Best-effort: a prune failure is logged, never fatal.
pub(crate) fn prune_recovery_refs(git_dir: &Path, branch: &str) {
    let pattern = format!("{RECOVERY_REF_PREFIX}/{branch}/");
    // Timestamp names sort lexically == chronologically → `-refname` == newest-first.
    let listing = match crate::git_helpers::git_cmd(
        git_dir,
        &[
            "for-each-ref",
            "--sort=-refname",
            "--format=%(refname)",
            &pattern,
        ],
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(branch, error = %e, "prune recovery refs: for-each-ref failed");
            return;
        }
    };
    let refs: Vec<&str> = listing.lines().filter(|l| !l.is_empty()).collect();
    let cutoff = chrono::Utc::now() - chrono::Duration::days(RECOVERY_TTL_DAYS);
    for (idx, ref_name) in refs.iter().enumerate() {
        let over_cap = idx >= RECOVERY_MAX_PER_BRANCH;
        if over_cap || recovery_ref_expired(ref_name, cutoff) {
            if let Err(e) = crate::git_helpers::git_cmd(git_dir, &["update-ref", "-d", ref_name]) {
                tracing::warn!(%ref_name, error = %e, "prune recovery refs: delete failed");
            }
        }
    }
}

/// Parse the trailing `YYYYMMDDTHHMMSSZ` segment of a recovery ref and report
/// whether it predates `cutoff`. An unparseable name is treated as NOT expired
/// (fail-safe — never delete WIP we can't date; the per-branch cap still bounds
/// growth).
fn recovery_ref_expired(ref_name: &str, cutoff: chrono::DateTime<chrono::Utc>) -> bool {
    let Some(ts) = ref_name.rsplit('/').next() else {
        return false;
    };
    match chrono::NaiveDateTime::parse_from_str(ts, "%Y%m%dT%H%M%SZ") {
        Ok(naive) => naive.and_utc() < cutoff,
        Err(_) => false,
    }
}

/// Resolve the recipient for a WIP-preserved notice: the release's CALLER when
/// known (`sender`), else the agent's team orchestrator, else the operator inbox
/// (`general`) as a last resort — never a hardcoded recipient (#t-…61315-2 Bug1).
fn wip_notice_recipient(home: &Path, agent: &str, sender: Option<&str>) -> String {
    sender
        .map(str::to_string)
        .or_else(|| crate::teams::find_team_for(home, agent).and_then(|t| t.orchestrator))
        .unwrap_or_else(|| "general".to_string())
}

/// The WIP-preserved notice BODY. The `[system:release_dirty_wip_preserved]`
/// marker is added by the notify layer (`NotifySource::System`); the body must
/// NOT embed a second (double-prefix bug, #t-…61315-2 Bug2). When `dual_parent`
/// (the staged index tree differed from BOTH HEAD and the working tree) the
/// recovery commit carries the staged snapshot as a second parent — surface where
/// to recover it (`<ref>^2`).
fn wip_preserved_notice(agent: &str, branch: &str, ref_name: &str, dual_parent: bool) -> String {
    let staged = if dual_parent {
        format!(
            "\nThe staged (index) state is preserved separately at `{ref_name}^2` \
                 (inspect: git show {ref_name}^2)."
        )
    } else {
        String::new()
    };
    format!(
        "Agent `{agent}` released a DIRTY worktree \
         for branch `{branch}`; its uncommitted WIP (tracked + untracked) was snapshotted \
         to recovery ref:\n  {ref_name}\nRecover it from the canonical repo with:\n  \
         git worktree add ../wip-recover {ref_name}\n(or inspect: git log -p {ref_name}).{staged} \
         Auto-pruned after {RECOVERY_TTL_DAYS}d / max {RECOVERY_MAX_PER_BRANCH} per branch. \
         #2158-adjacent."
    )
}

/// Notify the release's CALLER that a dirty worktree's WIP was preserved, with a
/// one-line recovery command. Recipient + body per the pure helpers above.
/// Best-effort.
fn notify_wip_preserved(
    home: &Path,
    agent: &str,
    branch: &str,
    ref_name: &str,
    dual_parent: bool,
    sender: Option<&str>,
) {
    let recipient = wip_notice_recipient(home, agent, sender);
    let text = wip_preserved_notice(agent, branch, ref_name, dual_parent);
    let source = crate::inbox::NotifySource::System("release_dirty_wip_preserved");
    crate::inbox::notify_agent(home, &recipient, &source, &text);
}

/// Monotonic sequence for per-worktree TEMP artifacts (temp index files + atomic
/// marker rename staging). `pid`-qualified names already avoid cross-process
/// collision; this avoids intra-process reuse across concurrent preserves.
static PRESERVE_TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Build the working-tree tree (tracked + untracked, respecting `.gitignore`) in a
/// TEMP index so the LIVE index is byte-unchanged. Every op runs with
/// `GIT_INDEX_FILE=<temp>` + `AGEND_GIT_BYPASS=1`, cwd = `wt_path`, bounded by
/// [`LOCAL_GIT_TIMEOUT`] via [`spawn_group_bounded`]. The temp index is removed on
/// EVERY return path (success and error). Err carries a human-readable reason the
/// caller maps to `Blocked`.
fn snapshot_worktree_tree(wt_path: &Path) -> Result<String, String> {
    let git_dir = crate::git_helpers::git_cmd(wt_path, &["rev-parse", "--absolute-git-dir"])
        .map_err(|e| format!("snapshot_worktree_tree: rev-parse --absolute-git-dir failed: {e}"))?;
    let seq = PRESERVE_TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = Path::new(&git_dir).join(format!(
        "index.agend-preserve.{}.{}",
        std::process::id(),
        seq
    ));
    // Run a git op against the TEMP index (bypass + bounded). Never touches the
    // live index (which lives at `<git_dir>/index`).
    let run = |args: &[&str], label: &str| -> Result<std::process::Output, String> {
        let mut cmd = std::process::Command::new("git");
        cmd.args(args)
            .current_dir(wt_path)
            .env("GIT_INDEX_FILE", &temp)
            .env("AGEND_GIT_BYPASS", "1");
        crate::git_helpers::spawn_group_bounded(cmd, label, crate::git_helpers::LOCAL_GIT_TIMEOUT)
            .map_err(|e| format!("snapshot_worktree_tree: {label} spawn failed: {e}"))
    };
    // Inner closure so the single `remove_file` below covers EVERY exit path.
    let result = (|| -> Result<String, String> {
        for (args, label) in [
            (&["read-tree", "HEAD"][..], "read-tree HEAD (temp index)"),
            (&["add", "-A"][..], "add -A (temp index)"),
        ] {
            let out = run(args, label)?;
            if !out.status.success() {
                return Err(format!(
                    "snapshot_worktree_tree: {label} failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
        }
        let out = run(&["write-tree"], "write-tree (temp index)")?;
        if !out.status.success() {
            return Err(format!(
                "snapshot_worktree_tree: write-tree failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let tree = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if tree.is_empty() {
            return Err("snapshot_worktree_tree: write-tree returned empty".to_string());
        }
        Ok(tree)
    })();
    let _ = std::fs::remove_file(&temp);
    result
}

/// A parsed `git status --porcelain=v2 -z` record we care about: its display
/// `token` (the XY field / `??` / `!!`), `path`, and the submodule `<sub>` field.
struct V2Entry {
    token: String,
    path: String,
    sub: String,
}

impl V2Entry {
    /// The `<sub>` field is `N...` for a non-submodule and `S<c><m><u>` for a
    /// submodule (`c`=commit/gitlink changed, `m`=modified tracked content,
    /// `u`=untracked content).
    fn is_submodule(&self) -> bool {
        self.sub.as_bytes().first() == Some(&b'S')
    }

    /// A submodule with INTERNAL working-tree dirt (`m` or `u`) — the kind a parent
    /// ref cannot preserve. A pure gitlink move (`SC..`) is NOT this (it IS
    /// preservable), so it is excluded.
    fn dirty_submodule(&self) -> bool {
        let b = self.sub.as_bytes();
        b.first() == Some(&b'S') && b.len() >= 4 && (b[2] == b'M' || b[3] == b'U')
    }
}

/// Parse a NUL-separated porcelain-v2 stream into path-bearing records. Walks
/// field-by-field so rename (`2`) records — which carry an EXTRA NUL-terminated
/// original-path field — keep the stream aligned. Paths (which may contain spaces)
/// are preserved verbatim via `splitn` (never `split_whitespace`).
fn parse_porcelain_v2_z(raw: &str) -> Vec<V2Entry> {
    let fields: Vec<&str> = raw.split('\0').collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < fields.len() {
        let rec = fields[i];
        match rec.as_bytes().first() {
            Some(b'1') => {
                // 1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>
                let parts: Vec<&str> = rec.splitn(9, ' ').collect();
                if parts.len() == 9 {
                    out.push(V2Entry {
                        token: parts[1].to_string(),
                        path: parts[8].to_string(),
                        sub: parts[2].to_string(),
                    });
                }
                i += 1;
            }
            Some(b'2') => {
                // 2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path> \0 <orig>
                let parts: Vec<&str> = rec.splitn(10, ' ').collect();
                if parts.len() == 10 {
                    out.push(V2Entry {
                        token: parts[1].to_string(),
                        path: parts[9].to_string(),
                        sub: parts[2].to_string(),
                    });
                }
                i += 2; // consume the trailing original-path field
            }
            Some(b'u') => {
                // u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>
                let parts: Vec<&str> = rec.splitn(11, ' ').collect();
                if parts.len() == 11 {
                    out.push(V2Entry {
                        token: format!("u{}", parts[1]),
                        path: parts[10].to_string(),
                        sub: String::new(),
                    });
                }
                i += 1;
            }
            Some(b'?') => {
                out.push(V2Entry {
                    token: "??".to_string(),
                    path: rec.get(2..).unwrap_or("").to_string(),
                    sub: String::new(),
                });
                i += 1;
            }
            Some(b'!') => {
                out.push(V2Entry {
                    token: "!!".to_string(),
                    path: rec.get(2..).unwrap_or("").to_string(),
                    sub: String::new(),
                });
                i += 1;
            }
            _ => i += 1, // blank tail field or `#` header — skip
        }
    }
    out
}

/// Upper bound on distinct submodules walked before we stop and say so EXPLICITLY
/// (never a silent depth cap — correction a).
const NESTED_ENUM_MAX: usize = 256;

/// Recursively enumerate the DIRTY nested (submodule) content of `wt_path` as a
/// human-readable, multi-line string: `<submodule-path>:` followed by its own
/// dirty entries, one per nested repo. Uses `git status --porcelain=v2 -z` (NUL
/// safe — handles spaces/newlines in paths). Every submodule descent is guarded by
/// canonical CONTAINMENT (reject `..`/symlink escape) + a visited-set (cycle
/// guard); exceeding [`NESTED_ENUM_MAX`] or any git error appends an explicit
/// `[truncated: …]` / `[skipped: …]` line rather than stopping silently.
fn enumerate_nested_dirty(wt_path: &Path) -> String {
    let root = match std::fs::canonicalize(wt_path) {
        Ok(p) => p,
        Err(e) => return format!("[truncated: canonicalize worktree root failed: {e}]\n"),
    };
    let mut visited: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    visited.insert(root.clone());
    let mut out = String::new();
    walk_nested_dirty(&root, "", &root, &mut visited, &mut out);
    out
}

/// One level of [`enumerate_nested_dirty`]. `dir_canon` is the canonical repo dir;
/// `display_prefix` is its path relative to the super (`""` for the super root).
fn walk_nested_dirty(
    dir_canon: &Path,
    display_prefix: &str,
    root: &Path,
    visited: &mut std::collections::HashSet<std::path::PathBuf>,
    out: &mut String,
) {
    let entries = match crate::git_helpers::git_cmd(dir_canon, &["status", "--porcelain=v2", "-z"])
    {
        Ok(s) => parse_porcelain_v2_z(&s),
        Err(e) => {
            out.push_str(&format!(
                "[truncated: git status failed at '{display_prefix}': {e}]\n"
            ));
            return;
        }
    };
    // A nested repo emits its OWN dirty (non-submodule) files under its header. The
    // super root (empty prefix) has no "own" nested files — only submodule pointers.
    if !display_prefix.is_empty() {
        out.push_str(&format!("{display_prefix}:\n"));
        for e in &entries {
            if !e.is_submodule() {
                out.push_str(&format!("  {} {}\n", e.token, e.path));
            }
        }
    }
    // Descend into each submodule with INTERNAL dirt.
    for e in &entries {
        if !e.dirty_submodule() {
            continue;
        }
        let disp = if display_prefix.is_empty() {
            e.path.clone()
        } else {
            format!("{display_prefix}/{}", e.path)
        };
        let sub_canon = match std::fs::canonicalize(dir_canon.join(&e.path)) {
            Ok(p) => p,
            Err(err) => {
                out.push_str(&format!(
                    "{disp}:\n  [skipped: containment (canonicalize failed: {err})]\n"
                ));
                continue;
            }
        };
        if !sub_canon.starts_with(root) {
            out.push_str(&format!("{disp}:\n  [skipped: containment]\n"));
            continue;
        }
        if !visited.insert(sub_canon.clone()) {
            out.push_str(&format!("{disp}:\n  [skipped: already visited]\n"));
            continue;
        }
        if visited.len() > NESTED_ENUM_MAX {
            out.push_str(&format!(
                "{disp}:\n  [truncated: visited-set exceeded {NESTED_ENUM_MAX}]\n"
            ));
            return;
        }
        walk_nested_dirty(&sub_canon, &disp, root, visited, out);
    }
}

/// Hex digest of a `Hash`-able value via `DefaultHasher` (bounded, allocation-free
/// key). Used both for the per-worktree marker filename (path) and the last-seen
/// nested-status content.
fn hash_hex<T: std::hash::Hash>(v: &T) -> String {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// The per-worktree refusal-notice directory: `<runtime>/release_refusal_notices`.
fn refusal_notice_dir(home: &Path) -> std::path::PathBuf {
    crate::paths::runtime_dir(home).join("release_refusal_notices")
}

/// The actionable UNPRESERVABLE-nested-dirty notice BODY. The `system:` marker is
/// added by the notify layer; the body must NOT embed a second one.
fn unpreservable_nested_dirty_notice(
    agent: &str,
    branch: &str,
    wt_path: &Path,
    nested_status: &str,
) -> String {
    let wt = wt_path.display();
    format!(
        "Agent `{agent}` could NOT release its worktree `{wt}` (branch `{branch}`): its \
         ONLY uncommitted changes live INSIDE a nested submodule's working tree — the gitlink \
         is unchanged, so a parent recovery ref CANNOT capture them. The release was REFUSED \
         (fail-closed) to avoid silently losing this nested WIP.\nDirty nested content:\n\
         {nested_status}\nResolve it IN PLACE (the worktree still exists), then release again:\n\
         1) commit or stash INSIDE the affected submodule, OR\n\
         2) discard it: cd {wt} && git submodule foreach 'git checkout -- . && git clean -fdx'\n\
         #2158-adjacent."
    )
}

/// Emit a SERIALIZED, de-duped actionable notice that a worktree's release was
/// refused for unpreservable nested-submodule dirt (correction b). Best-effort:
///  - Recipient = [`wip_notice_recipient`].
///  - Serialized per worktree by a file lock at `<dir>/<hash(wt)>.lock`, held
///    across read/compare/enqueue/marker-update.
///  - ONE marker per worktree at `<dir>/<hash(wt)>` whose CONTENT is the last
///    notified `hash(nested_status)`. Equal ⇒ SUPPRESS. A changed nested status
///    (incl. A→B→A) re-notifies.
///  - The marker advances ONLY after a successful (fallible) durable
///    [`crate::inbox::storage::enqueue`]; on enqueue failure the marker is left
///    unchanged so the next sweep re-notifies.
fn notify_unpreservable_nested_dirty(
    home: &Path,
    agent: &str,
    branch: &str,
    wt_path: &Path,
    nested_status: &str,
    sender: Option<&str>,
) {
    let recipient = wip_notice_recipient(home, agent, sender);
    let current_hash = hash_hex(&nested_status);
    let dir = refusal_notice_dir(home);
    let key = hash_hex(&wt_path);
    let marker_path = dir.join(&key);
    let lock_path = dir.join(format!("{key}.lock"));

    // SERIALIZE per worktree — hold across read/compare/enqueue/update.
    let _lock = match crate::store::acquire_file_lock(&lock_path) {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(agent, branch, error = %e,
                "unpreservable nested dirty: could not acquire notice lock — skipping notice");
            return;
        }
    };

    // Suppress if the last-notified nested status is byte-identical.
    if let Ok(prev) = std::fs::read_to_string(&marker_path) {
        if prev.trim() == current_hash {
            return;
        }
    }

    let text = unpreservable_nested_dirty_notice(agent, branch, wt_path, nested_status);
    let msg = crate::inbox::InboxMessage {
        from: crate::inbox::NotifySource::System("release_unpreservable_nested_dirty").to_string(),
        text,
        kind: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    };
    match crate::inbox::storage::enqueue(home, &recipient, msg) {
        Ok(()) => {
            // Advance the marker ONLY on a durable enqueue (temp + atomic rename).
            let seq = PRESERVE_TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp = dir.join(format!("{key}.tmp.{}.{}", std::process::id(), seq));
            let wrote = std::fs::write(&tmp, current_hash.as_bytes())
                .and_then(|_| std::fs::rename(&tmp, &marker_path));
            if let Err(e) = wrote {
                let _ = std::fs::remove_file(&tmp);
                tracing::warn!(agent, branch, error = %e,
                    "unpreservable nested dirty: notice enqueued but marker update failed — will re-notify");
            }
        }
        Err(e) => {
            tracing::warn!(agent, branch, error = %e,
                "unpreservable nested dirty: durable enqueue failed — marker NOT advanced (will re-notify)");
        }
    }
}

/// Best-effort removal of a worktree's refusal marker + lock, so a future re-lease
/// of the same path starts fresh. Called from `worktree_pool::release_full`'s
/// SUCCESS (worktree-removed) path.
pub(crate) fn clear_nested_refusal_marker(home: &Path, wt_path: &Path) {
    let dir = refusal_notice_dir(home);
    let key = hash_hex(&wt_path);
    let _ = std::fs::remove_file(dir.join(&key));
    let _ = std::fs::remove_file(dir.join(format!("{key}.lock")));
}

/// #2234 Phase 2: resolve the worktree dir to remove for `(agent, branch)`,
/// binding-driven so cure-(B) `workspace/<agent>` worktrees are removable.
///
/// - Derived `worktrees/<agent>/<branch>` exists → return it (OFF/legacy
///   byte-identical: the path that `remove_worktree` always used).
/// - Derived path GONE → fall back to the binding's recorded `worktree`, but
///   ONLY when the binding is bound to the SAME `branch`. A stale
///   `remove(branchX)` fired AFTER the agent rebound to `branchY` must NOT delete
///   the live `branchY` workspace (a `git worktree remove --force` is
///   destructive — the #2234-cluster lifecycle race). The branch guard makes
///   that case a correct no-op: `branchX`'s standalone worktree is genuinely gone
///   (in-place checkout folded it into another branch).
/// - `None` ⟹ nothing matching → the caller no-ops.
fn resolve_removable_worktree(home: &Path, agent: &str, branch: &str) -> Option<PathBuf> {
    let derived = worktree_path(home, agent, branch);
    if derived.exists() {
        return Some(derived);
    }
    let binding = crate::binding::read(home, agent)?;
    let wt = PathBuf::from(binding.get("worktree")?.as_str()?);
    let bound_branch = binding.get("branch")?.as_str()?;
    if bound_branch == branch && wt.exists() {
        Some(wt)
    } else {
        None
    }
}

/// Remove a worktree and its tracking branch. Returns Ok(()) on success,
/// Err with message on failure. Pre-flight: caller must check
/// `has_uncommitted_changes` first.
///
/// Sprint 57 Wave 4 (#546 Item 4): operates on the new external
/// layout `$AGEND_HOME/worktrees/<agent>/<branch>/`. Caller must
/// supply `home`, `agent`, and `branch` so the canonical path can
/// be resolved without re-deriving it from any remembered
/// `<source_repo>/.worktrees/...` literal.
pub fn remove_worktree(
    home: &Path,
    repo_dir: &Path,
    agent: &str,
    branch: &str,
) -> Result<(), String> {
    // #2234 Phase 2: binding-driven resolution (byte-identical OFF — the derived
    // path exists and is removed exactly as before). `None` = already gone, OR
    // the binding is bound to a DIFFERENT branch → no-op (never delete the wrong
    // branch's live worktree; see `resolve_removable_worktree`).
    let wt_dir = match resolve_removable_worktree(home, agent, branch) {
        Some(p) => p,
        None => return Ok(()),
    };
    // #2128: bounded `git_cmd` (AGEND_GIT_BYPASS + LOCAL_GIT_TIMEOUT +
    // process-group-kill) so a `worktree remove` wedged on `.git/index.lock`
    // contention fails in 60s instead of hanging. The two DISTINCT contracted Err
    // strings are preserved: `Err(Spawn)` → "git worktree remove failed: {e}",
    // `Err(NonZero{stderr})` → "git worktree remove: {stderr}". git_cmd's
    // NonZero.stderr is already trimmed (matches the prior `stderr.trim()`); a 60s
    // timeout surfaces as `Err(Spawn)` whose `{e}` carries "timed out after 60s" —
    // the remove-side timeout signal, for free (DP2).
    use crate::git_helpers::{git_cmd, GitError};
    match git_cmd(
        repo_dir,
        &[
            "worktree",
            "remove",
            "--force",
            &wt_dir.display().to_string(),
        ],
    ) {
        Ok(_) => {}
        Err(GitError::Spawn(e)) => return Err(format!("git worktree remove failed: {e}")),
        Err(GitError::NonZero { stderr, .. }) => {
            return Err(format!("git worktree remove: {stderr}"))
        }
    }
    // Delete tracking branch agend/<agent> (legacy default-branch shape).
    // Custom branches are not auto-deleted — operator workflow.
    let default_branch = format!("agend/{agent}");
    // W1.2: LOCAL best-effort branch delete via git_ok (result was already
    // discarded; git_ok keeps that and adds the LOCAL_GIT_TIMEOUT bound).
    let _ = crate::git_helpers::git_ok(repo_dir, &["branch", "-D", &default_branch]);
    tracing::info!(agent, branch, "auto-pruned worktree + branch");
    Ok(())
}

/// Checkout a branch in a worktree directory. Creates the branch from
/// current HEAD if it doesn't exist. Best-effort: returns Ok on success,
/// Err with message on failure.
pub fn checkout_branch(worktree_dir: &Path, branch: &str) -> Result<(), String> {
    use crate::git_helpers::{git_cmd, GitError};
    // W1.2: both switch attempts via git_cmd. A SPAWN failure propagates as Err
    // (matching the prior `.map_err(...)?`); a NON-ZERO exit on the first switch
    // means "branch absent" → fall through to create (matching the prior
    // `if success { Ok } else { fall through }`). git_cmd's NonZero stderr is
    // already trimmed, so the final Err string is byte-identical.
    // Try switching to existing branch first
    match git_cmd(worktree_dir, &["switch", branch]) {
        Ok(_) => {
            tracing::info!(branch, dir = %worktree_dir.display(), "checked out branch");
            return Ok(());
        }
        Err(GitError::Spawn(e)) => return Err(format!("git switch: {e}")),
        Err(GitError::NonZero { .. }) => {} // branch absent — create below
    }
    // Branch doesn't exist — create from current HEAD
    match git_cmd(worktree_dir, &["switch", "-c", branch]) {
        Ok(_) => {
            tracing::info!(branch, dir = %worktree_dir.display(), "created and checked out branch");
            Ok(())
        }
        Err(GitError::Spawn(e)) => Err(format!("git switch -c: {e}")),
        Err(GitError::NonZero { stderr, .. }) => Err(format!("git switch -c {branch}: {stderr}")),
    }
}

/// #2115: force-sync a REUSED worktree's index + working tree to its current
/// HEAD at the lease-acquisition choke point.
///
/// The reuse return paths (`create`'s same-branch reuse + clean-reattach, and
/// `repo checkout bind:true`'s idempotent short-circuit) hand an EXISTING
/// worktree back to a fresh occupant. When `ensure_branch_exists` (#869)
/// fast-forwarded the branch ref via `update-ref` between leases, the worktree's
/// HEAD symref now points at the new SHA while its index + working tree still
/// hold the prior commit's content — so the tree reads DIRTY on hand-off (r6
/// caught this twice: #2196, #2223), and a reviewer who runs runtime on the
/// polluted tree gets a false-red/false-green verdict. Syncing to HEAD here
/// closes that at a single choke point.
///
/// WIP-safety (dual-review focus): a reuse is a FRESH lease = fresh ownership.
/// Cross-branch reuse already reject-on-dirty protects genuine WIP (the
/// `has_uncommitted_changes` drift guard in `create`), so the only trees reset
/// here belong to a re-acquired SAME branch (or an explicit re-`checkout`),
/// where sync-to-HEAD matches the caller's intent. A CLEAN tree is never
/// touched (early return) — the destructive `reset --hard` only ever runs on an
/// actually-dirty tree, and the porcelain entries are WARN-logged BEFORE the
/// reset so any discarded content is auditable, never silent.
///
/// `clean -fd` (NOT `-fdx`): removes untracked files but PRESERVES `.gitignore`'d
/// build artifacts (e.g. `target/`) so the build cache survives the lease churn.
pub fn sync_worktree_to_head(worktree_dir: &Path) {
    use crate::git_helpers::git_cmd;
    // Bounded git_cmd (AGEND_GIT_BYPASS + LOCAL_GIT_TIMEOUT + process-group-kill),
    // matching the WIP guard above — can't wedge on a contended `.git/index.lock`.
    match git_cmd(worktree_dir, &["status", "--porcelain"]) {
        // Clean tree: working tree already == HEAD, nothing to sync. Never run a
        // destructive reset on a clean worktree (minimises blast radius).
        Ok(status) if status.is_empty() => return,
        Ok(status) => {
            tracing::warn!(
                dir = %worktree_dir.display(),
                discarded = %status,
                "#2115 sync-on-reuse: reused worktree is dirty — resetting to HEAD \
                 (stale-after-#869-ref-advance or prior-lease residue); listed paths are discarded"
            );
        }
        Err(e) => {
            // Couldn't read status (spawn failure / 60s timeout). The bug we are
            // closing is exactly a dirty tree and `reset --hard` is the corrective
            // action, so proceed — but log so it isn't invisible.
            tracing::warn!(
                dir = %worktree_dir.display(),
                error = %e,
                "#2115 sync-on-reuse: status probe failed — resetting to HEAD anyway"
            );
        }
    }
    let _ = git_cmd(worktree_dir, &["reset", "--hard", "HEAD"]);
    let _ = git_cmd(worktree_dir, &["clean", "-fd"]);
}

/// Sprint 57 Wave 4 (#546 Item 4): list agent names present under
/// `$AGEND_HOME/worktrees/`. The `repo_dir` parameter is retained
/// for API compatibility with pre-Wave-4 callers but the new layout
/// is repo-independent — agent dirs live under the central daemon
/// state, not per-repo.
pub fn list_residual(home: &Path) -> Vec<String> {
    // `worktrees/<...>` first-level entries — UNCHANGED (byte-identical OFF).
    let wt_base = home.join("worktrees");
    let mut out: Vec<String> = std::fs::read_dir(&wt_base)
        .ok()
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    // #2234 Phase 2: also surface cure-(B) `workspace/<agent>` gitlink worktrees
    // (shared single-impl scan). Empty when (B) is OFF → byte-identical.
    out.extend(
        crate::worktree_pool::workspace_gitlink_worktrees(home)
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(String::from)),
    );
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn tmp_repo(name: &str) -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-wt-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        // git init
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
            .output()
            .ok();
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@test",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(&dir)
            .output()
            .ok();
        dir
    }

    /// Sprint 57 Wave 4 (#546 Item 4): test home dir distinct from
    /// the test repo dir so the new external worktree layout
    /// `<home>/worktrees/<agent>/<branch>/` is verifiable in isolation.
    fn tmp_home(name: &str) -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-wt-home-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn test_is_git_repo() {
        let repo = tmp_repo("is_git");
        assert!(is_git_repo(&repo));
        assert!(!is_git_repo(&std::env::temp_dir()));
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn test_create_worktree() {
        let home = tmp_home("create");
        let repo = tmp_repo("create");
        let info = create(&home, &repo, "agent1", None);
        assert!(info.is_some());
        let info = info.expect("worktree created");
        assert!(info.path.exists());
        assert_eq!(info.branch, "agend/agent1");
        assert_eq!(info.source_repo, repo);
        // Sprint 57 Wave 4 (#546 Item 4): worktree must live under
        // `<home>/worktrees/<agent>/<branch>/`, NOT `<repo>/.worktrees/`.
        let expected = home.join("worktrees").join("agent1").join("agend/agent1");
        assert_eq!(
            info.path, expected,
            "worktree path must follow new external layout"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Fixture helper: init a bare-ish local git repo with identity + one commit.
    fn tmp_repo_with_file(name: &str, rel: &str, body: &str) -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-wt-subfix-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        git_run_ok(&dir, &["init", "-b", "main"], /*allow_file*/ false);
        git_run_ok(&dir, &["config", "user.email", "test@test"], false);
        git_run_ok(&dir, &["config", "user.name", "test"], false);
        if let Some(parent) = Path::new(rel).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(dir.join(parent)).unwrap();
            }
        }
        std::fs::write(dir.join(rel), body).unwrap();
        git_run_ok(&dir, &["add", rel], false);
        git_run_ok(&dir, &["commit", "-m", "init"], false);
        dir
    }

    /// Run git; panic with stderr on non-zero. When `allow_file`, sets
    /// `protocol.file.allow=always` so local-path submodule fixtures work.
    fn git_run_ok(dir: &Path, args: &[&str], allow_file: bool) {
        let mut cmd = std::process::Command::new("git");
        cmd.env("AGEND_GIT_BYPASS", "1").current_dir(dir);
        if allow_file {
            cmd.args(["-c", "protocol.file.allow=always"]);
        }
        cmd.args(args);
        let out = cmd.output().expect("spawn git");
        assert!(
            out.status.success(),
            "git {:?} in {} failed: {}",
            args,
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Hermetic superproject with **two** submodule levels:
    ///   super → `vendor/mid` (A) → `nested` (B, holds `nested_b.txt`)
    /// Proves fresh `worktree::create` initializes submodules **recursively**
    /// (`--init --recursive`). A single-level fixture would not pin recursion.
    ///
    /// Fixture `submodule add` uses `-c protocol.file.allow=always` (via
    /// `git_run_ok(..., true)`). Production `init_submodules_after_create`
    /// also passes that `-c` on `git_cmd` — local-path clone helpers ignore
    /// repo-stored `protocol.file.allow` alone. No stored config required.
    fn tmp_super_with_nested_submodules(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agend-wt-nest-root-{}-{}-{}",
            std::process::id(),
            name,
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();

        // Level B (innermost)
        let b = tmp_repo_with_file(&format!("{name}-b"), "nested_b.txt", "level-b-payload\n");

        // Level A: depends on B at nested/
        let a = {
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = root.join(format!("a-{id}"));
            std::fs::create_dir_all(&dir).unwrap();
            git_run_ok(&dir, &["init", "-b", "main"], false);
            git_run_ok(&dir, &["config", "user.email", "test@test"], false);
            git_run_ok(&dir, &["config", "user.name", "test"], false);
            git_run_ok(
                &dir,
                &["submodule", "add", &b.display().to_string(), "nested"],
                true,
            );
            git_run_ok(&dir, &["commit", "-m", "A with nested B"], false);
            dir
        };

        // Super: depends on A at vendor/mid/
        let super_repo = {
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = root.join(format!("super-{id}"));
            std::fs::create_dir_all(&dir).unwrap();
            git_run_ok(&dir, &["init", "-b", "main"], false);
            git_run_ok(&dir, &["config", "user.email", "test@test"], false);
            git_run_ok(&dir, &["config", "user.name", "test"], false);
            git_run_ok(
                &dir,
                &["submodule", "add", &a.display().to_string(), "vendor/mid"],
                true,
            );
            git_run_ok(&dir, &["commit", "-m", "super with A→B nest"], false);
            dir
        };

        // Keep root alive via super_repo living under it; B and A are siblings.
        // Drop path to B/A is fine — git objects live in their dirs which remain.
        let _ = (b, a);
        super_repo
    }

    /// Fresh daemon worktree provision must materialize nested submodule
    /// content (level B file) without a manual `git submodule update`.
    #[test]
    fn create_initializes_nested_submodules_recursively() {
        let home = tmp_home("submod-rec");
        let super_repo = tmp_super_with_nested_submodules("submod-rec");

        // Sanity: super has .gitmodules and the nested path is recorded.
        assert!(
            super_repo.join(".gitmodules").is_file(),
            "fixture super must have .gitmodules"
        );
        // Nested content must be present in the *source* super (already inited
        // by `submodule add`); the bug is only on the *fresh worktree* side.
        assert!(
            super_repo.join("vendor/mid/nested/nested_b.txt").is_file()
                || super_repo.join("vendor/mid").join(".gitmodules").is_file(),
            "fixture: A must be present under super (init by submodule add)"
        );

        let info = create(&home, &super_repo, "agent-sub", Some("feat/submod-rec"))
            .expect("worktree::create must succeed for hermetic super");

        // Decisive pin: level-B file exists inside the fresh worktree.
        // Without --recursive init after worktree add, vendor/mid is empty.
        let nested_b = info.path.join("vendor/mid/nested/nested_b.txt");
        assert!(
            nested_b.is_file(),
            "fresh worktree must recursively init submodules so level-B file \
             exists at {}; worktree add alone leaves submodule dirs empty",
            nested_b.display()
        );
        // Windows git may rewrite LF→CRLF on checkout (core.autocrlf); pin payload only.
        let body = std::fs::read_to_string(&nested_b).unwrap();
        assert_eq!(
            body.trim_end_matches(['\r', '\n']),
            "level-b-payload",
            "payload text must match regardless of CRLF vs LF"
        );

        std::fs::remove_dir_all(&home).ok();
        // super_repo's parent root holds A/B siblings — best-effort clean.
        if let Some(root) = super_repo.parent() {
            std::fs::remove_dir_all(root).ok();
        }
    }

    /// Soft-warn contract: when a committed submodule's source is unavailable
    /// (optional/private/offline), `create` must still return `Some` with a
    /// managed worktree — never hard-fail the lease. Nested content stays empty.
    #[test]
    fn create_soft_fails_when_submodule_source_unavailable() {
        let home = tmp_home("submod-soft");
        let root = std::env::temp_dir().join(format!(
            "agend-wt-soft-root-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();

        let sub = tmp_repo_with_file("soft-sub", "payload.txt", "should-not-appear\n");
        let super_repo = {
            let dir = root.join("super");
            std::fs::create_dir_all(&dir).unwrap();
            git_run_ok(&dir, &["init", "-b", "main"], false);
            git_run_ok(&dir, &["config", "user.email", "test@test"], false);
            git_run_ok(&dir, &["config", "user.name", "test"], false);
            git_run_ok(
                &dir,
                &["submodule", "add", &sub.display().to_string(), "vendor/dep"],
                true,
            );
            git_run_ok(&dir, &["commit", "-m", "super with dep"], false);
            dir
        };
        assert!(super_repo.join(".gitmodules").is_file());

        // Make the recorded submodule URL unusable BEFORE production create.
        std::fs::remove_dir_all(&sub).expect("remove submodule source");

        let info = create(&home, &super_repo, "agent-soft", Some("feat/submod-soft"))
            .expect("create must soft-warn and still return Some when submodule init fails");

        assert!(
            info.path.join(".agend-managed").is_file(),
            "managed marker must still land on soft-fail path"
        );
        assert!(
            !info.path.join("vendor/dep/payload.txt").is_file(),
            "nested content must remain unavailable when source is gone"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&root).ok();
    }

    // ── #2158-adjacent: dirty-WIP preservation helpers ──────────────────
    // (reuses the module's existing `git_out`/`git_run` bypass helpers below)

    fn recovery_ref_names(repo: &Path, branch: &str) -> Vec<String> {
        git_out(
            repo,
            &[
                "for-each-ref",
                "--format=%(refname)",
                &format!("refs/agend/recovery/{branch}/"),
            ],
        )
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
    }

    fn pres_kind(p: &WipPreservation) -> &'static str {
        match p {
            WipPreservation::Clean => "Clean",
            WipPreservation::Preserved => "Preserved",
            WipPreservation::Blocked(_) => "Blocked",
            WipPreservation::UnpreservableNestedDirty(_) => "UnpreservableNestedDirty",
        }
    }

    #[test]
    fn preserve_dirty_worktree_captures_untracked_wip() {
        let home = tmp_home("preserve-untracked");
        let repo = tmp_repo("preserve-untracked");
        let info = create(&home, &repo, "agent1", None).expect("worktree created");
        // Untracked WIP — the loss-prone case (`clean -fd` would delete it).
        std::fs::write(info.path.join("scratch-wip.txt"), b"unsaved work").unwrap();

        let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, &info.branch, None);
        assert!(
            matches!(outcome, WipPreservation::Preserved),
            "dirty worktree must be Preserved, got {}",
            pres_kind(&outcome)
        );
        // Verify the ref via git (authoritative — the ref name is not returned).
        let refs = recovery_ref_names(&repo, &info.branch);
        assert_eq!(refs.len(), 1, "exactly one recovery ref: {refs:?}");
        let tree = git_out(&repo, &["ls-tree", "-r", "--name-only", &refs[0]]);
        assert!(
            tree.contains("scratch-wip.txt"),
            "untracked WIP captured in recovery ref tree: {tree}"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn preserve_dirty_worktree_clean_is_noop() {
        let home = tmp_home("preserve-clean");
        let repo = tmp_repo("preserve-clean");
        let info = create(&home, &repo, "agent1", None).expect("worktree created");
        // No real WIP (a freshly-created worktree carries at most the daemon
        // marker, which is not preservable) → helper must report Clean.
        assert!(
            matches!(
                preserve_dirty_worktree(&home, "agent1", &info.path, &info.branch, None),
                WipPreservation::Clean
            ),
            "clean worktree must be Clean (no recovery ref)"
        );
        assert!(
            recovery_ref_names(&repo, &info.branch).is_empty(),
            "no recovery ref for a clean release"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// The linked worktree's private index lives at `<gitdir>/index` (gitdir read
    /// from the `.git` gitlink file). Planting `<gitdir>/index.lock` makes any
    /// index write (`git add -A`) fail — reviewer4's #2672 counterexample for a
    /// contended index.
    fn plant_index_lock(wt_path: &Path) -> PathBuf {
        let gitlink = std::fs::read_to_string(wt_path.join(".git")).expect("read .git gitlink");
        let gitdir = gitlink
            .strip_prefix("gitdir:")
            .expect("gitlink form")
            .trim();
        let lock = Path::new(gitdir).join("index.lock");
        std::fs::write(&lock, b"").expect("plant index.lock");
        lock
    }

    #[test]
    fn preserve_dirty_worktree_blocks_when_index_locked() {
        // reviewer4 #2672 fail-OPEN counterexample: dirty untracked WIP + a
        // contended index (index.lock) → `git add -A` fails. The old code returned
        // a silently-ignored `None` and the caller removed the worktree, evaporating
        // the WIP. It must now be Blocked (fail-closed) with NO recovery ref.
        let home = tmp_home("preserve-blocked");
        let repo = tmp_repo("preserve-blocked");
        let info = create(&home, &repo, "agent1", None).expect("worktree created");
        std::fs::write(info.path.join("precious-wip.txt"), b"must not vanish").unwrap();
        let lock = plant_index_lock(&info.path);

        let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, &info.branch, None);
        assert!(
            outcome.blocked_reason().is_some(),
            "unpreservable WIP must be Blocked (fail-closed), got {}",
            pres_kind(&outcome)
        );
        assert!(
            recovery_ref_names(&repo, &info.branch).is_empty(),
            "Blocked must not leave a (partial) recovery ref"
        );
        std::fs::remove_file(&lock).ok();
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Bug1 (#t-…61315-2): a known caller is the notice recipient — never the
    /// hardcoded `general` the pre-fix `notify_agent(home, "general", …)` used.
    #[test]
    fn wip_notice_recipient_prefers_the_caller() {
        let home = tmp_home("wip-recipient-caller");
        assert_eq!(
            wip_notice_recipient(&home, "agent1", Some("lead-x")),
            "lead-x",
            "a known caller must be the recipient, not a hardcoded one"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Bug1 last-resort fallback: no caller AND no team → operator inbox
    /// (`general`). This is the ONLY path that may legitimately use `general`.
    #[test]
    fn wip_notice_recipient_no_team_falls_back_to_general() {
        let home = tmp_home("wip-recipient-no-team");
        assert_eq!(
            wip_notice_recipient(&home, "agent1", None),
            "general",
            "no caller and no team → operator inbox as last resort"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── release RCA: unpreservable nested-submodule WIP (data-safety) ─────────
    //
    // A super-repo worktree whose ONLY dirt lives inside a submodule's working
    // tree (gitlink unchanged) MUST NOT be reported `Preserved`: a parent
    // recovery ref (tree = super's `add -A`) captures nothing of the nested WIP,
    // so removing the worktree silently loses it. The fix classifies this via a
    // dual-tree compare and refuses removal (`UnpreservableNestedDirty`), while
    // ordinary parent WIP + submodule-pointer moves still preserve+release.

    /// Superproject with `commit_marker_gitignore` + one committed submodule at
    /// `vendor/dep` holding `sub_file`. `create()` inits it recursively.
    fn tmp_super_one_sub_file(name: &str, sub_file: &str) -> PathBuf {
        let sub = tmp_repo_with_file(&format!("{name}-sub"), sub_file, "vendored-v1\n");
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-wt-super1-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        git_run_ok(&dir, &["init", "-b", "main"], false);
        git_run_ok(&dir, &["config", "user.email", "test@test"], false);
        git_run_ok(&dir, &["config", "user.name", "test"], false);
        commit_marker_gitignore(&dir); // marker gitignored, like a real repo
        git_run_ok(
            &dir,
            &["submodule", "add", &sub.display().to_string(), "vendor/dep"],
            true,
        );
        git_run_ok(&dir, &["commit", "-m", "add vendor/dep submodule"], false);
        dir
    }

    fn tmp_super_one_sub(name: &str) -> PathBuf {
        tmp_super_one_sub_file(name, "vendored.txt")
    }

    /// Run git with piped stdin; panic with stderr on non-zero.
    fn git_stdin_ok(dir: &Path, args: &[&str], input: &[u8]) {
        use std::io::Write;
        let mut child = std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(args)
            .current_dir(dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn git");
        child
            .stdin
            .take()
            .expect("stdin pipe")
            .write_all(input)
            .expect("write stdin");
        let out = child.wait_with_output().expect("wait git");
        assert!(
            out.status.success(),
            "git {:?} in {} failed: {}",
            args,
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Count inbox messages for `recipient` under `home` whose `from` matches
    /// `from_marker` (reads the JSONL file directly — fresh test home ⇒ no
    /// id-redirect, so the file is `home/inbox/<recipient>.jsonl`).
    fn inbox_count_from(home: &Path, recipient: &str, from_marker: &str) -> usize {
        let path = home.join("inbox").join(format!("{recipient}.jsonl"));
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        body.lines()
            .filter(|l| !l.trim().is_empty())
            .filter(|l| l.contains(from_marker))
            .count()
    }

    const NESTED_FROM: &str = "system:release_unpreservable_nested_dirty";

    /// (1) Nested-only dirt (tracked file inside the submodule, gitlink unchanged)
    /// ⇒ `UnpreservableNestedDirty`, `blocked_reason().is_some()`, NO recovery ref,
    /// and the LIVE index/worktree are UNCHANGED by classification.
    #[cfg(unix)]
    #[test]
    fn preserve_refuses_nested_only_dirt() {
        let home = tmp_home("nested-refuse");
        let super_repo = tmp_super_one_sub("nested-refuse");
        let info = create(&home, &super_repo, "agent1", Some("feat/nest")).expect("worktree");
        let vendored = info.path.join("vendor/dep/vendored.txt");
        assert!(
            vendored.is_file(),
            "fixture: submodule file present in worktree"
        );

        std::fs::write(&vendored, b"DIRTY-nested-edit\n").unwrap();
        // Capture the DIRTY status immediately before the call: classification must
        // leave the live index + working tree byte-untouched, so this is identical
        // afterwards (still the nested dirt, nothing staged).
        let status_before = git_out(&info.path, &["status", "--porcelain"]);
        assert!(
            status_before.contains("vendor/dep"),
            "precondition: nested submodule reads dirty: {status_before}"
        );

        let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/nest", None);
        assert_eq!(
            pres_kind(&outcome),
            "UnpreservableNestedDirty",
            "nested-only dirt must refuse (not falsely Preserved)"
        );
        assert!(
            outcome.blocked_reason().is_some(),
            "refusal must be fail-closed (blocked_reason Some)"
        );
        assert!(
            recovery_ref_names(&super_repo, "feat/nest").is_empty(),
            "no recovery ref may be minted for unpreservable nested dirt"
        );
        // Non-destructive: identical porcelain status before/after the call.
        let status_after = git_out(&info.path, &["status", "--porcelain"]);
        assert_eq!(
            status_before, status_after,
            "classification must not mutate the live index or working tree"
        );

        std::fs::remove_dir_all(&home).ok();
        if let Some(root) = super_repo.parent() {
            std::fs::remove_dir_all(root).ok();
        }
        std::fs::remove_dir_all(&super_repo).ok();
    }

    /// (2) Staged-only WIP (index=v2, working tree reverted to HEAD=v1) ⇒
    /// `Preserved`, a ref exists, the staged content is recoverable, and the LIVE
    /// index still has v2 staged afterwards (non-destructive).
    #[cfg(unix)]
    #[test]
    fn preserve_keeps_staged_only_wip_recoverable() {
        let home = tmp_home("staged-only");
        let repo = tmp_repo_with_file("staged-only", "f", "v1\n");
        commit_marker_gitignore(&repo);
        let info = create(&home, &repo, "agent1", Some("feat/staged")).expect("worktree");
        let f = info.path.join("f");
        std::fs::write(&f, b"v2\n").unwrap();
        git_run_ok(&info.path, &["add", "f"], false); // stage v2
        std::fs::write(&f, b"v1\n").unwrap(); // revert working tree to HEAD

        let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/staged", None);
        assert_eq!(
            pres_kind(&outcome),
            "Preserved",
            "staged-only WIP is preservable (parent tree captures it)"
        );
        let refs = recovery_ref_names(&repo, "feat/staged");
        assert_eq!(refs.len(), 1, "exactly one recovery ref: {refs:?}");
        // Staged v2 must be recoverable: dual-parent (staged ≠ worktree) ⇒ `^2`.
        let staged = git_out(&repo, &["show", &format!("{}^2:f", refs[0])]);
        assert_eq!(
            staged, "v2",
            "staged snapshot recoverable at ref^2: got {staged:?}"
        );
        // CRUCIAL non-destructive: the LIVE index still has v2 staged.
        let live_staged = git_out(&info.path, &["show", ":f"]);
        assert_eq!(
            live_staged, "v2",
            "live index must still hold staged v2 after call"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// (3) Staged (v2) DIFFERS from working tree (v3), both ≠ HEAD (v1) ⇒ ONE ref,
    /// dual-parent: `ref:f`==v3, `ref^2:f`==v2, and both commits are reachable.
    #[cfg(unix)]
    #[test]
    fn preserve_dual_parent_when_staged_differs_from_worktree() {
        let home = tmp_home("dual-parent");
        let repo = tmp_repo_with_file("dual-parent", "f", "v1\n");
        commit_marker_gitignore(&repo);
        let info = create(&home, &repo, "agent1", Some("feat/dual")).expect("worktree");
        let f = info.path.join("f");
        std::fs::write(&f, b"v2\n").unwrap();
        git_run_ok(&info.path, &["add", "f"], false); // stage v2
        std::fs::write(&f, b"v3\n").unwrap(); // working tree v3

        let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/dual", None);
        assert_eq!(pres_kind(&outcome), "Preserved");
        let refs = recovery_ref_names(&repo, "feat/dual");
        assert_eq!(refs.len(), 1, "exactly one recovery ref: {refs:?}");
        assert_eq!(
            git_out(&repo, &["show", &format!("{}:f", refs[0])]),
            "v3",
            "ref tree captures the WORKING tree (v3)"
        );
        assert_eq!(
            git_out(&repo, &["show", &format!("{}^2:f", refs[0])]),
            "v2",
            "ref^2 captures the STAGED index (v2)"
        );
        let staged_commit = git_out(&repo, &["rev-parse", &format!("{}^2", refs[0])]);
        let reachable = git_out(&repo, &["rev-list", &refs[0]]);
        assert!(
            reachable.lines().any(|l| l == staged_commit),
            "staged parent must be reachable from the recovery ref"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// (4) Nested submodule dirt AND an untracked PARENT file ⇒ prefer Preserve
    /// (the parent WIP is real), and the ref captures the untracked parent file.
    #[cfg(unix)]
    #[test]
    fn preserve_mixed_parent_and_submodule_prefers_preserve() {
        let home = tmp_home("mixed");
        let super_repo = tmp_super_one_sub("mixed");
        let info = create(&home, &super_repo, "agent1", Some("feat/mixed")).expect("worktree");
        // Dirty the submodule internal file AND drop an untracked parent file.
        std::fs::write(info.path.join("vendor/dep/vendored.txt"), b"nested-dirty\n").unwrap();
        std::fs::write(info.path.join("parent-wip.txt"), b"parent untracked WIP\n").unwrap();

        let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/mixed", None);
        assert_eq!(
            pres_kind(&outcome),
            "Preserved",
            "mixed parent+submodule dirt must Preserve, not refuse"
        );
        let refs = recovery_ref_names(&super_repo, "feat/mixed");
        assert_eq!(refs.len(), 1, "one recovery ref: {refs:?}");
        let tree = git_out(&super_repo, &["ls-tree", "-r", "--name-only", &refs[0]]);
        assert!(
            tree.contains("parent-wip.txt"),
            "recovery ref must capture the untracked parent file: {tree}"
        );

        std::fs::remove_dir_all(&home).ok();
        if let Some(root) = super_repo.parent() {
            std::fs::remove_dir_all(root).ok();
        }
        std::fs::remove_dir_all(&super_repo).ok();
    }

    /// (5) A submodule POINTER move (new commit inside the submodule ⇒ gitlink
    /// changes) is a real, preservable parent change ⇒ `Preserved`, not refused.
    #[cfg(unix)]
    #[test]
    fn preserve_submodule_pointer_move_is_preserved() {
        let home = tmp_home("ptr-move");
        let super_repo = tmp_super_one_sub("ptr-move");
        let info = create(&home, &super_repo, "agent1", Some("feat/ptr")).expect("worktree");
        let sub = info.path.join("vendor/dep");
        // Commit INSIDE the submodule so its HEAD (the gitlink) moves.
        std::fs::write(sub.join("vendored.txt"), b"vendored-v2\n").unwrap();
        git_run_ok(&sub, &["add", "vendored.txt"], false);
        git_run_ok(
            &sub,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "bump inside submodule",
            ],
            false,
        );

        let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/ptr", None);
        assert_eq!(
            pres_kind(&outcome),
            "Preserved",
            "submodule gitlink move is preservable via the parent tree"
        );
        assert_eq!(
            recovery_ref_names(&super_repo, "feat/ptr").len(),
            1,
            "one recovery ref for the pointer move"
        );

        std::fs::remove_dir_all(&home).ok();
        if let Some(root) = super_repo.parent() {
            std::fs::remove_dir_all(root).ok();
        }
        std::fs::remove_dir_all(&super_repo).ok();
    }

    /// (6) An UNMERGED live index makes `git write-tree` fail ⇒ `Blocked`
    /// (fail-closed), no ref. Planted deterministically via `update-index
    /// --index-info` with stage 1/2/3 entries for one path.
    #[cfg(unix)]
    #[test]
    fn preserve_unmerged_index_is_blocked() {
        let home = tmp_home("unmerged");
        let repo = tmp_repo_with_file("unmerged", "f.txt", "base\n");
        commit_marker_gitignore(&repo);
        let info = create(&home, &repo, "agent1", Some("feat/unmerged")).expect("worktree");
        // Object for the conflict path (hash-object on a real file → no stdin).
        std::fs::write(info.path.join("c.txt"), b"conflicted\n").unwrap();
        let blob = git_out(&info.path, &["hash-object", "-w", "c.txt"]);
        let index_info =
            format!("100644 {blob} 1\tc.txt\n100644 {blob} 2\tc.txt\n100644 {blob} 3\tc.txt\n");
        git_stdin_ok(
            &info.path,
            &["update-index", "--index-info"],
            index_info.as_bytes(),
        );
        // Sanity: the live index is genuinely unmerged (write-tree fails).
        assert!(
            crate::git_helpers::git_cmd(&info.path, &["write-tree"]).is_err(),
            "precondition: unmerged index ⇒ write-tree fails"
        );

        let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/unmerged", None);
        assert_eq!(
            pres_kind(&outcome),
            "Blocked",
            "unmerged index (snapshot failure) is Blocked, not nested-refused"
        );
        assert!(
            recovery_ref_names(&repo, "feat/unmerged").is_empty(),
            "Blocked must not mint a recovery ref"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Marker directory for the per-worktree refusal notice (test-visible mirror
    /// of the production path).
    fn refusal_marker_dir(home: &Path) -> PathBuf {
        crate::paths::runtime_dir(home).join("release_refusal_notices")
    }

    /// (7) `release_full` on a nested-only-dirty bound worktree ⇒ refused:
    /// NOT released, NOT removed, path intact, no ref, binding retained. Then a
    /// CLEAN re-release removes the worktree AND clears the refusal marker.
    #[cfg(unix)]
    #[test]
    fn release_full_refuses_nested_only() {
        let home = tmp_home("release-refuse");
        let super_repo = tmp_super_one_sub("release-refuse");
        let info = create(&home, &super_repo, "agent1", Some("feat/rel")).expect("worktree");
        crate::binding::bind_full(
            &home,
            "agent1",
            "",
            "feat/rel",
            &info.path,
            &super_repo,
            false,
        )
        .expect("bind");
        let vendored = info.path.join("vendor/dep/vendored.txt");
        std::fs::write(&vendored, b"nested-dirty\n").unwrap();

        let out = crate::worktree_pool::release_full(&home, "agent1", false);
        assert!(
            !out.released,
            "release must be refused for unpreservable nested WIP"
        );
        assert!(
            !out.worktree_removed,
            "worktree must NOT be removed on refusal"
        );
        assert!(
            info.path.exists(),
            "worktree dir must remain for in-place recovery"
        );
        assert!(
            recovery_ref_names(&super_repo, "feat/rel").is_empty(),
            "no recovery ref for refused nested dirt"
        );
        assert!(
            crate::binding::read(&home, "agent1").is_some(),
            "binding must be retained on refusal"
        );
        // The refusal wrote a per-worktree marker.
        let markers: Vec<_> = std::fs::read_dir(refusal_marker_dir(&home))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) != Some("lock"))
            .collect();
        assert!(!markers.is_empty(), "refusal must persist a de-dup marker");

        // Now make the worktree clean and re-release: it removes + clears marker.
        std::fs::write(&vendored, b"vendored-v1\n").unwrap(); // revert nested dirt
        assert!(
            !has_uncommitted_changes(&info.path),
            "precondition: worktree is clean before re-release"
        );
        let out2 = crate::worktree_pool::release_full(&home, "agent1", false);
        assert!(out2.released, "clean re-release must succeed");
        assert!(out2.worktree_removed, "clean worktree must be removed");
        let leftover = std::fs::read_dir(refusal_marker_dir(&home))
            .into_iter()
            .flatten()
            .flatten()
            .count();
        assert_eq!(
            leftover, 0,
            "clean release must clear this worktree's marker + lock"
        );

        std::fs::remove_dir_all(&home).ok();
        if let Some(root) = super_repo.parent() {
            std::fs::remove_dir_all(root).ok();
        }
        std::fs::remove_dir_all(&super_repo).ok();
    }

    /// (8) The refusal notice de-dups per (worktree, nested-status) and re-notifies
    /// on a status TRANSITION: A,A ⇒ 1 msg; then B ⇒ 2; then A again ⇒ 3.
    #[cfg(unix)]
    #[test]
    fn nested_notice_dedups_and_transition_renotifies() {
        let home = tmp_home("notice-dedup");
        let wt = tmp_repo("notice-dedup-wt"); // any stable path for the marker key
        let notify = |status: &str| {
            notify_unpreservable_nested_dirty(&home, "agent1", "feat/x", &wt, status, None)
        };
        notify("nested-A");
        notify("nested-A");
        assert_eq!(
            inbox_count_from(&home, "general", NESTED_FROM),
            1,
            "identical status must be notified once"
        );
        notify("nested-B");
        assert_eq!(
            inbox_count_from(&home, "general", NESTED_FROM),
            2,
            "a changed nested status must re-notify"
        );
        notify("nested-A");
        assert_eq!(
            inbox_count_from(&home, "general", NESTED_FROM),
            3,
            "A→B→A: returning to A differs from the last-seen B ⇒ re-notify"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&wt).ok();
    }

    /// (9) Two threads racing the SAME (worktree, status) notice ⇒ EXACTLY one
    /// inbox message (the per-worktree file lock serializes claim+enqueue+marker).
    #[cfg(unix)]
    #[test]
    fn nested_notice_concurrent_claim_single_notify() {
        let home = tmp_home("notice-concurrent");
        let wt = tmp_repo("notice-concurrent-wt");
        std::thread::scope(|s| {
            for _ in 0..2 {
                let home = &home;
                let wt = &wt;
                s.spawn(move || {
                    notify_unpreservable_nested_dirty(
                        home,
                        "agent1",
                        "feat/x",
                        wt,
                        "same-status",
                        None,
                    );
                });
            }
        });
        assert_eq!(
            inbox_count_from(&home, "general", NESTED_FROM),
            1,
            "concurrent identical notices must collapse to one message"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&wt).ok();
    }

    /// (10) `enumerate_nested_dirty` handles a nested tracked file whose name has a
    /// SPACE (porcelain-v2 `-z` NUL parsing) — the exact path appears in the output.
    #[cfg(unix)]
    #[test]
    fn nested_dirty_enumeration_includes_paths_with_spaces() {
        let home = tmp_home("enum-spaces");
        let super_repo = tmp_super_one_sub_file("enum-spaces", "my file.txt");
        let info = create(&home, &super_repo, "agent1", Some("feat/space")).expect("worktree");
        let spaced = info.path.join("vendor/dep/my file.txt");
        assert!(spaced.is_file(), "fixture: spaced submodule file present");
        std::fs::write(&spaced, b"dirtied\n").unwrap();

        let listing = enumerate_nested_dirty(&info.path);
        assert!(
            listing.contains("my file.txt"),
            "enumeration must preserve the spaced nested path: {listing:?}"
        );
        assert!(
            listing.contains("vendor/dep"),
            "enumeration must name the dirty submodule: {listing:?}"
        );

        std::fs::remove_dir_all(&home).ok();
        if let Some(root) = super_repo.parent() {
            std::fs::remove_dir_all(root).ok();
        }
        std::fs::remove_dir_all(&super_repo).ok();
    }

    /// P0 (codex R2): `submodule.<name>.ignore=all` — set in EITHER `.git/config`
    /// OR a committed `.gitmodules` — makes a PLAIN `git status` HIDE submodule
    /// working-tree dirt. Without `--ignore-submodules=none` the safety classifier
    /// reads the worktree as Clean and would REMOVE it, silently losing the nested
    /// WIP. Both sources must be OVERRIDDEN: config-hidden nested dirt must still
    /// classify `UnpreservableNestedDirty` and refuse release.
    #[cfg(unix)]
    #[test]
    fn preserve_refuses_config_ignored_submodule_dirt() {
        for (label, use_gitmodules) in [("repo-config", false), ("gitmodules", true)] {
            let home = tmp_home(&format!("cfg-ignore-{label}"));
            let super_repo = tmp_super_one_sub(&format!("cfg-ignore-{label}"));
            // Configure submodule-ignore=all BEFORE leasing, from the requested
            // source. `.gitmodules` is COMMITTED so the file itself stays clean
            // (else it would be preservable parent dirt, defeating the fixture).
            if use_gitmodules {
                git_run_ok(
                    &super_repo,
                    &[
                        "config",
                        "-f",
                        ".gitmodules",
                        "submodule.vendor/dep.ignore",
                        "all",
                    ],
                    false,
                );
                git_run_ok(&super_repo, &["add", ".gitmodules"], false);
                git_run_ok(&super_repo, &["commit", "-m", "gitmodules ignore=all"], false);
            } else {
                git_run_ok(
                    &super_repo,
                    &["config", "submodule.vendor/dep.ignore", "all"],
                    false,
                );
            }
            let info = create(&home, &super_repo, "agent1", Some("feat/cfgig")).expect("worktree");
            crate::binding::bind_full(&home, "agent1", "", "feat/cfgig", &info.path, &super_repo, false)
                .expect("bind");
            let vendored = info.path.join("vendor/dep/vendored.txt");
            assert!(vendored.is_file(), "{label}: fixture submodule file present");
            std::fs::write(&vendored, b"DIRTY-nested-edit\n").unwrap();

            // Mechanism: plain status is BLIND to the dirt; forced status reveals it.
            let plain = git_out(&info.path, &["status", "--porcelain"]);
            assert!(
                plain.is_empty(),
                "{label}: precondition — ignore=all hides dirt from PLAIN status: {plain:?}"
            );
            let forced = git_out(
                &info.path,
                &[
                    "--no-optional-locks",
                    "status",
                    "--porcelain",
                    "--ignore-submodules=none",
                ],
            );
            assert!(
                forced.contains("vendor/dep"),
                "{label}: --ignore-submodules=none must reveal the dirt: {forced:?}"
            );

            // Direct classification MUST refuse (not falsely Clean → remove).
            let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/cfgig", None);
            assert_eq!(
                pres_kind(&outcome),
                "UnpreservableNestedDirty",
                "{label}: config-hidden submodule dirt must refuse, not read Clean"
            );
            assert!(
                outcome.blocked_reason().is_some(),
                "{label}: refusal must be fail-closed (blocked_reason Some)"
            );
            assert!(
                recovery_ref_names(&super_repo, "feat/cfgig").is_empty(),
                "{label}: no recovery ref may be minted for unpreservable nested dirt"
            );

            // Integration: release_full must refuse and RETAIN the worktree.
            let out = crate::worktree_pool::release_full(&home, "agent1", false);
            assert!(!out.released, "{label}: release must be refused");
            assert!(!out.worktree_removed, "{label}: worktree must not be removed");
            assert!(info.path.exists(), "{label}: worktree dir retained for recovery");

            std::fs::remove_dir_all(&home).ok();
            if let Some(root) = super_repo.parent() {
                std::fs::remove_dir_all(root).ok();
            }
            std::fs::remove_dir_all(&super_repo).ok();
        }
    }

    /// P1 (codex R2): a plain `git status` opportunistically REFRESHES the stat
    /// cache and REWRITES the live index; `git write-tree` persists the cache-tree.
    /// `preserve_dirty_worktree` must leave the LIVE index BYTE-IDENTICAL. Setup
    /// pre-persists the cache-tree (a `write-tree` in-fixture) so the ONLY residual
    /// mutator is the plain-status stat refresh that `--no-optional-locks` removes.
    /// `a` is made STAT-dirty (mtime changed, content identical) to arm that refresh;
    /// `b` carries a distinct STAGED change so preserve runs its full classify path.
    #[cfg(unix)]
    #[test]
    fn preserve_leaves_live_index_bytes_identical() {
        let home = tmp_home("index-identity");
        let repo = tmp_repo_with_file("index-identity", "a", "va\n");
        std::fs::write(repo.join("b"), "vb\n").unwrap();
        git_run_ok(&repo, &["add", "b"], false);
        git_run_ok(&repo, &["commit", "-m", "add b"], false);
        commit_marker_gitignore(&repo);
        let info = create(&home, &repo, "agent1", Some("feat/idx")).expect("worktree");

        // Stage a distinct change to `b` (index != HEAD ⇒ full preserve path).
        std::fs::write(info.path.join("b"), "vb2\n").unwrap();
        git_run_ok(&info.path, &["add", "b"], false);
        // Arm the stat-cache refresh: bump `a`'s mtime far into the past, content
        // unchanged. A plain `status` would then refresh + rewrite the index.
        let fa = std::fs::OpenOptions::new()
            .write(true)
            .open(info.path.join("a"))
            .unwrap();
        fa.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(946_684_800))
            .unwrap();
        drop(fa);
        // Pre-persist the cache-tree so `write-tree` inside preserve is a pure no-op;
        // this isolates the status stat-refresh as the sole index mutator under test.
        let _ = git_out(&info.path, &["write-tree"]);

        let git_dir = git_out(&info.path, &["rev-parse", "--absolute-git-dir"]);
        let index_path = Path::new(&git_dir).join("index");
        let before = std::fs::read(&index_path).expect("read index before");

        let outcome = preserve_dirty_worktree(&home, "agent1", &info.path, "feat/idx", None);
        assert_eq!(
            pres_kind(&outcome),
            "Preserved",
            "staged change is real, preservable parent WIP"
        );

        let after = std::fs::read(&index_path).expect("read index after");
        assert_eq!(
            before, after,
            "preserve must NOT mutate the live index (byte-identical)"
        );
        // And the staged change genuinely survived (non-destructive read-only path).
        assert_eq!(
            git_out(&info.path, &["show", ":b"]),
            "vb2",
            "staged content must remain in the live index"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// P2 (codex R2): `clear_nested_refusal_marker` must remove ONLY the marker and
    /// KEEP the `.lock` inode durable — flock is per-inode, so unlinking the lock a
    /// concurrent notice may hold breaks serialization. Assert the marker is gone,
    /// the `.lock` remains, and the notice path still serializes (re-notifies once,
    /// then de-dups) afterwards — proving the lock path is intact.
    #[cfg(unix)]
    #[test]
    fn clear_nested_refusal_marker_keeps_lock_and_serializes() {
        let home = tmp_home("clear-keeps-lock");
        let wt = tmp_repo("clear-keeps-lock-wt");
        let dir = refusal_marker_dir(&home);
        let has_lock = || {
            std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("lock"))
        };
        let markers = || {
            std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) != Some("lock"))
                .count()
        };

        // Drive one refusal notice ⇒ writes marker + lock.
        notify_unpreservable_nested_dirty(&home, "agent1", "feat/x", &wt, "status-A", None);
        assert_eq!(markers(), 1, "notify writes exactly one refusal marker");
        assert!(has_lock(), "notify creates the per-worktree lock file");

        clear_nested_refusal_marker(&home, &wt);
        assert_eq!(markers(), 0, "clear must remove the marker");
        assert!(
            has_lock(),
            "clear must KEEP the .lock inode durable (never unlink the held lock)"
        );

        // The lock path still serializes: identical status after clear re-notifies
        // exactly once (marker gone), then de-dups (marker re-created under lock).
        let base = inbox_count_from(&home, "general", NESTED_FROM);
        notify_unpreservable_nested_dirty(&home, "agent1", "feat/x", &wt, "status-A", None);
        assert_eq!(
            inbox_count_from(&home, "general", NESTED_FROM),
            base + 1,
            "after clear, the same status must notify once more"
        );
        notify_unpreservable_nested_dirty(&home, "agent1", "feat/x", &wt, "status-A", None);
        assert_eq!(
            inbox_count_from(&home, "general", NESTED_FROM),
            base + 1,
            "an immediate identical repeat must de-dup (lock+marker path intact)"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&wt).ok();
    }

    /// Bug1 middle fallback: no caller but the agent belongs to a team → route to
    /// that team's ORCHESTRATOR, not the hardcoded `general`.
    #[test]
    fn wip_notice_recipient_no_caller_uses_team_orchestrator() {
        let home = tmp_home("wip-recipient-team");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "teams:\n  gapfix:\n    members: [agent1, lead-y]\n    orchestrator: lead-y\n",
        )
        .expect("write fleet.yaml");
        assert_eq!(
            wip_notice_recipient(&home, "agent1", None),
            "lead-y",
            "no caller but a team → the team orchestrator, not hardcoded general"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Bug2 (#t-…61315-2): the `[system:…]` marker is added exactly once by the
    /// notify layer (`NotifySource::System`); the body builder must NOT embed a
    /// second copy — else the delivered message double-prefixes.
    #[test]
    fn wip_preserved_notice_has_no_embedded_marker() {
        let notice = wip_preserved_notice("agent1", "feat/x", "refs/agend-wip/agent1/x-1", false);
        assert!(
            !notice.contains("[system:"),
            "notice body must not embed a [system:…] marker (notify layer adds it): {notice}"
        );
        // Single-parent: no staged-snapshot sentence. Dual-parent: mentions `^2`.
        assert!(
            !notice.contains("^2"),
            "single-parent notice must not mention ^2"
        );
        let dual = wip_preserved_notice("agent1", "feat/x", "refs/agend-wip/agent1/x-1", true);
        assert!(
            dual.contains("refs/agend-wip/agent1/x-1^2"),
            "dual-parent notice must point at the staged snapshot ref^2: {dual}"
        );
    }

    /// Seed a recovery ref dated `days_ago` (distinct days → distinct names).
    fn seed_recovery_ref(repo: &Path, branch: &str, days_ago: i64) -> String {
        let ts = (chrono::Utc::now() - chrono::Duration::days(days_ago))
            .format("%Y%m%dT%H%M%SZ")
            .to_string();
        let name = format!("refs/agend/recovery/{branch}/{ts}");
        git_run(repo, &["update-ref", &name, "HEAD"]);
        name
    }

    #[test]
    fn prune_recovery_refs_enforces_per_branch_cap() {
        let repo = tmp_repo("prune-cap");
        let branch = "feat/prune-cap";
        // 5 recent refs (all within TTL); each day is a distinct date so no ts
        // collision. names[0] = day-1 (newest) … names[4] = day-5 (oldest). Capture
        // the returned names and assert on THEM (never recompute ts from `now()` —
        // seed vs assert straddling a second boundary would spuriously mismatch).
        let names: Vec<String> = [1, 2, 3, 4, 5]
            .iter()
            .map(|&d| seed_recovery_ref(&repo, branch, d))
            .collect();
        assert_eq!(recovery_ref_names(&repo, branch).len(), 5, "seeded 5");
        prune_recovery_refs(&repo, branch);
        let survivors = recovery_ref_names(&repo, branch);
        assert_eq!(survivors.len(), 3, "cap=3 enforced: {survivors:?}");
        for keep in &names[0..3] {
            assert!(survivors.contains(keep), "newest ref must survive: {keep}");
        }
        for gone in &names[3..5] {
            assert!(
                !survivors.contains(gone),
                "over-cap ref must be pruned: {gone}"
            );
        }
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn prune_recovery_refs_ttl_deletes_expired_within_cap() {
        let repo = tmp_repo("prune-ttl");
        let branch = "feat/prune-ttl";
        let recent = seed_recovery_ref(&repo, branch, 1);
        let _expired = seed_recovery_ref(&repo, branch, 15); // > 14d TTL
        prune_recovery_refs(&repo, branch);
        let survivors = recovery_ref_names(&repo, branch);
        assert_eq!(
            survivors,
            vec![recent],
            "expired (>14d) ref pruned even under the per-branch cap: {survivors:?}"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn recovery_ref_expired_parses_timestamp() {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(RECOVERY_TTL_DAYS);
        assert!(
            recovery_ref_expired("refs/agend/recovery/b/20200101T000000Z", cutoff),
            "a year-2020 ref is well past the 14d cutoff"
        );
        let now_ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        assert!(
            !recovery_ref_expired(&format!("refs/agend/recovery/b/{now_ts}"), cutoff),
            "a just-created ref is not expired"
        );
        assert!(
            !recovery_ref_expired("refs/agend/recovery/b/not-a-timestamp", cutoff),
            "unparseable name is fail-safe (NOT expired)"
        );
    }

    #[test]
    fn test_reuse_existing_worktree() {
        let home = tmp_home("reuse");
        let repo = tmp_repo("reuse");
        let info1 = create(&home, &repo, "agent1", None);
        assert!(info1.is_some());
        let info2 = create(&home, &repo, "agent1", None);
        assert!(info2.is_some());
        assert_eq!(info1.expect("i1").path, info2.expect("i2").path);
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn test_non_git_returns_none() {
        let home = tmp_home("nongit");
        let dir = std::env::temp_dir().join(format!("agend-wt-test-nongit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        assert!(create(&home, &dir, "agent1", None).is_none());
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_custom_branch() {
        let home = tmp_home("custom_branch");
        let repo = tmp_repo("custom_branch");
        let info = create(&home, &repo, "agent1", Some("my-feature"));
        assert!(info.is_some());
        assert_eq!(info.expect("i").branch, "my-feature");
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn test_list_residual() {
        let home = tmp_home("residual");
        let repo = tmp_repo("residual");
        create(&home, &repo, "agent1", None);
        create(&home, &repo, "agent2", None);
        // Sprint 57 Wave 4 (#546 Item 4): list_residual now scans the
        // CENTRAL `$AGEND_HOME/worktrees/` location (repo-independent).
        let residual = list_residual(&home);
        assert_eq!(residual.len(), 2);
        assert!(residual.contains(&"agent1".to_string()));
        assert!(residual.contains(&"agent2".to_string()));
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn test_empty_repo_gets_initial_commit() {
        // git init without any commit — should auto-create initial commit
        let home = tmp_home("empty");
        let dir = std::env::temp_dir().join(format!(
            "agend-wt-test-empty-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).ok();
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
            .output()
            .ok();
        // No commit — HEAD is invalid
        assert!(!has_commits(&dir));
        // create() should handle this gracefully
        let info = create(&home, &dir, "agent1", None);
        assert!(info.is_some(), "worktree should be created in empty repo");
        assert!(has_commits(&dir), "initial commit should exist now");
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    // `test_validate_branch_valid` + `test_validate_branch_rejects` migrated
    // to `src/agent_ops.rs::tests` as part of Task #9 Option C epilogue — the
    // `validate_branch` fn itself lives in `agent_ops.rs` now, so tests are
    // colocated with their subject.

    #[test]
    #[allow(clippy::unwrap_used)]
    fn checkout_branch_creates_new_branch() {
        let dir = std::env::temp_dir().join(format!("agend-wt-checkout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(&dir)
            .output()
            .unwrap();

        // Checkout a new branch
        assert!(checkout_branch(&dir, "feat/test-branch").is_ok());

        // Verify we're on the new branch
        let output = std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["branch", "--show-current"])
            .current_dir(&dir)
            .output()
            .unwrap();
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert_eq!(branch, "feat/test-branch");

        std::fs::remove_dir_all(&dir).ok();
    }

    // ── P0-1.6: actual HEAD verification on reuse ─────────────────────

    /// Smoke test 2 regression: same agent, different branch → must reject.
    /// Pre-fix this returned Some with `branch = requested`, falsely echoing
    /// the requested branch back even though the worktree HEAD was unchanged.
    ///
    /// Sprint 57 Wave 4 (#546 Item 4): the new external layout puts each
    /// (agent, branch) at a distinct path, so a different branch creates a
    /// different worktree dir. The "reject on mismatch" semantic still
    /// applies WHEN the same path is reused — but with branch in the path,
    /// the second `create` lands at a NEW location and the conflict check
    /// (which fires only when `wt_dir.exists()`) doesn't trigger. Pin the
    /// updated semantic: same-agent-different-branch creates a SECOND
    /// worktree at the second branch's path, leaving the first untouched.
    #[test]
    fn reuse_rejects_when_branch_mismatch() {
        let home = tmp_home("reuse-mismatch");
        let repo = tmp_repo("reuse-mismatch");
        let first = create(&home, &repo, "agent1", Some("feat/A")).expect("first lease");
        assert!(first.path.exists());
        // Second lease, same instance, DIFFERENT branch → lands at a
        // distinct path under the new layout; the first remains intact.
        let second = create(&home, &repo, "agent1", Some("feat/B"));
        assert!(
            second.is_some(),
            "Wave 4: same agent on a different branch lands at a distinct path"
        );
        let second = second.expect("second lease");
        assert_ne!(
            first.path, second.path,
            "different-branch worktrees must occupy different paths"
        );
        assert!(first.path.exists(), "first worktree must remain intact");
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Idempotent path: same agent, same custom branch → reuse OK.
    /// Confirms the actual-HEAD check does not break the idempotent re-lease
    /// semantics that P0-1.5 relies on.
    #[test]
    fn reuse_idempotent_same_custom_branch() {
        let home = tmp_home("reuse-idem");
        let repo = tmp_repo("reuse-idem");
        let first = create(&home, &repo, "agent1", Some("feat/X")).expect("first lease");
        let second =
            create(&home, &repo, "agent1", Some("feat/X")).expect("second lease idempotent");
        assert_eq!(first.path, second.path);
        assert_eq!(second.branch, "feat/X");
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    // ── #2010 2b: clean-guarded detached-HEAD reattach on reuse ──────────

    /// Commit a `.gitignore` that ignores the `.agend-managed` lease marker.
    /// `create()` writes that marker into every worktree (worktree.rs ~255), and
    /// every REAL source repo gitignores it (this repo's own .gitignore line 29),
    /// so production worktrees read CLEAN. Without it the marker shows as an
    /// untracked `??` and a freshly-created worktree would falsely read "dirty" —
    /// the fixture must represent production (representative-fixture rule). Adds
    /// one commit on top of `tmp_repo`'s init, before any worktree is created.
    fn commit_marker_gitignore(repo: &std::path::Path) {
        std::fs::write(repo.join(".gitignore"), ".agend-managed\n").unwrap();
        for args in [
            vec!["add", ".gitignore"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "gitignore marker",
            ],
        ] {
            std::process::Command::new("git")
                .env("AGEND_GIT_BYPASS", "1")
                .args(&args)
                .current_dir(repo)
                .output()
                .expect("git");
        }
    }

    /// Detach the worktree's HEAD (the `git branch --show-current` ⇒ `Some("")`
    /// shape the issue describes — e.g. a reviewer's detached `repo checkout`).
    fn detach_head(wt: &std::path::Path) {
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["checkout", "--detach", "HEAD"])
            .current_dir(wt)
            .output()
            .expect("detach HEAD");
    }

    /// §3.9: a CLEAN detached worktree is reattached to the requested branch and
    /// REUSED — pre-#2010 the empty `branch --show-current` mismatched and
    /// returned None (LeaseConflict), forcing a manual release before re-dispatch.
    #[test]
    fn reuse_reattaches_clean_detached_worktree_2010() {
        let home = tmp_home("reattach-clean");
        let repo = tmp_repo("reattach-clean");
        commit_marker_gitignore(&repo); // representative: marker is gitignored in prod
        let first = create(&home, &repo, "agent1", Some("feat/X")).expect("first lease");
        // Sanity: it really is on feat/X before we detach.
        detach_head(&first.path);
        let cur = std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["branch", "--show-current"])
            .current_dir(&first.path)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&cur.stdout).trim().is_empty(),
            "precondition: HEAD is detached (empty show-current)"
        );

        // Re-lease the same (agent, branch): clean-guarded reattach → reuse.
        let second = create(&home, &repo, "agent1", Some("feat/X"))
            .expect("clean detached worktree must reattach + reuse (#2010 2b)");
        assert_eq!(second.path, first.path, "same worktree reused");
        assert_eq!(second.branch, "feat/X");
        // HEAD is back on the requested branch.
        let after = std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["branch", "--show-current"])
            .current_dir(&second.path)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&after.stdout).trim(),
            "feat/X",
            "reattach must put HEAD back on the requested branch"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// §3.9: a DIRTY detached worktree still conflicts (returns None) — the
    /// clean-guard protects in-flight review WIP, unchanged from pre-#2010.
    #[test]
    fn reuse_rejects_dirty_detached_worktree_2010() {
        let home = tmp_home("reattach-dirty");
        let repo = tmp_repo("reattach-dirty");
        commit_marker_gitignore(&repo); // representative: marker is gitignored in prod
        let first = create(&home, &repo, "agent1", Some("feat/X")).expect("first lease");
        detach_head(&first.path);
        // A REAL uncommitted change (not the gitignored marker) → dirty.
        std::fs::write(first.path.join("wip.txt"), "review notes in progress").unwrap();
        assert!(
            has_uncommitted_changes(&first.path),
            "precondition: worktree is dirty"
        );

        let second = create(&home, &repo, "agent1", Some("feat/X"));
        assert!(
            second.is_none(),
            "dirty detached worktree must still conflict (protect review WIP)"
        );
        // And the WIP is untouched.
        assert!(
            first.path.join("wip.txt").exists(),
            "the dirty WIP file must be left intact"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    // ── #2115: force-sync reused worktree to HEAD (review-integrity) ─────────

    fn git_out(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn git_run(dir: &std::path::Path, args: &[&str]) {
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git");
    }

    /// #2115 (r6 #2196/#2223 repro): when the branch ref is fast-forwarded
    /// (#869 `update-ref`) between leases, the reused worktree's HEAD points at
    /// the new SHA but its index + working tree are stale → DIRTY on hand-off.
    /// The same-branch reuse path must force-sync to HEAD so the new occupant
    /// gets a clean tree at the current SHA (else reviewers run on a polluted
    /// tree → false verdicts).
    #[test]
    fn reuse_syncs_stale_worktree_to_head_after_ref_advance_2115() {
        let home = tmp_home("sync-on-reuse");
        let repo = tmp_repo("sync-on-reuse");
        commit_marker_gitignore(&repo); // representative: marker gitignored in prod

        // First lease lands the worktree on feat/X at c1.
        let first = create(&home, &repo, "agent1", Some("feat/X")).expect("first lease");
        let wt = first.path.clone();

        // Advance feat/X to a NEW commit c2 WITHOUT touching the worktree's tree
        // — exactly what ensure_branch_exists (#869) does via `update-ref`. Build
        // c2 on the repo's own checkout, then repoint the branch ref at it.
        std::fs::write(repo.join("feature.txt"), "c2-content\n").unwrap();
        git_run(&repo, &["add", "feature.txt"]);
        git_run(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "c2",
            ],
        );
        let c2 = git_out(&repo, &["rev-parse", "HEAD"]);
        git_run(&repo, &["update-ref", "refs/heads/feat/X", &c2]);

        // The worktree is now stale (HEAD=c2 via the symref, tree=c1) → dirty —
        // and add a stray untracked file to prove `clean -fd` runs too.
        std::fs::write(wt.join("scratch.txt"), "stray").unwrap();
        assert!(
            has_uncommitted_changes(&wt),
            "precondition: reused worktree is dirty after ref advance"
        );
        assert_eq!(
            git_out(&wt, &["branch", "--show-current"]),
            "feat/X",
            "precondition: HEAD symref still on feat/X (update-ref does not detach)"
        );

        // Re-lease the same (agent, branch): same-branch reuse → force-sync.
        let second = create(&home, &repo, "agent1", Some("feat/X")).expect("reuse lease");
        assert_eq!(second.path, wt, "same worktree reused");

        // The tree is now CLEAN at the advanced HEAD (c2).
        assert_eq!(
            git_out(&wt, &["status", "--porcelain"]),
            "",
            "worktree must be clean after sync-on-reuse"
        );
        assert_eq!(
            git_out(&wt, &["rev-parse", "HEAD"]),
            c2,
            "HEAD must be the advanced commit c2"
        );
        let feature = std::fs::read_to_string(wt.join("feature.txt")).expect("feature.txt synced");
        // trim_end: Windows git checkout rewrites the LF to CRLF (`c2-content\r\n`)
        // — assert on content, not the platform line ending.
        assert_eq!(
            feature.trim_end(),
            "c2-content",
            "tracked content synced to HEAD (c2)"
        );
        assert!(
            !wt.join("scratch.txt").exists(),
            "untracked stray file must be removed by clean -fd"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    // ─────────────────────────────────────────────────────────────
    // Sprint 57 Wave 4 (#546 Item 4) — path layout invariants.
    // ─────────────────────────────────────────────────────────────

    #[test]
    fn worktree_path_resolves_to_agend_terminal_external_location() {
        // Pin the canonical layout: `<home>/worktrees/<agent>/<branch>/`.
        let home = std::path::Path::new("/test/home");
        let path = worktree_path(home, "dev", "feat/track-x");
        assert_eq!(
            path,
            std::path::Path::new("/test/home/worktrees/dev/feat/track-x")
        );
    }

    #[test]
    fn worktree_path_handles_simple_branch_without_slash() {
        let home = std::path::Path::new("/test/home");
        let path = worktree_path(home, "dev", "feat-test");
        assert_eq!(
            path,
            std::path::Path::new("/test/home/worktrees/dev/feat-test")
        );
    }

    #[test]
    fn path_layout_invariant_against_regression() {
        // Regression-proof: ensure the new path is NOT under the
        // source repo. This is the load-bearing invariant Wave 4
        // ships — re-introducing `<repo>/.worktrees/<agent>/` as the
        // production path would silently undo the migration.
        let home = std::env::temp_dir().join(format!(
            "agend-wt-invariant-home-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let repo = std::env::temp_dir().join(format!(
            "agend-wt-invariant-repo-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let path = worktree_path(&home, "agent-x", "feat-x");
        assert!(
            path.starts_with(&home),
            "new layout MUST live under home, got: {}",
            path.display()
        );
        assert!(
            !path.starts_with(&repo),
            "new layout MUST NOT live under source_repo, got: {}",
            path.display()
        );
        let path_str = path.display().to_string();
        assert!(
            !path_str.contains(".worktrees"),
            "Wave 4: path must NOT contain `.worktrees` (legacy layout marker), got: {}",
            path_str
        );
    }

    #[test]
    fn list_residual_scans_central_worktrees_dir_not_legacy() {
        // Defensive: list_residual MUST scan `<home>/worktrees/`, not
        // `<repo>/.worktrees/`. Plant entries in BOTH locations and
        // verify only the central one is reported.
        let home = tmp_home("residual-scan");
        let repo = tmp_repo("residual-scan");

        // Central (new layout) — should be reported.
        std::fs::create_dir_all(home.join("worktrees").join("dev").join("feat-a")).unwrap();
        std::fs::create_dir_all(home.join("worktrees").join("lead").join("main-mirror")).unwrap();

        // Legacy (old layout) entry on disk — must NOT be reported by
        // list_residual (which only scans the central new layout).
        std::fs::create_dir_all(repo.join(".worktrees").join("ghost-agent")).unwrap();

        let new_residual = list_residual(&home);
        assert_eq!(
            new_residual.len(),
            2,
            "central scan must surface both new-layout entries, got: {new_residual:?}"
        );
        assert!(new_residual.contains(&"dev".to_string()));
        assert!(new_residual.contains(&"lead".to_string()));
        assert!(
            !new_residual.contains(&"ghost-agent".to_string()),
            "legacy entries must NOT be reported by central scan"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    fn mk_resolved(
        working_directory: PathBuf,
        source_repo: Option<PathBuf>,
        git_branch: Option<String>,
        worktree: Option<bool>,
    ) -> crate::fleet::ResolvedInstance {
        crate::fleet::ResolvedInstance {
            name: "agent".into(),
            backend: crate::backend::Backend::ClaudeCode,
            backend_command: "claude".into(),
            args: vec![],
            env: std::collections::HashMap::new(),
            working_directory: Some(working_directory),
            ready_pattern: None,
            submit_key: "\r".into(),
            role: None,
            cols: None,
            rows: None,
            topic_id: None,
            git_branch,
            model: None,
            worktree,
            instructions: None,
            source_repo,
            repo: None,
        }
    }

    /// §3.9 (b)+(c) (#1858): the shared auto-worktree gate must (b) still create
    /// a worktree for an EXPLICIT real-repo `working_directory` + `source_repo`
    /// (no over-kill of legitimate opt-in), and (c) SKIP the daemon-managed
    /// default `workspace/<name>` dir even when it has been git-init'd and
    /// `source_repo` is set (the deploy non-branch shape — `deployments.rs`
    /// writes exactly `source_repo` + a `workspace/<name>` working_directory).
    #[test]
    fn resolve_auto_worktree_skips_workspace_default_allows_explicit_repo_1858() {
        // (b) explicit real repo as working_directory → worktree still created.
        let home_b = tmp_repo("1858-b-home");
        let repo = tmp_repo("1858-b-repo");
        let resolved_b = mk_resolved(repo.clone(), Some(repo.clone()), None, None);
        let got_b = resolve_auto_worktree(&home_b, "agent", &resolved_b);
        assert!(
            got_b
                .as_ref()
                .is_some_and(|p| p.to_string_lossy().contains("worktrees")),
            "#1858 (b): explicit real-repo working_directory must still auto-worktree, got {got_b:?}"
        );

        // (c) deploy non-branch shape: source_repo set + working_directory is the
        // default workspace dir (even git-init'd) → NO worktree.
        let home_c = tmp_repo("1858-c-home");
        let work_dir = crate::paths::workspace_dir(&home_c).join("team-dev");
        std::fs::create_dir_all(&work_dir).unwrap();
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["init", "-b", "main"])
            .current_dir(&work_dir)
            .output()
            .ok();
        assert!(is_git_repo(&work_dir), "fixture: workspace dir git-init'd");
        let resolved_c = mk_resolved(work_dir.clone(), Some(home_c.join("realrepo")), None, None);
        assert!(
            resolve_auto_worktree(&home_c, "team-dev", &resolved_c).is_none(),
            "#1858 (c): deploy non-branch (source_repo + default workspace dir) must not auto-worktree"
        );

        // (d) #1919 team-deploy: the per-instance default NESTED under a team subdir
        // (`<home>/workspace/<team>/<instance>`). The old exact `== workspace/<name>`
        // check missed this (workspace/member1 ≠ workspace/myteam/member1), so the
        // git-init'd default fell through to auto-worktree and broke `claude
        // --continue` session resume on restart. The `starts_with` gate catches the
        // whole `workspace/` subtree. (This case FAILS on the pre-#1919 exact match.)
        let home_d = tmp_repo("1919-d-home");
        let nested = crate::paths::workspace_dir(&home_d)
            .join("myteam")
            .join("member1");
        std::fs::create_dir_all(&nested).unwrap();
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["init", "-b", "main"])
            .current_dir(&nested)
            .output()
            .ok();
        assert!(
            is_git_repo(&nested),
            "fixture: team-nested workspace dir git-init'd"
        );
        let resolved_d = mk_resolved(nested.clone(), Some(home_d.join("realrepo")), None, None);
        assert!(
            resolve_auto_worktree(&home_d, "member1", &resolved_d).is_none(),
            "#1919 (d): team-nested default workspace (workspace/<team>/<instance>) must not auto-worktree"
        );

        for d in [home_b, repo, home_c, home_d] {
            std::fs::remove_dir_all(&d).ok();
        }
    }

    /// #2234 cure-(B): with the flag OFF (default), a default workspace dir
    /// resolves to `None` exactly as pre-(B) — byte-identical, no reconcile.
    #[test]
    fn resolve_auto_worktree_flag_off_workspace_none_2234() {
        let _flag = crate::worktree_pool::workspace_worktree_test_seam::force(false);
        let home = tmp_repo("2234-off-home");
        let repo = tmp_repo("2234-off-repo");
        let ws = crate::paths::workspace_dir(&home).join("agent");
        let resolved = mk_resolved(ws.clone(), Some(repo.clone()), None, None);
        assert!(
            resolve_auto_worktree(&home, "agent", &resolved).is_none(),
            "flag OFF → workspace stays a non-worktree (byte-identical)"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// #2234 cure-(B): with the flag ON + a `source_repo`, the gate reconciles
    /// the workspace dir into a worktree and returns that SAME path (stable cwd).
    #[test]
    fn resolve_auto_worktree_flag_on_workspace_reconciles_2234() {
        let home = tmp_repo("2234-on-home");
        let repo = tmp_repo("2234-on-repo");
        let ws = crate::paths::workspace_dir(&home).join("agent");
        let resolved = mk_resolved(ws.clone(), Some(repo.clone()), None, None);

        // Thread-local seam (not process-global set_var) → no cross-test leak.
        let got = {
            let _flag = crate::worktree_pool::workspace_worktree_test_seam::force(true);
            resolve_auto_worktree(&home, "agent", &resolved)
        };

        assert_eq!(
            got.as_deref(),
            Some(ws.as_path()),
            "flag ON → gate returns the workspace path itself (cwd == worktree)"
        );
        assert!(
            ws.join(".git").is_file(),
            "workspace reconciled into a gitlink worktree"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    // ── #2234 Phase 2: remove_worktree binding-driven (destroy-work-safe) ──
    fn write_test_binding(home: &Path, agent: &str, branch: &str, worktree: &Path) {
        let dir = crate::paths::runtime_dir(home).join(agent);
        std::fs::create_dir_all(&dir).unwrap();
        let v = serde_json::json!({
            "version": 1, "agent": agent, "task_id": "T-test",
            "branch": branch, "worktree": worktree.display().to_string(),
        });
        std::fs::write(
            dir.join("binding.json"),
            serde_json::to_string_pretty(&v).unwrap(),
        )
        .unwrap();
    }

    /// ① OFF/legacy: the derived `worktrees/<agent>/<branch>` exists → resolve to
    /// it (byte-identical with the pre-#2234 behavior).
    #[test]
    fn resolve_removable_derived_exists_off_byte_identical_2234() {
        let home = tmp_home("rrw-derived");
        let derived = worktree_path(&home, "dev", "fix/x");
        std::fs::create_dir_all(&derived).unwrap();
        assert_eq!(
            resolve_removable_worktree(&home, "dev", "fix/x"),
            Some(derived)
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// ② cure-(B): derived path gone, binding bound to the SAME branch → resolve
    /// to the binding's `workspace/<agent>` worktree.
    #[test]
    fn resolve_removable_b_same_branch_uses_binding_2234() {
        let home = tmp_home("rrw-b-same");
        let ws = crate::paths::workspace_dir(&home).join("devb");
        std::fs::create_dir_all(&ws).unwrap();
        write_test_binding(&home, "devb", "feat/y", &ws);
        assert_eq!(
            resolve_removable_worktree(&home, "devb", "feat/y"),
            Some(ws)
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// ③ branch-mismatch (the destroy-work guard): derived gone + binding bound
    /// to a DIFFERENT branch → None. A stale `remove(branchX)` after the agent
    /// rebound to branchY must NOT resolve (and thus must not delete) the live
    /// branchY workspace.
    #[test]
    fn resolve_removable_branch_mismatch_is_noop_no_destroy_2234() {
        let home = tmp_home("rrw-mismatch");
        let ws = crate::paths::workspace_dir(&home).join("devm");
        std::fs::create_dir_all(&ws).unwrap();
        write_test_binding(&home, "devm", "feat/Y", &ws);
        assert_eq!(
            resolve_removable_worktree(&home, "devm", "feat/X"),
            None,
            "#2234: stale remove(branchX) after rebind to branchY must NOT resolve the live branchY workspace"
        );
        assert!(
            ws.exists(),
            "the live workspace must be untouched by resolution"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// ④ derive-miss + no binding → None (already gone).
    #[test]
    fn resolve_removable_no_binding_is_noop_2234() {
        let home = tmp_home("rrw-none");
        assert_eq!(resolve_removable_worktree(&home, "devn", "feat/z"), None);
        std::fs::remove_dir_all(&home).ok();
    }

    /// End-to-end destroy-work prevention: a REAL `workspace/<agent>` worktree on
    /// branchY + a binding to branchY; a stale `remove_worktree(agent, branchX)`
    /// must be a graceful no-op and leave the live workspace intact (the critical
    /// #2234-cluster guard — `git worktree remove --force` is destructive).
    #[test]
    fn remove_worktree_stale_branch_does_not_destroy_live_workspace_2234() {
        let home = tmp_home("rrw-e2e");
        let repo = tmp_repo("rrw-e2e-repo");
        let ws = crate::paths::workspace_dir(&home).join("deve");
        std::fs::create_dir_all(ws.parent().unwrap()).unwrap();
        let out = std::process::Command::new("git")
            .args(["worktree", "add", "-b", "feat/Y", &ws.display().to_string()])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git worktree add: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        write_test_binding(&home, "deve", "feat/Y", &ws);

        let r = remove_worktree(&home, &repo, "deve", "feat/X");

        assert!(
            r.is_ok(),
            "stale-branch remove must be a graceful no-op: {r:?}"
        );
        assert!(
            ws.exists(),
            "#2234: the live branchY workspace must NOT be destroyed by a stale remove(branchX)"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// #2234 Phase 2: list_residual also surfaces cure-(B) `workspace/<agent>`
    /// gitlink worktrees (the worktrees_root first-level scan is unchanged →
    /// byte-identical OFF; this adds the workspace coverage when (B) is on).
    #[test]
    fn list_residual_includes_workspace_gitlink_2234() {
        let home = tmp_home("lr-ws");
        let repo = tmp_repo("lr-ws-repo");
        let ws = crate::paths::workspace_dir(&home).join("devw");
        std::fs::create_dir_all(ws.parent().unwrap()).unwrap();
        let out = std::process::Command::new("git")
            .args(["worktree", "add", "-b", "feat/y", &ws.display().to_string()])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git worktree add: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            list_residual(&home).contains(&"devw".to_string()),
            "#2234: cure-(B) workspace gitlink agent must appear in list_residual"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }
}

#[cfg(test)]
mod review_repro_xcut_concurrency;
