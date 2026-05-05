//! Binding manager — writes per-agent binding.json for git shim + hook.
//!
//! Daemon-only writer. Shim and hooks are read-only consumers.
//! Uses atomic_write (temp + fsync + rename) + flock for safety.

use serde_json::json;
use std::path::Path;

/// Write a binding for an agent (task assigned).
pub fn bind(home: &Path, agent: &str, task_id: &str, branch: &str) {
    let dir = home.join("runtime").join(agent);
    std::fs::create_dir_all(&dir).ok();
    let path = dir.join("binding.json");
    let lock_path = dir.join(".binding.json.lock");
    let _lock = crate::store::acquire_file_lock(&lock_path);
    let binding = json!({
        "version": 1,
        "agent": agent,
        "task_id": task_id,
        "branch": branch,
        "issued_at": chrono::Utc::now().to_rfc3339(),
    });
    let body = serde_json::to_string_pretty(&binding).unwrap_or_default();
    let _ = crate::store::atomic_write(&path, body.as_bytes());
}

/// Clear a binding for an agent (task completed/released).
pub fn unbind(home: &Path, agent: &str) {
    let path = home.join("runtime").join(agent).join("binding.json");
    let _ = std::fs::remove_file(path);
}

/// Read the current binding for an agent (for internal use/tests).
#[allow(dead_code)] // Used by tests + Phase 2
pub fn read(home: &Path, agent: &str) -> Option<serde_json::Value> {
    let path = home.join("runtime").join(agent).join("binding.json");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
}

/// Install the prepare-commit-msg hook into a worktree via core.hooksPath.
/// Points to `$AGEND_HOME/hooks/` unified directory.
pub fn install_hooks(home: &Path, worktree: &Path) {
    let hooks_dir = home.join("hooks");
    std::fs::create_dir_all(&hooks_dir).ok();

    // Extract embedded hook script.
    let hook_content = include_str!("../assets/hooks/prepare-commit-msg");
    let hook_path = hooks_dir.join("prepare-commit-msg");
    let _ = std::fs::write(&hook_path, hook_content);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755));
    }

    // Set core.hooksPath on the worktree.
    let _ = std::process::Command::new("git")
        .args(["config", "core.hooksPath", &hooks_dir.display().to_string()])
        .current_dir(worktree)
        .output();
}

/// Install hooks on all existing worktrees (daemon startup reconcile).
pub fn reconcile_hooks(home: &Path) {
    let worktrees_base = home.join("workspace");
    if !worktrees_base.exists() {
        return;
    }
    // Scan for .worktrees directories in workspace subdirs.
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
    let link_path = bin_dir.join("git");

    // Find the agend-git binary alongside the main binary.
    let shim_src = std::env::current_exe().ok().and_then(|exe| {
        let candidate = exe.with_file_name("agend-git");
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
    let runtime_dir = home.join("runtime");
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
                                    let _ = std::fs::remove_file(&binding_path);
                                    tracing::info!(
                                        path = %binding_path.display(),
                                        "removed orphan binding (>24h old)"
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
}
