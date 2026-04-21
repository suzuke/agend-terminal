//! Git worktree management — create, reuse, prune.
//!
//! Rule: if working_directory is set and is a git repo, create a worktree.

use crate::agent_ops::validate_branch;
use std::path::{Path, PathBuf};

/// Info about a created worktree.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct WorktreeInfo {
    /// Actual working directory (the worktree path).
    pub path: PathBuf,
    /// Original repo root.
    pub source_repo: PathBuf,
    /// Branch name.
    pub branch: String,
}

/// Check if a directory is a git repo (has .git).
pub fn is_git_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Recover the source repo path from a worktree working directory.
///
/// Matches the layout `agent_resolve` creates via `worktree::create`:
/// `{source_repo}/.worktrees/{name}`. Returns `None` when `working_dir` is
/// not inside a `.worktrees/` directory or lacks the two expected ancestors.
pub fn source_repo_of(working_dir: &Path) -> Option<PathBuf> {
    if !working_dir.display().to_string().contains(".worktrees/") {
        return None;
    }
    working_dir.parent()?.parent().map(|p| p.to_path_buf())
}

/// Check if a git repo has at least one commit (valid HEAD).
fn has_commits(repo_dir: &Path) -> bool {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Create a worktree for an instance. Returns WorktreeInfo if created,
/// None if not a git repo.
///
/// - If worktree already exists, reuses it.
/// - Branch name: custom_branch or "agend/{instance_name}".
/// - Worktree path: {repo}/.worktrees/{instance_name}/.
/// - Auto-adds .worktrees to .gitignore.
pub fn create(
    repo_dir: &Path,
    instance_name: &str,
    custom_branch: Option<&str>,
) -> Option<WorktreeInfo> {
    if !is_git_repo(repo_dir) {
        return None;
    }

    // Empty repo (git init without any commits) → HEAD is invalid.
    // Worktree creation requires at least one commit.
    if !has_commits(repo_dir) {
        tracing::info!(repo = %repo_dir.display(), "empty repo, creating initial commit for worktree support");
        let ok = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=agend-terminal",
                "-c",
                "user.email=agend@localhost",
                "commit",
                "--allow-empty",
                "-m",
                "init (agend-terminal)",
            ])
            .current_dir(repo_dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            tracing::warn!(repo = %repo_dir.display(), "failed to create initial commit in empty repo");
            return None;
        }
    }

    let branch = custom_branch
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("agend/{instance_name}"));

    if !validate_branch(&branch) {
        tracing::warn!(branch = %branch, "invalid branch name, rejecting worktree creation");
        return None;
    }

    let wt_dir = repo_dir.join(".worktrees").join(instance_name);

    // Already exists — reuse
    if wt_dir.exists() {
        tracing::info!(
            instance = instance_name,
            path = %wt_dir.display(),
            "reusing existing worktree"
        );
        return Some(WorktreeInfo {
            path: wt_dir,
            source_repo: repo_dir.to_path_buf(),
            branch,
        });
    }

    // Ensure .worktrees is in .gitignore
    ensure_gitignore(repo_dir);

    // Try creating worktree: first with -b (new branch), fallback without -b (existing branch)
    let output = std::process::Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            &branch,
            &wt_dir.display().to_string(),
        ])
        .current_dir(repo_dir)
        .output();

    match output {
        Ok(o) if o.status.success() => {
            tracing::info!(
                instance = instance_name,
                path = %wt_dir.display(),
                branch = %branch,
                "created worktree"
            );
            Some(WorktreeInfo {
                path: wt_dir,
                source_repo: repo_dir.to_path_buf(),
                branch,
            })
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            // Branch or worktree already exists — try without -b (exit code 128 = git error)
            if o.status.code() == Some(128)
                && (stderr.contains("already exists") || stderr.contains("is already checked out"))
            {
                let output2 = std::process::Command::new("git")
                    .args(["worktree", "add", &wt_dir.display().to_string(), &branch])
                    .current_dir(repo_dir)
                    .output();
                match output2 {
                    Ok(o2) if o2.status.success() => {
                        tracing::info!(
                            instance = instance_name,
                            %branch,
                            "created worktree on existing branch"
                        );
                        Some(WorktreeInfo {
                            path: wt_dir,
                            source_repo: repo_dir.to_path_buf(),
                            branch,
                        })
                    }
                    Ok(o2) => {
                        tracing::warn!(
                            instance = instance_name,
                            error = %String::from_utf8_lossy(&o2.stderr).trim(),
                            "worktree creation failed"
                        );
                        None
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "git not available");
                        None
                    }
                }
            } else {
                tracing::warn!(instance = instance_name, error = %stderr.trim(), "worktree creation failed");
                None
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "git not available");
            None
        }
    }
}

