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
    /// Source repo resolution fell through to stub (tier 4) while
    /// `AGEND_BIND_STRICT_MODE=1`.
    ResolveSourceRepo,
    /// `bind_full` write failed after worktree was leased.
    Bind,
}

/// Canonical `code` enum — stable across releases. Callers MUST match
/// on this rather than parsing `message` substrings.
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
    /// `AGEND_BIND_STRICT_MODE=1` and source_repo resolved to stub (tier 4).
    StubRejected,
    /// `bind_full` failed — worktree was rolled back.
    BindFailed,
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
// #931 Fix 2: production callers now route through
// `dispatch_auto_bind_lease_with_chain` (comms.rs) or
// `dispatch_auto_bind_lease_with_source` (worktree.rs). This bare entry
// is kept as the canonical "no chain, no source override" convenience
// for tests (`p0b_tests.rs` + `dispatch_hook/tests.rs` together call it
// ~30 times); the cfg(test)-only callers don't get clippy-counted in
// the non-test build.
#[allow(dead_code)]
pub(crate) fn dispatch_auto_bind_lease(
    home: &Path,
    target: &str,
    task_id: &str,
    branch: &str,
    repo: Option<&str>,
) -> Result<DispatchOutcome, DispatchError> {
    dispatch_auto_bind_lease_with_source_and_chain(
        home, target, task_id, branch, repo, None, None, None,
    )
}

/// #931 Fix 2 (H5a): variant that propagates `next_after_ci` so the
/// dispatch chain knowledge (e.g. lead → dev with `next_after_ci=reviewer`)
/// lands on the auto-armed ci-watch and survives across bind cycles.
///
/// Pre-#931 the auto-watch path armed by `dispatch_auto_bind_lease` never
/// set `next_after_ci`, so `[ci-ready-for-action]` only fired when a
/// caller had explicitly called `ci action=watch ... next_after_ci=…`
/// AFTER the auto-arm. The 4-in-a-row overnight stalls
/// (#920/#925/#928/#929) were caused by this gap interacting with the
/// release-time subscriber sweep — once the auto-armed watch was the
/// sole subscription and got swept, the chain handoff vanished.
///
/// Callers: `comms.rs::handle_delegate_task` (kind=task dispatches that
/// declare a workflow chain via `args["next_after_ci"]`).
///
/// t-ci-ready-pr2-drop-derive-reviewer (operator-approved B): the #1037
/// `<team>-reviewer` name-derived `next_after_ci` auto-default
/// (`derive_team_reviewer`) was DELETED. Both call sites (this auto-watch path
/// and the `ci/mod.rs` self-claim path) leave `next_after_ci` explicit-only now,
/// so the helper became dead code. Role-based auto-handoff is a future follow-up
/// (needs role infra), not a naming convention.
pub(crate) fn dispatch_auto_bind_lease_with_chain(
    home: &Path,
    target: &str,
    task_id: &str,
    branch: &str,
    repo: Option<&str>,
    next_after_ci: Option<&str>,
    // #1877: dual-review directive ("dual" iff the dispatch set second_reviewer).
    review_class: Option<&str>,
) -> Result<DispatchOutcome, DispatchError> {
    dispatch_auto_bind_lease_with_source_and_chain(
        home,
        target,
        task_id,
        branch,
        repo,
        None,
        next_after_ci,
        review_class,
    )
}

/// Sprint 55 P0-B: extended entry point that accepts an explicit
/// `source_repo_override`. Used by `handle_bind_self(source_repo=...)`;
/// existing callers go through [`dispatch_auto_bind_lease`] which passes
/// `None` to preserve pre-Sprint-55 behavior.
///
/// **Callers**: `handle_bind_self` (post-`release_worktree` re-claim,
/// rebase recovery) + `handle_checkout_repo` with `bind:true` (#779
/// Option 1 atomic fresh provision). Both share this dispatch path —
/// caller chooses entry based on whether the caller already knows the
/// source repo (bind:true) or relies on fleet.yaml resolution (bind_self).
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
    dispatch_auto_bind_lease_with_source_and_chain(
        home,
        target,
        task_id,
        branch,
        repo,
        source_repo_override,
        None,
        None,
    )
}

/// #1750 A1: classify a `handle_watch_ci` result. The handler returns a JSON
/// `{"error":..,"code":..}` object on a failed arm and an ok-shaped object
/// otherwise; this extracts `(code, error)` when arming failed, else `None`.
/// Pure so the dispatch-time auto-watch error-surfacing is unit-testable without
/// provoking a real disk-write failure.
fn auto_watch_arm_error(result: &serde_json::Value) -> Option<(&str, &str)> {
    let err = result.get("error")?.as_str()?;
    let code = result
        .get("code")
        .and_then(|c| c.as_str())
        .unwrap_or("unknown");
    Some((code, err))
}

