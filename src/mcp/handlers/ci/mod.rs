use crate::agent_ops::validate_branch;
use crate::git_helpers::git_bypass;
use serde_json::{json, Value};
use std::path::Path;

/// #1447: resolve the checkout source repo from `repository_path` — the
/// cross-tool standard name used by bind_self / team update. Returns `None`
/// when absent or empty.
fn checkout_source(args: &Value) -> Option<&str> {
    args.get("repository_path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

pub(super) fn handle_checkout_repo(home: &Path, args: &Value, instance_name: &str) -> Value {
    let result = handle_checkout_repo_inner(home, args, instance_name);
    log_checkout_outcome(home, args, instance_name, &result);
    result
}

/// #1466: record every `repo action=checkout` outcome — success AND every
/// error path — to the daemon-observable event-log, so a silently-failed
/// checkout (e.g. the partial-worktree bootstrap race that motivated #1466:
/// `src/` present but no `.git`) leaves a diagnosable trace. Reuses
/// `event_log::log` (the same freeform-msg helper as `worktree_released_full`
/// — no new schema). Best-effort: `event_log::log` is fire-and-forget, so a
/// logging failure can never affect the checkout result (observability must
/// not become an availability risk). Logging once at the single wrapper exit
/// guarantees coverage of all current and future return paths.
fn log_checkout_outcome(home: &Path, args: &Value, instance_name: &str, result: &Value) {
    let branch = args["branch"].as_str().unwrap_or("HEAD");
    let source = checkout_source(args).unwrap_or("");
    let ok = result.get("error").is_none();
    let mut msg = format!("branch={branch} source={source} ok={ok}");
    if let Some(err) = result.get("error").and_then(Value::as_str) {
        msg.push_str(&format!(" err={err}"));
    }
    if let Some(path) = result.get("path").and_then(Value::as_str) {
        msg.push_str(&format!(" path={path}"));
    }
    crate::event_log::log(home, "worktree_checkout", instance_name, &msg);
}

fn handle_checkout_repo_inner(home: &Path, args: &Value, instance_name: &str) -> Value {
    let source = match checkout_source(args) {
        Some(s) => s,
        None => return json!({"error": "missing 'repository_path'"}),
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
    if bind {
        if let Err(e) = crate::agent_ops::ensure_not_protected_json(branch) {
            return e;
        }
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
    // #1494: idempotent bind. When `bind:true` and THIS agent already holds a
    // binding on the SAME branch whose worktree is live on disk, the branch was
    // already provisioned — by the dispatch pre-build hook (binding.json +
    // worktree at `<agent>/<branch>`) or a prior `repo checkout`. The direct
    // `git worktree add` below would otherwise fail with "is already checked
    // out" (the same branch is leased at the dispatch path, a DIFFERENT dir
    // than this handler's `<agent>-<source>` scheme), forcing a manual `cd`.
    // Return the EXISTING worktree path as success instead — same spirit as
    // #1465 idempotent-release: operation already at target state = success.
    //
    // Cross-agent safety: `binding::read` is keyed per-agent, so a DIFFERENT
    // agent holding the branch leaves this agent's binding absent/mismatched —
    // the short-circuit does NOT fire and the genuine `git worktree add`
    // conflict error below is preserved. Same-agent DIFFERENT-branch bindings
    // also fall through (branch mismatch), unchanged.
    //
    // #1882 (reviewer-2): repo checkout is the THIRD production bind path (besides
    // the dispatch + bind_self funnel through dispatch_auto_bind_lease). Hold the
    // SAME per-branch lease flock across its check-then-act (cross-agent scan +
    // idempotent read + git worktree add + bind_full) so a concurrent dispatch or
    // another repo checkout can't double-bind the branch. Bind-only (a `--detach`
    // checkout writes no binding); the guard lives to fn end so it covers bind_full.
    let _lease_lock = if bind {
        match crate::binding::acquire_branch_lease_lock(home, branch) {
            Ok(g) => Some(g),
            Err(e) => {
                return json!({
                    "error": format!("could not acquire branch lease lock for '{branch}': {e}"),
                    "code": "lease_lock",
                    "branch": branch,
                })
            }
        }
    } else {
        None
    };
    if bind {
        // #1882: cross-agent P0-1.5 reject UNDER the lock — another agent holding
        // this branch is refused (mirrors the dispatch path's scan), rather than
        // leaning on `git worktree add`'s "already checked out" error. The
        // same-agent idempotent short-circuit below handles THIS agent re-checkout.
        if let Some(other) =
            crate::binding::scan_existing_branch_binding(home, branch, instance_name)
        {
            return json!({
                "error": format!(
                    "branch '{branch}' already leased by '{other}' — release first or use a different branch"
                ),
                "code": "cross_agent_conflict",
                "branch": branch,
            });
        }
        if let Some(existing) = crate::binding::read(home, instance_name) {
            let same_branch = existing.get("branch").and_then(|v| v.as_str()) == Some(branch);
            let live_wt = existing
                .get("worktree")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .filter(|p| p.exists());
            if same_branch {
                if let Some(wt) = live_wt {
                    let wt_str = wt.display().to_string();
                    tracing::info!(
                        instance = instance_name,
                        %branch,
                        path = %wt_str,
                        "repo checkout bind:true idempotent — agent already bound to this branch, returning existing worktree"
                    );
                    return json!({
                        "path": wt_str,
                        "source": source_path,
                        "branch": branch,
                        "bound": true,
                        "idempotent": true,
                        "auto_created_branch": auto_created_branch,
                        "fetch_attempted": fetch_attempted,
                    });
                }
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
            // #1275: write .agend-managed unconditionally so
            // release_worktree and GC can always clean up.
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
            if bind {
                if let Err(e) = crate::binding::bind_full(
                    home,
                    instance_name,
                    "",
                    branch,
                    &worktree_dir,
                    &source_canonical,
                ) {
                    // #1310: rollback worktree on binding failure to prevent orphans
                    tracing::warn!(
                        %branch, path = %worktree_dir.display(),
                        error = %e,
                        "bind_full failed after worktree add — rolling back worktree"
                    );
                    // #1899: bounded via git_bypass (LOCAL 60s) — best-effort rollback.
                    let _ = crate::git_helpers::git_bypass(
                        Path::new(&source_path),
                        &["worktree", "remove", "--force", &worktree_path_str],
                    );
                    return json!({
                        "error": format!("bind_full failed, worktree rolled back: {e}"),
                        "code": "bind_rollback",
                        "branch": branch,
                    });
                }
                if let Some(r) = crate::mcp::handlers::dispatch_hook::derive_repo_from_remote_pub(
                    &source_canonical,
                ) {
                    // t-ci-ready-pr2-drop-derive-reviewer (operator-approved B): the
                    // #1040/#1037 `<team>-reviewer` auto-derive was REMOVED from the
                    // dev-self-claim path too (consistent decouple with the dispatch
                    // side). A self-claimed `repo action=checkout bind=true` now arms
                    // the watch with NO chain target → on CI pass the dev (a
                    // subscriber) gets the informational `[ci-pass]`; chaining the
                    // actionable `[ci-ready-for-action]` to a reviewer requires an
                    // EXPLICIT `next_after_ci` (review handoff is now explicit, not a
                    // silent naming-convention auto-handoff).
                    let watch_args = json!({"repository": &r, "branch": branch});
                    let watch_resp = handle_watch_ci(home, &watch_args, instance_name);
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
            }
            if !warnings.is_empty() {
                resp["warnings"] = json!(warnings);
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
    let agent = args["instance"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(instance_name);
    if agent.is_empty() {
        return json!({
            "error": "missing 'instance' (and no caller instance_name available)",
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

    // Derive source repo from worktree .git link before any removal —
    // needed for post-removal prune if git worktree remove fails.
    let source_repo = canonical
        .join(".git")
        .is_file()
        .then(|| std::fs::read_to_string(canonical.join(".git")).ok())
        .flatten()
        .and_then(|content| {
            let gitdir = content.strip_prefix("gitdir: ")?.trim();
            let p = std::path::Path::new(gitdir);
            p.parent()?.parent()?.parent().map(|pp| pp.to_path_buf())
        });

    // #1899: bounded via spawn_group_bounded with a BARE Command — this site
    // deliberately does NOT set AGEND_GIT_BYPASS and does NOT set current_dir
    // (runs from the daemon cwd, best-effort). Preserve that exact behaviour;
    // spawn_group_bounded only adds the LOCAL timeout + safe process-group kill,
    // without forcing the bypass env. (Whether it SHOULD bypass like ci/mod:270
    // is a separate behaviour question, out of scope for this timeout PR.)
    // git-raw-allowed: deliberate non-bypass + no current_dir; already bounded via
    // spawn_group_bounded; the Ok(non-zero) arm surfaces stderr in the JSON `note`
    // (git_ok would discard it), so git_cmd/git_ok would not be byte-identical.
    let mut cmd = std::process::Command::new("git");
    cmd.args(["worktree", "remove", "--force", &path_str]);
    match crate::git_helpers::spawn_group_bounded(
        cmd,
        "git worktree remove (cleanup)",
        crate::git_helpers::LOCAL_GIT_TIMEOUT,
    ) {
        Ok(o) if o.status.success() => json!({"path": path}),
        Ok(o) => {
            let _ = std::fs::remove_dir_all(&canonical);
            if let Some(src) = &source_repo {
                crate::worktree::prune(src);
            }
            json!({"path": path, "note": String::from_utf8_lossy(&o.stderr).to_string()})
        }
        Err(_) => {
            let _ = std::fs::remove_dir_all(&canonical);
            if let Some(src) = &source_repo {
                crate::worktree::prune(src);
            }
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
/// #1619: resolve the target `owner/repo` for a PR/CI handler.
///
/// Resolution order: explicit `repository` arg (canonicalized) → the
/// caller's `binding.json` `source_repo` origin remote → error. It
/// NEVER falls back to a hardcoded repo slug: a detection miss on
/// someone else's deployment must fail loud, not silently operate
/// (merge/checks/state) on the maintainer's repo.
///
/// Originally inline in `handle_watch_ci` (Sprint 55 P0-B); extracted so
/// `handle_merge_repo` shares the exact same resolution instead of the
/// old `.unwrap_or("suzuke/agend-terminal")` footgun. EC1: explicit
/// error when neither arg nor binding present (no silent cwd-derivation).
/// EC15: validate the binding's source_repo path still exists.
fn resolve_repo_or_error(home: &Path, instance_name: &str, args: &Value) -> Result<String, Value> {
    match args["repository"].as_str().filter(|s| !s.is_empty()) {
        Some(r) => {
            // #942: canonicalize on entry so the hash key + stored
            // `repo` field both reflect the single canonical form.
            // Rejects obviously-malformed input (non-GitHub URL, malformed
            // slug) with operator-actionable error.
            match crate::mcp::handlers::dispatch_hook::canonicalize_repo_slug(r) {
                Some(c) => Ok(c),
                None => Err(json!({
                    "error": format!(
                        "invalid 'repository' format: {r:?} — expected `owner/repo` or full GitHub URL"
                    ),
                    "code": "invalid_repo_format",
                })),
            }
        }
        None => {
            let binding = home
                .join("runtime")
                .join(instance_name)
                .join("binding.json");
            let Ok(content) = std::fs::read_to_string(&binding) else {
                return Err(json!({
                    "error": "could not determine repo slug; pass explicit 'repository' arg or call bind_self first (no active binding)",
                    "code": "no_binding_no_repo"
                }));
            };
            let Ok(v) = serde_json::from_str::<Value>(&content) else {
                return Err(json!({
                    "error": "binding.json corrupt — re-bind or pass explicit 'repository'",
                    "code": "binding_corrupt"
                }));
            };
            let Some(src) = v["source_repo"].as_str().filter(|s| !s.is_empty()) else {
                return Err(json!({
                    "error": "binding has no source_repo — pass explicit 'repository' arg",
                    "code": "no_binding_no_repo"
                }));
            };
            let src_path = std::path::Path::new(src);
            if !src_path.exists() {
                return Err(json!({
                    "error": format!("binding source_repo '{src}' no longer exists — re-bind or pass explicit 'repository'"),
                    "code": "source_repo_path_deleted"
                }));
            }
            match crate::mcp::handlers::dispatch_hook::derive_repo_from_remote_pub(src_path) {
                Some(r) => Ok(r),
                None => Err(json!({
                    "error": format!("could not derive owner/repo from '{src}' origin remote — pass explicit 'repository' arg or set fleet.yaml `repo:` override"),
                    "code": "non_github_remote_no_override"
                })),
            }
        }
    }
}

pub(crate) fn handle_watch_ci(home: &Path, args: &Value, instance_name: &str) -> Value {
    // Sprint 55 P0-B: when caller omits `repo` arg, auto-derive from
    // sender's binding.json source_repo (set by `bind_self` /
    // `dispatch_auto_bind_lease`). #1619: shared `resolve_repo_or_error`
    // — explicit error when neither arg nor binding present (no silent
    // cwd-derivation, no hardcoded fallback).
    let repo_owned = match resolve_repo_or_error(home, instance_name, args) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let repo: &str = &repo_owned;
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
    if let Err(e) = crate::agent_ops::ensure_not_protected_json(branch) {
        return e;
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
    // #1991: an explicit (re-)watch overrides a prior unwatch tombstone —
    // the human/agent decision to watch again clears the auto-arm optout.
    if let Some(obj) = watch.as_object_mut() {
        obj.remove("auto_arm_optout");
    }
    // Refresh expires_at on each subscribe — keeps the watch alive
    // as long as at least one agent stays interested.
    watch["expires_at"] = json!((chrono::Utc::now()
        + chrono::Duration::hours(crate::daemon::ci_watch::WATCH_TTL_HOURS))
    .to_rfc3339());
    // Issue #650: store next_after_ci for auto-routing on CI pass
    if let Some(next) = args["next_after_ci"].as_str().filter(|s| !s.is_empty()) {
        watch["next_after_ci"] = json!(next);
    }
    // #1031: persist dispatch task_id when supplied (by
    // dispatch_auto_bind_lease) so the ci_check_repo emit site can
    // populate `[ci-ready-for-action]` InboxMessage's task_id field,
    // giving the reviewer a structured back-link to the originating
    // dispatch. Manual `ci action=watch` callers may also pass
    // task_id explicitly to bind the watch to a specific task.
    if let Some(tid) = args["task_id"].as_str().filter(|s| !s.is_empty()) {
        watch["task_id"] = json!(tid);
    }
    // #972 reviewer-rejection fix: persist `review_class` so the
    // pr_state aggregator can honor §3.5 dual-review at runtime. Accepted
    // values: `"single"` (default — §3.6) or `"dual"` (§3.5). Other
    // strings are tolerated and treated as Single at read time
    // (see `daemon::ci_watch::poller::parse_review_class`). Without
    // this field operator must currently `delete fleet.yaml` to
    // remove the watch and re-arm with `--review-class dual` —
    // documented as a workflow gap to close in a follow-up CLI/MCP
    // exposure.
    if let Some(rc) = args["review_class"].as_str().filter(|s| !s.is_empty()) {
        watch["review_class"] = json!(rc);
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
// pub(crate): the #1991 auto-arm tombstone regression test (pr_state::auto_arm)
// exercises the real unwatch → tombstone → no-re-arm chain.
pub(crate) fn handle_unwatch_ci(home: &Path, args: &Value) -> Value {
    let repo = match args["repository"].as_str() {
        Some(r) => r,
        None => return json!({"error": "missing 'repository'"}),
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
        // #1991: keep the file as a TOMBSTONE instead of deleting it. PR-3
        // auto-arm (`pr_state::auto_arm`) re-arms any open PR whose watch file
        // is ABSENT — deleting here re-subscribed the very agent that just
        // unwatched, ~60s later (the #1991 storm: unwatch → file gone → next
        // pr_state scan auto-arms → notifications resume). Unwatch is an
        // EXPLICIT decision: the tombstone suppresses auto-arm until the PR
        // goes terminal or someone explicitly re-watches (handle_watch_ci
        // clears the flag). It is never polled (`prepare_poll_context` →
        // SkipReason::Invalid, zero API budget) and gc exempts it from the
        // TTL/inactivity reaps (P6: a TTL-reap → re-arm is the same betrayal,
        // only slower); end-of-life = PR-terminal gc or the unwatched_at
        // age-cap backstop.
        watch["subscribers"] = json!([]);
        watch["instance"] = json!("");
        watch["auto_arm_optout"] = json!(true);
        watch["unwatched_at"] = json!(chrono::Utc::now().to_rfc3339());
        if let Err(e) = crate::store::atomic_write(
            &path,
            serde_json::to_string_pretty(&watch)
                .unwrap_or_default()
                .as_bytes(),
        ) {
            return json!({
                "error": format!("failed to persist unwatch tombstone: {e}"),
                "code": "unwatch_write_failed",
            });
        }
        return json!({
            "repo": repo,
            "watching": false,
            "subscribers": Vec::<String>::new(),
            "tombstone": true,
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

    if let Err(e) = crate::store::atomic_write(
        &path,
        serde_json::to_string_pretty(&watch)
            .unwrap_or_default()
            .as_bytes(),
    ) {
        return json!({
            "error": format!("failed to persist unwatch: {e}"),
            "code": "unwatch_write_failed",
        });
    }
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
    let filter_repo = args["repository"].as_str();
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
            // #1473 display gap: surface the stored CI-pass handoff target so
            // `ci action=status` shows it (previously omitted → operators
            // mis-read it as unset even when armed).
            "next_after_ci": watch["next_after_ci"].as_str(),
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

/// #817: handle `repo action=cleanup_merged_branches`. Dry-run by
/// default; apply requires explicit `apply=true` + `confirm_ids`
/// subset + non-empty `audit_reason`. Mirrors #806 sweep's
/// double-opt-in contract.
///
/// Args:
/// - `agent` (optional, defaults to caller's `instance_name`) —
///   used to resolve the operator's bound source_repo via
///   `binding.json`.
/// - `base` (optional, default "main") — branch to compare against
///   for clean_merged / squash_merged detection.
/// - `min_age_days` (optional, default 90) — stale_idle threshold.
/// - `apply` (bool, default false) — when false, returns dry-run.
/// - `confirm_ids` (array<string>) — required when apply=true.
///   Subset of dry-run's all_ids.
/// - `audit_reason` (string) — required when apply=true. Logged
///   to event-log.jsonl per deleted branch.
pub(crate) fn handle_cleanup_merged_branches(
    home: &Path,
    args: &Value,
    instance_name: &str,
) -> Value {
    let agent = args["instance"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(instance_name);
    let source_repo = match crate::binding::read(home, agent)
        .and_then(|v| v["source_repo"].as_str().map(std::path::PathBuf::from))
    {
        Some(p) => p,
        None => {
            return json!({
                "error": format!("no binding source_repo for agent '{agent}'"),
                "code": "no_binding_source_repo",
            });
        }
    };
    let base = args["base"].as_str().unwrap_or("main");
    let min_age_days = args["min_age_days"]
        .as_i64()
        .unwrap_or(crate::branch_sweep::STALE_IDLE_DEFAULT_DAYS);
    let apply = args["apply"].as_bool().unwrap_or(false);
    let now = chrono::Utc::now();
    let categories = match crate::branch_sweep::scan(&source_repo, base, min_age_days, now) {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "error": format!("branch sweep scan failed: {e}"),
                "code": "scan_failed",
            });
        }
    };
    if !apply {
        return json!({
            "dry_run": true,
            "categories": &categories,
            "candidate_ids": categories.deletable_ids(),
            "total_candidates": categories.total(),
            "to_apply_hint": "repo action=cleanup_merged_branches apply=true confirm_ids=<subset> audit_reason=<...>",
            "active_unknown_note": "active_unknown bucket is NOT in candidate_ids by default — include those names explicitly in confirm_ids if you really want to delete them",
        });
    }
    let confirm_ids: std::collections::HashSet<String> = args["confirm_ids"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if confirm_ids.is_empty() {
        return json!({
            "error": "apply=true requires non-empty 'confirm_ids' (subset of candidate_ids from prior dry-run)",
            "code": "missing_confirm_ids",
        });
    }
    let audit_reason = args["audit_reason"].as_str().unwrap_or("");
    if audit_reason.is_empty() {
        return json!({
            "error": "apply=true requires non-empty 'audit_reason' for the event-log entry",
            "code": "missing_audit_reason",
        });
    }
    // Validate confirm_ids ⊆ all_ids (including active_unknown for
    // explicit opt-in). Mismatch ⇒ reject loudly — operator must
    // re-run dry-run to refresh the candidate list.
    let candidate_set: std::collections::HashSet<String> =
        categories.all_ids().into_iter().collect();
    let unknown: Vec<String> = confirm_ids.difference(&candidate_set).cloned().collect();
    if !unknown.is_empty() {
        return json!({
            "error": "confirm_ids contained entries not in current sweep candidates",
            "unknown": unknown,
            "hint": "re-run dry-run; candidates may have changed since last scan",
            "code": "stale_confirm_ids",
        });
    }
    match crate::branch_sweep::emit_delete_batch(
        home,
        &source_repo,
        &categories,
        &confirm_ids,
        audit_reason,
    ) {
        Ok(count) => json!({
            "applied": count,
            "audit_reason": audit_reason,
            "restore_hint": "see event-log.jsonl `branch_sweep_apply` entries for source SHAs (git branch <name> <sha>)",
        }),
        Err(e) => json!({
            "error": format!("branch sweep apply failed: {e}"),
            "code": "apply_failed",
        }),
    }
}

/// #1467: outcome of post-merge verification via `gh pr view`.
enum MergeVerdict {
    /// PR confirmed merged: `state == "MERGED"` AND a non-empty merge commit
    /// oid. Carries the merge commit SHA.
    Confirmed(String),
    /// Not (yet) confirmed merged. May be transient (merge queue / eventual
    /// consistency) — caller should re-query, not treat as a hard failure.
    Unconfirmed {
        state: String,
        merge_state_status: String,
    },
}

/// #1467: classify a `gh pr view` result into a [`MergeVerdict`]. PURE —
/// tests drive it directly without shelling `gh`. A PR is confirmed merged
/// only when GitHub reports `state == "MERGED"` AND a non-empty merge-commit
/// oid. #PR-D: takes the typed [`crate::scm::PrSummary`] (was a raw `Value`);
/// the three reads map 1:1 (`state` → `state`; `mergeCommit.oid` →
/// `merge_commit_oid`, empty→None; `mergeStateStatus` → `merge_state_status`),
/// so the verdict is byte-for-byte the same.
fn classify_merge_summary(s: &crate::scm::PrSummary) -> MergeVerdict {
    let state = s.state.clone().unwrap_or_else(|| "UNKNOWN".to_string());
    let oid = s.merge_commit_oid.clone().unwrap_or_default();
    if state == "MERGED" && !oid.is_empty() {
        MergeVerdict::Confirmed(oid)
    } else {
        MergeVerdict::Unconfirmed {
            state,
            merge_state_status: s.merge_state_status.clone().unwrap_or_default(),
        }
    }
}

/// #1467: after `gh pr merge` reports success, confirm the PR actually landed.
/// Bounded poll (≤3 attempts, 2s apart) to tolerate merge-queue / eventual-
/// consistency lag — NOT an infinite wait. Returns the last verdict seen; the
/// first `Confirmed` short-circuits.
fn verify_merge_landed(repo: &str, pr: u64) -> MergeVerdict {
    // #PR-D site 1: the single `gh pr view` goes through ScmProvider. argv
    // byte-identical (`pr view <pr> --repo R --json state,mergeCommit,
    // mergedAt,mergeStateStatus`). The retry loop stays here (deliberately
    // NOT folded into the trait — spike §4). On any gh failure pr_view
    // returns Err → keep polling / fall back to `last` (was the prior
    // non-success / parse-fail skip).
    let provider = crate::scm::make_scm_provider(repo, None);
    let mut last = MergeVerdict::Unconfirmed {
        state: "UNKNOWN".to_string(),
        merge_state_status: String::new(),
    };
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        if let Ok(summary) = provider.pr_view(
            repo,
            pr,
            &["state", "mergeCommit", "mergedAt", "mergeStateStatus"],
        ) {
            match classify_merge_summary(&summary) {
                MergeVerdict::Confirmed(c) => return MergeVerdict::Confirmed(c),
                unconfirmed => last = unconfirmed,
            }
        }
    }
    last
}

/// #base-drift: pure decision — should GitHub's `mergeStateStatus` REFUSE the
/// merge? `BEHIND` (PR base behind main → an `--admin` squash lands a
/// phantom-reversion diff, dev-2 #1798) and `DIRTY` (conflicts) refuse;
/// everything else (CLEAN / UNSTABLE / BLOCKED / UNKNOWN / empty) proceeds —
/// fail-OPEN, because GitHub may still be computing mergeability and we must not
/// block a real merge on a transient (#813 pattern). Returns `Some((why, hint))`
/// to refuse, `None` to proceed.
fn base_drift_refusal(merge_state_status: &str) -> Option<(&'static str, &'static str)> {
    match merge_state_status {
        "BEHIND" => Some((
            "PR base is behind main (phantom-reversion risk)",
            "rebase onto current main: git fetch && git rebase origin/main && git push --force-with-lease",
        )),
        "DIRTY" => Some((
            "PR has merge conflicts with main",
            "resolve: git fetch && git rebase origin/main, fix conflicts, git push --force-with-lease",
        )),
        _ => None,
    }
}

pub(super) fn handle_merge_repo(home: &Path, args: &Value, instance_name: &str) -> Value {
    let pr = match args["pr"].as_u64() {
        Some(n) => n,
        None => return json!({"error": "missing 'pr' (PR number)"}),
    };
    // #1619: resolve via the shared helper instead of the old
    // `.unwrap_or("suzuke/agend-terminal")` — a detection miss must NOT
    // silently merge/check/state-query against the maintainer's repo.
    let repo = match resolve_repo_or_error(home, instance_name, args) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let force = args["force"].as_bool().unwrap_or(false);
    let force_reason = args["force_reason"].as_str().unwrap_or("");

    if force && force_reason.is_empty() {
        return json!({"error": "force=true requires non-empty force_reason"});
    }

    if !force {
        // #PR-D site 2: `gh pr checks` via ScmProvider. argv byte-identical
        // (`pr checks <pr> --repo R --json name,state`). The client-side
        // filter (state ≠ SUCCESS/SKIPPED) reproduces the prior inline one;
        // a null/empty state counts as failing (lenient parse_checks) — same
        // as the prior `as_str().unwrap_or("")`, preserving the fail-closed
        // gate. Intentional observable delta: the prior code surfaced two
        // distinct errors (parse-fail vs query-fail) which pr_checks can't
        // tell apart — both now collapse to ONE fail-closed message. The
        // merge DECISION (any checks problem → refuse) is unchanged.
        let checks = match crate::scm::make_scm_provider(&repo, None).pr_checks(&repo, pr) {
            Ok(c) => c,
            Err(_) => {
                return json!({
                    "error": "CI checks could not be determined — merge refused",
                    "hint": "Verify PR number and repo, or use force=true with force_reason (fail-closed)"
                });
            }
        };
        let failing: Vec<&crate::scm::CheckState> = checks
            .iter()
            .filter(|c| c.state != "SUCCESS" && c.state != "SKIPPED")
            .collect();
        if !failing.is_empty() {
            let summary: Vec<String> = failing
                .iter()
                .map(|c| {
                    // Preserve the prior `unwrap_or("?")` placeholder for an
                    // empty/null name or state.
                    let name = if c.name.is_empty() {
                        "?"
                    } else {
                        c.name.as_str()
                    };
                    let state = if c.state.is_empty() {
                        "?"
                    } else {
                        c.state.as_str()
                    };
                    format!("{name}: {state}")
                })
                .collect();
            return json!({
                "error": "CI checks not all passed — merge refused",
                "failing_checks": summary,
                "hint": "Wait for CI to pass, or use force=true with force_reason for emergency bypass"
            });
        }

        // #base-drift: refuse a stacked/behind PR. GitHub's `mergeStateStatus`
        // BEHIND means the PR base is behind main (another PR merged first) → an
        // `--admin` squash lands a phantom-reversion diff (looks like reverting the
        // already-merged PR — dev-2 #1798, only caught by a manual diff-check +
        // rebase). DIRTY = conflicts (can't merge cleanly). Critically, the
        // `--admin` merge BYPASSES branch-protection's
        // `required_status_checks.strict`, so GitHub will NOT block these — the
        // daemon must. Any other state (CLEAN/UNSTABLE/BLOCKED/UNKNOWN) or a
        // pr_view error → fail-OPEN (proceed): GitHub may still be computing
        // mergeability and we must not block a real merge on a transient (the #813
        // mergeable-check pattern). Reuses the same `pr_view` path
        // `verify_merge_landed` uses — no new infra. `force=true` bypasses (the
        // audit block below logs it, like the CI gate).
        if let Ok(summary) =
            crate::scm::make_scm_provider(&repo, None).pr_view(&repo, pr, &["mergeStateStatus"])
        {
            let mss = summary.merge_state_status.as_deref().unwrap_or("");
            if let Some((why, hint)) = base_drift_refusal(mss) {
                return json!({
                    "error": format!("base is stale — merge refused: {why}"),
                    "merge_state_status": mss,
                    "hint": format!("{hint}; or force=true with force_reason for emergency bypass"),
                });
            }
        }
    }

    if force {
        let event = serde_json::json!({
            "kind": "merge_force_bypass",
            "agent": instance_name,
            "pr": pr,
            "repo": repo,
            "force_reason": force_reason,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });
        let events_path = home.join("fleet_events.jsonl");
        let audit_written = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(events_path)?;
            writeln!(f, "{event}")?;
            Ok(())
        })();
        if let Err(e) = audit_written {
            return json!({
                "error": format!("force-merge refused: audit log write failed: {e}"),
                "hint": "fix fleet_events.jsonl permissions or disk space, then retry"
            });
        }
    }

    // #PR-Z site 3: the ONLY write — `gh pr merge` via ScmProvider. argv
    // byte-identical (`pr merge <pr> --repo R --admin --squash
    // --delete-branch`, pinned by scm::tests::pr_merge_args_match_existing_gh_call).
    // MergeOutcome maps the original exit-status branches 1:1: Submitted =
    // exit-0 (→ verify_merge_landed post-condition, unchanged; retry loop
    // stays in that caller), Failed = non-zero (→ "gh pr merge failed" +
    // raw stderr), Err = spawn failure (→ "failed to run gh: {e}").
    match crate::scm::make_scm_provider(&repo, None).pr_merge(
        &repo,
        pr,
        &crate::scm::MergeOpts {
            admin: true,
            squash: true,
            delete_branch: true,
        },
    ) {
        // #1467: `gh pr merge` exit 0 is NECESSARY but not SUFFICIENT — a
        // merge-queue / branch-protection / eventual-consistency situation can
        // exit 0 without the PR actually landing (observed: cross-team PRs
        // reported merged:true while still OPEN, commits unpushed). Verify the
        // post-condition with `gh pr view` before claiming success.
        Ok(crate::scm::MergeOutcome::Submitted) => match verify_merge_landed(&repo, pr) {
            MergeVerdict::Confirmed(merge_commit) => json!({
                "merged": true,
                "pr": pr,
                "forced": force,
                "mergeCommit": merge_commit,
            }),
            MergeVerdict::Unconfirmed {
                state,
                merge_state_status,
            } => json!({
                // NOT merged, but NOT a hard error either: `gh pr merge`
                // succeeded and the PR may still land (merge queue / eventual
                // consistency). Report the true state so the caller can re-query
                // rather than trust a false merged:true.
                "merged": false,
                "pending": true,
                "code": "merge_unconfirmed",
                "pr": pr,
                "state": state,
                "mergeStateStatus": merge_state_status,
                "hint": "gh pr merge reported success but the PR is not yet confirmed MERGED \
                         (possible merge-queue / eventual consistency). Re-query `gh pr view` \
                         before acting; do NOT blindly re-merge.",
            }),
        },
        Ok(crate::scm::MergeOutcome::Failed { stderr }) => {
            json!({
                "error": "gh pr merge failed",
                "stderr": stderr,
            })
        }
        // pr_merge's spawn-failure Err already carries "failed to run gh: …"
        // (set in GitHubScmProvider::run), so surface it as-is — using
        // `e.to_string()` reproduces the original `format!("failed to run
        // gh: {e}")` exactly (no double prefix).
        Err(e) => json!({"error": e.to_string()}),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;
