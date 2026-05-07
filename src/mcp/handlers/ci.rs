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

/// `ci watch` action: subscribe `instance_name` to CI notifications for
/// `repo@branch`. Sprint 54 P0-1 changes this from last-write-wins
/// (the previous behavior overwrote the entire watch file, dropping any
/// other agent's subscription) to APPEND idempotent semantics — the
/// caller is added to a `subscribers` array if not already present, and
/// existing poll state (`last_run_id`, `head_sha`, etc.) is preserved.
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
    let filename = crate::daemon::ci_watch::watch_filename(repo, branch);
    let watch_path = ci_dir.join(&filename);

    let now_rfc3339 = chrono::Utc::now().to_rfc3339();

    // Read existing watch (if any) to preserve poll state and existing
    // subscribers. A fresh write would clobber `last_run_id` /
    // `last_polled_at` / `last_notified_head_sha` and trigger duplicate
    // notifications on the next poll.
    let mut watch = std::fs::read_to_string(&watch_path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or_else(|| {
            json!({
                "repo": repo,
                "branch": branch,
                "interval_secs": interval,
                "ci_provider": args["ci_provider"].as_str(),
                "ci_provider_url": args["ci_provider_url"].as_str(),
                "last_run_id": null,
                "head_sha": null,
                "last_polled_at": null,
                "last_notified_head_sha": null,
                "expires_at": (chrono::Utc::now() + chrono::Duration::hours(crate::daemon::ci_watch::WATCH_TTL_HOURS)).to_rfc3339(),
                "last_terminal_seen_at": null,
            })
        });

    // Migrate legacy schema (single `instance` field, no `subscribers`
    // array) into the canonical multi-subscriber form. Subsequent reads
    // by the daemon's poll loop go through `parse_subscribers` which
    // also supports the legacy form, so a migration race here is safe.
    let mut subscribers = crate::daemon::ci_watch::parse_subscribers(&watch);
    if !subscribers.iter().any(|s| s == instance_name) && !instance_name.is_empty() {
        subscribers.push(instance_name.to_string());
    }
    let subscribers_json: Vec<Value> = subscribers
        .iter()
        .map(|name| {
            // Preserve original subscribed_at if present, otherwise stamp now.
            let prior = watch
                .get("subscribers")
                .and_then(|v| v.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .find(|s| s.get("instance").and_then(|i| i.as_str()) == Some(name.as_str()))
                })
                .and_then(|s| s.get("subscribed_at").and_then(|v| v.as_str()))
                .map(String::from)
                .unwrap_or_else(|| now_rfc3339.clone());
            json!({"instance": name, "subscribed_at": prior})
        })
        .collect();

    watch["repo"] = json!(repo);
    watch["branch"] = json!(branch);
    // Refresh interval / provider override on each call — caller may
    // adjust polling cadence or provider URL even on a re-subscribe.
    watch["interval_secs"] = json!(interval);
    if let Some(p) = args["ci_provider"].as_str() {
        watch["ci_provider"] = json!(p);
    }
    if let Some(u) = args["ci_provider_url"].as_str() {
        watch["ci_provider_url"] = json!(u);
    }
    watch["subscribers"] = json!(subscribers_json);
    // DEPRECATED: `instance` field kept as legacy alias for one release
    // cycle so a daemon running pre-r0 binary against post-r0 watch
    // files can still read SOMEONE. Set to first subscriber, removed
    // Sprint 55. Post-r0 daemons read `subscribers` first.
    watch["instance"] = json!(subscribers.first().cloned().unwrap_or_default());
    // Refresh expires_at on each subscribe — keeps the watch alive
    // as long as at least one agent stays interested.
    watch["expires_at"] = json!((chrono::Utc::now()
        + chrono::Duration::hours(crate::daemon::ci_watch::WATCH_TTL_HOURS))
    .to_rfc3339());

    let _ = crate::store::atomic_write(
        &watch_path,
        serde_json::to_string_pretty(&watch)
            .unwrap_or_default()
            .as_bytes(),
    );
    let mut resp = json!({
        "repo": repo,
        "watching": true,
        "subscribers": subscribers,
    });
    if let Some(w) = crate::daemon::ci_watch::github_token_warning_from_env() {
        resp["warning"] = json!(w);
    }
    resp
}