/// #931 Fix 2 (H5a): unified entry that accepts both `source_repo_override`
/// (Sprint 55 P0-B) and `next_after_ci` (the workflow chain target).
/// All four convenience entry points (`dispatch_auto_bind_lease`,
/// `_with_source`, `_with_chain`, this one) route through here so the
/// auto-watch arming logic has a single source of truth.
// #1877: the dispatch directives (repo/source/next_after_ci/review_class) are
// each meaningful and threaded individually; a params struct would obscure the
// call sites more than it'd help.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dispatch_auto_bind_lease_with_source_and_chain(
    home: &Path,
    target: &str,
    task_id: &str,
    branch: &str,
    repo: Option<&str>,
    source_repo_override: Option<&Path>,
    next_after_ci: Option<&str>,
    // #1877: `review_class` (e.g. "dual" from a `second_reviewer=true` dispatch)
    // must reach the auto-armed watch — else the dual-review signal accepted at
    // the MCP boundary (comms.rs) silently evaporates and CI gates single review.
    // 4th re-marshal-drop of an MCP-accepted dispatch directive.
    review_class: Option<&str>,
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
    let (source_repo, source_repo_tier) =
        resolve_source_repo(home, target, source_repo_override, resolved.as_ref());

    if source_repo_tier == SourceRepoTier::Stub
        && std::env::var("AGEND_BIND_STRICT_MODE").as_deref() == Ok("1")
    {
        return Err(DispatchError {
            message: format!(
                "AGEND_BIND_STRICT_MODE=1: stub fallback rejected for '{target}' — \
                 set source_repo in fleet.yaml"
            ),
            code: ErrorCode::StubRejected,
            stage: Stage::ResolveSourceRepo,
            fetch_attempted: false,
            raw: None,
        });
    }

    // #1882: hold a per-BRANCH lease flock across the P0-1.5 scan + the bind_full
    // below so the check-then-bind is ATOMIC. Without it, two DIFFERENT agents
    // racing to lease the SAME branch both passed the scan (neither had bound yet)
    // and both bound — violating "a branch is held by at most one agent". Keyed on
    // the branch (not the agent): the second racer blocks here, then its scan below
    // sees the first's binding and rejects. Two of the three production bind paths
    // funnel through this fn (dispatch auto-bind AND bind_self via
    // dispatch_auto_bind_lease_with_source); the third — repo checkout
    // (`ci/mod.rs`, reviewer-2 #1882) — takes the SAME `acquire_branch_lease_lock`
    // around its own scan + bind_full, so all three serialize on the one branch
    // lock. Lock order is consistent (per-agent BindGuard
    // above → this branch lock → per-agent binding lock inside bind_full), so no
    // deadlock; different branches use different lock files → no cross-branch
    // contention. Held only across the bind (a short mutex), so release is unaffected.
    let _lease_lock =
        crate::binding::acquire_branch_lease_lock(home, branch).map_err(|e| DispatchError {
            message: format!("could not acquire branch lease lock for '{branch}': {e}"),
            code: ErrorCode::LeaseConflict,
            stage: Stage::WorktreeLeaseConflict,
            fetch_attempted: false,
            raw: None,
        })?;

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

    // #781 Piece 6: ensure branch exists in `source_repo` BEFORE the
    // lease attempt. Centralizing branch provisioning at the dispatch
    // layer surfaces auto-create observability (DispatchOutcome's
    // auto_created_branch / fetch_attempted) and shares one decision
    // tree with `repo action=checkout bind:true` (the #784 entry).
    //
    // `from_ref` is hard-coded to `"origin/main"` (mirror #784 default).
    //
    // Strict error contract (#781 Phase 3 r1, Path A — restored after
    // initial fail-soft fix was found to weaken Piece 7's structured-
    // error promise): all `ensure_branch_exists` errors propagate to
    // the caller as `DispatchError` so they can programmatically
    // dispatch on `code` / `stage` instead of receiving silent
    // fallbacks that mask the original failure class. Legacy local-
    // only test fixtures must register an origin URL + populate
    // `refs/remotes/origin/main` (see `setup_test_repo` in
    // `dispatch_hook/tests.rs` and `setup_git_repo_with_remote` in
    // `p0b_tests.rs` for the canonical fixture pattern).
    let (auto_created_branch, fetch_attempted) =
        ensure_branch_exists(home, &source_repo, branch, "origin/main", target)?;

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
            fetch_attempted,
            raw: Some(msg),
        }
    })?;

    // Clean empty "init" commits left by kiro-cli session checkpoints.
    // Best-effort: failure here is non-fatal (worktree is still usable).
    // #789: existing call site preserves pre-#789 silent semantic via
    // `.ok()` so dispatch path has zero observable behavior change.
    // The Result is consumed by `bind_self` / `task action=done` /
    // `release_worktree` / `repo action=cleanup_init_commits` callers.
    let _ = clean_empty_init_commits(&lease.path).ok();

    // Bind with worktree + source-repo paths. Bind file write error stays graceful (Q1).
    // source_repo persistence (P0-X r1): release_full uses it to run
    // `git worktree remove` from the owning repo's cwd.
    // #779 P2: bind_full now returns Result; this non-target caller preserves
    // pre-#779-P2 silent semantic via `.ok()` — zero behavior change.
    match crate::binding::bind_full(home, target, task_id, branch, &lease.path, &source_repo) {
        Ok(()) => tracing::info!(
            %target, %branch, path = %lease.path.display(),
            "dispatch auto-bind + lease OK"
        ),
        Err(e) => {
            // #1310: rollback worktree on binding failure to prevent orphans
            tracing::warn!(
                %target, %branch, path = %lease.path.display(),
                error = %e,
                "dispatch auto-bind bind_full failed — rolling back worktree"
            );
            // #1899: bounded via git_bypass (LOCAL 60s) — best-effort rollback.
            let _ = crate::git_helpers::git_bypass(
                &source_repo,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    &lease.path.display().to_string(),
                ],
            );
            // #1324: surface rollback as dispatch error instead of silent success
            return Err(DispatchError {
                message: format!(
                    "bind_full failed for {target}@{branch}, worktree rolled back: {e}"
                ),
                code: ErrorCode::BindFailed,
                stage: Stage::Bind,
                fetch_attempted,
                raw: Some(e),
            });
        }
    }

    // P0-2 + Sprint 55 P0-B EC4: auto-watch_ci. Resolution order:
    //   1. caller-supplied `repo` arg → canonicalize (#942)
    //   2. fleet.yaml `repo:` override → canonicalize (#942)
    //   3. `derive_repo_from_remote(source_repo)` (existing Sprint 53 P0-2;
    //      already canonical via `parse_github_owner_repo` → `canonicalize_repo_slug`)
    let resolved_repo = repo
        .filter(|s| !s.is_empty())
        .and_then(canonicalize_repo_slug)
        .or_else(|| {
            resolved
                .as_ref()
                .and_then(|r| r.repo.clone())
                .filter(|s| !s.is_empty())
                .and_then(|s| canonicalize_repo_slug(&s))
        })
        .or_else(|| derive_repo_from_remote(&source_repo));
    if let Some(r) = resolved_repo {
        // #931 Fix 2 (H5a): when the dispatcher declared a workflow chain
        // via `next_after_ci` (e.g. lead → dev with reviewer as the next
        // step), propagate it into the auto-armed watch so the daemon's
        // poll loop fires `[ci-ready-for-action]` to the chain target on
        // CI pass. Pre-#931 callers had to issue a follow-up
        // `ci action=watch next_after_ci=…` manually — easily forgotten
        // and one of the root causes of the 4-in-a-row PR stalls.
        //
        // t-ci-ready-pr2-drop-derive-reviewer (operator-approved B): the #1037
        // `<team>-reviewer` name-derived auto-default was REMOVED — it baked the
        // `-reviewer` naming convention into the daemon and only worked for teams
        // that follow it. `next_after_ci` is now explicit-only: unset → the watch
        // arms with no chain target, and on CI pass the dev/subscribers receive
        // the informational `[ci-pass]` (PR-1 #1796); routing the actionable
        // `[ci-ready-for-action]` to a reviewer requires an EXPLICIT
        // `next_after_ci` (lead dispatches the reviewer, or passes it). Role-based
        // auto-handoff is a future follow-up (needs role infra).
        let effective_next = next_after_ci.filter(|s| !s.is_empty()).map(String::from);
        let mut watch_args = serde_json::json!({"repository": &r, "branch": branch});
        if let Some(ref next) = effective_next {
            watch_args["next_after_ci"] = serde_json::json!(next);
        }
        // #1031: persist the dispatch task_id alongside the watch so
        // the ci_check_repo emit site can back-link the
        // `[ci-ready-for-action]` event to the originating dispatch.
        // Reviewer's verdict report can then echo this task_id in
        // its correlation_id, closing the [[feedback-dispatch-task-id-mirror]]
        // false-positive class on the verdict-reply side.
        if !task_id.is_empty() {
            watch_args["task_id"] = serde_json::json!(task_id);
        }
        // #1877: propagate the dual-review directive so a `second_reviewer=true`
        // dispatch arms a `review_class=dual` watch (handle_watch_ci → ci/mod.rs
        // stores it; the poller's `record_ci_result` then enforces dual review).
        // Unset (normal dispatch) leaves the watch single — no over-upgrade.
        if let Some(rc) = review_class.filter(|s| !s.is_empty()) {
            watch_args["review_class"] = serde_json::json!(rc);
        }
        let watch_result = crate::mcp::handlers::ci::handle_watch_ci(home, &watch_args, target);
        // #1750 A1: `handle_watch_ci` returns `{"error":..,"code":..}` on a failed
        // arm (watch_write_failed / ci_watches_dir_create_failed / binding-stale).
        // Previously this Result was discarded and the success log fired
        // unconditionally — a silently-failed arm left no trace, the surface
        // behind "CI green but no watch armed, ci-ready never fires". Surface the
        // failure as an error log instead of a false success (the dispatch itself
        // still proceeds — auto-watch is best-effort convenience, not a gate).
        if let Some((code, err)) = auto_watch_arm_error(&watch_result) {
            tracing::error!(
                %target, repo = %r, %branch, code, error = %err,
                "dispatch auto-watch_ci FAILED — no CI watch armed (ci-ready will not fire)"
            );
        } else {
            tracing::info!(
                %target,
                repo = %r,
                %branch,
                next_after_ci = ?effective_next,
                explicit = next_after_ci.is_some(),
                "dispatch auto-watch_ci"
            );
        }
    }

    Ok(DispatchOutcome {
        source_repo_tier,
        auto_created_branch,
        fetch_attempted,
    })
}

