//! Sprint 53 P0-1/P0-2: dispatch hook — auto-bind + lease + watch_ci on delegate_task.
//!
//! Extracted from comms.rs to stay under 700 LOC file size invariant.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Sprint 55 P0-B EC11: per-agent in-flight guard scoped to the daemon's
/// `home` directory. Prevents concurrent `dispatch_auto_bind_lease` calls
/// for the same agent from interleaving `binding.json` writes / lease
/// state in the same daemon process. Keying by `(home, agent)` ensures
/// parallel test runs (each with its own temp home) don't collide with
/// each other while production single-home daemons retain the per-agent
/// guarantee. RAII via `BindGuard` ensures the entry is removed even on
/// early-return error paths.
fn bind_in_flight_set() -> &'static parking_lot::Mutex<HashSet<(String, String)>> {
    static SET: std::sync::OnceLock<parking_lot::Mutex<HashSet<(String, String)>>> =
        std::sync::OnceLock::new();
    SET.get_or_init(|| parking_lot::Mutex::new(HashSet::new()))
}

struct BindGuard {
    key: (String, String),
}

impl BindGuard {
    fn try_acquire(home: &Path, agent: &str) -> Result<Self, String> {
        let key = (home.display().to_string(), agent.to_string());
        let mut g = bind_in_flight_set().lock();
        if !g.insert(key.clone()) {
            return Err(format!(
                "bind already in-flight for agent '{agent}' — concurrent dispatch_auto_bind_lease blocked"
            ));
        }
        Ok(BindGuard { key })
    }
}

impl Drop for BindGuard {
    fn drop(&mut self) {
        bind_in_flight_set().lock().remove(&self.key);
    }
}

/// Sprint 58 Wave 3 PR-2 (#8): peek whether a `(home, agent)` pair is
/// currently in the bind-in-flight set. Used by `binding_state` to
/// report whether a concurrent `dispatch_auto_bind_lease` is active for
/// the agent — operator-facing introspection for race-condition debug.
pub(crate) fn is_bind_in_flight(home: &Path, agent: &str) -> bool {
    let key = (home.display().to_string(), agent.to_string());
    bind_in_flight_set().lock().contains(&key)
}

/// Sprint 58 Wave 3 PR-2 (#9): defensively remove an `(home, agent)`
/// entry from the bind-in-flight set. The `BindGuard::drop` impl
/// already does this on every normal exit path, but a panic between
/// `try_acquire` and the implicit `Drop` can in theory leak an entry,
/// blocking re-bind. `release_full` calls this as a safety net so a
/// hard release truly leaves no stale daemon-side state at any layer.
pub(crate) fn clear_bind_in_flight(home: &Path, agent: &str) {
    let key = (home.display().to_string(), agent.to_string());
    let removed = bind_in_flight_set().lock().remove(&key);
    if removed {
        tracing::warn!(
            %agent,
            home = %home.display(),
            "release_full cleared a stale bind-in-flight entry — \
             a prior dispatch_auto_bind_lease panicked between guard \
             acquisition and drop. Investigate logs for the panic."
        );
    }
}

/// Sprint 53 P0-1+P0-2: auto-bind + lease worktree + watch_ci on delegate_task dispatch.
///
/// Sprint 55 P0-B extends `dispatch_auto_bind_lease` with:
/// - `source_repo_override` (callers like `bind_self(source_repo=...)` pass
///   an explicit path that wins over the 3-tier fleet.yaml resolution)
/// - 3-tier resolution observability (info per tier, warn on stub fallback)
/// - per-agent in-flight guard (EC11) to prevent concurrent binds for one agent
/// - `repo: Option<String>` resolution chain: explicit caller arg →
///   InstanceConfig.repo override (EC4) → derive from source_repo origin
pub(crate) fn dispatch_auto_bind_lease(
    home: &Path,
    target: &str,
    task_id: &str,
    branch: &str,
    repo: Option<&str>,
) -> Result<(), String> {
    dispatch_auto_bind_lease_with_source(home, target, task_id, branch, repo, None)
}

