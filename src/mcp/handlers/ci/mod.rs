use crate::agent_ops::validate_branch;
use crate::git_helpers::git_bypass;
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
    // #778 Option 1: optional atomic provision + bind. When `bind:true`,
    // tail-ops mirror `bind_self → dispatch_auto_bind_lease` (marker +
    // binding.json + ci_watches arm) directly on the just-provisioned
    // worktree. Default `false` preserves existing back-compat callers
    // (review pool, operator triage) that materialize a detached-HEAD
    // inspection worktree without claiming it.
    let bind = args["bind"].as_bool().unwrap_or(false);
    if bind && crate::agent_ops::is_protected_ref(branch) {
        return json!({
            "error": format!("E4.5 violation: bind=true rejects protected branch '{branch}'"),
            "code": "e4_5_protected_branch"
        });
    }
    if bind && instance_name.is_empty() {
        return json!({
            "error": "bind=true requires AGEND_INSTANCE_NAME — anonymous callers cannot claim a worktree",
            "code": "needs_identity"
        });
    }
    // Windows-safe path mangling: also collapse `\` (path separator) and
    // `:` (drive letter) so a source like `C:\Users\runner\...` doesn't
    // produce a worktree path with mid-name colons (rejected by NTFS).
    // Pre-existing tests didn't exercise Windows-built happy-path until
    // #778's new bind:true coverage.
    let worktree_dir = home.join("worktrees").join(format!(
        "{}-{}",
        instance_name,
        source.replace(['/', '\\', ':'], "_").replace('~', "")
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
    // #780: auto-create branch from `from_ref` when bind:true + branch
    // missing locally. #781 Piece 6 extracts the decision tree into
    // `dispatch_hook::ensure_branch_exists` so the same logic services
    // both this MCP-tool entry and the `send kind=task` dispatch hook
    // (single source of truth, no #780-vs-#781 logic drift). `bind:false`
    // preserves current back-compat (no auto-create) per decision
    // `d-20260514102305998399-0` scope.
    let mut auto_created_branch = false;
    let mut fetch_attempted = false;
    if bind {
        let from_ref = args["from_ref"].as_str().unwrap_or("origin/main");
        let src = Path::new(&source_path);
        match crate::mcp::handlers::dispatch_hook::ensure_branch_exists(
            home,
            src,
            branch,
            from_ref,
            instance_name,
        ) {
            Ok((created, fetched)) => {
                auto_created_branch = created;
                fetch_attempted = fetched;
            }
            Err(err) => {
                let mut e = json!({
                    "error": err.message,
                    "code": serde_json::to_value(err.code).unwrap_or(json!("unknown")),
                    "stage": serde_json::to_value(err.stage).unwrap_or(json!("unknown")),
                    "fetch_attempted": err.fetch_attempted,
                });
                if let Some(raw) = err.raw {
                    e["raw"] = json!(raw);
                }
                return e;
            }
        }
    }
    // When `bind:true`, omit `--detach` so HEAD lands on the named
    // branch — subsequent commits write to the right ref without the
    // extra `git switch` that triggered the #778 chicken-and-egg.
    let worktree_path_str = worktree_dir.display().to_string();
    let git_args: Vec<&str> = if bind {
        vec!["worktree", "add", &worktree_path_str, branch]
    } else {
        vec!["worktree", "add", "--detach", &worktree_path_str, branch]
    };
    match git_bypass(Path::new(&source_path), &git_args) {
        Ok(o) if o.status.success() => {
            let mut resp =
                json!({"path": worktree_path_str, "source": source_path, "branch": branch});
            if bind {
                // #779 P2 Option B: collect tail-op partial failures
                // into a `warnings` array. `bound: true` remains
                // load-bearing (lease succeeded — the main op went
                // through); `warnings` flags degraded daemon-side
                // state so callers can diagnose without grepping logs.
                // Vec ordering preserves tail-op execution order
                // (marker → bind_full → watch_ci) for positional
                // semantics. Prefix convention: `<step>: <message>`
                // — machine-greppable by callers.
                let mut warnings: Vec<String> = Vec::new();
                let marker_path = worktree_dir.join(crate::worktree_pool::MANAGED_MARKER);
                if let Err(e) = std::fs::write(
                    &marker_path,
                    format!(
                        "agent={instance_name}\nbranch={branch}\nleased_at={}\n",
                        chrono::Utc::now().to_rfc3339()
                    ),
                ) {
                    warnings.push(format!("marker: {e}"));
                }
                if let Err(e) = crate::binding::bind_full(
                    home,
                    instance_name,
                    "self",
                    branch,
                    &worktree_dir,
                    &source_canonical,
                ) {
                    warnings.push(format!("bind_full: {e}"));
                }
                if let Some(r) = crate::mcp::handlers::dispatch_hook::derive_repo_from_remote_pub(
                    &source_canonical,
                ) {
                    let watch_resp = handle_watch_ci(
                        home,
                        &json!({"repo": &r, "branch": branch}),
                        instance_name,
                    );
                    if let Some(err_msg) = watch_resp.get("error").and_then(|v| v.as_str()) {
                        let code = watch_resp
                            .get("code")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        warnings.push(format!("watch_ci: {err_msg} (code={code})"));
                    }
                }
                resp["bound"] = json!(true);
                resp["auto_created_branch"] = json!(auto_created_branch);
                resp["fetch_attempted"] = json!(fetch_attempted);
                if !warnings.is_empty() {
                    resp["warnings"] = json!(warnings);
                }
            }
            resp
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            let mut err = json!({
                "error": format!("git worktree add failed: {}", stderr.trim()),
                "code": "worktree_add_failed",
                "stage": "worktree_add",
                "raw": stderr,
            });
            if bind {
                err["fetch_attempted"] = json!(fetch_attempted);
                err["auto_created_branch"] = json!(auto_created_branch);
            }
            err
        }
        Err(e) => {
            let mut err = json!({
                "error": format!("git worktree add spawn failed: {e}"),
                "code": "worktree_add_failed",
                "stage": "worktree_add",
                "raw": e.to_string(),
            });
            if bind {
                err["fetch_attempted"] = json!(fetch_attempted);
                err["auto_created_branch"] = json!(auto_created_branch);
            }
            err
        }
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

/// #789 — explicit MCP entry point for cleaning empty `init` commits
/// from a bound worktree. Operators / agents call this before push to
/// scrub backend session-checkpoint pollution that accumulated between
/// `dispatch_auto_bind_lease` (bind-time cleanup) and now.
///
/// Args:
/// - `agent` (optional, defaults to caller's `instance_name`): which
///   agent's bound worktree to clean.
///
/// Response shape (idempotent, observable):
/// - `{cleaned_count: N}` — N empty inits removed (0 = noop)
/// - `{cleaned_count: 0, skipped_reason: "no binding"}` — agent has no
///   active binding; explicit reason so callers don't mistake for noop
/// - `{error: "...", code: "cleanup_failed"}` — git subprocess failure
pub(super) fn handle_cleanup_init_commits(home: &Path, args: &Value, instance_name: &str) -> Value {
    let agent = args["agent"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(instance_name);
    if agent.is_empty() {
        return json!({
            "error": "missing 'agent' (and no caller instance_name available)",
            "code": "needs_agent"
        });
    }
    let worktree = match crate::binding::read(home, agent)
        .and_then(|v| v["worktree"].as_str().map(std::path::PathBuf::from))
    {
        Some(wt) => wt,
        None => {
            return json!({
                "cleaned_count": 0,
                "skipped_reason": format!("no binding for agent '{agent}'"),
            });
        }
    };
    match crate::mcp::handlers::dispatch_hook::clean_empty_init_commits(&worktree) {
        Ok(count) => json!({"cleaned_count": count}),
        Err(msg) => json!({
            "error": msg,
            "code": "cleanup_failed",
        }),
    }
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
    // #779 P2 Piece 3 site A: pre-#779-P2 swallowed dir-create errors
    // silently and continued, returning happy-path Value even when the
    // subsequent atomic_write was destined to fail. Now surface as
    // structured error matching the existing `{error, code}` shape so
    // handle_checkout_repo (and direct callers of `ci action=watch`)
    // observe the partial-failure class.
    if let Err(e) = std::fs::create_dir_all(&ci_dir) {
        return json!({
            "error": format!("ci-watches dir create failed: {e}"),
            "code": "ci_watches_dir_create_failed",
        });
    }
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

    // #779 P2 Piece 3 site B: atomic_write failure (disk full,
    // permission, etc.) previously surfaced as `let _ = ...` silent
    // discard, returning happy-path Value with `watching: true` even
    // when the watch file was never written. Now surface as structured
    // error so callers don't act on phantom state. NOTE: site C
    // (line ~362 `read_to_string(&watch_path).ok()`) is intentionally
    // NOT hardened — its None case is the load-bearing fresh-watch
    // init path; hardening there would block legitimate first
    // subscribes.
    if let Err(e) = crate::store::atomic_write(
        &watch_path,
        serde_json::to_string_pretty(&watch)
            .unwrap_or_default()
            .as_bytes(),
    ) {
        return json!({
            "error": format!("watch file write failed: {e}"),
            "code": "watch_write_failed",
        });
    }
    // #813: on-watch-start mergeable check. Builds a default provider
    // for the repo (GitHub-only impl; GitLab/Bitbucket inherit the
    // Unknown stub per §3.7), queries mergeable_state synchronously,
    // and emits `[ci-conflict-detected]` to every subscriber if the
    // PR is in DIRTY state. Fail-open on any provider error.
    let subscribers_for_alert: Vec<String> = crate::daemon::ci_watch::parse_subscribers(&watch);
    if let Some(provider) = build_default_provider(repo) {
        crate::daemon::ci_watch::watch_start_check_mergeable(
            home,
            &watch_path,
            repo,
            branch,
            &subscribers_for_alert,
            provider.as_ref(),
        );
    }
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
pub(crate) fn handle_status_ci(home: &Path, args: &Value, instance_name: &str) -> Value {
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
            // #813: surface cached mergeable state so callers can
            // distinguish "CI running" silence from "CONFLICTING
            // blocked forever" silence. Field is `null` for watches
            // that haven't run their first mergeable check yet.
            "pr_mergeable_state": watch["last_mergeable_state"].as_str(),
            "pr_mergeable_check_at": watch["last_mergeable_check_at"].as_str(),
        }));
    }
    let mut resp = json!({"watches": out});
    if let Some(w) = crate::daemon::ci_watch::github_token_warning_from_env() {
        resp["setup_warning"] = json!(w);
    }
    resp
}

/// #813: build the default `CiProvider` for a repo URL. Mirrors
/// `watcher.rs::check_ci_watches`'s factory but with the canonical
/// host URLs (no per-watch URL override) — sufficient for the
/// on-watch-start mergeable check at dispatch time. GitHub fully
/// implemented; GitLab/Bitbucket return Unknown via the trait
/// default (§3.7 cross-backend stance — promotion blocked behind
/// real operator usage).
fn build_default_provider(repo: &str) -> Option<Box<dyn crate::daemon::ci_watch::CiProvider>> {
    use crate::daemon::ci_watch::{
        detect_provider_from_remote, BitbucketCiProvider, CiProvider, GitHubCiProvider,
        GitLabCiProvider,
    };
    let (kind, _is_custom) = detect_provider_from_remote(repo);
    let provider: Option<Box<dyn CiProvider>> = match kind {
        "gitlab" => GitLabCiProvider::with_base_url("https://gitlab.com".to_string())
            .ok()
            .map(|p| Box::new(p) as Box<dyn CiProvider>),
        "bitbucket_cloud" => {
            BitbucketCiProvider::with_base_url("https://api.bitbucket.org".to_string())
                .ok()
                .map(|p| Box::new(p) as Box<dyn CiProvider>)
        }
        _ => GitHubCiProvider::with_base_url("https://api.github.com".to_string())
            .ok()
            .map(|p| Box::new(p) as Box<dyn CiProvider>),
    };
    provider
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