/// Sprint 55 P0-B EC6 — 3-tier source_repo resolution with observability.
/// Tier order: explicit override → fleet.yaml `source_repo:` → fleet.yaml
/// `working_directory` → `home/workspace/<agent>` stub. INFO logs which
/// tier was hit; WARN when the stub fallback (tier 4) fires; optional
/// `AGEND_BIND_STRICT_MODE=1` env flag rejects tier 4 in production.
///
/// #781 Piece 6: returns the resolved path AND the [`SourceRepoTier`]
/// that fired so callers (`dispatch_auto_bind_lease_with_source`) can
/// surface tier via [`DispatchOutcome::source_repo_tier`] — operator
/// audits configuration completeness without parsing logs.
fn resolve_source_repo(
    home: &Path,
    target: &str,
    override_path: Option<&Path>,
    resolved: Option<&crate::fleet::ResolvedInstance>,
) -> (PathBuf, SourceRepoTier) {
    if let Some(p) = override_path {
        tracing::info!(%target, tier = "override", path = %p.display(),
            "source_repo resolved via explicit caller override (tier 1)");
        return (p.to_path_buf(), SourceRepoTier::Override);
    }
    if let Some(p) = resolved.and_then(|r| r.source_repo.clone()) {
        tracing::info!(%target, tier = "fleet_source_repo", path = %p.display(),
            "source_repo resolved via fleet.yaml source_repo (tier 2)");
        return (p, SourceRepoTier::FleetSourceRepo);
    }
    // Tier 2.5: team source_repo
    if let Some(p) = resolve_team_source_repo(home, target) {
        tracing::info!(%target, tier = "team_source_repo", path = %p.display(),
            "source_repo resolved via team source_repo (tier 2.5)");
        return (p, SourceRepoTier::TeamSourceRepo);
    }
    if let Some(p) = resolved.and_then(|r| r.working_directory.clone()) {
        tracing::info!(%target, tier = "working_directory", path = %p.display(),
            "source_repo resolved via fleet.yaml working_directory (tier 3, deprecation candidate)");
        return (p, SourceRepoTier::WorkingDirectory);
    }
    let stub = crate::paths::workspace_dir(home).join(target);
    tracing::warn!(%target, tier = "stub", path = %stub.display(),
        "source_repo using home/workspace stub (tier 4) — fleet.yaml has no source_repo OR working_directory; binding may target wrong git history");
    (stub, SourceRepoTier::Stub)
}