/// Sprint 55 P0-B: extended entry point that accepts an explicit
/// `source_repo_override`. Used by `handle_bind_self(source_repo=...)`;
/// existing callers go through [`dispatch_auto_bind_lease`] which passes
/// `None` to preserve pre-Sprint-55 behavior.
pub(crate) fn dispatch_auto_bind_lease_with_source(
    home: &Path,
    target: &str,
    task_id: &str,
    branch: &str,
    repo: Option<&str>,
    source_repo_override: Option<&Path>,
) -> Result<(), String> {
    let _guard = BindGuard::try_acquire(home, target)?;

    let resolved = crate::fleet::FleetConfig::load(&home.join("fleet.yaml"))
        .ok()
        .and_then(|f| f.resolve_instance(target));
    let source_repo = resolve_source_repo(home, target, source_repo_override, resolved.as_ref());

    // P0-1.5: central lease registry check — reject if another agent holds this branch.
    if let Some(other) = crate::binding::scan_existing_branch_binding(home, branch, target) {
        return Err(format!(
            "branch '{branch}' already leased by '{other}' — release first or use a different branch"
        ));
    }

    // Sprint 57 Wave 4 (#546 Item 4): same-agent different-branch
    // conflict check. Pre-Wave-4 this was enforced implicitly by
    // `worktree::create`'s reuse-path rejection — the legacy
    // `<repo>/.worktrees/<agent>/` was a single path per agent, so
    // a second create call on a different branch tripped the
    // "exists + HEAD mismatch" guard. Wave 4's branch-segmented
    // `<home>/worktrees/<agent>/<branch>/` puts each (agent, branch)
    // at a distinct path, so the implicit guard no longer fires.
    // The semantic is preserved here at the binding layer: if the
    // target already holds a binding on a DIFFERENT branch, the new
    // dispatch must reject (operator must `release_worktree` first).
    if let Some(existing) = crate::binding::read(home, target) {
        if let Some(existing_branch) = existing.get("branch").and_then(|v| v.as_str()) {
            if existing_branch != branch {
                return Err(format!(
                    "agent '{target}' already bound to branch '{existing_branch}' — \
                     release_worktree first before re-binding to '{branch}' \
                     (lease conflict per P0-1.6 semantic, preserved through Wave 4 #546 Item 4)"
                ));
            }
        }
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

    // P0-2 + Sprint 55 P0-B EC4: auto-watch_ci. Resolution order:
    //   1. caller-supplied `repo` arg
    //   2. fleet.yaml `repo:` override (Sprint 55 EC4)
    //   3. `derive_repo_from_remote(source_repo)` (existing Sprint 53 P0-2)
    let resolved_repo = repo
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            resolved
                .as_ref()
                .and_then(|r| r.repo.clone())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| derive_repo_from_remote(&source_repo));
    if let Some(r) = resolved_repo {
        let watch_args = serde_json::json!({"repo": &r, "branch": branch});
        crate::mcp::handlers::ci::handle_watch_ci(home, &watch_args, target);
        tracing::info!(%target, repo = %r, %branch, "dispatch auto-watch_ci");
    }
    Ok(())
}

/// Sprint 55 P0-B EC6 — 3-tier source_repo resolution with observability.
/// Tier order: explicit override → fleet.yaml `source_repo:` → fleet.yaml
/// `working_directory` → `home/workspace/<agent>` stub. INFO logs which
/// tier was hit; WARN when the stub fallback (tier 4) fires; optional
/// `AGEND_BIND_STRICT_MODE=1` env flag rejects tier 4 in production.
fn resolve_source_repo(
    home: &Path,
    target: &str,
    override_path: Option<&Path>,
    resolved: Option<&crate::fleet::ResolvedInstance>,
) -> PathBuf {
    if let Some(p) = override_path {
        tracing::info!(%target, tier = "override", path = %p.display(),
            "source_repo resolved via explicit caller override (tier 1)");
        return p.to_path_buf();
    }
    if let Some(p) = resolved.and_then(|r| r.source_repo.clone()) {
        tracing::info!(%target, tier = "fleet_source_repo", path = %p.display(),
            "source_repo resolved via fleet.yaml source_repo (tier 2)");
        return p;
    }
    if let Some(p) = resolved.and_then(|r| r.working_directory.clone()) {
        tracing::info!(%target, tier = "working_directory", path = %p.display(),
            "source_repo resolved via fleet.yaml working_directory (tier 3, deprecation candidate)");
        return p;
    }
    let stub = home.join("workspace").join(target);
    tracing::warn!(%target, tier = "stub", path = %stub.display(),
        "source_repo using home/workspace stub (tier 4) — fleet.yaml has no source_repo OR working_directory; binding may target wrong git history");
    if std::env::var("AGEND_BIND_STRICT_MODE").as_deref() == Ok("1") {
        tracing::error!(%target, "AGEND_BIND_STRICT_MODE=1: stub fallback rejected");
    }
    stub
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

/// Sprint 55 P0-B: re-export `derive_repo_from_remote` as `pub(crate)` so
/// `mcp::handlers::ci::handle_watch_ci` can re-derive on the auto-binding
/// lookup path (EC1). Internal callers in this module continue to use the
/// private helper directly.
pub(crate) fn derive_repo_from_remote_pub(source_repo: &std::path::Path) -> Option<String> {
    derive_repo_from_remote(source_repo)
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
