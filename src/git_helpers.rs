//! Git helper functions — single source of truth for remote/branch detection.

use std::path::Path;

/// Detect the default branch of a repository.
/// Reads `refs/remotes/origin/HEAD` → extracts branch name.
/// Falls back to "main" if detection fails.
pub fn default_branch(repo_dir: &Path) -> String {
    let remote = primary_remote(repo_dir);
    let ref_path = format!("refs/remotes/{remote}/HEAD");
    let output = std::process::Command::new("git")
        .args(["symbolic-ref", &ref_path])
        .current_dir(repo_dir)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let prefix = format!("refs/remotes/{remote}/");
            s.strip_prefix(&prefix).unwrap_or(&s).to_string()
        }
        _ => "main".to_string(),
    }
}

/// Detect the primary remote name.
/// Returns the first remote listed by `git remote`, typically "origin".
/// Falls back to "origin" if detection fails.
pub fn primary_remote(repo_dir: &Path) -> String {
    let output = std::process::Command::new("git")
        .args(["remote"])
        .current_dir(repo_dir)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            s.lines().next().unwrap_or("origin").to_string()
        }
        _ => "origin".to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn default_branch_fallback_when_no_repo() {
        let fake = std::env::temp_dir().join("no-repo-690");
        std::fs::create_dir_all(&fake).ok();
        assert_eq!(default_branch(&fake), "main");
        std::fs::remove_dir_all(&fake).ok();
    }

    #[test]
    fn primary_remote_fallback_when_no_repo() {
        let fake = std::env::temp_dir().join("no-remote-690");
        std::fs::create_dir_all(&fake).ok();
        assert_eq!(primary_remote(&fake), "origin");
        std::fs::remove_dir_all(&fake).ok();
    }

    #[test]
    fn strip_prefix_preserves_slashes() {
        // Simulate the parsing logic directly
        let output = "refs/remotes/origin/release/2026";
        let prefix = "refs/remotes/origin/";
        let result = output.strip_prefix(prefix).unwrap_or(output);
        assert_eq!(result, "release/2026");
    }
}