/// #781 Piece 6: shared auto-create-branch helper. Encapsulates the
/// decision tree previously inlined in
/// `mcp::handlers::ci::handle_checkout_repo` (introduced in #780). Both
/// the `repo action=checkout bind:true` MCP tool entry and
/// `dispatch_auto_bind_lease` now route through this single helper so
/// the fast path (zero network on missing-branch-with-local-origin/main)
/// and the lazy fetch fallback live in one place.
///
/// Behavior (mirror #784 / decision d-20260514102305998399-0):
/// 1. `rev-parse --verify refs/heads/<branch>` — if exists, **fetch
///    `origin <branch>` + `update-ref refs/heads/<branch>
///    refs/remotes/origin/<branch>`** so the (about-to-be-bound)
///    local ref tracks the remote PR HEAD (#869 fix — stale local
///    refs from prior cycles were landing the bound worktree at the
///    wrong SHA). Returns `(auto_created=false, fetch_attempted=N)`
///    where N reflects whether the fetch actually succeeded.
///    `update-ref` is no-op when `origin/<branch>` doesn't exist
///    (newly-pushed branches not yet observed locally) or when the
///    fetch fails (network outage); in both cases the local ref is
///    left unchanged and the caller still gets a usable lease.
/// 2. Else `git branch <branch> <from_ref>`:
///    - success → `(true, false)`
///    - stderr `"already exists"` (concurrent race) → `(false, false)`
///    - stderr `"not a valid object name"` / `"not a valid ref"` →
///      `git fetch origin --quiet` then retry; success → `(true, true)`;
///      retry race "already exists" → `(false, true)`; otherwise
///      structured error.
///
/// `from_ref` runs through `validate_branch` (defense in depth) to
/// reject option-injection (`--upload-pack=...`) at the daemon API
/// boundary — same rule applied to the user-supplied `branch` arg.
///
/// `actor` is the agent / instance name used as `event_log` identifier
/// for the fetch-duration breadcrumb (helps post-mortem who triggered
/// the network I/O).
pub(crate) fn ensure_branch_exists(
    home: &Path,
    source: &Path,
    branch: &str,
    from_ref: &str,
    actor: &str,
) -> Result<(bool, bool), DispatchError> {
    if !crate::agent_ops::validate_branch(from_ref) {
        return Err(DispatchError {
            message: format!("invalid from_ref '{from_ref}'"),
            code: ErrorCode::InvalidFromRef,
            stage: Stage::ValidateFromRef,
            fetch_attempted: false,
            raw: None,
        });
    }
    let branch_ref = format!("refs/heads/{branch}");
    let branch_exists =
        crate::git_helpers::git_bypass(source, &["rev-parse", "--verify", &branch_ref])
            .map(|o| o.status.success())
            .unwrap_or(false);
    if branch_exists {
        // #869: the local ref may be stale from a prior dispatch cycle
        // (r0 push → reviewer bind → r1 push observed this 3× across
        // PR-B/PR-C/etc; local ref pinned at the r0 SHA, downstream
        // `worktree::create` then lands the bound worktree at that
        // stale SHA instead of the remote PR HEAD).
        //
        // Refresh `origin/<branch>` + fast-forward the local ref via
        // `update-ref` BEFORE the lease so `worktree::create` reads
        // the now-current ref. `update-ref` is an atomic ref write
        // (no working-tree mutation, no checkout side-effects), safe
        // even when the branch is checked out elsewhere — about to
        // be replaced by this very lease anyway.
        //
        // Best-effort: `fetch` failure (network outage / fake-remote
        // fixture) leaves `origin/<branch>` at its prior value; the
        // update-ref still runs against whatever `refs/remotes/origin/
        // <branch>` is present (defensible because at-least-as-fresh-as-
        // local is the invariant we want). If `origin/<branch>` doesn't
        // exist at all (branch never pushed), update-ref is skipped
        // and the local ref is left untouched — dispatch then falls
        // through to lease with the existing local SHA, matching the
        // pre-fix behaviour for that edge case.
        let fetch_out = crate::git_helpers::git_bypass_timeout(
            source,
            &["fetch", "origin", branch, "--quiet"],
            crate::git_helpers::NETWORK_GIT_TIMEOUT,
        );
        let fetched_ok = matches!(&fetch_out, Ok(o) if o.status.success());
        let remote_branch_ref = format!("refs/remotes/origin/{branch}");
        let remote_exists =
            crate::git_helpers::git_bypass(source, &["rev-parse", "--verify", &remote_branch_ref])
                .map(|o| o.status.success())
                .unwrap_or(false);
        if remote_exists {
            let _ = crate::git_helpers::git_bypass(
                source,
                &["update-ref", &branch_ref, &remote_branch_ref],
            );
        }
        return Ok((false, fetched_ok));
    }
    // Step 2: create from `from_ref`. #1755: a remote-tracking `from_ref` like
    // `origin/main` ALWAYS resolves as a local ref, so a bare `git branch` here
    // silently bases the new branch on a STALE local ref (whatever was last
    // fetched) — the reverse-revert hazard where a fresh checkout starts behind
    // main and would clobber merges that landed since. Refresh the remote ref
    // FIRST (mirrors the #869 branch-EXISTS path above) so the create lands on
    // current `origin/<x>`. Best-effort: a fetch failure (offline / no-remote
    // fixture) leaves the local ref as-is and the create still succeeds against
    // whatever's present (degraded but functional, same contract as #869).
    // `fetch_attempted` reports SUCCESS (matches #869's `fetched_ok`), so the
    // no-remote test fixtures keep reporting `false`.
    let mut create_fetched = false;
    if let Some(remote_branch) = from_ref.strip_prefix("origin/") {
        let fetch_start = std::time::Instant::now();
        let fetch_out = crate::git_helpers::git_bypass_timeout(
            source,
            &["fetch", "origin", remote_branch, "--quiet"],
            crate::git_helpers::NETWORK_GIT_TIMEOUT,
        );
        create_fetched = matches!(&fetch_out, Ok(o) if o.status.success());
        crate::event_log::log(
            home,
            "ensure_branch_fetch",
            actor,
            &format!(
                "branch={branch} from_ref={from_ref} duration_ms={} ok={create_fetched} (#1755 pre-create refresh)",
                fetch_start.elapsed().as_millis()
            ),
        );
    }
    match crate::git_helpers::git_bypass(source, &["branch", branch, from_ref]) {
        Ok(o) if o.status.success() => Ok((true, create_fetched)),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).to_string();
            if stderr.contains("already exists") {
                // Race: concurrent caller authored the branch between
                // rev-parse and branch. Idempotent fall-through —
                // auto_created stays false so callers can distinguish
                // "I created it" vs "I observed it pre-existing".
                Ok((false, false))
            } else if stderr.contains("not a valid object name")
                || stderr.contains("not a valid ref")
            {
                tracing::warn!(
                    target: "dispatch_hook",
                    %branch,
                    %from_ref,
                    "ensure_branch_exists fallback: from_ref unresolved locally — fetching origin"
                );
                let fetch_start = std::time::Instant::now();
                let fetch_out = crate::git_helpers::git_bypass_timeout(
                    source,
                    &["fetch", "origin", "--quiet"],
                    crate::git_helpers::NETWORK_GIT_TIMEOUT,
                );
                let fetch_ms = fetch_start.elapsed().as_millis();
                crate::event_log::log(
                    home,
                    "ensure_branch_fetch",
                    actor,
                    &format!("branch={branch} from_ref={from_ref} duration_ms={fetch_ms}"),
                );
                match fetch_out {
                    Ok(fo) if fo.status.success() => {
                        match crate::git_helpers::git_bypass(source, &["branch", branch, from_ref])
                        {
                            Ok(ro) if ro.status.success() => Ok((true, true)),
                            Ok(ro) => {
                                let rstderr = String::from_utf8_lossy(&ro.stderr).to_string();
                                if rstderr.contains("already exists") {
                                    Ok((false, true))
                                } else {
                                    tracing::warn!(
                                        target: "dispatch_hook",
                                        %branch, %from_ref, stderr = %rstderr,
                                        "ensure_branch_exists retry failed after fetch"
                                    );
                                    Err(DispatchError {
                                        message: format!(
                                            "from_ref '{from_ref}' invalid (branch creation failed after fetch)"
                                        ),
                                        code: ErrorCode::InvalidFromRef,
                                        stage: Stage::RetryCreate,
                                        fetch_attempted: true,
                                        raw: Some(rstderr),
                                    })
                                }
                            }
                            Err(e) => Err(DispatchError {
                                message: format!("git branch retry spawn failed: {e}"),
                                code: ErrorCode::BranchCreateFailed,
                                stage: Stage::RetryCreate,
                                fetch_attempted: true,
                                raw: Some(e.to_string()),
                            }),
                        }
                    }
                    Ok(fo) => {
                        let fstderr = String::from_utf8_lossy(&fo.stderr).to_string();
                        tracing::warn!(
                            target: "dispatch_hook",
                            %branch, %from_ref, stderr = %fstderr,
                            "ensure_branch_exists fetch failed"
                        );
                        Err(DispatchError {
                            message: format!(
                                "git fetch origin failed (from_ref '{from_ref}' cannot be resolved)"
                            ),
                            code: ErrorCode::FetchFailed,
                            stage: Stage::Fetch,
                            fetch_attempted: true,
                            raw: Some(fstderr),
                        })
                    }
                    Err(e) => Err(DispatchError {
                        message: format!("git fetch spawn failed: {e}"),
                        code: ErrorCode::FetchFailed,
                        stage: Stage::Fetch,
                        fetch_attempted: true,
                        raw: Some(e.to_string()),
                    }),
                }
            } else {
                Err(DispatchError {
                    message: format!("git branch failed: {}", stderr.trim()),
                    code: ErrorCode::BranchCreateFailed,
                    stage: Stage::CreateBranch,
                    fetch_attempted: false,
                    raw: Some(stderr),
                })
            }
        }
        Err(e) => Err(DispatchError {
            message: format!("git branch spawn failed: {e}"),
            code: ErrorCode::BranchCreateFailed,
            stage: Stage::CreateBranch,
            fetch_attempted: false,
            raw: Some(e.to_string()),
        }),
    }
}

