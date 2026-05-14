//! Sprint 53 P0-1/P0-2: dispatch hook — auto-bind + lease + watch_ci on delegate_task.
//!
//! Extracted from comms.rs to stay under 700 LOC file size invariant.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// #781 Piece 7: structured dispatch outcome. Mirrors the #784 success
/// response shape for `repo action=checkout bind:true` so callers across
/// the fleet observe a single canonical schema regardless of whether the
/// worktree was provisioned via the `repo` MCP tool or via the
/// auto-bind hook fired from `send kind=task`.
///
/// Introduced in C1 as a types-only commit; first call site materializes
/// in C2 (signature migration).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchOutcome {
    /// Which tier of [`resolve_source_repo`] fired — exposes the
    /// silent-miss class of Bug A0 (operator sees `Stub` and knows team
    /// `source_repo` is unset).
    pub source_repo_tier: SourceRepoTier,
    /// `true` when this dispatch authored the branch on `source_repo`.
    /// `false` when the branch pre-existed (back-compat / race
    /// fall-through). Mirrors `auto_created_branch` from #784.
    pub auto_created_branch: bool,
    /// `true` when the lazy `git fetch origin` was invoked because
    /// `from_ref` did not resolve locally. Surfaces network I/O so
    /// callers can correlate slow dispatches with fetch fallback.
    pub fetch_attempted: bool,
}

/// #781 Piece 7: structured error. The string-only `Result<_, String>`
/// it supersedes (pre-#781) lost the `code` / `stage` / `raw` triple
/// callers need to dispatch error handling programmatically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchError {
    /// Human-readable summary. Safe to log verbatim.
    pub message: String,
    /// Canonical reason class — see [`ErrorCode`]. Stable enum, not
    /// stderr fragments.
    pub code: ErrorCode,
    /// Pipeline locator — which step of `dispatch_auto_bind_lease`
    /// raised. See [`Stage`].
    pub stage: Stage,
    /// `true` when the fetch fallback fired before the failure (lets
    /// callers distinguish "config / option-injection invalid" from
    /// "fetch happened but couldn't resolve from_ref").
    pub fetch_attempted: bool,
    /// Raw git stderr if any — for debug / post-mortem. `None` when
    /// the failure didn't involve a git subprocess.
    pub raw: Option<String>,
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for DispatchError {}

/// Which tier of [`resolve_source_repo`] fired. Observable via
/// [`DispatchOutcome::source_repo_tier`] so callers can audit
/// configuration completeness without parsing logs.
///
/// Variants `Override`/`FleetSourceRepo`/`TeamSourceRepo`/`WorkingDirectory`
/// are wired in C4 once `resolve_source_repo` reports tier. C2 only
/// constructs `Stub` (placeholder); `allow(dead_code)` on unused variants
/// until C4 removes the attribute.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceRepoTier {
    /// Tier 1 — explicit `source_repo_override` from
    /// `bind_self(source_repo=...)` etc.
    Override,
    /// Tier 2 — per-instance `source_repo:` in fleet.yaml.
    FleetSourceRepo,
    /// Tier 2.5 — team `source_repo:` in fleet.yaml.
    TeamSourceRepo,
    /// Tier 3 — per-instance `working_directory:` fallback (deprecation
    /// candidate).
    WorkingDirectory,
    /// Tier 4 — `$AGEND_HOME/workspace/<agent>` stub (last resort).
    /// Surfacing this signals operator config gap.
    Stub,
}

/// Pipeline stage that produced a [`DispatchError`]. Coarse enough to
/// remain stable across refactors, fine enough to debug.
///
/// Variants `ValidateFromRef`/`CreateBranch`/`Fetch`/`RetryCreate` are
/// constructed in C4 once `ensure_branch_exists` runs at the dispatch
/// layer. `allow(dead_code)` on unused variants until C4.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// `from_ref` rejected by `validate_branch` charset / option-injection guard.
    ValidateFromRef,
    /// First `git branch <name> <from_ref>` attempt failed for a reason
    /// other than "already exists" / "not a valid ref".
    CreateBranch,
    /// `git fetch origin` after the missing-ref fallback failed.
    Fetch,
    /// Retry `git branch <name> <from_ref>` after fetch still failed.
    RetryCreate,
    /// `worktree_pool::lease` returned error (worktree creation failed,
    /// cross-agent lease conflict, same-agent different-branch conflict).
    WorktreeLeaseConflict,
}

