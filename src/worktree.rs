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

/// Whether [`create`] freshly provisioned the worktree or reused an existing one.
/// The spawn rollback path uses this to decide whether cleanup should remove the
/// directory: a `CreatedByThisAttempt` worktree is this call's responsibility;
/// a `Reused` one predates it and must survive a rollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeProvenance {
    CreatedByThisAttempt,
    Reused,
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
    /// Whether this call created the worktree or reused a pre-existing one.
    pub provenance: WorktreeProvenance,
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
                        provenance: WorktreeProvenance::Reused,
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
            provenance: WorktreeProvenance::Reused,
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
            // arch14 (d-20260719234211852352-4): the FIRST write already carries the
            // canonical four-field identity — no crash window may leave a sourceless
            // marker — and a write/sync failure aborts (fail-loud) instead of handing
            // back a worktree whose identity the deep-validated release would refuse.
            let marker_path = wt_dir.join(".agend-managed");
            let marker_write = std::fs::write(
                &marker_path,
                format!(
                    "agent={instance_name}\nbranch={branch}\nsource_repo={}\nleased_at={}\n",
                    repo_dir.display(),
                    chrono::Utc::now().to_rfc3339()
                ),
            )
            .and_then(|()| crate::worktree_pool::sync_marker_contents(&marker_path));
            if let Err(e) = marker_write {
                tracing::warn!(
                    instance = instance_name,
                    path = %wt_dir.display(),
                    error = %e,
                    "managed-marker write/sync failed — rolling back fresh worktree (fail-loud)"
                );
                // Rollback the worktree THIS attempt just created (never a reused
                // tree — that path returned earlier), so no half-managed dir leaks.
                let _ = git_cmd(
                    repo_dir,
                    &[
                        "worktree",
                        "remove",
                        "--force",
                        &wt_dir.display().to_string(),
                    ],
                );
                return None;
            }
            // Fresh worktree: `git worktree add` copies gitlinks + .gitmodules
            // but does NOT populate submodule content — init recursively here.
            init_submodules_after_create(&wt_dir);
            Some(WorktreeInfo {
                path: wt_dir,
                source_repo: repo_dir.to_path_buf(),
                branch,
                provenance: WorktreeProvenance::CreatedByThisAttempt,
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
                    // arch14: canonical four-field identity + fail-loud, mirroring
                    // the primary `-b` arm exactly.
                    let marker_path = wt_dir.join(".agend-managed");
                    let marker_write = std::fs::write(
                        &marker_path,
                        format!(
                            "agent={instance_name}\nbranch={branch}\nsource_repo={}\nleased_at={}\n",
                            repo_dir.display(),
                            chrono::Utc::now().to_rfc3339()
                        ),
                    )
                    .and_then(|()| crate::worktree_pool::sync_marker_contents(&marker_path));
                    if let Err(e) = marker_write {
                        tracing::warn!(
                            instance = instance_name,
                            path = %wt_dir.display(),
                            error = %e,
                            "managed-marker write/sync failed — rolling back fresh worktree (fail-loud)"
                        );
                        let _ = git_cmd(
                            repo_dir,
                            &[
                                "worktree",
                                "remove",
                                "--force",
                                &wt_dir.display().to_string(),
                            ],
                        );
                        return None;
                    }
                    init_submodules_after_create(&wt_dir);
                    Some(WorktreeInfo {
                        path: wt_dir,
                        source_repo: repo_dir.to_path_buf(),
                        branch,
                        provenance: WorktreeProvenance::CreatedByThisAttempt,
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
    match init_submodules_strict(wt_dir) {
        Ok(()) => {
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

/// #2755: recursively initialize submodules in a freshly-added worktree, returning
/// the git error string on failure (a HARD variant of the soft-warn
/// [`init_submodules_after_create`]). `repo action=checkout`'s SubmodulesReady
/// phase uses this so a failed init ABORTS into rollback instead of returning a
/// worktree with missing path-dependency content. No-op (`Ok`) when `.gitmodules`
/// is absent.
///
/// `-c protocol.file.allow=always` is intentional: git's submodule clone helper
/// ignores the superproject's stored `protocol.file.allow` (security default
/// post-2.38), so hermetic file-path submodule fixtures — and rare local-path
/// submodules — need the command-line override. https/ssh remotes are unaffected.
pub(crate) fn init_submodules_strict(wt_dir: &Path) -> Result<(), String> {
    if !wt_dir.join(".gitmodules").is_file() {
        return Ok(());
    }
    crate::git_helpers::git_cmd(
        wt_dir,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "--recursive",
        ],
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// #2755 R3 (root review): after a recursive init, PROVE the working tree's submodules
/// are at the EXACT commits the superproject's gitlinks record. `git worktree add` +
/// init succeeding is NOT sufficient for a REUSED tree whose branch ref advanced (or a
/// partial init) — a submodule can be left at a stale commit. `git submodule status
/// --recursive` prefixes each line: ` ` = at the recorded commit, `-` = not initialized,
/// `+` = a DIFFERENT commit than the gitlink, `U` = merge conflict. Any of `-`/`+`/`U`
/// ⇒ the final tree is NOT proven buildable ⇒ Err (fail closed). No `.gitmodules` ⇒ Ok.
pub(crate) fn verify_submodules_at_gitlinks(wt_dir: &Path) -> Result<(), String> {
    if !wt_dir.join(".gitmodules").is_file() {
        return Ok(());
    }
    let status = crate::git_helpers::git_cmd(
        wt_dir,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "status",
            "--recursive",
        ],
    )
    .map_err(|e| e.to_string())?;
    for line in status.lines() {
        if matches!(line.chars().next(), Some('-' | '+' | 'U')) {
            return Err(format!(
                "submodule not at its recorded gitlink commit: {}",
                line.trim()
            ));
        }
    }
    Ok(())
}

/// #2755 R3 (root review / decision d-…37): FAIL-CLOSED variant of
/// [`sync_worktree_to_head`] for idempotent reuse — the reused tree must be synced to
/// the FINAL HEAD (an externally advanced branch may change gitlinks) BEFORE recursive
/// init, and a sync failure must ABORT the reuse (return no success), not be swallowed.
/// A clean tree is a no-op (never a destructive reset on a clean worktree); a
/// reset/clean failure returns `Err`.
pub(crate) fn sync_worktree_to_head_strict(worktree_dir: &Path) -> Result<(), String> {
    use crate::git_helpers::git_cmd;
    match git_cmd(worktree_dir, &["status", "--porcelain"]) {
        Ok(status) if status.is_empty() => return Ok(()), // clean ⇒ nothing to sync
        Ok(_) => {}
        Err(e) => return Err(format!("status probe failed: {e}")),
    }
    git_cmd(worktree_dir, &["reset", "--hard", "HEAD"])
        .map_err(|e| format!("reset failed: {e}"))?;
    git_cmd(worktree_dir, &["clean", "-fd"]).map_err(|e| format!("clean failed: {e}"))?;
    Ok(())
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
    // P0/P1 (codex R2): `--ignore-submodules=none` overrides any
    // `submodule.<name>.ignore=all|dirty` config that would HIDE submodule dirt
    // (else a submodule-only-dirty worktree reads clean → WIP loss); global
    // `--no-optional-locks` (first) stops `status` opportunistically rewriting the
    // live index. Both are git GLOBAL options, so they precede `status`.
    crate::git_helpers::git_cmd(
        worktree_dir,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--ignore-submodules=none",
        ],
    )
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
    // P0/P1 (codex R2): force submodule dirt to SHOW (`--ignore-submodules=none`
    // overrides `submodule.<name>.ignore=all|dirty`, which would otherwise hide it
    // and make this classifier read Clean → the worktree removed → nested WIP lost)
    // and keep the live index byte-untouched (`--no-optional-locks`). Both global.
    match crate::git_helpers::git_cmd(
        wt_path,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--ignore-submodules=none",
        ],
    ) {
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

/// Operator-visible effects produced while classifying/preserving WIP. Release
/// transactions collect these under their flocks and emit them only after the
/// branch + agent + binding guards have dropped.
pub(crate) enum ReleaseNotice {
    WipPreserved {
        recipient: String,
        text: String,
    },
    UnpreservableNestedDirty {
        agent: String,
        branch: String,
        wt_path: PathBuf,
        nested_status: String,
        sender: Option<String>,
    },
}

impl ReleaseNotice {
    pub(crate) fn emit(self, home: &Path) {
        #[cfg(test)]
        crate::worktree_pool::release_test_seam::hit(
            crate::worktree_pool::ReleaseTestPhase::BeforeNoticeEmit,
        );
        match self {
            ReleaseNotice::WipPreserved { recipient, text } => {
                let source = crate::inbox::NotifySource::System("release_dirty_wip_preserved");
                crate::inbox::notify_agent(home, &recipient, &source, &text);
            }
            ReleaseNotice::UnpreservableNestedDirty {
                agent,
                branch,
                wt_path,
                nested_status,
                sender,
            } => notify_unpreservable_nested_dirty(
                home,
                &agent,
                &branch,
                &wt_path,
                &nested_status,
                sender.as_deref(),
            ),
        }
    }
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
///  - Classification: if the recursive nested walk finds ANY submodule with
///    INTERNAL working-tree dirt (`m`/`u`, gitlink unchanged) — sole OR mixed with
///    parent dirt — a parent recovery commit cannot capture it (`git add -A`
///    records the gitlink, not the submodule's internal edits), so minting one is a
///    false "preserved" that silently loses the nested WIP on removal. Refuse
///    (`UnpreservableNestedDirty`) instead. A pure gitlink MOVE (`SC..`) is NOT
///    internal dirt and stays preservable. The same refusal also fires for an
///    UNTRACKED embedded git repo at any depth (a `?`-row dir with a `.git`; the
///    walk uses `--untracked-files=all` so git surfaces each foreign-repo boundary
///    as its own row) — its internal content is equally uncapturable by a parent ref.
///  - BOUNDARY (out of contract): content under a GITIGNORE'd path — including an
///    ignored embedded repo — is NOT covered. `git status`/`add -A` never list or
///    capture ignored paths, so no gitlink or recovery ref ever falsely claims it;
///    it is universal accepted-loss on every release path (a plain ignored file is
///    dropped identically). Preserving it would be an ignore-semantics change, not
///    this data-safety fix. Pinned by `preserve_ignored_dir_embedded_repo_*`.
///  - Otherwise PRESERVE: anchor a commit whose tree = `worktree_tree` to a ref in
///    the SHARED object/ref store (survives the worktree-dir removal). When the
///    STAGED tree differs from BOTH HEAD and the working tree, it is captured as a
///    second parent (`ref^2`) so staged-only WIP stays recoverable. Unlike
///    `git stash create` this captures untracked files; unlike `git stash push` it
///    never touches the shared `refs/stash` stack, so two concurrent dirty releases
///    can't cross-contaminate.
#[allow(dead_code)]
pub(crate) fn preserve_dirty_worktree(
    home: &Path,
    agent: &str,
    wt_path: &Path,
    branch: &str,
    sender: Option<&str>,
) -> WipPreservation {
    let (outcome, notices) = preserve_dirty_worktree_collect(home, agent, wt_path, branch, sender);
    for notice in notices {
        notice.emit(home);
    }
    outcome
}

/// Transaction-facing variant of [`preserve_dirty_worktree`]. It performs the
/// same snapshot/classification work but returns notice payloads instead of
/// emitting while the caller's release flocks are held.
pub(crate) fn preserve_dirty_worktree_collect(
    home: &Path,
    agent: &str,
    wt_path: &Path,
    branch: &str,
    sender: Option<&str>,
) -> (WipPreservation, Vec<ReleaseNotice>) {
    use crate::git_helpers::git_cmd;
    if branch.is_empty() {
        return (WipPreservation::Clean, Vec::new()); // unknown branch → nothing to key a recovery ref on
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
        return (WipPreservation::Clean, Vec::new());
    }
    if !worktree_has_preservable_wip(wt_path) {
        return (WipPreservation::Clean, Vec::new()); // clean / marker-only → zero behaviour change
    }
    // Branch tip tree. Err → Blocked (fail-closed): a repo we can't read HEAD^{tree}
    // from is one we can't safely snapshot, so refuse removal.
    let head_tree = match git_cmd(wt_path, &["rev-parse", "HEAD^{tree}"]) {
        Ok(t) if !t.is_empty() => t,
        other => {
            tracing::warn!(agent, branch, ?other,
                "preserve dirty WIP: HEAD^{{tree}} resolve failed — refusing to remove (fail-closed)");
            return (
                WipPreservation::Blocked(format!(
                    "`git rev-parse HEAD^{{tree}}` failed: {other:?}"
                )),
                Vec::new(),
            );
        }
    };
    // LIVE index tree — READ-ONLY. `write-tree` reads the current index and writes
    // tree objects WITHOUT mutating the index file (no `add`/`read-tree`/`reset`),
    // so the live index stays byte-identical on every path below. An UNMERGED index
    // makes `write-tree` fail → Blocked (fail-closed).
    // P1 (codex R2): global `--no-optional-locks` (first) so `write-tree` does not
    // persist a refreshed cache-tree back to the LIVE index — keeps it byte-identical.
    let index_tree = match git_cmd(wt_path, &["--no-optional-locks", "write-tree"]) {
        Ok(t) if !t.is_empty() => t,
        other => {
            tracing::warn!(agent, branch, ?other,
                "preserve dirty WIP: `write-tree` (live index) failed — refusing to remove (fail-closed)");
            return (
                WipPreservation::Blocked(format!(
                    "`git write-tree` (live index) failed: {other:?}"
                )),
                Vec::new(),
            );
        }
    };
    // Working-tree tree via a TEMP index so the LIVE index is byte-untouched.
    let worktree_tree = match snapshot_worktree_tree(wt_path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(agent, branch, error = %e,
                "preserve dirty WIP: working-tree snapshot failed — refusing to remove (fail-closed)");
            return (WipPreservation::Blocked(e), Vec::new());
        }
    };
    // Classification (R5 data-safety, 2nd-seat blocker @ fc8481d3): ANY nested-
    // submodule INTERNAL dirt (a submodule's own modified/untracked content, gitlink
    // unchanged) is UNPRESERVABLE by a parent snapshot — `git add -A` records the
    // gitlink, NEVER the submodule's internal edits — WHETHER OR NOT parent dirt
    // co-occurs. Detect it DIRECTLY via the recursive walk (which excludes a
    // preservable gitlink MOVE, `SC..`) and refuse fail-closed + emit a serialized,
    // de-duped notice; the caller MUST NOT remove the worktree.
    //
    // The prior `index_tree == head_tree && worktree_tree == head_tree` proxy caught
    // ONLY the sole-nested case; any co-occurring parent dirt made `worktree_tree !=
    // HEAD`, skipped the refusal, and the preserve path below then snapshotted the
    // gitlink only → the nested WIP was SILENTLY LOST on removal. A non-empty walk —
    // including an explicit `[truncated:/skipped:]` can't-enumerate line — is treated
    // as "nested dirt present" → fail-closed. (Trees are still computed above so an
    // unmerged live index / snapshot failure keeps its `Blocked` precedence.)
    let nested = enumerate_nested_dirty(wt_path);
    if !nested.is_empty() {
        tracing::warn!(agent, branch,
            "preserve dirty WIP: nested submodule-internal dirt present (unpreservable by a parent ref — sole or mixed with parent dirt) — refusing removal (fail-closed)");
        return (
            WipPreservation::UnpreservableNestedDirty(
                "worktree has uncommitted changes inside a nested submodule's working tree \
                 (gitlink unchanged); a parent recovery ref cannot capture them, so removal was \
                 refused to preserve the nested WIP in place"
                    .into(),
            ),
            vec![ReleaseNotice::UnpreservableNestedDirty {
                agent: agent.to_string(),
                branch: branch.to_string(),
                wt_path: wt_path.to_path_buf(),
                nested_status: nested,
                sender: sender.map(str::to_string),
            }],
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
                return (
                    WipPreservation::Blocked(format!(
                        "`git commit-tree` (staged snapshot) failed: {other:?}"
                    )),
                    Vec::new(),
                );
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
            return (
                WipPreservation::Blocked(format!("`git commit-tree` failed: {other:?}")),
                Vec::new(),
            );
        }
    };
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let ref_name = format!("{RECOVERY_REF_PREFIX}/{branch}/{ts}");
    if let Err(e) = git_cmd(wt_path, &["update-ref", &ref_name, &commit]) {
        tracing::warn!(agent, branch, error = %e,
            "preserve dirty WIP: `update-ref` failed — refusing to remove (fail-closed)");
        return (
            WipPreservation::Blocked(format!("`git update-ref {ref_name}` failed: {e}")),
            Vec::new(),
        );
    }
    tracing::info!(agent, branch, %ref_name, dual_parent = parent2.is_some(),
        "preserve dirty WIP: uncommitted worktree changes snapshotted before manual release");
    prune_recovery_refs(wt_path, branch);
    let recipient = wip_notice_recipient(home, agent, sender);
    let text = wip_preserved_notice(agent, branch, &ref_name, parent2.is_some());
    (
        WipPreservation::Preserved,
        vec![ReleaseNotice::WipPreserved { recipient, text }],
    )
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
        // git-raw-allowed: this temp-index snapshot needs per-command
        // GIT_INDEX_FILE while retaining the shared bounded process-group runner.
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
    // P0/P1 (codex R2): reveal submodule dirt (`--ignore-submodules=none`) at BOTH
    // the super and every recursive nested call (this one line serves both via the
    // recursion), and never rewrite the live index (`--no-optional-locks`, global).
    //
    // r7 (P4 class-closure): `--untracked-files=all`. Without it git COLLAPSES an
    // entirely-untracked tree into one `?? junk/` row, hiding an embedded repo at
    // `junk/deep/repo/` from the `?`-row check below (depth ≥2 slipped — reviewer5
    // P4). With `-uall` git enumerates every untracked path AND — crucially — never
    // descends INTO a foreign git repo, so it emits every embedded repo as its own
    // `? <path>/` row at ANY depth. The check below then catches each. (IGNORED
    // content is deliberately still NOT listed — it is out of the no-silent-loss
    // contract: `add -A` never captures it, so no gitlink/recovery-ref falsely claims
    // it; a documented boundary, pinned by a test.)
    let entries = match crate::git_helpers::git_cmd(
        dir_canon,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "-z",
            "--ignore-submodules=none",
            "--untracked-files=all",
        ],
    ) {
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
    // r6 (P3b): an UNTRACKED directory that is itself a git repo (an in-worktree
    // clone / `git init`, NOT a registered submodule — so it shows as a porcelain
    // `?` row with no `S` field and the submodule descent above never saw it) holds
    // content a superproject `add -A` records only as a GITLINK. On removal the
    // embedded repo's uncommitted WIP AND its `.git` object store are destroyed and
    // any recovery ref's gitlink dangles — the SAME unpreservable-content invariant
    // as a dirty submodule. Surface it so the refusal + notice fire. (A commit-LESS
    // embed already fails the temp-index `add -A` upstream → `Blocked`, so only the
    // committed shape reaches here.)
    for e in &entries {
        if e.token != "??" {
            continue; // only an untracked `?` row can be an unregistered embedded repo
        }
        let candidate = dir_canon.join(&e.path);
        if !candidate.join(".git").exists() {
            continue; // an ordinary untracked file/dir, not an embedded repo
        }
        let disp = if display_prefix.is_empty() {
            e.path.clone()
        } else {
            format!("{display_prefix}/{}", e.path)
        };
        let embed_canon = match std::fs::canonicalize(&candidate) {
            Ok(p) => p,
            Err(err) => {
                out.push_str(&format!(
                    "{disp}: [untracked embedded git repo — skipped: canonicalize failed: {err}]\n"
                ));
                continue;
            }
        };
        if !embed_canon.starts_with(root) {
            out.push_str(&format!(
                "{disp}: [untracked embedded git repo — skipped: containment]\n"
            ));
            continue;
        }
        if !visited.insert(embed_canon) {
            continue; // already surfaced via the submodule descent or a prior entry
        }
        out.push_str(&format!(
            "{disp}: [untracked embedded git repo — internal WIP/object store is unpreservable by a parent ref]\n"
        ));
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
        "Agent `{agent}` could NOT release its worktree `{wt}` (branch `{branch}`): it has \
         uncommitted changes INSIDE a nested submodule's working tree — the gitlink \
         is unchanged, so a parent recovery ref CANNOT capture them (any co-occurring parent \
         changes are NOT preserved either; the release was REFUSED whole to avoid silently \
         losing the nested WIP).\nDirty nested content:\n\
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
    let marker_path = dir.join(&key);
    let lock_path = dir.join(format!("{key}.lock"));
    // P2 (codex R2 + R3): remove the marker ONLY while HOLDING the same
    // per-worktree lock a concurrent notice takes. NON-BLOCKING (`try_`) so that if
    // a notice is mid-flight (lock held elsewhere) we FAIL SAFE — leave the marker
    // untouched and return; unlinking it here would delete a marker the in-flight
    // notifier still owns, re-opening the cleanup-vs-notify race. A later
    // lock-owning cleanup (or the next release) removes it. NEVER remove the marker
    // without the lock, and NEVER unlink the `.lock` inode (flock is per-inode; a
    // fresh inode is a fresh useless lock that silently breaks serialization).
    match crate::store::try_acquire_file_lock(&lock_path) {
        Ok(Some(_guard)) => {
            let _ = std::fs::remove_file(&marker_path);
        }
        Ok(None) => {
            tracing::debug!(
                "clear nested refusal marker: lock held by an in-flight notice — leaving marker (fail-safe)");
        }
        Err(e) => {
            tracing::warn!(error = %e,
                "clear nested refusal marker: could not open lock — leaving marker untouched (fail-safe)");
        }
    }
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
#[path = "worktree/tests.rs"]
mod tests;

#[cfg(test)]
mod review_repro_xcut_concurrency;
