use crate::agent_ops::validate_branch;
use crate::git_helpers::git_bypass;
use serde_json::{json, Value};
use std::path::Path;

/// #1447: resolve the checkout source repo from `repository_path` — the
/// cross-tool standard name used by bind_self / team update. Returns `None`
/// when absent or empty.
pub(crate) fn checkout_source(args: &Value) -> Option<&str> {
    args.get("repository_path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

pub(crate) fn handle_checkout_repo(home: &Path, args: &Value, instance_name: &str) -> Value {
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
    // #2158 PR1: resolve + validate the source repo path fail-closed (absolute or
    // known agent name only; canonicalize; reject system dirs). Extracted to
    // `source_resolve` — keeps this oversized handler under the file_size ceiling
    // (t-61 split debt) and isolates the security-sensitive resolution for review.
    let (source_path, source_canonical) =
        match super::source_resolve::resolve_checkout_source_path(home, source) {
            Ok(pair) => pair,
            Err(e) => return e,
        };
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
    // #1494: idempotent bind. If THIS agent already holds a binding on the SAME
    // branch with a live worktree (provisioned by the dispatch pre-build hook or a
    // prior `repo checkout`), the `git worktree add` below would fail "is already
    // checked out" (leased at a DIFFERENT dir than this handler's `<agent>-<source>`
    // scheme). Return the EXISTING worktree as success (#1465 idempotent-release
    // spirit). Cross-agent-safe: `binding::read` is per-agent, so a DIFFERENT agent
    // (or same-agent DIFFERENT branch) does NOT short-circuit — the genuine `git
    // worktree add` conflict error below is preserved.
    // #1882 (reviewer-2): repo checkout is the THIRD bind path (besides dispatch +
    // bind_self via dispatch_auto_bind_lease); hold the per-branch lease flock
    // across its check-then-act (cross-agent scan + idempotent read + worktree add +
    // bind_full) so a concurrent dispatch/checkout can't double-bind. Bind-only (a
    // `--detach` checkout writes no binding); guard lives to fn end (covers bind_full).
    // #2117 P3b: lease key is (source_repo, branch); `source_canonical` is the same
    // repo path bind_full persists below, so lock/scan/bind keys agree.
    let source_repo_str = source_canonical.display().to_string();
    let _lease_lock = if bind {
        match crate::binding::acquire_branch_lease_lock(home, &source_repo_str, branch) {
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
        if let Some(other) = crate::binding::scan_existing_branch_binding(
            home,
            &source_repo_str,
            branch,
            instance_name,
        ) {
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
                    // #2115: idempotent reuse bypasses worktree::create — force-sync the stale tree to HEAD here too (rationale: sync_worktree_to_head doc).
                    crate::worktree::sync_worktree_to_head(&wt);
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
                    true, // #2158 GR1: agent self-claim (repo checkout bind:true) → notify operator
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
                // #2158 GR1 (operator-approved): a self-claimed `repo action=checkout
                // bind=true` no longer SILENTLY arms a ci_watch — neither here (this
                // inline arm is removed) NOR via the shared dispatch_hook path
                // (`bind_self` self-claims pass `arm_ci_watch=false`). The silent
                // auto-arm was part of the #2158 incident blast: a transient sub-agent
                // (sharing the primary's identity) self-claiming a worktree also armed a
                // watch the operator never asked for. The daemon DISPATCH path passes
                // `arm_ci_watch=true` and STILL arms for normal delegation. A
                // self-claiming agent that wants CI notifications arms it explicitly via
                // `ci action=watch`.
                resp["bound"] = json!(true);
                resp["ci_watch_armed"] = json!(false);
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