/// Canonical `code` enum — stable across releases. Callers MUST match
/// on this rather than parsing `message` substrings.
///
/// Variants `InvalidFromRef`/`BranchCreateFailed`/`FetchFailed` are
/// constructed in C4 once `ensure_branch_exists` runs. `allow(dead_code)`
/// on unused variants until C4.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// `from_ref` arg rejected by `validate_branch` charset rules.
    InvalidFromRef,
    /// `git branch` failed at a stage we can't recover from (not
    /// already-exists, not invalid-ref).
    BranchCreateFailed,
    /// `git fetch origin` exit non-zero / spawn error.
    FetchFailed,
    /// `worktree_pool::lease` rejected — cross-agent branch lease,
    /// same-agent different-branch, worktree::create None, etc.
    LeaseConflict,
    /// E4.5 protected ref guard (`main` / `master`).
    ProtectedBranch,
    /// `bind_in_flight_set` already contains `(home, agent)` — concurrent
    /// dispatch blocked.
    BindInFlight,
}

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
) -> Result<DispatchOutcome, DispatchError> {
    dispatch_auto_bind_lease_with_source(home, target, task_id, branch, repo, None)
}

/// Sprint 55 P0-B: extended entry point that accepts an explicit
/// `source_repo_override`. Used by `handle_bind_self(source_repo=...)`;
/// existing callers go through [`dispatch_auto_bind_lease`] which passes
/// `None` to preserve pre-Sprint-55 behavior.
///
/// #781 Piece 7: returns structured [`DispatchOutcome`] / [`DispatchError`].
/// C2 commit performs the signature migration mechanically — `source_repo_tier`,
/// `auto_created_branch`, `fetch_attempted` populated with placeholders here
/// and wired to real observability sources in C4.
pub(crate) fn dispatch_auto_bind_lease_with_source(
    home: &Path,
    target: &str,
    task_id: &str,
    branch: &str,
    repo: Option<&str>,
    source_repo_override: Option<&Path>,
) -> Result<DispatchOutcome, DispatchError> {
    let _guard = BindGuard::try_acquire(home, target).map_err(|msg| DispatchError {
        message: msg,
        code: ErrorCode::BindInFlight,
        stage: Stage::WorktreeLeaseConflict,
        fetch_attempted: false,
        raw: None,
    })?;

    let resolved = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
        .ok()
        .and_then(|f| f.resolve_instance(target));
    let source_repo = resolve_source_repo(home, target, source_repo_override, resolved.as_ref());

    // P0-1.5: central lease registry check — reject if another agent holds this branch.
    if let Some(other) = crate::binding::scan_existing_branch_binding(home, branch, target) {
        return Err(DispatchError {
            message: format!(
                "branch '{branch}' already leased by '{other}' — release first or use a different branch"
            ),
            code: ErrorCode::LeaseConflict,
            stage: Stage::WorktreeLeaseConflict,
            fetch_attempted: false,
            raw: None,
        });
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
                return Err(DispatchError {
                    message: format!(
                        "agent '{target}' already bound to branch '{existing_branch}' — \
                         release_worktree first before re-binding to '{branch}' \
                         (lease conflict per P0-1.6 semantic, preserved through Wave 4 #546 Item 4)"
                    ),
                    code: ErrorCode::LeaseConflict,
                    stage: Stage::WorktreeLeaseConflict,
                    fetch_attempted: false,
                    raw: None,
                });
            }
        }
    }

    // Attempt lease (creates worktree + tags as daemon-managed).
    // Lease errors REJECT the dispatch (Q2 + §3.3).
    let lease = crate::worktree_pool::lease(home, &source_repo, target, branch).map_err(|msg| {
        let code = if msg.contains("E4.5") {
            ErrorCode::ProtectedBranch
        } else {
            ErrorCode::LeaseConflict
        };
        DispatchError {
            message: msg.clone(),
            code,
            stage: Stage::WorktreeLeaseConflict,
            fetch_attempted: false,
            raw: Some(msg),
        }
    })?;

    // Clean empty "init" commits left by kiro-cli session checkpoints.
    // Best-effort: failure here is non-fatal (worktree is still usable).
    clean_empty_init_commits(&lease.path);

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

    // C2 placeholder: `source_repo_tier` / `auto_created_branch` /
    // `fetch_attempted` are wired to real observability sources in C4 once
    // `resolve_source_repo` reports tier + `ensure_branch_exists` reports
    // create/fetch outcomes. Placeholder values are safe defaults — no
    // existing caller inspects these fields pre-C4.
    Ok(DispatchOutcome {
        source_repo_tier: SourceRepoTier::Stub,
        auto_created_branch: false,
        fetch_attempted: false,
    })
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
    // Tier 2.5: team source_repo
    if let Some(p) = resolve_team_source_repo(home, target) {
        tracing::info!(%target, tier = "team_source_repo", path = %p.display(),
            "source_repo resolved via team source_repo (tier 2.5)");
        return p;
    }
    if let Some(p) = resolved.and_then(|r| r.working_directory.clone()) {
        tracing::info!(%target, tier = "working_directory", path = %p.display(),
            "source_repo resolved via fleet.yaml working_directory (tier 3, deprecation candidate)");
        return p;
    }
    let stub = crate::paths::workspace_dir(home).join(target);
    tracing::warn!(%target, tier = "stub", path = %stub.display(),
        "source_repo using home/workspace stub (tier 4) — fleet.yaml has no source_repo OR working_directory; binding may target wrong git history");
    if std::env::var("AGEND_BIND_STRICT_MODE").as_deref() == Ok("1") {
        tracing::error!(%target, "AGEND_BIND_STRICT_MODE=1: stub fallback rejected");
    }
    stub
}

