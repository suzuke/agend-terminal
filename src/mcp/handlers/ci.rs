use crate::agent_ops::validate_branch;
use serde_json::{json, Value};
use std::path::Path;

pub(super) fn handle_checkout_repo(home: &Path, args: &Value, instance_name: &str) -> Value {
    let source = match args["source"].as_str() {
        Some(s) => s,
        None => return json!({"error": "missing 'source'"}),
    };
    let branch = args["branch"].as_str().unwrap_or("HEAD");
    if !validate_branch(branch) {
        return json!({"error": format!("invalid branch name '{branch}'")});
    }
    let worktree_dir = home.join("worktrees").join(format!(
        "{}-{}",
        instance_name,
        source.replace('/', "_").replace('~', "")
    ));
    std::fs::create_dir_all(worktree_dir.parent().unwrap_or(home)).ok();
    let source_path = if source.starts_with('/') || source.starts_with('~') {
        source
            .strip_prefix("~/")
            .map(|rest| format!("{}/{rest}", crate::user_home_dir().display()))
            .unwrap_or_else(|| source.to_string())
    } else {
        crate::api::call(home, &json!({"method": crate::api::method::LIST}))
            .ok()
            .and_then(|r| {
                r["result"]["agents"]
                    .as_array()?
                    .iter()
                    .find(|a| a["name"].as_str() == Some(source))
                    .and_then(|a| a["working_directory"].as_str().map(String::from))
            })
            .unwrap_or_else(|| source.to_string())
    };
    // H2: validate source_path — reject path traversal and system paths
    let source_canonical = match std::path::Path::new(&source_path).canonicalize() {
        Ok(p) => p,
        Err(e) => return json!({"error": format!("invalid source path: {e}")}),
    };
    if source_canonical.starts_with("/etc")
        || source_canonical.starts_with("/usr")
        || source_canonical.starts_with("/sys")
        || source_canonical.starts_with("/proc")
    {
        return json!({"error": "source path rejected: system directory"});
    }
    match std::process::Command::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            &worktree_dir.display().to_string(),
            branch,
        ])
        .current_dir(&source_path)
        .output()
    {
        Ok(o) if o.status.success() => {
            json!({"path": worktree_dir.display().to_string(), "source": source_path, "branch": branch})
        }
        Ok(o) => json!({"error": String::from_utf8_lossy(&o.stderr).to_string()}),
        Err(e) => json!({"error": format!("{e}")}),
    }
}

/// Reject paths that would be dangerous to `remove_dir_all`.
/// Validate and canonicalize a release path. Returns canonical absolute
/// path on success, or error message on rejection.
fn validate_release_path(path_str: &str) -> Result<std::path::PathBuf, String> {
    let path_str = path_str.trim();
    if path_str.is_empty() {
        return Err("rejected: empty path".into());
    }
    let path = std::path::Path::new(path_str);
    let canonical = std::fs::canonicalize(path)
        .map_err(|e| format!("path does not exist or unreadable: {e}"))?;
    if canonical.parent().is_none() {
        return Err(format!("rejected: root: {}", canonical.display()));
    }
    if let Ok(home) = std::env::var("HOME") {
        if canonical == std::path::Path::new(&home) {
            return Err(format!("rejected: HOME: {}", canonical.display()));
        }
    }
    let system_prefixes: &[&str] = if cfg!(windows) {
        &[
            "C:\\Windows",
            "C:\\Program Files",
            "C:\\Program Files (x86)",
            "C:\\ProgramData",
        ]
    } else {
        &[
            "/etc",
            "/usr",
            "/var",
            "/bin",
            "/sbin",
            "/boot",
            "/sys",
            "/proc",
            "/dev",
            "/Library",
            "/System",
            "/Applications",
            "/opt",
            "/tmp",
            "/private",
        ]
    };
    for prefix in system_prefixes {
        if canonical.starts_with(prefix) {
            return Err(format!("rejected: system path: {}", canonical.display()));
        }
    }
    if canonical.components().count() < 3 {
        return Err(format!("rejected: too shallow: {}", canonical.display()));
    }
    Ok(canonical)
}

pub(super) fn handle_release_repo(args: &Value) -> Value {
    let path = match args["path"].as_str() {
        Some(p) => p,
        None => return json!({"error": "missing 'path'"}),
    };

    // H3 fix: validate + canonicalize path before any filesystem ops.
    let canonical = match validate_release_path(path) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };
    let path_str = canonical.to_string_lossy();

    match std::process::Command::new("git")
        .args(["worktree", "remove", "--force", &path_str])
        .output()
    {
        Ok(o) if o.status.success() => json!({"path": path}),
        Ok(o) => {
            let _ = std::fs::remove_dir_all(&canonical);
            json!({"path": path, "note": String::from_utf8_lossy(&o.stderr).to_string()})
        }
        Err(_) => {
            let _ = std::fs::remove_dir_all(&canonical);
            json!({"path": path})
        }
    }
}