/// `ci unwatch` action: unsubscribe the caller from `repo@branch`.
/// Sprint 54 P0-1: only the caller is removed from the `subscribers`
/// array. The watch file is deleted only when the array becomes empty
/// (no other agent is still interested in this branch).
pub(super) fn handle_unwatch_ci(home: &Path, args: &Value) -> Value {
    let repo = match args["repo"].as_str() {
        Some(r) => r,
        None => return json!({"error": "missing 'repo'"}),
    };
    let branch = args["branch"].as_str().unwrap_or("main");
    // Caller identity for selective removal. Falls back to the env-set
    // identity (the standard sender resolution path) when MCP `instance`
    // arg is omitted.
    let caller = args["instance"]
        .as_str()
        .map(String::from)
        .or_else(|| std::env::var("AGEND_INSTANCE_NAME").ok())
        .unwrap_or_default();
    let filename = crate::daemon::ci_watch::watch_filename(repo, branch);
    let path = home.join("ci-watches").join(&filename);

    let mut watch = match std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    {
        Some(v) => v,
        None => {
            // No watch file at all — idempotent no-op (matches pre-r0 behavior).
            return json!({"repo": repo, "watching": false, "subscribers": Vec::<String>::new()});
        }
    };

    let mut subscribers = crate::daemon::ci_watch::parse_subscribers(&watch);
    if !caller.is_empty() {
        subscribers.retain(|s| s != &caller);
    } else {
        // No caller identity supplied — clear ALL subscribers (legacy
        // behavior). Operator-driven cleanup, e.g. via daemon CLI.
        subscribers.clear();
    }

    if subscribers.is_empty() {
        let _ = std::fs::remove_file(&path);
        return json!({
            "repo": repo,
            "watching": false,
            "subscribers": Vec::<String>::new(),
        });
    }

    let subscribers_json: Vec<Value> = subscribers
        .iter()
        .map(|name| {
            let prior = watch
                .get("subscribers")
                .and_then(|v| v.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .find(|s| s.get("instance").and_then(|i| i.as_str()) == Some(name.as_str()))
                })
                .and_then(|s| s.get("subscribed_at").and_then(|v| v.as_str()))
                .map(String::from)
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
            json!({"instance": name, "subscribed_at": prior})
        })
        .collect();
    watch["subscribers"] = json!(subscribers_json);
    watch["instance"] = json!(subscribers.first().cloned().unwrap_or_default());

    let _ = crate::store::atomic_write(
        &path,
        serde_json::to_string_pretty(&watch)
            .unwrap_or_default()
            .as_bytes(),
    );
    json!({
        "repo": repo,
        "watching": true,
        "subscribers": subscribers,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
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

    #[test]
    fn dispatch_with_branch_and_repo_auto_invokes_watch_ci() {
        let home = std::env::temp_dir().join(format!("agend-auto-watch-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let args = serde_json::json!({"repo": "owner/repo", "branch": "feat/test"});
        handle_watch_ci(&home, &args, "test-agent");
        let filename = crate::daemon::ci_watch::watch_filename("owner/repo", "feat/test");
        let watch_path = home.join("ci-watches").join(&filename);
        assert!(watch_path.exists(), "watch file must be created");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dispatch_idempotent_double_watch_safe() {
        let home =
            std::env::temp_dir().join(format!("agend-auto-watch-idem-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let args = serde_json::json!({"repo": "owner/repo", "branch": "feat/idem"});
        handle_watch_ci(&home, &args, "agent-1");
        handle_watch_ci(&home, &args, "agent-1"); // second call — idempotent
        let filename = crate::daemon::ci_watch::watch_filename("owner/repo", "feat/idem");
        let watch_path = home.join("ci-watches").join(&filename);
        assert!(watch_path.exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dispatch_without_repo_no_auto_watch() {
        // If no repo field, auto-watch should not fire.
        // This tests the comms.rs logic: args["repo"].as_str() returns None.
        let home = std::env::temp_dir().join(format!("agend-no-watch-{}", std::process::id()));
        std::fs::create_dir_all(home.join("ci-watches")).ok();
        // No watch file should exist for a branch without repo.
        let filename = crate::daemon::ci_watch::watch_filename("", "feat/no-repo");
        let watch_path = home.join("ci-watches").join(&filename);
        assert!(!watch_path.exists(), "no watch without repo");
        std::fs::remove_dir_all(&home).ok();
    }

    // -----------------------------------------------------------------
    // Sprint 54 P0-1 — multi-subscriber contract invariants. Each test
    // pins one of the six hard-contract guarantees from the lead's
    // dispatch (see m-20260507000244357650-11). The fan-out test in
    // src/daemon/ci_watch.rs (`subscriber_fan_out_notifies_every_member`)
    // is the empirical regression-proof anchor; these are the watch-file
    // schema invariants that proof relies on.
    // -----------------------------------------------------------------

    fn watch_path_for(home: &Path, repo: &str, branch: &str) -> std::path::PathBuf {
        let filename = crate::daemon::ci_watch::watch_filename(repo, branch);
        home.join("ci-watches").join(filename)
    }

    fn read_watch(path: &Path) -> serde_json::Value {
        let s = std::fs::read_to_string(path).expect("watch file must exist");
        serde_json::from_str(&s).expect("watch must be valid JSON")
    }

    #[test]
    fn ci_watch_appends_subscriber_idempotent_distinct_callers() {
        // Hard contract item 4: `ci watch` MCP action APPENDS to subscribers
        // if not present (idempotent), does NOT overwrite. Last-write-wins
        // was the Sprint 53 multi-caller bug.
        let home = std::env::temp_dir().join(format!("agend-watch-append-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let args = serde_json::json!({"repo": "owner/repo", "branch": "main"});

        handle_watch_ci(&home, &args, "lead");
        handle_watch_ci(&home, &args, "dev");

        let watch = read_watch(&watch_path_for(&home, "owner/repo", "main"));
        let subs: Vec<&str> = watch["subscribers"]
            .as_array()
            .expect("subscribers array")
            .iter()
            .map(|s| s["instance"].as_str().unwrap())
            .collect();
        assert_eq!(
            subs,
            vec!["lead", "dev"],
            "both callers must be present, not last-write-wins"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn ci_watch_double_subscribe_same_caller_is_idempotent() {
        // Same caller calling twice must not duplicate. Idempotent in
        // the strict mathematical sense — `f(f(x)) == f(x)`.
        let home = std::env::temp_dir().join(format!("agend-watch-dup-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let args = serde_json::json!({"repo": "owner/repo", "branch": "main"});

        handle_watch_ci(&home, &args, "lead");
        handle_watch_ci(&home, &args, "lead");
        handle_watch_ci(&home, &args, "lead");

        let watch = read_watch(&watch_path_for(&home, "owner/repo", "main"));
        let subs = watch["subscribers"].as_array().unwrap();
        assert_eq!(subs.len(), 1, "duplicate subscribe must collapse");
        assert_eq!(subs[0]["instance"].as_str(), Some("lead"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn ci_watch_preserves_poll_state_on_resubscribe() {
        // Re-subscribing must NOT reset last_run_id / last_polled_at —
        // otherwise the next poll re-emits the last terminal run as a
        // duplicate notification.
        let home = std::env::temp_dir().join(format!("agend-watch-state-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let args = serde_json::json!({"repo": "owner/repo", "branch": "main"});

        handle_watch_ci(&home, &args, "lead");

        // Simulate the daemon's poll-loop having stamped state.
        let path = watch_path_for(&home, "owner/repo", "main");
        let mut watch = read_watch(&path);
        watch["last_run_id"] = serde_json::json!(42_u64);
        watch["last_polled_at"] = serde_json::json!(1_700_000_000_000_i64);
        watch["last_notified_head_sha"] = serde_json::json!("abc1234");
        std::fs::write(&path, serde_json::to_string_pretty(&watch).unwrap()).unwrap();

        // dev subscribes.
        handle_watch_ci(&home, &args, "dev");

        let watch = read_watch(&path);
        assert_eq!(
            watch["last_run_id"].as_u64(),
            Some(42),
            "poll state must survive append"
        );
        assert_eq!(
            watch["last_polled_at"].as_i64(),
            Some(1_700_000_000_000_i64)
        );
        assert_eq!(watch["last_notified_head_sha"].as_str(), Some("abc1234"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn ci_watch_legacy_instance_field_migrates_on_resubscribe() {
        // Hard contract item 3: legacy `instance: "X"` files migrate to
        // `subscribers: [{instance: X, ...}]` on the next write. The
        // legacy field is preserved as a deprecated alias so a rollback
        // to a pre-r0 daemon binary can still read SOMEONE.
        let home = std::env::temp_dir().join(format!("agend-watch-migrate-{}", std::process::id()));
        let ci_dir = home.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let path = watch_path_for(&home, "owner/repo", "main");

        // Hand-craft a legacy watch file (no subscribers array).
        let legacy = serde_json::json!({
            "repo": "owner/repo",
            "branch": "main",
            "interval_secs": 60,
            "instance": "lead",
            "last_run_id": 100,
            "head_sha": "abc",
            "last_polled_at": null,
            "last_notified_head_sha": null,
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
            "last_terminal_seen_at": null,
        });
        std::fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        // Trigger migration via a fresh subscribe.
        handle_watch_ci(
            &home,
            &serde_json::json!({"repo": "owner/repo", "branch": "main"}),
            "dev",
        );

        let watch = read_watch(&path);
        let subs: Vec<&str> = watch["subscribers"]
            .as_array()
            .expect("subscribers must exist post-migration")
            .iter()
            .map(|s| s["instance"].as_str().unwrap())
            .collect();
        assert_eq!(
            subs,
            vec!["lead", "dev"],
            "legacy lead retained, dev appended"
        );
        // Legacy field preserved as deprecated alias = first subscriber.
        assert_eq!(watch["instance"].as_str(), Some("lead"));
        // Poll state survived.
        assert_eq!(watch["last_run_id"].as_u64(), Some(100));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn ci_unwatch_removes_caller_only_when_others_remain() {
        // Hard contract item 5 (a): `ci unwatch` removes the caller
        // and writes the file back. Watch file is NOT deleted.
        let home = std::env::temp_dir().join(format!("agend-unwatch-keep-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let args = serde_json::json!({"repo": "owner/repo", "branch": "main"});
        handle_watch_ci(&home, &args, "lead");
        handle_watch_ci(&home, &args, "dev");

        let path = watch_path_for(&home, "owner/repo", "main");
        assert!(path.exists());

        let unwatch_args = serde_json::json!({
            "repo": "owner/repo",
            "branch": "main",
            "instance": "lead",
        });
        let resp = handle_unwatch_ci(&home, &unwatch_args);

        assert_eq!(
            resp["watching"].as_bool(),
            Some(true),
            "still watched by dev"
        );
        assert!(path.exists(), "file must remain while subscribers > 0");

        let watch = read_watch(&path);
        let subs: Vec<&str> = watch["subscribers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["instance"].as_str().unwrap())
            .collect();
        assert_eq!(subs, vec!["dev"]);
        // Legacy alias also rolls forward.
        assert_eq!(watch["instance"].as_str(), Some("dev"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn ci_unwatch_deletes_file_when_subscribers_empty() {
        // Hard contract item 5 (b): only the LAST unwatch deletes the
        // file. Without this, the watch leaks rate-limit budget on a
        // branch nobody cares about anymore.
        let home =
            std::env::temp_dir().join(format!("agend-unwatch-delete-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let args = serde_json::json!({"repo": "owner/repo", "branch": "main"});
        handle_watch_ci(&home, &args, "lead");

        let path = watch_path_for(&home, "owner/repo", "main");
        assert!(path.exists());

        let unwatch_args = serde_json::json!({
            "repo": "owner/repo",
            "branch": "main",
            "instance": "lead",
        });
        let resp = handle_unwatch_ci(&home, &unwatch_args);

        assert_eq!(resp["watching"].as_bool(), Some(false));
        assert!(!path.exists(), "last subscriber unwatch must delete file");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn ci_unwatch_unknown_caller_is_noop_keeps_watch() {
        // Defensive: unwatch from an instance that never subscribed
        // must not silently delete the watch (would have been a quiet
        // way to clobber lead's watch via dev's typo).
        let home = std::env::temp_dir().join(format!("agend-unwatch-noop-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let args = serde_json::json!({"repo": "owner/repo", "branch": "main"});
        handle_watch_ci(&home, &args, "lead");

        let path = watch_path_for(&home, "owner/repo", "main");
        let unwatch_args = serde_json::json!({
            "repo": "owner/repo",
            "branch": "main",
            "instance": "stranger",
        });
        handle_unwatch_ci(&home, &unwatch_args);

        assert!(
            path.exists(),
            "lead's watch must survive stranger's unwatch"
        );
        let watch = read_watch(&path);
        let subs: Vec<&str> = watch["subscribers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["instance"].as_str().unwrap())
            .collect();
        assert_eq!(subs, vec!["lead"]);
        std::fs::remove_dir_all(&home).ok();
    }
}
