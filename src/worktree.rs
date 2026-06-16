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

/// Recover the source repo path from a worktree working directory.
///
/// Sprint 57 Wave 4 (#546 Item 4): post-migration, source_repo
/// CANNOT be derived from worktree path alone (worktrees live under
/// `$AGEND_HOME/worktrees/<agent>/<branch>/` external to the source
/// repo). Production code reads `binding.source_repo` directly.
/// This helper is retained for legacy-layout detection only — it
/// matches the pre-Wave-4 `{source_repo}/.worktrees/{name}` layout
/// and returns `None` for the new layout.
pub fn source_repo_of(working_dir: &Path) -> Option<PathBuf> {
    if !working_dir
        .components()
        .any(|c| c.as_os_str() == ".worktrees")
    {
        return None;
    }
    working_dir.parent()?.parent().map(|p| p.to_path_buf())
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
