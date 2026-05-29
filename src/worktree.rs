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
    std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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

    // Empty repo (git init without any commits) → HEAD is invalid.
    // Worktree creation requires at least one commit.
    if !has_commits(repo_dir) {
        tracing::info!(repo = %repo_dir.display(), "empty repo, creating initial commit for worktree support");
        let ok = std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args([
                "-c",
                "user.name=agend-terminal",
                "-c",
                "user.email=agend@localhost",
                "commit",
                "--allow-empty",
                "-m",
                "init (agend-terminal)",
            ])
            .current_dir(repo_dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
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
        let actual = std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["branch", "--show-current"])
            .current_dir(&wt_dir)
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                } else {
                    None
                }
            });
        if actual.as_deref() != Some(branch.as_str()) {
            tracing::warn!(
                instance = instance_name,
                requested = %branch,
                actual = ?actual,
                path = %wt_dir.display(),
                "lease conflict: worktree exists on a different branch — rejecting"
            );
            return None;
        }
        tracing::info!(
            instance = instance_name,
            path = %wt_dir.display(),
            branch = %branch,
            "reusing existing worktree (branch verified)"
        );
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

    // Try creating worktree: first with -b (new branch), fallback without -b (existing branch)
    let output = std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args([
            "worktree",
            "add",
            "-b",
            &branch,
            &wt_dir.display().to_string(),
        ])
        .current_dir(repo_dir)
        .output();

    match output {
        Ok(o) if o.status.success() => {
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
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            // #781 Piece 2 (Bug B): the prior `o.status.code() == Some(128)`
            // gate was too strict. `git worktree add -b <existing-branch>`
            // can exit with code 255 (not 128) when the failure happens
            // after the "Preparing worktree (new branch …)" progress line
            // has already been emitted to stderr — observed in macOS git
            // 2.42+ in the #781 spike (raw capture: exit 255, stderr
            // "fatal: a branch named '…' already exists"). Exit codes
            // from `git worktree add` are not contracted in any released
            // git manpage we could find; the stderr substring is the
            // load-bearing semantic signal. Rely on it alone.
            //
            // Reasoning: across git versions / locales the stderr
            // wording stays stable (English) for the duplicate-branch
            // case ("already exists") and the cross-worktree-checkout
            // case ("is already checked out"); the exit code drift is
            // version-specific. Adding more codes to the allow-list
            // would just chase the next git release — the substring
            // check is what we actually want.
            if stderr.contains("already exists") || stderr.contains("is already checked out") {
                let output2 = std::process::Command::new("git")
                    .env("AGEND_GIT_BYPASS", "1")
                    .args(["worktree", "add", &wt_dir.display().to_string(), &branch])
                    .current_dir(repo_dir)
                    .output();
                match output2 {
                    Ok(o2) if o2.status.success() => {
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
                    Ok(o2) => {
                        tracing::warn!(
                            instance = instance_name,
                            error = %String::from_utf8_lossy(&o2.stderr).trim(),
                            "worktree creation failed"
                        );
                        None
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "git not available");
                        None
                    }
                }
            } else {
                tracing::warn!(instance = instance_name, error = %stderr.trim(), "worktree creation failed");
                None
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "git not available");
            None
        }
    }
}

/// Run `git worktree prune` on a repo to clean stale worktree entries.
pub fn prune(repo_dir: &Path) {
    if !is_git_repo(repo_dir) {
        return;
    }
    let output = std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["worktree", "prune"])
        .current_dir(repo_dir)
        .output();
    match output {
        Ok(o) if o.status.success() => {
            tracing::info!(repo = %repo_dir.display(), "pruned stale worktree entries");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            if !stderr.trim().is_empty() {
                tracing::warn!(warning = %stderr.trim(), "worktree prune warning");
            }
        }
        Err(e) => {
            tracing::warn!(repo = %repo_dir.display(), error = %e, "git worktree prune failed");
        }
    }
}

/// Check if a worktree directory has uncommitted changes.
/// Returns true if `git status --porcelain` produces non-empty output.
pub fn has_uncommitted_changes(worktree_dir: &Path) -> bool {
    let output = std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["status", "--porcelain"])
        .current_dir(worktree_dir)
        .output();
    match output {
        Ok(o) => !o.stdout.is_empty(),
        Err(_) => true, // fail-closed: assume dirty if we can't check
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
    let wt_dir = worktree_path(home, agent, branch);
    if !wt_dir.exists() {
        return Ok(()); // already gone
    }
    // git worktree remove --force <path>
    let output = std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["worktree", "remove", "--force"])
        .arg(&wt_dir)
        .current_dir(repo_dir)
        .output()
        .map_err(|e| format!("git worktree remove failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree remove: {}", stderr.trim()));
    }
    // Delete tracking branch agend/<agent> (legacy default-branch shape).
    // Custom branches are not auto-deleted — operator workflow.
    let default_branch = format!("agend/{agent}");
    let _ = std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["branch", "-D", &default_branch])
        .current_dir(repo_dir)
        .output();
    tracing::info!(agent, branch, "auto-pruned worktree + branch");
    Ok(())
}

/// Checkout a branch in a worktree directory. Creates the branch from
/// current HEAD if it doesn't exist. Best-effort: returns Ok on success,
/// Err with message on failure.
pub fn checkout_branch(worktree_dir: &Path, branch: &str) -> Result<(), String> {
    // Try switching to existing branch first
    let switch = std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["switch", branch])
        .current_dir(worktree_dir)
        .output()
        .map_err(|e| format!("git switch: {e}"))?;
    if switch.status.success() {
        tracing::info!(branch, dir = %worktree_dir.display(), "checked out branch");
        return Ok(());
    }
    // Branch doesn't exist — create from current HEAD
    let create = std::process::Command::new("git")
        .env("AGEND_GIT_BYPASS", "1")
        .args(["switch", "-c", branch])
        .current_dir(worktree_dir)
        .output()
        .map_err(|e| format!("git switch -c: {e}"))?;
    if create.status.success() {
        tracing::info!(branch, dir = %worktree_dir.display(), "created and checked out branch");
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&create.stderr);
    Err(format!("git switch -c {branch}: {}", stderr.trim()))
}

/// Sprint 57 Wave 4 (#546 Item 4): list agent names present under
/// `$AGEND_HOME/worktrees/`. The `repo_dir` parameter is retained
/// for API compatibility with pre-Wave-4 callers but the new layout
/// is repo-independent — agent dirs live under the central daemon
/// state, not per-repo.
pub fn list_residual(home: &Path) -> Vec<String> {
    let wt_base = home.join("worktrees");
    if !wt_base.exists() {
        return Vec::new();
    }
    std::fs::read_dir(&wt_base)
        .ok()
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default()
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
}
