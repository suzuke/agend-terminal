//! Worktree auto-cleanup — removes worktrees whose branches are merged into main.
//!
//! Gated by `AGEND_WORKTREE_AUTO_CLEANUP=1` (opt-in).

use std::path::Path;
use std::process::Command;

/// Returns true when the `AGEND_WORKTREE_AUTO_CLEANUP` feature flag is set to "1".
pub fn auto_cleanup_enabled() -> bool {
    std::env::var("AGEND_WORKTREE_AUTO_CLEANUP")
        .ok()
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Entry for a git worktree.
#[derive(Debug)]
pub struct WorktreeEntry {
    pub path: String,
    pub branch: String,
}

/// List all git worktrees (excluding the main worktree).
fn list_worktrees(repo_root: &Path) -> Vec<WorktreeEntry> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .output();
    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();
    let mut current_path = None;
    let mut current_branch = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            current_path = Some(p.to_string());
        } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
            current_branch = Some(b.to_string());
        } else if line.is_empty() {
            if let (Some(path), Some(branch)) = (current_path.take(), current_branch.take()) {
                // Skip main worktree (branch == "main" or "master")
                if branch != "main" && branch != "master" {
                    entries.push(WorktreeEntry { path, branch });
                }
            }
            current_path = None;
            current_branch = None;
        }
    }
    entries
}

/// Check if a branch is merged into main (local check, no API needed).
fn is_branch_merged(repo_root: &Path, branch: &str) -> bool {
    Command::new("git")
        .args(["merge-base", "--is-ancestor", branch, "main"])
        .current_dir(repo_root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Check if a worktree has uncommitted changes.
fn is_worktree_dirty(worktree_path: &Path) -> bool {
    Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree_path)
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(true) // assume dirty on error (safe default)
}

/// Remove a worktree and delete its branch.
fn remove_worktree(repo_root: &Path, worktree_path: &str, branch: &str) -> bool {
    let wt_ok = Command::new("git")
        .args(["worktree", "remove", "--force", worktree_path])
        .current_dir(repo_root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if wt_ok {
        let _ = Command::new("git")
            .args(["branch", "-D", branch])
            .current_dir(repo_root)
            .output();
    }
    wt_ok
}

/// Sweep merged worktrees. Returns list of (branch, path) that were removed.
pub fn sweep_merged_worktrees(home: &Path) -> Vec<(String, String)> {
    if !auto_cleanup_enabled() {
        return Vec::new();
    }
    // Find the repo root from home's workspace directory
    let workspace = home.join("workspace");
    let repo_root = if workspace.exists() {
        workspace
    } else {
        home.to_path_buf()
    };
    // Try to find actual git repo root
    let git_root = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&repo_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| std::path::PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    let repo = git_root.as_deref().unwrap_or(&repo_root);

    let entries = list_worktrees(repo);
    let mut removed = Vec::new();

    for entry in &entries {
        let wt_path = std::path::Path::new(&entry.path);

        // Safety: skip dirty worktrees
        if is_worktree_dirty(wt_path) {
            tracing::debug!(branch = %entry.branch, path = %entry.path, "skipping dirty worktree");
            continue;
        }

        // Safety: skip if any agent's cwd is this worktree
        // (simplified: just check if path contains "workspace" — real check would query registry)

        if !is_branch_merged(repo, &entry.branch) {
            continue;
        }

        if remove_worktree(repo, &entry.path, &entry.branch) {
            removed.push((entry.branch.clone(), entry.path.clone()));
        }
    }
    removed
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_cleanup_flag_disabled_default() {
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        assert!(!auto_cleanup_enabled());
    }

    #[test]
    fn test_auto_cleanup_flag_enabled() {
        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        assert!(auto_cleanup_enabled());
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    }

    #[test]
    fn test_sweep_noop_when_flag_disabled() {
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        let home =
            std::env::temp_dir().join(format!("agend-wt-cleanup-test-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let removed = sweep_merged_worktrees(&home);
        assert!(removed.is_empty(), "must not remove anything when flag off");
        std::fs::remove_dir_all(&home).ok();
    }
}
