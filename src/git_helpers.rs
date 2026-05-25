//! Git helper functions — single source of truth for remote/branch detection.

use std::path::Path;

/// #781 Piece 6 — daemon-internal `git` subprocess wrapper that always
/// sets `AGEND_GIT_BYPASS=1`. Centralizes the bypass-env contract so
/// adding a new git call cannot silently trip the fleet-managed
/// `git worktree` / `git branch` shim deny.
///
/// Originated in #780 as a private `fn` inside `mcp::handlers::ci::mod`.
/// Promoted to `pub(crate)` in `git_helpers` for #781 so both
/// `handle_checkout_repo` and `dispatch_auto_bind_lease`'s
/// `ensure_branch_exists` extraction share a single bypass-env helper.
pub(crate) fn git_bypass(cwd: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
}

/// Like `git_bypass` but with a process-level timeout. Spawns the git
/// subprocess and polls `try_wait` until either the process exits or the
/// deadline is reached, at which point the child is killed.
pub(crate) fn git_bypass_timeout(
    cwd: &Path,
    args: &[&str],
    timeout: std::time::Duration,
) -> std::io::Result<std::process::Output> {
    let mut child = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("AGEND_GIT_BYPASS", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    let start = std::time::Instant::now();
    loop {
        match child.try_wait()? {
            Some(_status) => return child.wait_with_output(),
            None if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("git {:?} timed out after {timeout:?}", &args[..1]),
                ));
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    }
}

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
