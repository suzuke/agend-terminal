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
    // Sprint 55 P0-B: when caller omits `repo` arg, auto-derive from
    // sender's binding.json source_repo (set by `bind_self` /
    // `dispatch_auto_bind_lease`). EC1: surface explicit error if
    // neither path nor binding present (no silent cwd-derivation). EC15:
    // validate the binding's source_repo path still exists before reading.
    let derived_repo: Option<String>;
    let repo: &str = match args["repo"].as_str().filter(|s| !s.is_empty()) {
        Some(r) => r,
        None => {
            let binding = home
                .join("runtime")
                .join(instance_name)
                .join("binding.json");
            let Ok(content) = std::fs::read_to_string(&binding) else {
                return json!({
                    "error": "ci(watch) needs explicit 'repo' arg OR active binding (call bind_self first)",
                    "code": "no_binding_no_repo"
                });
            };
            let Ok(v) = serde_json::from_str::<Value>(&content) else {
                return json!({
                    "error": "binding.json corrupt — re-bind or pass explicit 'repo'",
                    "code": "binding_corrupt"
                });
            };
            let Some(src) = v["source_repo"].as_str().filter(|s| !s.is_empty()) else {
                return json!({
                    "error": "binding has no source_repo — pass explicit 'repo' arg",
                    "code": "no_binding_no_repo"
                });
            };
            let src_path = std::path::Path::new(src);
            if !src_path.exists() {
                return json!({
                    "error": format!("binding source_repo '{src}' no longer exists — re-bind or pass explicit 'repo'"),
                    "code": "source_repo_path_deleted"
                });
            }
            match crate::mcp::handlers::dispatch_hook::derive_repo_from_remote_pub(src_path) {
                Some(r) => {
                    derived_repo = Some(r);
                    derived_repo.as_deref().unwrap_or("")
                }
                None => {
                    return json!({
                        "error": format!("could not derive owner/repo from '{src}' origin remote — pass explicit 'repo' arg or set fleet.yaml `repo:` override"),
                        "code": "non_github_remote_no_override"
                    });
                }
            }
        }
    };
    let branch = args["branch"].as_str().unwrap_or("main");
    let interval = args["interval_secs"].as_u64().unwrap_or(60);

    // Sprint 57 Wave 2 Track B (#546 Item 3) — E4.5 protected-ref
    // gate. Closes the bypass that let any agent subscribe to `main`
    // (or `master`) CI by calling `ci action=watch` directly. Mirrors
    // the worktree-lease gate in `worktree_pool::lease`; both go
    // through `agent_ops::is_protected_ref` so the protected set is
    // edited in exactly one place. The "main" default at the line
    // above is the backstop the gate catches when callers omit both
    // `branch` and explicit-protected branch — both flows land here.
    if crate::agent_ops::is_protected_ref(branch) {
        return json!({
            "error": format!("E4.5 violation: ci action=watch rejects protected branch '{branch}' — use lead/operator dashboards for protected-ref CI surveillance, not per-agent subscriptions"),
            "code": "e4_5_protected_branch"
        });
    }

    // Reject unsupported providers early with operator-actionable error.
    if args["ci_provider"].as_str() == Some("bitbucket_server") {
        return json!({"error": "Bitbucket Server not yet supported — track Sprint 41+ candidate. Use bitbucket_cloud for Bitbucket Cloud repos."});
    }

    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(home);
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
    // Issue #650: store next_after_ci for auto-routing on CI pass
    if let Some(next) = args["next_after_ci"].as_str().filter(|s| !s.is_empty()) {
        watch["next_after_ci"] = json!(next);
    }

    let _ = crate::store::atomic_write(
        &watch_path,
        serde_json::to_string_pretty(&watch)
            .unwrap_or_default()
            .as_bytes(),
    );
    // Sprint 54 P0-5 (sub-scope A): response enrichment — agents see
    // CI health without polling the watch file. Read state freshly
    // from `watch` JSON we just composed; populate diagnostic fields
    // when the data is available, leave as `null` otherwise.
    let now_secs = chrono::Utc::now().timestamp();
    let rate_limit_until = watch["rate_limit_until"].as_i64();
    let rate_limit_active = match rate_limit_until {
        Some(reset) => reset > now_secs,
        None => false,
    };
    let next_poll_eta = compute_next_poll_eta(&watch);

    let mut resp = json!({
        "repo": repo,
        "watching": true,
        "subscribers": subscribers,
        "rate_limit_active": rate_limit_active,
        "rate_limit_until": rate_limit_until,
        "next_poll_eta": next_poll_eta,
    });
    // Sprint 54 P0-4: surface `setup_warning` (canonical field name per
    // FLEET-DEV-PROTOCOL §X) so agents can advise users to install
    // `gh` or set `GITHUB_TOKEN`. Only fires when neither env nor
    // `gh auth` produced a token.
    if let Some(w) = crate::daemon::ci_watch::github_token_warning_from_env() {
        resp["setup_warning"] = json!(w);
    }
    resp
}

