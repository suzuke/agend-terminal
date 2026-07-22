//! Admin utilities — manual maintenance commands.

pub mod cleanup_zombies;

use std::path::Path;

/// Result of checking one local branch for cleanup eligibility.
#[derive(Debug)]
pub struct BranchCheck {
    pub branch: String,
    pub action: BranchAction,
}

#[derive(Debug, PartialEq)]
pub enum BranchAction {
    /// Branch has a merged PR — safe to delete.
    Delete { pr_number: u64 },
    /// Branch is checked out in a worktree — skip.
    SkipWorktree,
    /// No merged PR found — keep.
    SkipUnmerged,
    /// Branch is main — never touch.
    SkipMain,
}

/// List local branches that are NOT checked out in any worktree.
fn worktree_branches(repo: &Path) -> std::collections::HashSet<String> {
    // #1899/#2550: bounded via git_worktree::list_porcelain (git_bypass, LOCAL
    // 60s) — a stuck local git returns Err/empty instead of hanging.
    crate::git_worktree::list_porcelain(repo)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, branch)| branch)
        .collect()
}

/// List all local branch names.
fn local_branches(repo: &Path) -> Vec<String> {
    // #1899: bounded via git_bypass (LOCAL 60s).
    let output = crate::git_helpers::git_bypass(repo, &["branch", "--format=%(refname:short)"]);
    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Err(_) => vec![],
    }
}

/// Check if a branch has a merged PR via `gh pr list` (through the
/// [`crate::scm::ScmProvider`] abstraction — #PR-D).
fn has_merged_pr(repo: &Path, branch: &str) -> Option<u64> {
    // #PR-D site 10: the prior call ran `gh pr list --head B --state merged
    // --json number --limit 1` with `.current_dir(repo)` and NO `--repo`
    // (gh auto-detects the repo from the cwd) — `repo` here is a filesystem
    // path, not an owner/repo slug. Routed through pr_list with `cwd =
    // Some(repo)` so `--repo` is omitted and gh still runs in that dir:
    // argv set-equal to the original (flag-order canonicalized, gh
    // order-insensitive; decision d-20260601151209762922-0). Any failure /
    // no PR → None (unchanged).
    let prs = crate::scm::make_scm_provider("", None)
        .pr_list(
            "",
            &crate::scm::ListFilter {
                state: Some("merged"),
                head: Some(branch.to_string()),
                limit: Some(1),
                ..Default::default()
            },
            &["number"],
            Some(repo),
        )
        .ok()?;
    prs.first().map(|s| s.number).filter(|n| *n != 0)
}

/// Analyze all local branches and determine cleanup actions.
pub fn analyze_branches(repo: &Path) -> Vec<BranchCheck> {
    let wt_branches = worktree_branches(repo);
    let branches = local_branches(repo);
    let mut results = Vec::new();

    for branch in branches {
        let action = if branch == "main" || branch == "master" {
            BranchAction::SkipMain
        } else if wt_branches.contains(&branch) {
            BranchAction::SkipWorktree
        } else if let Some(pr) = has_merged_pr(repo, &branch) {
            BranchAction::Delete { pr_number: pr }
        } else {
            BranchAction::SkipUnmerged
        };
        results.push(BranchCheck { branch, action });
    }
    results
}

