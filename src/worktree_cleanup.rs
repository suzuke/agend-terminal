//! Worktree auto-cleanup v2 — runtime registry based.
//!
//! On by default; gated **opt-out** via `AGEND_WORKTREE_AUTO_CLEANUP=0`
//! (any other value, or unset, leaves it enabled — see `auto_cleanup_enabled`).
//! Sweeps worktrees whose branches are merged into main OR whose remote
//! tracking ref has been deleted (squash-merged PRs), using the daemon's
//! live AgentConfig registry to find repos and detect in-use worktrees.
//! Also prunes orphaned local branches with no worktree.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Returns true unless `AGEND_WORKTREE_AUTO_CLEANUP` is explicitly set to "0".
/// Cleanup is on by default — set `AGEND_WORKTREE_AUTO_CLEANUP=0` to disable.
pub fn auto_cleanup_enabled() -> bool {
    std::env::var("AGEND_WORKTREE_AUTO_CLEANUP")
        .ok()
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// Entry for a git worktree.
#[derive(Debug, Clone)]
pub struct WorktreeEntry {
    pub path: String,
    pub branch: String,
}

/// List all git worktrees (excluding the main worktree).
fn list_worktrees(repo_root: &Path) -> Vec<WorktreeEntry> {
    // git-raw-allowed: TRIM-SENSITIVE parser. `--porcelain` terminates each
    // worktree record with a blank line; the loop below flushes a pending entry
    // on that blank line. `git_cmd` trims trailing whitespace → the final record's
    // terminator is dropped → the last (often only) worktree is never pushed →
    // the sweep silently finds nothing. Must read raw, untrimmed stdout.
    // (Already AGEND_GIT_BYPASS.)
    let output = match Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
    {
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

/// Check if a branch is merged into the default branch (local check, no API needed).
fn is_branch_merged(repo_root: &Path, branch: &str) -> bool {
    let default = crate::git_helpers::default_branch(repo_root);
    // W1.2: git_ok = always-bypass + bounded, true iff exit-0 (the
    // `output().map(success).unwrap_or(false)` idiom, byte-for-byte).
    crate::git_helpers::git_ok(
        repo_root,
        &["merge-base", "--is-ancestor", branch, &default],
    )
}

/// Check if a branch's remote tracking ref has been deleted (i.e. the PR was
/// squash-merged or the remote branch was deleted). This catches the common
/// case where `is_branch_merged` returns false because GitHub squash-merge
/// rewrites the commit hash.
fn is_remote_gone(repo_root: &Path, branch: &str) -> bool {
    // Read upstream tracking remote name
    // W1.2: git_cmd → trimmed stdout on success; the `success && !stdout.is_empty()`
    // filter becomes Ok-then-non-empty.
    let remote =
        crate::git_helpers::git_cmd(repo_root, &["config", &format!("branch.{branch}.remote")])
            .ok()
            .filter(|s| !s.is_empty());
    let Some(remote) = remote else {
        // No remote configured — not a remote-tracking branch, don't treat as "gone"
        return false;
    };
    // Check if the remote ref still exists
    let remote_ref = format!("refs/remotes/{remote}/{branch}");
    // git-raw-allowed: error→EXISTS (`unwrap_or(true)`) is a deliberate safe
    // default — a transient git error must NOT be read as "remote gone" (which
    // would auto-delete a live branch). `git_ok`'s error→false would INVERT this,
    // so do not "tidy" this into git_ok. (Already AGEND_GIT_BYPASS.)
    let exists = Command::new("git")
        .args(["rev-parse", "--verify", &remote_ref])
        .current_dir(repo_root)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(true);
    !exists
}

/// Check if a worktree has uncommitted changes.
fn is_worktree_dirty(worktree_path: &Path) -> bool {
    // git-raw-allowed: error→DIRTY (`unwrap_or(true)`) is a deliberate safe
    // default — a git error must protect uncommitted work, not let it be swept.
    // `git_ok`'s error→false would invert this (also: needs `!stdout.is_empty()`,
    // not exit-status). (Already AGEND_GIT_BYPASS.)
    Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree_path)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(true)
}

/// Remove a worktree and delete its branch.
///
/// On Windows, retries up to 3 times with exponential backoff (200ms, 400ms)
/// to absorb transient EACCES from file locks held by preceding git processes.
fn remove_worktree(repo_root: &Path, worktree_path: &str, branch: &str) -> bool {
    let max_attempts: u32 = if cfg!(windows) { 3 } else { 1 };
    let mut wt_ok = false;
    for attempt in 0..max_attempts {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(100 * (1 << attempt)));
        }
        wt_ok = crate::git_helpers::git_ok(
            repo_root,
            &["worktree", "remove", "--force", worktree_path],
        );
        if wt_ok {
            break;
        }
    }
    if wt_ok {
        // W1.2: best-effort branch delete (result was already ignored).
        let _ = crate::git_helpers::git_ok(repo_root, &["branch", "-D", branch]);
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

    for (working_dir, worktree_source) in configs.values() {
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
        // Prune stale remote refs before remote-gone detection
        let remote = crate::git_helpers::primary_remote(repo);
        // git-raw-allowed: NETWORK op — `git_cmd` hardcodes LOCAL_GIT_TIMEOUT (60s),
        // too tight for a fetch; use the raw form (already AGEND_GIT_BYPASS) rather
        // than shoehorn a network op through the local helper. (A `git_cmd_network`
        // variant is YAGNI for this single fire-and-forget fetch.)
        let _ = Command::new("git")
            .args(["fetch", "--prune", &remote])
            .current_dir(repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output();

        // Phase 1: clean worktrees (existing logic + remote-gone)
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

            let merged = is_branch_merged(repo, &entry.branch);
            let gone = is_remote_gone(repo, &entry.branch);
            if !merged && !gone {
                continue;
            }

            tracing::info!(
                branch = %entry.branch,
                path = %entry.path,
                reason = if merged { "merged" } else { "remote-gone" },
                "removing stale worktree"
            );
            if remove_worktree(repo, &entry.path, &entry.branch) {
                removed.push((entry.branch.clone(), entry.path.clone()));
            }
        }

        // Phase 2: prune orphaned branches (no worktree, remote gone or merged)
        prune_stale_worktrees(repo);
        let pruned = prune_orphaned_branches(repo);
        for branch in pruned {
            removed.push((branch, String::new()));
        }
    }
    removed
}

/// #1750-B3: minimum branch-tip age before the SQUASH-merged path will auto-GC
/// a branch. The `--merged`/remote-gone signals are definitive and need no age
/// belt, but the cherry/tree-diff squash detection is heuristic — a young branch
/// that happens to be tree-equal to main (or a PR merged moments ago that a
/// human may still follow up on locally) is left for a later tick. A
/// genuinely-orphaned squash-merged branch's tip predates the merge, so it
/// clears this floor on the next sweep.
const SQUASH_GC_MIN_TIP_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// #1750-B3: age of `branch`'s tip commit (committer date), or `None` if it
/// can't be resolved. `%ct` is a unix timestamp (seconds), so no date parsing.
fn branch_tip_age(repo_root: &Path, branch: &str) -> Option<Duration> {
    // W1.2: git_cmd → trimmed stdout; spawn-error + non-zero both collapse to `None`.
    let ts_str =
        crate::git_helpers::git_cmd(repo_root, &["log", "-1", "--format=%ct", branch]).ok()?;
    let ts: u64 = ts_str.parse().ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(Duration::from_secs(now.saturating_sub(ts)))
}

/// #1750-B3: is `branch` a squash-merge orphan eligible for auto-GC? True when
/// it is squash-merged into the default branch AND its tip is older than
/// [`SQUASH_GC_MIN_TIP_AGE`]. Reuses `branch_sweep`'s detection (git cherry +
/// #1280 tree-diff fallback) so the auto path matches the operator sweep.
fn is_squash_gc_eligible(repo_root: &Path, branch: &str, default: &str) -> bool {
    crate::branch_sweep::is_squash_merged(repo_root, default, branch)
        && branch_tip_age(repo_root, branch).is_some_and(|age| age >= SQUASH_GC_MIN_TIP_AGE)
}

/// Run `git worktree prune` then delete local branches whose remote tracking
/// ref is gone, that are merged into main, or that are squash-merge orphans
/// (#1750-B3). Skips branches checked out in any worktree.
fn prune_orphaned_branches(repo_root: &Path) -> Vec<String> {
    let default = crate::git_helpers::default_branch(repo_root);
    // Collect branches currently checked out in worktrees — cannot delete these
    let wt_branches: HashSet<String> = list_worktrees(repo_root)
        .into_iter()
        .map(|e| e.branch)
        .collect();

    // W1.2: git_cmd → trimmed stdout on success; spawn-error + non-zero collapse to `Err → []`.
    let branches: Vec<String> =
        match crate::git_helpers::git_cmd(repo_root, &["branch", "--format=%(refname:short)"]) {
            Ok(stdout) => stdout
                .lines()
                .filter(|b| *b != default.as_str())
                .map(String::from)
                .collect(),
            _ => return Vec::new(),
        };

    let mut pruned = Vec::new();
    for branch in &branches {
        if wt_branches.contains(branch) {
            continue;
        }
        let merged = is_branch_merged(repo_root, branch);
        let gone = is_remote_gone(repo_root, branch);
        // #1750-B3: also reap squash-merge orphans (the 95/99 case the
        // squash-blind `--merged` missed) — gated on tip-age for the heuristic
        // squash detection only.
        let squash = !merged && !gone && is_squash_gc_eligible(repo_root, branch, &default);
        if !merged && !gone && !squash {
            continue;
        }
        let ok = crate::git_helpers::git_ok(repo_root, &["branch", "-D", branch]);
        if ok {
            let reason = if merged {
                "merged"
            } else if gone {
                "remote-gone"
            } else {
                "squash-merged"
            };
            tracing::info!(branch, reason, "pruned orphaned branch");
            pruned.push(branch.clone());
        }
    }
    pruned
}

/// Run `git worktree prune` to clean stale worktree bookkeeping entries.
fn prune_stale_worktrees(repo_root: &Path) {
    // W1.2: best-effort prune (result was already ignored).
    let _ = crate::git_helpers::git_ok(repo_root, &["worktree", "prune"]);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

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
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git");
    }

    #[test]
    fn test_flag_disabled_default() {
        let _lock = ENV_LOCK.lock();
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        assert!(auto_cleanup_enabled());
    }

    #[test]
    fn test_flag_disabled_explicit() {
        let _lock = ENV_LOCK.lock();
        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "0");
        assert!(!auto_cleanup_enabled());
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    }

    #[test]
    fn test_flag_enabled() {
        let _lock = ENV_LOCK.lock();
        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        assert!(auto_cleanup_enabled());
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    }

    #[test]
    fn test_sweep_noop_when_flag_disabled() {
        let _lock = ENV_LOCK.lock();
        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "0");
        let configs = HashMap::new();
        let removed = sweep_from_registry(&configs, &[]);
        assert!(removed.is_empty());
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    }

    #[test]
    fn test_v2_merged_worktree_removed() {
        let _lock = ENV_LOCK.lock();
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
        let _lock = ENV_LOCK.lock();
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
        let _lock = ENV_LOCK.lock();
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
        let _lock = ENV_LOCK.lock();
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

    #[test]
    fn test_v2_remote_gone_worktree_removed() {
        // Simulate squash-merge: branch is NOT merged (different hash) but
        // remote tracking ref is gone after `git fetch --prune`.
        let _lock = ENV_LOCK.lock();

        // Create "remote" bare repo
        let remote_dir = std::env::temp_dir().join(format!(
            "agend-wt-v2-remote-gone-{}-{}",
            std::process::id(),
            std::sync::atomic::AtomicU32::new(0).fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&remote_dir).ok();
        git_in(&remote_dir, &["init", "--bare", "-b", "main"]);

        // Clone it
        let repo = std::env::temp_dir().join(format!(
            "agend-wt-v2-remote-gone-clone-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&repo);
        Command::new("git")
            .args([
                "clone",
                remote_dir.to_str().unwrap(),
                repo.to_str().unwrap(),
            ])
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("clone");
        std::fs::write(repo.join("README.md"), "init").ok();
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-m", "init"]);
        git_in(&repo, &["push", "-u", "origin", "main"]);

        // Create a feature branch, push it, then delete remote ref
        git_in(&repo, &["checkout", "-b", "feat/squashed"]);
        std::fs::write(repo.join("feat.txt"), "feature").ok();
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-m", "feature work"]);
        git_in(&repo, &["push", "-u", "origin", "feat/squashed"]);
        git_in(&repo, &["checkout", "main"]);

        // Create worktree on that branch
        let wt = repo.join("wt-squashed");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/squashed"],
        );

        // Simulate: remote branch deleted (squash-merged on GitHub)
        git_in(&remote_dir, &["branch", "-D", "feat/squashed"]);
        git_in(&repo, &["fetch", "--prune"]);

        // Branch is NOT merged (different commit hash) but remote is gone
        assert!(
            !is_branch_merged(&repo, "feat/squashed"),
            "branch should NOT be detected as merged"
        );
        assert!(
            is_remote_gone(&repo, "feat/squashed"),
            "branch remote should be detected as gone"
        );

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        let mut configs = HashMap::new();
        configs.insert(
            "other".to_string(),
            (Some(repo.join("other")), Some(repo.clone())),
        );
        let removed = sweep_from_registry(&configs, &[]);
        assert!(
            removed.iter().any(|(b, _)| b == "feat/squashed"),
            "remote-gone worktree must be removed: {removed:?}"
        );
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&remote_dir).ok();
    }

    // ── #1750-B3: local squash-merge orphan auto-GC ──

    /// Commit like `git_in`'s commit but with a fixed author+committer DATE, so
    /// `branch_tip_age` is deterministic regardless of wall-clock.
    fn git_commit_dated(dir: &Path, msg: &str, date: &str) {
        Command::new("git")
            .args(["commit", "-m", msg])
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .output()
            .expect("dated commit");
    }

    /// Build a LOCAL squash-merge orphan: `branch` carries `feat.txt`, then main
    /// diverges (`other.txt`) and cherry-picks `branch`'s patch — so `branch` is
    /// NOT a `--merged` ancestor (different SHA) but IS squash-merged (git cherry
    /// shows `-`). `branch`'s tip is committed at `tip_date`.
    fn make_squash_orphan(repo: &Path, branch: &str, tip_date: &str) {
        git_in(repo, &["checkout", "-b", branch]);
        std::fs::write(repo.join("feat.txt"), "feature").ok();
        git_in(repo, &["add", "."]);
        git_commit_dated(repo, "feature work", tip_date);
        git_in(repo, &["checkout", "main"]);
        // Diverge main on a DIFFERENT file so the cherry-pick applies cleanly.
        std::fs::write(repo.join("other.txt"), "main-side").ok();
        git_in(repo, &["add", "."]);
        git_in(repo, &["commit", "-m", "main diverge"]);
        git_in(repo, &["cherry-pick", branch]);
    }

    #[test]
    fn prune_squash_merged_old_branch_1750_b3() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("b3-squash-old");
        // Old tip (well past SQUASH_GC_MIN_TIP_AGE) + squash-merged into main.
        make_squash_orphan(&repo, "feat/squash-old", "2024-01-01T00:00:00 +0000");
        // Precondition: the squash-blind signals MISS it (the #1750 bug).
        assert!(
            !is_branch_merged(&repo, "feat/squash-old"),
            "not a --merged ancestor"
        );
        assert!(
            !is_remote_gone(&repo, "feat/squash-old"),
            "no remote configured"
        );

        let pruned = prune_orphaned_branches(&repo);
        assert!(
            pruned.iter().any(|b| b == "feat/squash-old"),
            "#1750-B3: a squash-merged orphan past the age floor must be auto-GC'd, got: {pruned:?}"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn prune_skips_squash_merged_too_new_1750_b3() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("b3-squash-new");
        // Squash-merged but tip committed NOW (git_in default date) → under the
        // age floor → must NOT be deleted yet (a later sweep reaps it).
        git_in(&repo, &["checkout", "-b", "feat/squash-new"]);
        std::fs::write(repo.join("feat.txt"), "feature").ok();
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-m", "feature work"]); // now-dated tip
        git_in(&repo, &["checkout", "main"]);
        std::fs::write(repo.join("other.txt"), "main-side").ok();
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-m", "main diverge"]);
        git_in(&repo, &["cherry-pick", "feat/squash-new"]);

        assert!(
            crate::branch_sweep::is_squash_merged(&repo, "main", "feat/squash-new"),
            "precondition: detected as squash-merged"
        );
        let pruned = prune_orphaned_branches(&repo);
        assert!(
            !pruned.iter().any(|b| b == "feat/squash-new"),
            "#1750-B3: a squash-merged branch under the tip-age floor must NOT be GC'd yet"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn prune_skips_unmerged_branch_1750_b3() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("b3-unmerged");
        // A genuinely unmerged branch (old tip) — squash detection must NOT fire.
        git_in(&repo, &["checkout", "-b", "feat/wip"]);
        std::fs::write(repo.join("feat.txt"), "wip").ok();
        git_in(&repo, &["add", "."]);
        git_commit_dated(&repo, "wip", "2024-01-01T00:00:00 +0000");
        git_in(&repo, &["checkout", "main"]);

        assert!(
            !crate::branch_sweep::is_squash_merged(&repo, "main", "feat/wip"),
            "precondition: NOT squash-merged"
        );
        let pruned = prune_orphaned_branches(&repo);
        assert!(
            !pruned.iter().any(|b| b == "feat/wip"),
            "#1750-B3: a real unmerged branch must NOT be GC'd"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn prune_skips_checked_out_squash_orphan_1750_b3() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("b3-squash-checkedout");
        make_squash_orphan(&repo, "feat/squash-wt", "2024-01-01T00:00:00 +0000");
        // Check the squash-merged branch out in a worktree → must be skipped.
        let wt = repo.join("wt-squash");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/squash-wt"],
        );

        let pruned = prune_orphaned_branches(&repo);
        assert!(
            !pruned.iter().any(|b| b == "feat/squash-wt"),
            "#1750-B3: a squash-merged branch checked out in a worktree must NOT be GC'd"
        );
        std::fs::remove_dir_all(&repo).ok();
    }
}