/// Sprint 54 P0-5 helper: estimate the next poll's epoch-millis tick
/// from `last_polled_at` + `effective_interval_secs` (or `interval_secs`
/// when adaptive backoff hasn't been computed yet). Returns `None` for
/// fresh watches that haven't polled yet.
///
/// Pure function — no IO, no global state. Same input shape used by
/// the `ci status` aggregator below so the two surfaces never disagree.
fn compute_next_poll_eta(watch: &Value) -> Option<i64> {
    let last_polled_at = watch["last_polled_at"].as_i64()?;
    let interval_secs = watch["effective_interval_secs"]
        .as_u64()
        .or_else(|| watch["interval_secs"].as_u64())
        .unwrap_or(60);
    Some(last_polled_at + (interval_secs as i64) * 1000)
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
    let path = crate::daemon::ci_watch::ci_watches_dir(home).join(&filename);

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

/// `ci status` MCP action (Sprint 54 P0-5 sub-scope C). Returns a
/// snapshot of every CI watch the caller subscribes to, with full
/// health diagnostics inlined. Optional `repo` / `branch` args narrow
/// the result; both must match when both are provided.
///
/// Caller filtering: agents only see watches they're subscribed to —
/// avoids leaking lead's polling targets to every dev. The empty
/// instance name (anonymous CLI) sees all watches.
pub(super) fn handle_status_ci(home: &Path, args: &Value, instance_name: &str) -> Value {
    let filter_repo = args["repo"].as_str();
    let filter_branch = args["branch"].as_str();
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(home);
    let entries = match std::fs::read_dir(&ci_dir) {
        Ok(e) => e,
        Err(_) => return json!({"watches": Vec::<Value>::new()}),
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    let now_secs = chrono::Utc::now().timestamp();

    let mut out: Vec<Value> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let watch: Value = match std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
        {
            Some(v) => v,
            None => continue,
        };
        let repo = match watch["repo"].as_str() {
            Some(r) => r,
            None => continue,
        };
        let branch = watch["branch"].as_str().unwrap_or("main");
        if let Some(want) = filter_repo {
            if repo != want {
                continue;
            }
        }
        if let Some(want) = filter_branch {
            if branch != want {
                continue;
            }
        }
        let subscribers = crate::daemon::ci_watch::parse_subscribers(&watch);
        // Caller scoping: an agent with a name sees only the watches
        // they're a subscriber of. Anonymous calls (empty instance)
        // see everything — useful for operator triage via the CLI.
        if !instance_name.is_empty() && !subscribers.iter().any(|s| s == instance_name) {
            continue;
        }
        let rate_limit_until = watch["rate_limit_until"].as_i64();
        let rate_limit_active = match rate_limit_until {
            Some(reset) => reset > now_secs,
            None => false,
        };
        let _ = now_ms; // anchor: keep timestamp-millis consistency with response enrichment
        out.push(json!({
            "repo": repo,
            "branch": branch,
            "subscribers": subscribers,
            "rate_limit_active": rate_limit_active,
            "rate_limit_until": rate_limit_until,
            "rate_limit_remaining": watch["rate_limit_remaining"].as_u64(),
            "rate_limit_limit": watch["rate_limit_limit"].as_u64(),
            "effective_interval_secs": watch["effective_interval_secs"].as_u64(),
            "interval_secs": watch["interval_secs"].as_u64().unwrap_or(60),
            "next_poll_eta": compute_next_poll_eta(&watch),
            "consecutive_skips": watch["consecutive_skips"].as_u64().unwrap_or(0),
            "stalled_notified": watch["stalled_notified"].as_bool().unwrap_or(false),
            "stalled_since_ms": watch["stalled_since_ms"].as_i64(),
            "last_polled_at": watch["last_polled_at"].as_i64(),
            "last_terminal_seen_at": watch["last_terminal_seen_at"].as_str(),
            "head_sha": watch["head_sha"].as_str(),
            "expires_at": watch["expires_at"].as_str(),
        }));
    }
    let mut resp = json!({"watches": out});
    if let Some(w) = crate::daemon::ci_watch::github_token_warning_from_env() {
        resp["setup_warning"] = json!(w);
    }
    resp
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