pub(crate) fn handle_watch_ci(home: &Path, args: &Value, instance_name: &str) -> Value {
    let repo = match args["repo"].as_str() {
        Some(r) => r,
        None => return json!({"error": "missing 'repo'"}),
    };
    let branch = args["branch"].as_str().unwrap_or("main");
    let interval = args["interval_secs"].as_u64().unwrap_or(60);

    // Reject unsupported providers early with operator-actionable error.
    if args["ci_provider"].as_str() == Some("bitbucket_server") {
        return json!({"error": "Bitbucket Server not yet supported — track Sprint 41+ candidate. Use bitbucket_cloud for Bitbucket Cloud repos."});
    }

    let ci_dir = home.join("ci-watches");
    std::fs::create_dir_all(&ci_dir).ok();
    let watch = json!({
        "repo": repo,
        "branch": branch,
        "interval_secs": interval,
        "instance": instance_name,
        "ci_provider": args["ci_provider"].as_str(),
        "ci_provider_url": args["ci_provider_url"].as_str(),
        "last_run_id": null,
        "head_sha": null,
        "last_polled_at": null,
        "last_notified_head_sha": null,
        "expires_at": (chrono::Utc::now() + chrono::Duration::hours(crate::daemon::ci_watch::WATCH_TTL_HOURS)).to_rfc3339(),
        "last_terminal_seen_at": null,
    });
    let filename = crate::daemon::ci_watch::watch_filename(repo, branch);
    let watch_path = ci_dir.join(&filename);
    let _ = std::fs::write(
        &watch_path,
        serde_json::to_string_pretty(&watch).unwrap_or_default(),
    );
    let mut resp = json!({"repo": repo, "watching": true});
    if let Some(w) = crate::daemon::ci_watch::github_token_warning_from_env() {
        resp["warning"] = json!(w);
    }
    resp
}

pub(super) fn handle_unwatch_ci(home: &Path, args: &Value) -> Value {
    let repo = match args["repo"].as_str() {
        Some(r) => r,
        None => return json!({"error": "missing 'repo'"}),
    };
    let branch = args["branch"].as_str().unwrap_or("main");
    let filename = crate::daemon::ci_watch::watch_filename(repo, branch);
    let path = home.join("ci-watches").join(&filename);
    let _ = std::fs::remove_file(&path);
    json!({"repo": repo, "watching": false})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_repo_rejects_root_path() {
        let result = handle_release_repo(&serde_json::json!({"path": "/"}));
        assert!(result["error"].as_str().is_some(), "root must be rejected");
    }

    #[test]
    fn release_repo_rejects_system_path() {
        let result = super::validate_release_path("/etc");
        assert!(result.is_err(), "/etc must be rejected: {:?}", result);
    }

    #[test]
    fn release_repo_rejects_empty_path() {
        let result = handle_release_repo(&serde_json::json!({"path": ""}));
        assert!(result["error"].as_str().is_some(), "empty must be rejected");
    }

    #[test]
    fn validate_release_path_rejects_relative_dotdot() {
        let result = super::validate_release_path("../../etc");
        // Either fails canonicalize (doesn't exist) or rejects as system path.
        assert!(result.is_err(), "relative dotdot must be rejected");
    }

    #[test]
    fn validate_release_path_rejects_relative_no_root() {
        let result = super::validate_release_path("a/b/c");
        // Relative path that doesn't exist → canonicalize fails.
        assert!(result.is_err(), "relative path must be rejected");
    }

    #[test]
    #[cfg(unix)]
    fn validate_release_path_rejects_shallow() {
        // /tmp canonicalizes to /private/tmp on macOS → system prefix match.
        let result = super::validate_release_path("/tmp");
        assert!(result.is_err(), "/tmp must be rejected: {:?}", result);
    }

    #[test]
    #[cfg(unix)]
    fn validate_release_path_accepts_deep_existing() {
        // Create a temp dir deep enough to pass.
        let home = std::env::var("HOME").expect("HOME must be set");
        let dir = std::path::PathBuf::from(home)
            .join(format!(".agend-release-test-{}", std::process::id()));
        let deep = dir.join("sub");
        std::fs::create_dir_all(&deep).ok();
        let result = super::validate_release_path(deep.to_str().expect("valid UTF-8"));
        // Should pass (deep enough, not system dir).
        assert!(
            result.is_ok(),
            "deep existing path should pass: {:?}",
            result.err()
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