/// Execute cleanup: delete branches marked for deletion.
/// Returns (deleted_count, skipped_count).
pub fn execute_cleanup(repo: &Path, checks: &[BranchCheck], dry_run: bool) -> (usize, usize) {
    let log_path = repo.join(format!(
        ".agend-terminal-branch-cleanup-{}.log",
        chrono::Utc::now().format("%Y-%m-%d")
    ));
    let mut log_lines = Vec::new();
    let mut deleted = 0;
    let mut skipped = 0;

    for check in checks {
        match &check.action {
            BranchAction::Delete { pr_number } => {
                if dry_run {
                    let msg = format!(
                        "[dry-run] would delete: {} (PR #{})",
                        check.branch, pr_number
                    );
                    println!("{msg}");
                    log_lines.push(msg);
                } else {
                    // Capture the tip SHA before deletion for restore auditing.
                    let tip_sha =
                        crate::git_helpers::git_bypass(repo, &["rev-parse", &check.branch])
                            .ok()
                            .filter(|o| o.status.success())
                            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                            .unwrap_or_default();
                    // Try safe -d first; it succeeds for ancestry-reachable
                    // (true-merge) branches. For squash-merged branches -d
                    // refuses because the tip is not reachable from HEAD.
                    let safe =
                        crate::git_helpers::git_bypass(repo, &["branch", "-d", &check.branch]);
                    let deleted_ok = match &safe {
                        Ok(out) if out.status.success() => true,
                        _ => {
                            // -d refused: force-delete only if the branch is
                            // proven squash-equivalent to main; otherwise a
                            // reused branch name with new unique commits must
                            // be preserved.
                            if crate::branch_sweep::is_squash_merged(repo, "main", &check.branch) {
                                crate::git_helpers::git_bypass(
                                    repo,
                                    &["branch", "-D", &check.branch],
                                )
                                .is_ok_and(|o| o.status.success())
                            } else {
                                false
                            }
                        }
                    };
                    if deleted_ok {
                        let msg = format!(
                            "deleted: {} (PR #{}) [tip={}]",
                            check.branch, pr_number, tip_sha
                        );
                        println!("{msg}");
                        log_lines.push(msg);
                        deleted += 1;
                    } else {
                        let msg =
                            format!("FAILED to delete: {} (not squash-equivalent)", check.branch);
                        eprintln!("{msg}");
                        log_lines.push(msg);
                        skipped += 1;
                    }
                }
            }
            BranchAction::SkipWorktree => {
                let msg = format!("skip (worktree): {}", check.branch);
                log_lines.push(msg);
                skipped += 1;
            }
            BranchAction::SkipUnmerged => {
                let msg = format!("skip (unmerged/no PR): {}", check.branch);
                log_lines.push(msg);
                skipped += 1;
            }
            BranchAction::SkipMain => {
                skipped += 1;
            }
        }
    }

    if !log_lines.is_empty() {
        let _ = std::fs::write(&log_path, log_lines.join("\n") + "\n");
        println!("Audit log: {}", log_path.display());
    }
    (deleted, skipped)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("agend-admin-{}-{}-{}", std::process::id(), tag, id));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn init_repo(dir: &Path) {
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["init", "-b", "main"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
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
            .current_dir(dir)
            .output()
            .unwrap();
    }

    #[test]
    fn analyze_skips_main() {
        let dir = tmp_dir("skip-main");
        init_repo(&dir);
        let results = analyze_branches(&dir);
        assert!(results
            .iter()
            .any(|r| r.branch == "main" && r.action == BranchAction::SkipMain));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn analyze_detects_worktree_branch() {
        let dir = tmp_dir("wt-detect");
        init_repo(&dir);
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["branch", "feat-wt"])
            .current_dir(&dir)
            .output()
            .unwrap();
        let wt_path = dir.join("wt");
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["worktree", "add", wt_path.to_str().unwrap(), "feat-wt"])
            .current_dir(&dir)
            .output()
            .unwrap();

        let results = analyze_branches(&dir);
        let wt_check = results.iter().find(|r| r.branch == "feat-wt").unwrap();
        assert_eq!(wt_check.action, BranchAction::SkipWorktree);

        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["worktree", "remove", wt_path.to_str().unwrap()])
            .current_dir(&dir)
            .output()
            .ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn analyze_unmerged_branch_kept() {
        let dir = tmp_dir("unmerged");
        init_repo(&dir);
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["branch", "feat-unmerged"])
            .current_dir(&dir)
            .output()
            .unwrap();

        let results = analyze_branches(&dir);
        let check = results
            .iter()
            .find(|r| r.branch == "feat-unmerged")
            .unwrap();
        assert_eq!(check.action, BranchAction::SkipUnmerged);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Bug 1 RED: squash-merged branches (different SHA on main, same patch)
    /// must be force-deleted. Currently `execute_cleanup` uses `git branch -d`
    /// which refuses because the branch tip is not ancestry-reachable from HEAD.
    #[test]
    fn execute_cleanup_force_deletes_squash_merged_branch() {
        let dir = tmp_dir("squash-force");
        init_repo(&dir);

        // Create feat-squash with a unique commit
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["checkout", "-b", "feat-squash"])
            .current_dir(&dir)
            .output()
            .unwrap();
        std::fs::write(dir.join("squash.txt"), "squash content\n").unwrap();
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["add", "squash.txt"])
            .current_dir(&dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "feat: squash work",
            ])
            .current_dir(&dir)
            .output()
            .unwrap();

        // Switch back to main and cherry-pick (simulating squash merge — different SHA)
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["checkout", "main"])
            .current_dir(&dir)
            .output()
            .unwrap();
        // Add an unrelated commit first so cherry-pick doesn't fast-forward
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "main: diverge",
            ])
            .current_dir(&dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["cherry-pick", "--no-commit", "feat-squash"])
            .current_dir(&dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "squash: feat-squash",
            ])
            .current_dir(&dir)
            .output()
            .unwrap();

        // Precondition: feat-squash is NOT ancestry-reachable from main
        let ancestor_check = std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["merge-base", "--is-ancestor", "feat-squash", "main"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            !ancestor_check.status.success(),
            "precondition: feat-squash must NOT be ancestry-reachable from main"
        );

        let checks = vec![BranchCheck {
            branch: "feat-squash".to_string(),
            action: BranchAction::Delete { pr_number: 99 },
        }];
        let (deleted, skipped) = execute_cleanup(&dir, &checks, false);
        assert_eq!(deleted, 1, "squash-merged branch must be force-deleted");
        assert_eq!(skipped, 0);

        // Verify branch no longer exists
        let branches = std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["branch", "--list", "feat-squash"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
            "feat-squash must no longer exist after cleanup"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Reused branch name with new unique commits must NOT be force-deleted
    /// even if an old merged PR with the same branch name exists.
    #[test]
    fn execute_cleanup_preserves_reused_branch_with_new_commits() {
        let dir = tmp_dir("reused-branch");
        init_repo(&dir);

        // Create a branch with unique content that is NOT squash-equivalent
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["checkout", "-b", "feat-reused"])
            .current_dir(&dir)
            .output()
            .unwrap();
        std::fs::write(dir.join("new-work.txt"), "unique new work\n").unwrap();
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["add", "new-work.txt"])
            .current_dir(&dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "-m",
                "feat: new unique work on reused branch",
            ])
            .current_dir(&dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["checkout", "main"])
            .current_dir(&dir)
            .output()
            .unwrap();

        // Precondition: branch is NOT ancestry-reachable and NOT squash-equivalent
        let ancestor_check = std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["merge-base", "--is-ancestor", "feat-reused", "main"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            !ancestor_check.status.success(),
            "precondition: not reachable"
        );
        assert!(
            !crate::branch_sweep::is_squash_merged(&dir, "main", "feat-reused"),
            "precondition: not squash-equivalent"
        );

        // Even though analyze would have marked Delete (old merged PR),
        // execute_cleanup must refuse because the tip is not squash-equivalent.
        let checks = vec![BranchCheck {
            branch: "feat-reused".to_string(),
            action: BranchAction::Delete { pr_number: 50 },
        }];
        let (deleted, skipped) = execute_cleanup(&dir, &checks, false);
        assert_eq!(deleted, 0, "reused branch must NOT be deleted");
        assert_eq!(skipped, 1, "reused branch must be skipped");

        // Branch still exists
        let branches = std::process::Command::new("git")
            .env("AGEND_GIT_BYPASS", "1")
            .args(["branch", "--list", "feat-reused"])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
            "feat-reused must still exist"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dry_run_does_not_delete() {
        let dir = tmp_dir("dry-run");
        let checks = vec![BranchCheck {
            branch: "feat-test".to_string(),
            action: BranchAction::Delete { pr_number: 42 },
        }];
        let (deleted, _) = execute_cleanup(&dir, &checks, true);
        assert_eq!(deleted, 0, "dry-run must not delete");
        std::fs::remove_dir_all(&dir).ok();
    }
}
