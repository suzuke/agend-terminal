//! Worktree pool — daemon-managed lease/release lifecycle for git worktrees.
//!
//! Builds on existing `worktree.rs` (creation) + `binding.rs` (state).
//! Phase 3: lease/release + daemon-tag + E4.5 enforcement. GC deferred to Phase 4.

use std::path::{Path, PathBuf};

/// Marker file placed in daemon-managed worktrees (R14 mitigation).
const MANAGED_MARKER: &str = ".agend-managed";

/// A lease on a worktree — returned by `lease()`, consumed by `release()`.
#[derive(Debug, Clone)]
pub struct WorktreeLease {
    pub agent: String,
    pub branch: String,
    pub path: PathBuf,
}

/// Lease a worktree for an agent + branch. Creates if needed, tags as daemon-managed.
/// Rejects `main` branch per E4.5 enforcement.
pub fn lease(
    home: &Path,
    source_repo: &Path,
    agent: &str,
    branch: &str,
) -> Result<WorktreeLease, String> {
    // E4.5: reject main branch lease.
    if branch == "main" || branch == "master" {
        return Err(format!(
            "E4.5 violation: cannot lease worktree for protected branch '{branch}'"
        ));
    }

    // Create worktree using existing infrastructure.
    let info = match crate::worktree::create(source_repo, agent, Some(branch)) {
        Some(info) => info,
        None => return Err(format!("failed to create worktree for {agent}@{branch}")),
    };

    // Tag as daemon-managed (R14: only daemon-tagged worktrees are GC candidates).
    let marker = info.path.join(MANAGED_MARKER);
    let _ = std::fs::write(
        &marker,
        format!(
            "agent={agent}\nbranch={branch}\nleased_at={}\n",
            chrono::Utc::now().to_rfc3339()
        ),
    );

    // Write full binding with worktree path.
    crate::binding::bind_full(home, agent, "", branch, &info.path);

    Ok(WorktreeLease {
        agent: agent.to_string(),
        branch: branch.to_string(),
        path: info.path,
    })
}

/// Release a lease — marks worktree as GC candidate (does NOT delete, Phase 4).
pub fn release(home: &Path, lease: &WorktreeLease) {
    // Clear binding (task done).
    crate::binding::unbind(home, &lease.agent);
    // Log release event.
    crate::event_log::log(
        home,
        "worktree_lease_released",
        &lease.agent,
        &format!("branch={} path={}", lease.branch, lease.path.display()),
    );
}

/// Check if a worktree is daemon-managed (has .agend-managed marker).
pub fn is_daemon_managed(worktree_path: &Path) -> bool {
    worktree_path.join(MANAGED_MARKER).exists()
}

/// Pin a worktree (operator override — prevents GC in Phase 4).
pub fn pin(worktree_path: &Path) {
    let pin_file = worktree_path.join(".agend-pinned");
    let _ = std::fs::write(&pin_file, chrono::Utc::now().to_rfc3339());
}

/// Unpin a worktree (allow GC again).
pub fn unpin(worktree_path: &Path) {
    let pin_file = worktree_path.join(".agend-pinned");
    let _ = std::fs::remove_file(pin_file);
}

/// Check if a worktree is pinned.
#[allow(dead_code)]
pub fn is_pinned(worktree_path: &Path) -> bool {
    worktree_path.join(".agend-pinned").exists()
}

/// Reconcile orphan leases at daemon startup (log only, no delete in Phase 3).
pub fn reconcile_orphan_leases(home: &Path) {
    let runtime_dir = home.join("runtime");
    if !runtime_dir.exists() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(&runtime_dir) {
        for entry in entries.flatten() {
            let binding_path = entry.path().join("binding.json");
            if !binding_path.exists() {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&binding_path) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(wt_path) = v["worktree"].as_str() {
                        if !Path::new(wt_path).exists() {
                            tracing::warn!(
                                agent = entry.file_name().to_string_lossy().as_ref(),
                                worktree = wt_path,
                                "orphan lease: worktree path missing"
                            );
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

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-pool-test-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn tmp_repo(tag: &str) -> PathBuf {
        let dir = tmp_home(tag);
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .ok();
        std::process::Command::new("git")
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
            .current_dir(&dir)
            .output()
            .ok();
        dir
    }

    #[test]
    fn lease_main_branch_rejected() {
        let home = tmp_home("main-reject");
        let repo = tmp_repo("main-reject-repo");
        let result = lease(&home, &repo, "agent-1", "main");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("E4.5"));
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn lease_creates_daemon_tagged_worktree() {
        let home = tmp_home("lease-tag");
        let repo = tmp_repo("lease-tag-repo");
        let result = lease(&home, &repo, "agent-2", "feat/test");
        assert!(result.is_ok());
        let l = result.expect("lease");
        assert!(l.path.exists());
        assert!(is_daemon_managed(&l.path));
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_marks_candidate_no_delete() {
        let home = tmp_home("release");
        let repo = tmp_repo("release-repo");
        let l = lease(&home, &repo, "agent-3", "feat/release").expect("lease");
        release(&home, &l);
        // Worktree still exists (no delete in Phase 3).
        assert!(l.path.exists());
        // Binding cleared.
        assert!(crate::binding::read(&home, "agent-3").is_none());
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_idempotent() {
        let home = tmp_home("release-idem");
        let repo = tmp_repo("release-idem-repo");
        let l = lease(&home, &repo, "agent-4", "feat/idem").expect("lease");
        release(&home, &l);
        release(&home, &l); // second release — no panic
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn is_daemon_managed_excludes_human_worktrees() {
        let dir = tmp_home("human-wt");
        // No marker → not managed.
        assert!(!is_daemon_managed(&dir));
        // Add marker → managed.
        std::fs::write(dir.join(MANAGED_MARKER), "test").ok();
        assert!(is_daemon_managed(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pin_unpin_idempotent() {
        let dir = tmp_home("pin");
        pin(&dir);
        assert!(is_pinned(&dir));
        pin(&dir); // idempotent
        assert!(is_pinned(&dir));
        unpin(&dir);
        assert!(!is_pinned(&dir));
        unpin(&dir); // idempotent
        assert!(!is_pinned(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }
}
