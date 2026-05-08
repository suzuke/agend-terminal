//! Sprint 53 P0-1/P0-2: dispatch hook — auto-bind + lease + watch_ci on delegate_task.
//!
//! Extracted from comms.rs to stay under 700 LOC file size invariant.

/// Sprint 53 P0-1+P0-2: auto-bind + lease worktree + watch_ci on delegate_task dispatch.
///
/// Production call path: app::run_app / daemon::run → MCP tool call →
/// handle_send → is_task_kind → dispatch_auto_bind_lease.
///
/// Failure recovery per operator Q1+Q2+§3.3:
/// - Bind file write error: log warn, dispatch proceeds (Q1 graceful)
/// - Lease conflict: REJECT dispatch with explicit error (Q2)
/// - Lease creation fails: REJECT dispatch with explicit error (§3.3)
/// - Main branch rejected: REJECT dispatch (E4.5)
/// - watch_ci derive/write failure: log warn, dispatch still OK (P0-2 graceful)
///
/// P0-2 consolidation: `repo` is taken from caller-supplied arg first, else
/// derived from `git remote get-url origin` of source_repo. This replaces the
/// post-SEND Hotfix C #451 block in comms.rs (deleted as dead code) and gives
/// agent-to-agent `send` the same auto-watch coverage as operator dispatches.
pub(crate) fn dispatch_auto_bind_lease(
    home: &std::path::Path,
    target: &str,
    task_id: &str,
    branch: &str,
    repo: Option<&str>,
) -> Result<(), String> {
    // Sprint 54 P1-B Bug 2 fix Option A: resolve source repo from the
    // target agent's `source_repo` field first (decoupled per-agent
    // canonical source), falling back to `working_directory` for
    // backward compatibility with fleet.yaml that predates the field,
    // and finally to the `home/workspace/<agent>` stub for instances
    // that haven't been resolved at all. The fallback chain preserves
    // pre-fix behaviour for agents whose fleet.yaml hasn't been
    // hand-edited to opt in.
    let resolved = crate::fleet::FleetConfig::load(&home.join("fleet.yaml"))
        .ok()
        .and_then(|f| f.resolve_instance(target));
    let source_repo = resolved
        .as_ref()
        .and_then(|r| r.source_repo.clone())
        .or_else(|| resolved.as_ref().and_then(|r| r.working_directory.clone()))
        .unwrap_or_else(|| home.join("workspace").join(target));

    // P0-1.5: central lease registry check — reject if another agent holds this branch.
    if let Some(other) = crate::binding::scan_existing_branch_binding(home, branch, target) {
        return Err(format!(
            "branch '{branch}' already leased by '{other}' — release first or use a different branch"
        ));
    }

    // Attempt lease (creates worktree + tags as daemon-managed).
    // Lease errors REJECT the dispatch (Q2 + §3.3).
    let lease = crate::worktree_pool::lease(home, &source_repo, target, branch)?;

    // Bind with worktree + source-repo paths. Bind file write error stays graceful (Q1).
    // source_repo persistence (P0-X r1): release_full uses it to run
    // `git worktree remove` from the owning repo's cwd.
    crate::binding::bind_full(home, target, task_id, branch, &lease.path, &source_repo);
    tracing::info!(
        %target, %branch, path = %lease.path.display(),
        "dispatch auto-bind + lease OK"
    );

    // P0-2: auto-watch_ci. Caller arg wins; else derive from source_repo's origin.
    // Sprint 54 P0-1: handle_watch_ci is now idempotent + append-aware
    // (preserves prior poll state, adds caller to `subscribers` only if
    // not already present). Drop the prior `.exists()` skip — it caused
    // the second agent dispatched onto the same branch to never get
    // subscribed when a watch file already existed.
    // Graceful: any failure (no remote, non-GitHub remote, write error) is logged but does not
    // reject dispatch — auto-watch is a notification convenience, not load-bearing.
    let resolved_repo = repo
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| derive_repo_from_remote(&source_repo));
    if let Some(r) = resolved_repo {
        let watch_args = serde_json::json!({"repo": &r, "branch": branch});
        crate::mcp::handlers::ci::handle_watch_ci(home, &watch_args, target);
        tracing::info!(%target, repo = %r, %branch, "dispatch auto-watch_ci");
    }
    Ok(())
}

/// Parse `owner/repo` from a `git remote get-url origin` output.
///
/// Accepts the three formats GitHub commonly serves:
/// - `https://github.com/owner/repo(.git)`
/// - `http://github.com/owner/repo(.git)`
/// - `git@github.com:owner/repo(.git)`
///
/// Returns `None` for non-GitHub remotes — `watch_ci` only knows how to poll
/// GitHub Actions, so silently skipping non-GitHub repos is the right behavior
/// (the alternative would be writing a stale watch entry the poller can't act on).
fn parse_github_owner_repo(url: &str) -> Option<String> {
    let url = url.trim();
    let stripped = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let slug = stripped.trim_end_matches('/').trim_end_matches(".git");
    // Sanity: must look like `owner/repo` — exactly one '/' and both parts non-empty.
    let mut parts = slug.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some() || owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("{owner}/{name}"))
}

/// Run `git remote get-url origin` in `source_repo` and parse the GitHub slug.
///
/// Used as the dispatch-time fallback when the caller doesn't pass `repo`
/// explicitly. Returns `None` if the repo has no `origin` remote, the remote
/// isn't GitHub, or the git command fails.
fn derive_repo_from_remote(source_repo: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(source_repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?;
    parse_github_owner_repo(&url)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