/// Resolve source_repo from the agent's team configuration.
///
/// #781 Piece 5 (defensive logging): the prior `.ok()?` swallowed
/// FleetConfig::load errors silently — a malformed fleet.yaml or
/// transient I/O fault dropped Tier 2.5 to None with zero diagnostics,
/// making post-mortem investigation harder. The defensive branches
/// below surface the actual error class (load failure vs no team
/// match vs team match without `source_repo`) so operators can
/// distinguish "Bug A0 legacy-migration case" (team matched but
/// source_repo None) from "team membership setup gap".
pub(crate) fn resolve_team_source_repo(home: &Path, agent: &str) -> Option<PathBuf> {
    let fleet = match crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                %agent,
                home = %home.display(),
                error = %e,
                "Tier 2.5 resolution skipped: fleet.yaml load failed — \
                 dispatch will fall through to tier 3 (working_directory) or \
                 tier 4 (workspace stub)"
            );
            return None;
        }
    };
    for cfg in fleet.teams.values() {
        if cfg.members.contains(&agent.to_string()) {
            if cfg.source_repo.is_none() {
                tracing::warn!(
                    %agent,
                    "Tier 2.5 team match but `source_repo` is None — \
                     likely legacy migration from teams.json (Bug A0, see #781). \
                     Operator must run `team update name=<team> source_repo=<canonical>` \
                     to escape the workspace stub fallback at tier 4"
                );
            }
            return cfg.source_repo.clone();
        }
    }
    tracing::debug!(
        %agent,
        teams_searched = fleet.teams.len(),
        "Tier 2.5: no team membership found for agent — falling through to tier 3/4"
    );
    None
}

/// Parse `owner/repo` from a `git remote get-url origin` output.
///
/// Accepts the three formats GitHub commonly serves:
/// - `https://github.com/owner/repo(.git)`
/// - `http://github.com/owner/repo(.git)`
/// - `git@github.com:owner/repo(.git)`
///
/// #942 canonical form for GitHub `owner/repo` identity. Accepts both
/// full URL forms AND bare slugs; collapses the 7 known divergence forms
/// (`.git` suffix, casing, whitespace, full HTTPS URL, SSH URL, trailing
/// slash, HTTP vs HTTPS) to a single canonical `owner/repo` lowercase
/// string. GitHub itself treats repo identifiers case-insensitively for
/// routing, so lowercasing here matches server semantics.
///
/// Single source of truth for repo identity used by:
/// - `handle_watch_ci` (canonicalize on entry before hash)
/// - `dispatch_auto_bind_lease` (tiers 1+2 caller-supplied + fleet.yaml)
/// - `parse_github_owner_repo` (re-uses via delegate)
/// - `migrate_legacy_watch_filenames` (#942/#943 boot migration)
///
/// Returns `None` for inputs that cannot be canonicalized:
/// - Non-GitHub remotes (e.g. `https://gitlab.com/...`)
/// - Malformed slugs (single component, too many components)
/// - Empty input after trim
///
/// **Behavior shift from pre-#942 `parse_github_owner_repo`**: pre-fix
/// accepted only URL forms (returned `None` for bare slugs like
/// `owner/repo`). Post-fix accepts both. The only in-tree caller of
/// the pre-fix shape is `derive_repo_from_remote` which always passes
/// the output of `git remote get-url` (URL form); the lenient shift
/// is theoretical for that path. Documented in #942 PR body.
pub(crate) fn canonicalize_repo_slug(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let stripped = s
        .strip_prefix("https://github.com/")
        .or_else(|| s.strip_prefix("http://github.com/"))
        .or_else(|| s.strip_prefix("git@github.com:"))
        .or_else(|| s.strip_prefix("ssh://git@github.com/"))
        .unwrap_or(s);
    let slug = stripped.trim_end_matches('/').trim_end_matches(".git");
    let mut parts = slug.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some() || owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!(
        "{}/{}",
        owner.to_ascii_lowercase(),
        name.to_ascii_lowercase()
    ))
}