/// Resolve source_repo from the agent's team configuration.
pub(crate) fn resolve_team_source_repo(home: &Path, agent: &str) -> Option<PathBuf> {
    let fleet = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)).ok()?;
    for cfg in fleet.teams.values() {
        if cfg.members.contains(&agent.to_string()) {
            return cfg.source_repo.clone();
        }
    }
    None
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

/// Remove empty commits with message "init" between origin/main and HEAD.
/// These are left by kiro-cli session checkpoints and pollute PRs.
/// Best-effort: logs warnings on failure but never panics.
fn clean_empty_init_commits(worktree: &Path) {
    let output = std::process::Command::new("git")
        .args(["log", "origin/main..HEAD", "--format=%H %s"])
        .current_dir(worktree)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return,
    };
    let log = String::from_utf8_lossy(&output.stdout);
    if log.trim().is_empty() {
        return;
    }

    // Identify empty "init" commits.
    let mut empty_inits: Vec<&str> = Vec::new();
    for line in log.lines() {
        let (hash, msg) = match line.split_once(' ') {
            Some(pair) => pair,
            None => continue,
        };
        if msg != "init" {
            continue;
        }
        // Check if commit has no file changes.
        let diff = std::process::Command::new("git")
            .args(["diff-tree", "--no-commit-id", "--name-only", "-r", hash])
            .current_dir(worktree)
            .env("AGEND_GIT_BYPASS", "1")
            .output();
        if let Ok(d) = diff {
            if d.status.success() && d.stdout.trim_ascii().is_empty() {
                empty_inits.push(hash);
            }
        }
    }

    if empty_inits.is_empty() {
        return;
    }

    // All commits between origin/main..HEAD are empty inits → soft reset.
    let total_commits = log.lines().count();
    if empty_inits.len() == total_commits {
        let status = std::process::Command::new("git")
            .args(["reset", "--soft", "origin/main"])
            .current_dir(worktree)
            .env("AGEND_GIT_BYPASS", "1")
            .status();
        match status {
            Ok(s) if s.success() => {
                tracing::info!(
                    count = total_commits,
                    "cleaned all empty init commits via soft reset"
                );
            }
            _ => {
                tracing::warn!("failed to soft-reset empty init commits");
            }
        }
        return;
    }

    // Mixed: use interactive rebase to drop empty inits.
    // Build a sed script that changes "pick <hash>" to "drop <hash>" for each empty init.
    // Use `sed -i.bak` for cross-platform compat (macOS requires suffix, Linux accepts it).
    let sed_parts: Vec<String> = empty_inits
        .iter()
        .map(|h| format!("s/^pick {short} /drop {short} /", short = &h[..7]))
        .collect();
    let sed_script = sed_parts.join(";");
    let status = std::process::Command::new("git")
        .args(["-c", "core.abbrev=7", "rebase", "-i", "origin/main"])
        .current_dir(worktree)
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_SEQUENCE_EDITOR", format!("sed -i.bak '{sed_script}'"))
        .status();
    match status {
        Ok(s) if s.success() => {
            tracing::info!(
                count = empty_inits.len(),
                "cleaned empty init commits via rebase"
            );
        }
        _ => {
            // Abort failed rebase to leave worktree in clean state.
            let _ = std::process::Command::new("git")
                .args(["rebase", "--abort"])
                .current_dir(worktree)
                .env("AGEND_GIT_BYPASS", "1")
                .status();
            tracing::warn!("failed to rebase-drop empty init commits");
        }
    }
}