/// Run `git worktree prune` on a repo to clean stale worktree entries.
pub fn prune(repo_dir: &Path) {
    if !is_git_repo(repo_dir) {
        return;
    }
    let output = std::process::Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(repo_dir)
        .output();
    match output {
        Ok(o) if o.status.success() => {
            tracing::info!(repo = %repo_dir.display(), "pruned stale worktree entries");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            if !stderr.trim().is_empty() {
                tracing::warn!(warning = %stderr.trim(), "worktree prune warning");
            }
        }
        Err(e) => {
            tracing::warn!(repo = %repo_dir.display(), error = %e, "git worktree prune failed");
        }
    }
}

/// List worktree directories under {repo}/.worktrees/.
pub fn list_residual(repo_dir: &Path) -> Vec<String> {
    let wt_base = repo_dir.join(".worktrees");
    if !wt_base.exists() {
        return Vec::new();
    }
    std::fs::read_dir(&wt_base)
        .ok()
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Ensure .worktrees is listed in .gitignore.
fn ensure_gitignore(repo_dir: &Path) {
    let gitignore = repo_dir.join(".gitignore");
    let content = std::fs::read_to_string(&gitignore).unwrap_or_default();
    if !content
        .lines()
        .any(|line| line.trim() == ".worktrees" || line.trim() == ".worktrees/")
    {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&gitignore)
        {
            let prefix = if content.is_empty() || content.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            if let Err(e) = writeln!(f, "{prefix}.worktrees") {
                tracing::warn!(error = %e, "failed to update .gitignore");
            }
            tracing::info!("added .worktrees to .gitignore");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn tmp_repo(name: &str) -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-wt-test-{}-{}-{}",
            std::process::id(),
            name,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        // git init
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
                "user.email=test@test",
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
    fn test_is_git_repo() {
        let repo = tmp_repo("is_git");
        assert!(is_git_repo(&repo));
        assert!(!is_git_repo(&std::env::temp_dir()));
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn test_create_worktree() {
        let repo = tmp_repo("create");
        let info = create(&repo, "agent1", None);
        assert!(info.is_some());
        let info = info.expect("worktree created");
        assert!(info.path.exists());
        assert_eq!(info.branch, "agend/agent1");
        assert_eq!(info.source_repo, repo);

        // .gitignore should contain .worktrees
        let gitignore = std::fs::read_to_string(repo.join(".gitignore")).unwrap_or_default();
        assert!(gitignore.contains(".worktrees"));

        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn test_reuse_existing_worktree() {
        let repo = tmp_repo("reuse");
        let info1 = create(&repo, "agent1", None);
        assert!(info1.is_some());
        let info2 = create(&repo, "agent1", None);
        assert!(info2.is_some());
        assert_eq!(info1.expect("i1").path, info2.expect("i2").path);
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn test_non_git_returns_none() {
        let dir = std::env::temp_dir().join(format!("agend-wt-test-nongit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        assert!(create(&dir, "agent1", None).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_custom_branch() {
        let repo = tmp_repo("custom_branch");
        let info = create(&repo, "agent1", Some("my-feature"));
        assert!(info.is_some());
        assert_eq!(info.expect("i").branch, "my-feature");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn test_list_residual() {
        let repo = tmp_repo("residual");
        create(&repo, "agent1", None);
        create(&repo, "agent2", None);
        let residual = list_residual(&repo);
        assert_eq!(residual.len(), 2);
        assert!(residual.contains(&"agent1".to_string()));
        assert!(residual.contains(&"agent2".to_string()));
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn test_empty_repo_gets_initial_commit() {
        // git init without any commit — should auto-create initial commit
        let dir = std::env::temp_dir().join(format!(
            "agend-wt-test-empty-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).ok();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .ok();
        // No commit — HEAD is invalid
        assert!(!has_commits(&dir));
        // create() should handle this gracefully
        let info = create(&dir, "agent1", None);
        assert!(info.is_some(), "worktree should be created in empty repo");
        assert!(has_commits(&dir), "initial commit should exist now");
        std::fs::remove_dir_all(&dir).ok();
    }

    // `test_validate_branch_valid` + `test_validate_branch_rejects` migrated
    // to `src/agent_ops.rs::tests` as part of Task #9 Option C epilogue — the
    // `validate_branch` fn itself lives in `agent_ops.rs` now, so tests are
    // colocated with their subject.
}