/// Returns `None` for non-GitHub remotes — `watch_ci` only knows how to poll
/// GitHub Actions, so silently skipping non-GitHub repos is the right behavior
/// (the alternative would be writing a stale watch entry the poller can't act on).
///
/// Pre-#942 this was a separate stricter implementation; post-#942 it
/// delegates to [`canonicalize_repo_slug`] so derived-from-remote URLs
/// and operator-supplied slugs converge on identical canonical form.
fn parse_github_owner_repo(url: &str) -> Option<String> {
    canonicalize_repo_slug(url)
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
    let output =
        crate::git_helpers::git_bypass(source_repo, &["remote", "get-url", "origin"]).ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?;
    parse_github_owner_repo(&url)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;

/// #1787: run an IDEMPOTENT git subprocess with bounded retry on transient
/// failure. The #1784 windows-CI hang fix let the full suite run for the first
/// time and surfaced intermittent transient failures of these dispatch-hook git
/// calls — e.g. `git log origin/main..HEAD` exiting non-zero with empty stderr
/// (#1783) under windows scratch-repo lock contention — which a retry clears.
///
/// ONLY for idempotent commands (log / diff-tree / reset-to-a-fixed-ref): a
/// non-idempotent op (`rebase -i`) must NOT route through here and stays a direct
/// call. Returns the FINAL attempt's output (or the spawn error), so callers keep
/// their existing success/stderr handling unchanged. `AGEND_GIT_BYPASS=1` is
/// pinned exactly as the direct calls did. (Wall-clock timeout for the hang class
/// is the #1787 Phase-4 daemon-git audit; here the confirmed flake is a transient
/// non-zero exit, and test-level hangs are caught by the #1785 nextest guard.)
fn run_git_idempotent(args: &[&str], cwd: &Path) -> std::io::Result<std::process::Output> {
    const MAX_ATTEMPTS: u32 = 3;
    const BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);
    let mut last: Option<std::process::Output> = None;
    for attempt in 1..=MAX_ATTEMPTS {
        // #1897: bounded — these are LOCAL ops (log / diff / reset), so a stuck
        // git (contended index.lock) returns Err(TimedOut) instead of hanging the
        // daemon. A timeout is NOT a transient non-zero exit, so it `?`-propagates
        // immediately (no point retrying a wedge); the retry-on-failure loop below
        // is preserved for genuine transient git failures (#1787).
        let out = crate::git_helpers::git_bypass_timeout(
            cwd,
            args,
            crate::git_helpers::LOCAL_GIT_TIMEOUT,
        )?;
        if out.status.success() {
            return Ok(out);
        }
        tracing::debug!(
            target: "dispatch_hook",
            attempt,
            args = ?args,
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "#1787: transient git failure, retrying idempotent git command"
        );
        last = Some(out);
        if attempt < MAX_ATTEMPTS {
            std::thread::sleep(BACKOFF);
        }
    }
    Ok(last.expect("loop runs at least once"))
}

