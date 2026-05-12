//! Binding manager — writes per-agent binding.json for git shim + hook.
//!
//! Daemon-only writer. Shim and hooks are read-only consumers.
//! Uses atomic_write (temp + fsync + rename) + flock for safety.

use serde_json::json;
use std::path::Path;

/// Write a binding for an agent (task assigned).
#[allow(dead_code)] // Used by tests + auto-watch dispatch path
pub fn bind(home: &Path, agent: &str, task_id: &str, branch: &str) {
    bind_full(
        home,
        agent,
        task_id,
        branch,
        std::path::Path::new(""),
        std::path::Path::new(""),
    );
}

/// Write a full binding including worktree + source-repo paths.
///
/// `source_repo` is the parent repo that owns the worktree, persisted as a
/// schema field so `worktree_pool::release_full` (Sprint 53 P0-X r1) can run
/// `git worktree remove --force` from the owning repo's cwd. Without this,
/// the git registry leaves a stale prunable entry after a manual `remove_dir_all`
/// fallback. Pass an empty path when unknown — `release_full` falls back to
/// deriving the source from the worktree path's `.worktrees/<agent>` ancestor.
pub fn bind_full(
    home: &Path,
    agent: &str,
    task_id: &str,
    branch: &str,
    worktree: &std::path::Path,
    source_repo: &std::path::Path,
) {
    let dir = crate::paths::runtime_dir(home).join(agent);
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("binding.json");
    let lock_path = dir.join(".binding.json.lock");
    let _lock = crate::store::acquire_file_lock(&lock_path);
    let wt_str = worktree.display().to_string();
    let src_str = source_repo.display().to_string();
    let mut binding = json!({
        "version": 1,
        "agent": agent,
        "task_id": task_id,
        "branch": branch,
        "issued_at": chrono::Utc::now().to_rfc3339(),
    });
    if !wt_str.is_empty() {
        binding["worktree"] = json!(wt_str);
    }
    if !src_str.is_empty() {
        binding["source_repo"] = json!(src_str);
    }
    let body = serde_json::to_string_pretty(&binding).unwrap_or_default();
    let _ = crate::store::atomic_write(&path, body.as_bytes());
}

/// Clear a binding for an agent (task completed/released).
pub fn unbind(home: &Path, agent: &str) {
    let path = crate::paths::runtime_dir(home)
        .join(agent)
        .join("binding.json");
    let _ = std::fs::remove_file(path);
}

/// Returns Some(agent_name) if any other agent has bound this branch.
/// Used by dispatch_auto_bind_lease to enforce cross-agent branch uniqueness.
pub fn scan_existing_branch_binding(
    home: &Path,
    branch: &str,
    exclude_agent: &str,
) -> Option<String> {
    let runtime_dir = crate::paths::runtime_dir(home);
    let entries = std::fs::read_dir(&runtime_dir).ok()?;
    for entry in entries.flatten() {
        let agent = entry.file_name().to_string_lossy().to_string();
        if agent == exclude_agent {
            continue;
        }
        let binding_path = entry.path().join("binding.json");
        let Ok(content) = std::fs::read_to_string(&binding_path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        if v["branch"].as_str() == Some(branch) {
            return Some(agent);
        }
    }
    None
}

/// Read the current binding for an agent (for internal use/tests).
#[allow(dead_code)] // Used by tests + Phase 2
pub fn read(home: &Path, agent: &str) -> Option<serde_json::Value> {
    let path = crate::paths::runtime_dir(home)
        .join(agent)
        .join("binding.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
}

/// Check if an agent is bound in a daemon-managed worktree.
/// Returns true if the agent has a binding with a worktree path that
/// contains the `.agend-managed` marker file.
pub fn is_agent_in_managed_worktree(home: &Path, agent: &str) -> bool {
    read(home, agent)
        .and_then(|v| v["worktree"].as_str().map(std::path::PathBuf::from))
        .map(|wt| wt.join(".agend-managed").exists())
        .unwrap_or(false)
}

/// Install the prepare-commit-msg hook into a worktree via core.hooksPath.
/// Points to `$AGEND_HOME/hooks/` unified directory.
/// Installs bash hook on Unix, PowerShell hook on Windows.
pub fn install_hooks(home: &Path, worktree: &Path) {
    let hooks_dir = home.join("hooks");
    std::fs::create_dir_all(&hooks_dir).ok();

    // Extract embedded hook scripts (both platforms for portability).
    let bash_hook = include_str!("../assets/hooks/prepare-commit-msg");
    let bash_path = hooks_dir.join("prepare-commit-msg");
    let _ = std::fs::write(&bash_path, bash_hook);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&bash_path, std::fs::Permissions::from_mode(0o755));
    }

    // Windows: also install PowerShell version.
    let ps_hook = include_str!("../assets/hooks/prepare-commit-msg.ps1");
    let ps_path = hooks_dir.join("prepare-commit-msg.ps1");
    let _ = std::fs::write(&ps_path, ps_hook);

    // Set core.hooksPath on the worktree.
    let _ = std::process::Command::new("git")
        .args(["config", "core.hooksPath", &hooks_dir.display().to_string()])
        .current_dir(worktree)
        .output();
}

