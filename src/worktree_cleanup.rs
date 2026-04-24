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

/// Normalize a path: strip Windows `\\?\` UNC prefix for consistent comparison.
fn normalize_path(p: &std::path::Path) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    if s.starts_with(r"\\?\") {
        std::path::PathBuf::from(&s[4..])
    } else {
        p.to_path_buf()
    }
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

    // Collect agent working directories from fleet.yaml for in-use check
    let agent_dirs: Vec<std::path::PathBuf> =
        crate::fleet::FleetConfig::load(&home.join("fleet.yaml"))
            .ok()
            .map(|config| {
                config
                    .instance_names()
                    .iter()
                    .filter_map(|name| {
                        config
                            .resolve_instance(name)
                            .and_then(|r| r.working_directory)
                    })
                    .collect()
            })
            .unwrap_or_default();

    for entry in &entries {
        let wt_path = std::path::Path::new(&entry.path);

        // Safety: skip if any agent's working_directory is inside this worktree
        let wt_canonical = normalize_path(
            &wt_path
                .canonicalize()
                .unwrap_or_else(|_| wt_path.to_path_buf()),
        );
        let in_use = agent_dirs.iter().any(|wd| {
            let wd_canonical = normalize_path(&wd.canonicalize().unwrap_or_else(|_| wd.clone()));
            wd_canonical.starts_with(&wt_canonical)
        });
        if in_use {
            tracing::debug!(branch = %entry.branch, path = %entry.path, "skipping worktree (agent working_directory in use)");
            continue;
        }

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

    fn setup_test_repo(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agend-wt-git-{}-{}-{}",
            tag,
            std::process::id(),
            C.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).ok();
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&dir)
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("git");
        };
        git(&["init", "-b", "main"]);
        std::fs::write(dir.join("README.md"), "init").ok();
        git(&["add", "."]);
        git(&["commit", "-m", "init"]);
        dir
    }

    fn git_in(dir: &std::path::Path, args: &[&str]) {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git");
    }

    #[test]
    fn test_auto_cleanup_removes_merged_branch_worktree() {
        let repo = setup_test_repo("merged");
        git_in(&repo, &["branch", "feat/merged-test"]);
        let wt = repo.join("wt-merged");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/merged-test"],
        );
        git_in(&repo, &["merge", "feat/merged-test"]);

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        let removed = sweep_merged_worktrees(&repo);
        assert!(
            removed.iter().any(|(b, _)| b == "feat/merged-test"),
            "merged worktree must be removed: {removed:?}"
        );
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn test_auto_cleanup_preserves_dirty_worktree() {
        let repo = setup_test_repo("dirty");
        git_in(&repo, &["branch", "feat/dirty-test"]);
        let wt = repo.join("wt-dirty");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/dirty-test"],
        );
        git_in(&repo, &["merge", "feat/dirty-test"]);
        std::fs::write(wt.join("uncommitted.txt"), "dirty").ok();

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        let removed = sweep_merged_worktrees(&repo);
        assert!(removed.is_empty(), "dirty worktree must NOT be removed");
        assert!(wt.exists());
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn test_auto_cleanup_preserves_unmerged_worktree() {
        let repo = setup_test_repo("unmerged");
        git_in(&repo, &["branch", "feat/unmerged-test"]);
        let wt = repo.join("wt-unmerged");
        git_in(
            &repo,
            &[
                "worktree",
                "add",
                wt.to_str().unwrap(),
                "feat/unmerged-test",
            ],
        );
        // Commit on the branch (not merged into main)
        std::fs::write(wt.join("new.txt"), "x").ok();
        git_in(&wt, &["add", "."]);
        git_in(&wt, &["commit", "-m", "wip"]);

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        let removed = sweep_merged_worktrees(&repo);
        assert!(removed.is_empty(), "unmerged worktree must NOT be removed");
        assert!(wt.exists());
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn test_auto_cleanup_skips_worktree_when_agent_cwd_inside() {
        let repo = setup_test_repo("agent-cwd");
        git_in(&repo, &["branch", "feat/agent-use"]);
        let wt = repo.join("wt-agent");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/agent-use"],
        );
        git_in(&repo, &["merge", "feat/agent-use"]);
        // Merged + clean, but an agent's working_directory points here.
        // Write fleet.yaml with an instance whose working_directory is the worktree.
        std::fs::write(
            repo.join("fleet.yaml"),
            format!(
                "instances:\n  busy-agent:\n    backend: claude\n    working_directory: \"{}\"\n",
                wt.display()
            ),
        )
        .ok();

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        let removed = sweep_merged_worktrees(&repo);
        assert!(
            removed.is_empty(),
            "worktree in use by agent must NOT be removed: {removed:?}"
        );
        assert!(wt.exists(), "worktree dir must still exist");
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::fs::remove_dir_all(&repo).ok();
    }
}