/// Remove empty commits with message "init" between origin/main and HEAD.
/// These come from BACKEND session checkpoints (claude-code / kiro-cli)
/// that fire heartbeats every ~90s; not from agend-terminal production
/// code (worktree.rs uses message "init (agend-terminal)" which the
/// strict `subject == "init"` filter correctly skips).
///
/// #789 — returned `Result<usize, String>`:
/// - `Ok(count)` — number of empty init commits removed (0 = noop)
/// - `Err(msg)` — git subprocess failure with human-readable error
///
/// Caller chooses semantics:
/// - Existing `dispatch_auto_bind_lease` site preserves the pre-#789
///   silent semantic via `let _ = ...ok();` (zero observable change to
///   dispatch path, per #779 P2 convention).
/// - `bind_self` / `task action=done` / `release_worktree` / new MCP
///   `repo action=cleanup_init_commits` consume the Result so operator-
///   facing surfaces can report the cleaned count + surface failures.
pub(crate) fn clean_empty_init_commits(worktree: &Path) -> Result<usize, String> {
    // #814: auto-recover from prior failed-cleanup state. The
    // `.git/.../rebase-merge` dir survives when a previous
    // `git rebase -i` failed AND its companion `git rebase --abort`
    // also failed (or was skipped). Subsequent `git rebase -i`
    // refuses to start with "previous rebase in progress", returning
    // exit code 256 — exactly the failure mode that hit #807 prep
    // 3 consecutive times. Pre-clear the stale dir so retry can
    // proceed. Best-effort: a remove failure here doesn't abort the
    // helper — worst case we get the same status 256 we had before.
    clear_stale_rebase_state(worktree);

    // #1787: retry — the confirmed #1783 windows flake was this command exiting
    // non-zero with empty stderr under scratch-repo lock contention.
    let output = run_git_idempotent(&["log", "origin/main..HEAD", "--format=%H %s"], worktree);
    let output = match output {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            return Err(format!(
                "git log origin/main..HEAD failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
        Err(e) => return Err(format!("git log spawn failed: {e}")),
    };
    let log = String::from_utf8_lossy(&output.stdout);
    if log.trim().is_empty() {
        return Ok(0);
    }

    // Identify empty heartbeat commits.
    //
    // #822 C1 scaffolding: introduces `HEARTBEAT_NAMES` whitelist +
    // `is_heartbeat_subject` + `commit_body_is_empty` API surface but
    // keeps the whitelist at the production-equivalent value
    // (`["init"]`). The body-emptiness gate stub returns `true` so the
    // diff-tree gate immediately below remains the load-bearing
    // emptiness check. C2 fills in the synonym (`"initial"`) and
    // wires up the real body-gate impl.
    let mut empty_inits: Vec<&str> = Vec::new();
    for line in log.lines() {
        let (hash, msg) = match line.split_once(' ') {
            Some(pair) => pair,
            None => continue,
        };
        if !is_heartbeat_subject(msg) {
            continue;
        }
        if !commit_body_is_empty(worktree, hash) {
            continue;
        }
        // Check if commit has no file changes.
        let diff = run_git_idempotent(
            &["diff-tree", "--no-commit-id", "--name-only", "-r", hash],
            worktree,
        );
        if let Ok(d) = diff {
            if d.status.success() && d.stdout.trim_ascii().is_empty() {
                empty_inits.push(hash);
            }
        }
    }

    if empty_inits.is_empty() {
        return Ok(0);
    }

    // All commits between origin/main..HEAD are empty inits → soft reset.
    let total_commits = log.lines().count();
    if empty_inits.len() == total_commits {
        // #1787: retry — soft-reset to a fixed ref is idempotent.
        let status = run_git_idempotent(&["reset", "--soft", "origin/main"], worktree);
        match status {
            Ok(o) if o.status.success() => {
                tracing::info!(
                    count = total_commits,
                    "cleaned all empty init commits via soft reset"
                );
                return Ok(total_commits);
            }
            Ok(o) => {
                tracing::warn!("failed to soft-reset empty init commits");
                return Err(format!(
                    "git reset --soft origin/main exited with status {:?}",
                    o.status
                ));
            }
            Err(e) => {
                tracing::warn!("failed to soft-reset empty init commits");
                return Err(format!("git reset spawn failed: {e}"));
            }
        }
    }

    // #814: high-count diagnostic. Emit a tracing warn when the
    // empty-init count exceeds the threshold so post-incident
    // analysis can identify "slow rebase" cases. KISS hardcoded
    // constant — the warning is a "slow op may follow" signal,
    // not a hard cap. Operators seeing this regularly should
    // investigate upstream (why are session-checkpoint heartbeats
    // accumulating over 30 inits before the next push?).
    if empty_inits.len() > INIT_COUNT_WARN_THRESHOLD {
        tracing::warn!(
            count = empty_inits.len(),
            threshold = INIT_COUNT_WARN_THRESHOLD,
            "#814: high empty-init count — cleanup may be slow"
        );
    }

    // Mixed: use interactive rebase to drop empty inits.
    // Build a sed script that changes "pick <hash>" to "drop <hash>" for each empty init.
    // Use `sed -i.bak` for cross-platform compat (macOS requires suffix, Linux accepts it).
    let sed_parts: Vec<String> = empty_inits
        .iter()
        .map(|h| format!("s/^pick {short} /drop {short} /", short = &h[..7]))
        .collect();
    let sed_script = sed_parts.join(";");
    let cleaned = empty_inits.len();
    // #1787 audit: NOT routed through `run_git_idempotent` — `rebase -i` is not
    // idempotent (a partial/interrupted rebase leaves a rebase-merge dir that a
    // blind retry would trip over). `clear_stale_rebase_state` above pre-clears
    // that state, and the `Err` arm below already surfaces a failed abort.
    let status = std::process::Command::new("git")
        .args(["-c", "core.abbrev=7", "rebase", "-i", "origin/main"])
        .current_dir(worktree)
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_SEQUENCE_EDITOR", format!("sed -i.bak '{sed_script}'"))
        .status();
    match status {
        Ok(s) if s.success() => {
            tracing::info!(count = cleaned, "cleaned empty init commits via rebase");
            Ok(cleaned)
        }
        Ok(_) | Err(_) => {
            // Abort failed rebase to leave worktree in clean state.
            // #814: capture the abort status + warn on failure. Pre-fix
            // the abort was silently swallowed via `let _ = ...`, which
            // hid the very signal that becomes the next call's stale-
            // state issue. With this surfaced, post-incident audit can
            // confirm whether abort itself failed (the upstream cause
            // of the rebase-merge dir persisting across attempts).
            let abort = std::process::Command::new("git")
                .args(["rebase", "--abort"])
                .current_dir(worktree)
                .env("AGEND_GIT_BYPASS", "1")
                .status();
            match &abort {
                Ok(s) if !s.success() => {
                    tracing::warn!(
                        abort_status = ?s,
                        "#814: git rebase --abort failed — rebase-merge dir may persist; \
                         next clean_empty_init_commits call auto-clears via clear_stale_rebase_state"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "#814: git rebase --abort spawn failed — rebase-merge dir may persist"
                    );
                }
                _ => {}
            }
            tracing::warn!("failed to rebase-drop empty init commits");
            Err(match status {
                Ok(s) => format!("git rebase -i exited with status {s:?}"),
                Err(e) => format!("git rebase spawn failed: {e}"),
            })
        }
    }
}

/// #814: threshold for the high-init-count tracing warn. Empty-init
/// counts above this signal a slow `git rebase -i` ahead and an
/// upstream issue worth investigating (why are heartbeats
/// accumulating?). Hardcoded — not a hard cap, so config-ability
/// adds no operator value. Matches the observed #807 incident
/// count (32 inits > 30 threshold → warns).
const INIT_COUNT_WARN_THRESHOLD: usize = 30;

/// #822: subjects accepted as "heartbeat commit" candidates. The
/// daemon-side heartbeat producers (worktree.rs / bootstrap/
/// agent_resolve.rs) hardcode `"init"`, but a separate code path
/// or external session checkpoint can land an `"initial"` commit
/// (the #820 stray `9f619c2 initial` was the observed offender).
///
/// Empirical census across all 65 heartbeat commits in repo history
/// at the time of #822: 64 × `init` + 1 × `initial`. Zero `wip`,
/// `tmp`, `temp`, `checkpoint`, `draft` occurrences — those are
/// deferred to v1.5 if/when observed.
///
/// Hardcoded (KISS); case-sensitive exact match. Body-emptiness
/// gate (`commit_body_is_empty`) guards against the rare-but-real
/// case where a user has an empty-diff commit named `init` or
/// `initial` with real commit-body notes (e.g. an intentional
/// `--allow-empty` marker commit). That case is theoretical for
/// today's daemon producers (they never set a body) but the gate
/// is forward-compatible insurance against expanding the whitelist
/// in v1.5.
///
/// v1 value: `["init", "initial"]` — covers the two empirically
/// observed offenders. `wip`/`tmp`/`temp`/`checkpoint`/`draft` are
/// deferred to v1.5 if/when observed in real heartbeat traffic.
const HEARTBEAT_NAMES: &[&str] = &["init", "initial"];

/// #822: exact-match check against [`HEARTBEAT_NAMES`].
///
/// Lives as a function (not inline `.contains`) so the call site at
/// the match loop reads at the abstraction level of the contract
/// (`is_heartbeat_subject`) rather than the implementation
/// (`HEARTBEAT_NAMES.contains(&msg)`).
fn is_heartbeat_subject(msg: &str) -> bool {
    HEARTBEAT_NAMES.contains(&msg)
}

/// #833: trailer keys the daemon's `prepare-commit-msg` hook injects
/// into every bound-worktree commit (including the `commit
/// --allow-empty -m "init"` heartbeats). The body-emptiness gate
/// (`commit_body_is_empty`) must treat these as "empty for the gate's
/// purpose" so `cleanup_init_commits` can actually remove the
/// daemon-injected heartbeats — pre-#833 the gate kept them
/// indefinitely because the trailer block isn't whitespace.
///
/// Sourced verbatim from `assets/hooks/prepare-commit-msg` lines 44-47
/// (and the matching `.ps1` Windows variant). Exact-key match —
/// partial-key trailers like `Agend-Agent-Token` are NOT stripped,
/// preserving operator-extended trailer keys.
///
/// New daemon-injected trailers must extend this list. A follow-up
/// invariant test (lead's post-batch backlog) will grep the hook
/// script and pin the constants in sync.
const KNOWN_TRAILER_KEYS: &[&str] = &[
    "Agend-Agent",
    "Agend-Task",
    "Agend-Branch",
    "Agend-Issued-At",
];

/// #833: strip lines that look like `<KEY>: <value>` where KEY is in
/// [`KNOWN_TRAILER_KEYS`]. Leading whitespace tolerated (the hook
/// emits unindented but `.trim_start()` is defensive). The
/// `starts_with(k) && trimmed[k.len()..].starts_with(':')` two-step
/// ensures `Agend-Agent` matches only `Agend-Agent:`, NOT
/// `Agend-Agent-Token:` (partial-prefix regression-proof).
///
fn strip_known_trailers(body: &str) -> String {
    body.lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !KNOWN_TRAILER_KEYS
                .iter()
                .any(|k| trimmed.starts_with(k) && trimmed[k.len()..].starts_with(':'))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// #822: returns true iff the commit's body (the part after the
/// subject line) is empty or whitespace-only. Guards the whitelist
/// from dropping commits with legitimate body notes that happen to
/// share a heartbeat subject.
///
/// Reads the commit's body via `git log -1 --format=%b <hash>` and
/// trims. `%b` yields the body only (no subject, no trailing
/// newline-pad — git always emits a trailing newline that `.trim()`
/// strips).
///
/// Fail-soft: any git-log error returns `true` (treat as empty body)
/// rather than `false`. This preserves the existing "drop if diff is
/// empty" behavior when body-detection fails — neutral fallback that
/// does not block legitimate cleanups on transient git failures.
fn commit_body_is_empty(worktree: &Path, hash: &str) -> bool {
    let out = run_git_idempotent(&["log", "-1", "--format=%b", hash], worktree);
    match out {
        Ok(o) if o.status.success() => {
            // #833: strip daemon-injected trailers before the empty
            // check. The hook injects `Agend-*:` trailers into every
            // bound-worktree commit, so a heartbeat's "body" is never
            // literally empty post-hook — but it IS empty in the
            // operator-content sense.
            let body = String::from_utf8_lossy(&o.stdout);
            strip_known_trailers(&body).trim().is_empty()
        }
        _ => true,
    }
}

/// #814: clear `.git/.../rebase-merge` AND `rebase-apply` dirs that
/// survived a prior failed cleanup attempt. Called at the top of
/// `clean_empty_init_commits` so the next `git rebase -i` doesn't
/// trip over "previous rebase in progress" state inherited from a
/// previous run's failed `--abort`.
///
/// Safety: the daemon-managed worktree this helper operates on is
/// not shared with operator-driven rebases (operator runs from
/// canonical checkout or their own clones), so clearing the rebase
/// state here cannot clobber an operator's in-progress work. The
/// `tracing::warn!` documents the clear so post-incident audit can
/// confirm it fired and correlate with the prior failed call.
///
/// Fail-soft: any error (missing .git pointer, permission, etc.) is
/// logged but doesn't abort the helper. Worst case the subsequent
/// `git rebase -i` returns the same status 256 the operator saw
/// before — no regression beyond pre-#814 behavior.
fn clear_stale_rebase_state(worktree: &Path) {
    let git_dir = match resolve_worktree_gitdir(worktree) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "#814: could not resolve .git dir; skipping stale rebase-state clear"
            );
            return;
        }
    };
    for sub in ["rebase-merge", "rebase-apply"] {
        let path = git_dir.join(sub);
        if !path.exists() {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                tracing::warn!(
                    ?path,
                    "#814: removed stale {} dir from prior failed cleanup attempt",
                    sub
                );
            }
            Err(e) => {
                tracing::warn!(
                    ?path,
                    error = %e,
                    "#814: failed to clear stale {} dir — cleanup may still fail",
                    sub
                );
            }
        }
    }
}

/// #814: resolve the worktree's actual `.git` directory.
///
/// In a primary checkout `.git` is a directory. In a daemon-managed
/// `git worktree`-provisioned worktree, `.git` is a FILE containing
/// a `gitdir: <path>` line pointing at
/// `<repo>/.git/worktrees/<name>`. This helper handles both forms.
fn resolve_worktree_gitdir(worktree: &Path) -> Result<std::path::PathBuf, String> {
    let dotgit = worktree.join(".git");
    if dotgit.is_dir() {
        return Ok(dotgit);
    }
    if !dotgit.is_file() {
        return Err(format!(
            ".git missing at {}; not a directory or file",
            dotgit.display()
        ));
    }
    let content = std::fs::read_to_string(&dotgit)
        .map_err(|e| format!("read .git file at {}: {e}", dotgit.display()))?;
    let gitdir = content
        .lines()
        .find_map(|l| l.strip_prefix("gitdir: "))
        .ok_or_else(|| format!(".git file at {} missing 'gitdir:' prefix", dotgit.display()))?
        .trim();
    Ok(std::path::PathBuf::from(gitdir))
}