/// Install hooks on all existing worktrees (daemon startup reconcile).
pub fn reconcile_hooks(home: &Path) {
    // New layout: <home>/worktrees/<agent>/<branch>/
    let new_root = crate::worktree_pool::daemon_managed_worktree_root(home);
    if new_root.is_dir() {
        if let Ok(agents) = std::fs::read_dir(&new_root) {
            for agent_entry in agents.flatten() {
                if !agent_entry.path().is_dir() {
                    continue;
                }
                if let Ok(branches) = std::fs::read_dir(agent_entry.path()) {
                    for branch_entry in branches.flatten() {
                        if branch_entry.path().is_dir() {
                            install_hooks(home, &branch_entry.path());
                        }
                    }
                }
            }
        }
    }

    // Legacy layout: <home>/workspace/*/.worktrees/*/
    let worktrees_base = crate::paths::workspace_dir(home);
    if !worktrees_base.exists() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(&worktrees_base) {
        for entry in entries.flatten() {
            let wt_dir = entry.path().join(".worktrees");
            if wt_dir.is_dir() {
                if let Ok(wts) = std::fs::read_dir(&wt_dir) {
                    for wt in wts.flatten() {
                        if wt.path().is_dir() {
                            install_hooks(home, &wt.path());
                        }
                    }
                }
            }
        }
    }
}

/// Symlink the agend-git binary into $AGEND_HOME/bin/git.
/// Called at daemon startup so the shim shadows /usr/bin/git via PATH.
pub fn symlink_shim(home: &Path) {
    let bin_dir = home.join("bin");
    std::fs::create_dir_all(&bin_dir).ok();
    let link_name = if cfg!(windows) { "git.exe" } else { "git" };
    let link_path = bin_dir.join(link_name);

    // Find the agend-git binary alongside the main binary.
    let shim_name = if cfg!(windows) {
        "agend-git.exe"
    } else {
        "agend-git"
    };
    let shim_src = std::env::current_exe().ok().and_then(|exe| {
        let candidate = exe.with_file_name(shim_name);
        candidate.exists().then_some(candidate)
    });

    if let Some(src) = shim_src {
        // Remove stale symlink/file first.
        let _ = std::fs::remove_file(&link_path);
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(&src, &link_path);
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::copy(&src, &link_path);
        }
    }
}

/// Clear orphan bindings (agents no longer in registry).
/// Called at daemon startup.
pub fn reconcile_orphans(home: &Path) {
    let runtime_dir = crate::paths::runtime_dir(home);
    if !runtime_dir.exists() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(&runtime_dir) {
        for entry in entries.flatten() {
            let binding_path = entry.path().join("binding.json");
            if binding_path.exists() {
                // Check if binding is stale (issued_at > 24h ago).
                if let Ok(content) = std::fs::read_to_string(&binding_path) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(issued) = v["issued_at"].as_str() {
                            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(issued) {
                                let age = chrono::Utc::now()
                                    .signed_duration_since(dt.with_timezone(&chrono::Utc));
                                if age > chrono::Duration::hours(24) {
                                    // #693: check heartbeat — if agent is still active, don't delete
                                    let entry_path = entry.path();
                                    let agent_name = entry_path
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or("");
                                    let hb =
                                        crate::daemon::heartbeat_pair::snapshot_for(agent_name);
                                    let hb_age_ms = crate::daemon::heartbeat_pair::now_ms()
                                        .saturating_sub(hb.heartbeat_at_ms);
                                    if hb_age_ms < 3_600_000 {
                                        // Heartbeat within 1h — agent still active, skip
                                        continue;
                                    }
                                    let _ = std::fs::remove_file(&binding_path);
                                    tracing::info!(
                                        path = %binding_path.display(),
                                        "removed orphan binding (>24h old, heartbeat stale)"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-binding-test-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn bind_creates_binding_json() {
        let home = tmp_home("bind");
        bind(&home, "agent-1", "T-123", "feature-x");
        let binding = read(&home, "agent-1").expect("binding must exist");
        assert_eq!(binding["agent"], "agent-1");
        assert_eq!(binding["task_id"], "T-123");
        assert_eq!(binding["branch"], "feature-x");
        assert_eq!(binding["version"], 1);
        assert!(binding["issued_at"].as_str().is_some());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn unbind_removes_binding_json() {
        let home = tmp_home("unbind");
        bind(&home, "agent-2", "T-456", "fix-bug");
        assert!(read(&home, "agent-2").is_some());
        unbind(&home, "agent-2");
        assert!(read(&home, "agent-2").is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn read_missing_returns_none() {
        let home = tmp_home("read-miss");
        assert!(read(&home, "ghost").is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn marker_check_passes_for_managed_worktree() {
        let home = tmp_home("marker-pass");
        let wt = home.join("worktrees").join("agent-1").join("feat-branch");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".agend-managed"), "").unwrap();
        // Write binding pointing to this worktree
        let rt = crate::paths::runtime_dir(&home).join("agent-1");
        std::fs::create_dir_all(&rt).unwrap();
        let binding =
            serde_json::json!({"worktree": wt.to_str().unwrap(), "branch": "feat-branch"});
        std::fs::write(rt.join("binding.json"), binding.to_string()).unwrap();

        assert!(is_agent_in_managed_worktree(&home, "agent-1"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn marker_check_fails_for_unmanaged() {
        let home = tmp_home("marker-fail");
        let wt = home.join("worktrees").join("agent-2").join("feat-branch");
        std::fs::create_dir_all(&wt).unwrap();
        // No .agend-managed marker
        let rt = crate::paths::runtime_dir(&home).join("agent-2");
        std::fs::create_dir_all(&rt).unwrap();
        let binding =
            serde_json::json!({"worktree": wt.to_str().unwrap(), "branch": "feat-branch"});
        std::fs::write(rt.join("binding.json"), binding.to_string()).unwrap();

        assert!(!is_agent_in_managed_worktree(&home, "agent-2"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn marker_check_fails_for_no_binding() {
        let home = tmp_home("marker-no-bind");
        assert!(!is_agent_in_managed_worktree(&home, "nobody"));
        std::fs::remove_dir_all(&home).ok();
    }
}
