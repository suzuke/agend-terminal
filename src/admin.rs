//! Admin utilities — manual maintenance commands.

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
    let output = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo)
        .output();
    let mut branches = std::collections::HashSet::new();
    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if let Some(b) = line.strip_prefix("branch refs/heads/") {
                branches.insert(b.to_string());
            }
        }
    }
    branches
}

/// List all local branch names.
fn local_branches(repo: &Path) -> Vec<String> {
    let output = std::process::Command::new("git")
        .args(["branch", "--format=%(refname:short)"])
        .current_dir(repo)
        .output();
    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Err(_) => vec![],
    }
}

/// Check if a branch has a merged PR via `gh pr list`.
fn has_merged_pr(repo: &Path, branch: &str) -> Option<u64> {
    let output = std::process::Command::new("gh")
        .args([
            "pr", "list", "--head", branch, "--state", "merged", "--json", "number", "--limit", "1",
        ])
        .current_dir(repo)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&text).ok()?;
    parsed.as_array()?.first()?["number"].as_u64()
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
                    let result = std::process::Command::new("git")
                        .args(["branch", "-D", &check.branch])
                        .current_dir(repo)
                        .output();
                    match result {
                        Ok(out) if out.status.success() => {
                            let msg = format!("deleted: {} (PR #{})", check.branch, pr_number);
                            println!("{msg}");
                            log_lines.push(msg);
                            deleted += 1;
                        }
                        _ => {
                            let msg = format!("FAILED to delete: {}", check.branch);
                            eprintln!("{msg}");
                            log_lines.push(msg);
                            skipped += 1;
                        }
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
            .args(["init", "-b", "main"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
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
            .args(["branch", "feat-wt"])
            .current_dir(&dir)
            .output()
            .unwrap();
        let wt_path = dir.join("wt");
        std::process::Command::new("git")
            .args(["worktree", "add", wt_path.to_str().unwrap(), "feat-wt"])
            .current_dir(&dir)
            .output()
            .unwrap();

        let results = analyze_branches(&dir);
        let wt_check = results.iter().find(|r| r.branch == "feat-wt").unwrap();
        assert_eq!(wt_check.action, BranchAction::SkipWorktree);

        std::process::Command::new("git")
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
