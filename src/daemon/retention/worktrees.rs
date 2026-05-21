//! Worktree GC retention handler — safe removal with status pre-check.
//!
//! Replaces `remove_dir_all` with `git status --porcelain=v1 --ignored`
//! + `git worktree remove` (no --force). Catches .gitignored files that
//!   `git worktree remove` would silently delete.

use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) enum RemovalOutcome {
    Removed,
    Skipped { reason: String },
}

fn owning_repo(worktree: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(worktree.join(".git")).ok()?;
    let gitdir = content.strip_prefix("gitdir: ")?.trim();
    // gitdir = <repo>/.git/worktrees/<name> → 3 parents up = repo root
    Path::new(gitdir)
        .parent()?
        .parent()?
        .parent()
        .map(PathBuf::from)
}

pub(crate) fn maybe_remove_candidate(path: &Path) -> RemovalOutcome {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let path = path.as_path();
    let status_output =
        crate::git_helpers::git_bypass(path, &["status", "--porcelain=v1", "--ignored"]);
    match status_output {
        Ok(o) if o.status.success() => {
            if !o.stdout.is_empty() {
                let status_text = String::from_utf8_lossy(&o.stdout);
                tracing::warn!(
                    path = %path.display(),
                    status = %status_text.trim(),
                    "worktree has WIP (tracked/untracked/ignored), skipping GC"
                );
                return RemovalOutcome::Skipped {
                    reason: "wip_status_nonempty".to_string(),
                };
            }
        }
        Ok(o) => {
            tracing::warn!(
                path = %path.display(),
                stderr = %String::from_utf8_lossy(&o.stderr),
                "git status check failed, skipping GC"
            );
            return RemovalOutcome::Skipped {
                reason: "status_check_failed".to_string(),
            };
        }
        Err(e) => {
            tracing::error!(error = %e, "git status invocation failed");
            return RemovalOutcome::Skipped {
                reason: format!("invoke error: {e}"),
            };
        }
    }

    let path_str = path.to_str().unwrap_or_default();
    let repo = match owning_repo(path) {
        Some(r) => r,
        None => {
            tracing::warn!(path = %path.display(), "cannot determine owning repo, skipping GC");
            return RemovalOutcome::Skipped {
                reason: "owning_repo_unknown".to_string(),
            };
        }
    };
    let remove_output = std::process::Command::new("git")
        .args(["worktree", "remove", path_str])
        .current_dir(&repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    match remove_output {
        Ok(o) if o.status.success() => RemovalOutcome::Removed,
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            tracing::warn!(
                path = %path.display(),
                stderr = %stderr,
                "git worktree remove refused (post-status-check race), skipping"
            );
            RemovalOutcome::Skipped {
                reason: stderr.to_string(),
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "git worktree remove invocation failed");
            RemovalOutcome::Skipped {
                reason: format!("invoke error: {e}"),
            }
        }
    }
}

/// Sweep worktree GC candidates. Gated on AGEND_WORKTREE_GC=1.
/// Returns number of worktrees removed.
pub(super) fn sweep(home: &Path) -> usize {
    if std::env::var("AGEND_WORKTREE_GC").as_deref() != Ok("1") {
        return 0;
    }
    let candidates = crate::worktree_pool::gc_candidates(home);
    if candidates.is_empty() {
        return 0;
    }
    let mut removed = 0;
    for c in &candidates {
        match maybe_remove_candidate(&c.path) {
            RemovalOutcome::Removed => {
                removed += 1;
                tracing::info!(
                    agent = %c.agent,
                    path = %c.path.display(),
                    "retention: worktree removed"
                );
                crate::event_log::log(
                    home,
                    "retention_worktree_removed",
                    &c.agent,
                    &format!("path={}", c.path.display()),
                );
            }
            RemovalOutcome::Skipped { ref reason } => {
                crate::event_log::log(
                    home,
                    "retention_worktree_skipped",
                    &c.agent,
                    &format!("path={} reason={reason}", c.path.display()),
                );
            }
        }
    }
    removed
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_home(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-retention-worktrees-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn setup_git_repo(dir: &Path) -> PathBuf {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::process::Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();
        repo
    }

    fn add_worktree(repo: &Path, name: &str) -> PathBuf {
        let wt_path = repo.parent().unwrap().join(name);
        std::process::Command::new("git")
            .args(["worktree", "add", "-b", name, wt_path.to_str().unwrap()])
            .current_dir(repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        wt_path
    }

    /// T13a: worktree with .gitignored file → skip
    #[test]
    fn worktree_with_gitignored_file_is_skipped() {
        let dir = tmp_home("t13a");
        let repo = setup_git_repo(&dir);
        let wt = add_worktree(&repo, "wt-ignored");

        // Add .gitignore + matching file
        std::fs::write(wt.join(".gitignore"), "scratch.txt\n").unwrap();
        std::fs::write(wt.join("scratch.txt"), "operator data").unwrap();
        // Stage and commit .gitignore so git status sees scratch.txt as ignored
        std::process::Command::new("git")
            .args(["add", ".gitignore"])
            .current_dir(&wt)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "add gitignore"])
            .current_dir(&wt)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();

        let result = maybe_remove_candidate(&wt);
        assert!(matches!(result, RemovalOutcome::Skipped { .. }));
        assert!(wt.exists(), "worktree must NOT be deleted");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// T13b: worktree with untracked file → skip
    #[test]
    fn worktree_with_untracked_file_is_skipped() {
        let dir = tmp_home("t13b");
        let repo = setup_git_repo(&dir);
        let wt = add_worktree(&repo, "wt-untracked");

        std::fs::write(wt.join("new-file.txt"), "work in progress").unwrap();

        let result = maybe_remove_candidate(&wt);
        assert!(matches!(result, RemovalOutcome::Skipped { .. }));
        assert!(wt.exists(), "worktree must NOT be deleted");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// T13c: clean worktree → removed
    #[test]
    fn clean_worktree_is_removed() {
        let dir = tmp_home("t13c");
        let repo = setup_git_repo(&dir);
        let wt = add_worktree(&repo, "wt-clean");

        let result = maybe_remove_candidate(&wt);
        assert!(matches!(result, RemovalOutcome::Removed));
        assert!(!wt.exists(), "worktree should be deleted");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// T13d: git status non-zero exit → skip
    #[test]
    fn git_status_failure_skips() {
        let dir = tmp_home("t13d");
        let bad_path = dir.join("not-a-worktree");
        std::fs::create_dir_all(&bad_path).unwrap();

        let result = maybe_remove_candidate(&bad_path);
        assert!(matches!(result, RemovalOutcome::Skipped { .. }));

        std::fs::remove_dir_all(&dir).ok();
    }
}
