//! Worktree auto-cleanup v2 — runtime registry based.
//!
//! Gated by `AGEND_WORKTREE_AUTO_CLEANUP=1` (opt-in).
//! Sweeps worktrees whose branches are merged into main, using the daemon's
//! live AgentConfig registry to find repos and detect in-use worktrees.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Returns true when the `AGEND_WORKTREE_AUTO_CLEANUP` feature flag is set to "1".
pub fn auto_cleanup_enabled() -> bool {
    std::env::var("AGEND_WORKTREE_AUTO_CLEANUP")
        .ok()
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Entry for a git worktree.
#[derive(Debug, Clone)]
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

/// Normalize a path: strip Windows `\\?\` UNC prefix.
fn normalize_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string())
}

/// Check if a worktree path is in use by any active agent.
fn is_in_use(wt_path: &Path, active_dirs: &[PathBuf]) -> bool {
    let wt_norm = normalize_path(
        &wt_path
            .canonicalize()
            .unwrap_or_else(|_| wt_path.to_path_buf()),
    );
    active_dirs.iter().any(|wd| {
        let wd_norm = normalize_path(&wd.canonicalize().unwrap_or_else(|_| wd.clone()));
        wd_norm.starts_with(&wt_norm) || wd.starts_with(wt_path)
    })
}

/// Runtime-based sweep: uses AgentConfig data to find repos and detect in-use worktrees.
///
/// `configs`: map of agent name → (working_dir, worktree_source) from daemon's live registry.
/// `fleet_dirs`: fallback working_directories from fleet.yaml for stopped agents.
///
/// Returns list of (branch, path, repo) that were removed.
pub fn sweep_from_registry(
    configs: &HashMap<String, (Option<PathBuf>, Option<PathBuf>)>,
    fleet_dirs: &[PathBuf],
) -> Vec<(String, String)> {
    if !auto_cleanup_enabled() {
        return Vec::new();
    }

    // Collect unique source repos from active configs
    let mut repos: HashSet<PathBuf> = HashSet::new();
    let mut active_dirs: Vec<PathBuf> = Vec::new();

    for (_name, (working_dir, worktree_source)) in configs {
        if let Some(src) = worktree_source {
            repos.insert(src.clone());
        }
        if let Some(wd) = working_dir {
            active_dirs.push(wd.clone());
        }
    }
    // Add fleet.yaml dirs as fallback for stopped agents
    active_dirs.extend(fleet_dirs.iter().cloned());

    let mut removed = Vec::new();

    for repo in &repos {
        let entries = list_worktrees(repo);
        for entry in &entries {
            let wt_path = Path::new(&entry.path);

            if is_in_use(wt_path, &active_dirs) {
                tracing::debug!(branch = %entry.branch, path = %entry.path, "skipping worktree (in use by agent)");
                continue;
            }

            if is_worktree_dirty(wt_path) {
                tracing::debug!(branch = %entry.branch, path = %entry.path, "skipping dirty worktree");
                continue;
            }

            if !is_branch_merged(repo, &entry.branch) {
                continue;
            }

            if remove_worktree(repo, &entry.path, &entry.branch) {
                removed.push((entry.branch.clone(), entry.path.clone()));
            }
        }
    }
    removed
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn setup_test_repo(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agend-wt-v2-{}-{}-{}",
            tag,
            std::process::id(),
            C.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).ok();
        git_in(&dir, &["init", "-b", "main"]);
        std::fs::write(dir.join("README.md"), "init").ok();
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-m", "init"]);
        dir
    }

    fn git_in(dir: &Path, args: &[&str]) {
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
    fn test_flag_disabled_default() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        assert!(!auto_cleanup_enabled());
    }

    #[test]
    fn test_flag_enabled() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        assert!(auto_cleanup_enabled());
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    }

    #[test]
    fn test_sweep_noop_when_flag_disabled() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        let configs = HashMap::new();
        let removed = sweep_from_registry(&configs, &[]);
        assert!(removed.is_empty());
    }

    #[test]
    fn test_v2_merged_worktree_removed() {
        let _lock = ENV_LOCK.lock().unwrap();
        let repo = setup_test_repo("v2-merged");
        git_in(&repo, &["branch", "feat/done"]);
        let wt = repo.join("wt-done");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/done"],
        );
        git_in(&repo, &["merge", "feat/done"]);

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        // No active agent using this worktree
        let mut configs = HashMap::new();
        configs.insert(
            "other-agent".to_string(),
            (Some(repo.join("other")), Some(repo.clone())),
        );
        let removed = sweep_from_registry(&configs, &[]);
        assert!(
            removed.iter().any(|(b, _)| b == "feat/done"),
            "merged worktree must be removed: {removed:?}"
        );
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn test_v2_dirty_worktree_preserved() {
        let _lock = ENV_LOCK.lock().unwrap();
        let repo = setup_test_repo("v2-dirty");
        git_in(&repo, &["branch", "feat/dirty"]);
        let wt = repo.join("wt-dirty");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/dirty"],
        );
        git_in(&repo, &["merge", "feat/dirty"]);
        std::fs::write(wt.join("uncommitted.txt"), "dirty").ok();

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        let mut configs = HashMap::new();
        configs.insert(
            "agent".to_string(),
            (Some(repo.join("other")), Some(repo.clone())),
        );
        let removed = sweep_from_registry(&configs, &[]);
        assert!(removed.is_empty(), "dirty worktree must NOT be removed");
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn test_v2_unmerged_worktree_preserved() {
        let _lock = ENV_LOCK.lock().unwrap();
        let repo = setup_test_repo("v2-unmerged");
        git_in(&repo, &["branch", "feat/wip"]);
        let wt = repo.join("wt-wip");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/wip"],
        );
        std::fs::write(wt.join("new.txt"), "x").ok();
        git_in(&wt, &["add", "."]);
        git_in(&wt, &["commit", "-m", "wip"]);

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        let mut configs = HashMap::new();
        configs.insert(
            "agent".to_string(),
            (Some(repo.join("other")), Some(repo.clone())),
        );
        let removed = sweep_from_registry(&configs, &[]);
        assert!(removed.is_empty(), "unmerged worktree must NOT be removed");
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    #[cfg(unix)] // Windows path format — t-20260424173948421544-1
    fn test_v2_active_runtime_worktree_not_removed_under_bootstrap_redirect() {
        // Production shape: agent's working_dir is <repo>/.worktrees/<agent>,
        // worktree_source is <repo>. Sweep must NOT remove the active worktree.
        let _lock = ENV_LOCK.lock().unwrap();
        let repo = setup_test_repo("v2-active");
        git_in(&repo, &["branch", "feat/active"]);
        let wt = repo.join("wt-active");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/active"],
        );
        git_in(&repo, &["merge", "feat/active"]);
        // Merged + clean, but agent is actively using this worktree

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        let mut configs = HashMap::new();
        // Agent's working_dir points to the worktree (bootstrap redirect)
        configs.insert(
            "active-agent".to_string(),
            (Some(wt.clone()), Some(repo.clone())),
        );
        let removed = sweep_from_registry(&configs, &[]);
        assert!(
            removed.is_empty(),
            "active agent worktree must NOT be removed: {removed:?}"
        );
        assert!(wt.exists(), "worktree dir must still exist");
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::fs::remove_dir_all(&repo).ok();
    }
}
