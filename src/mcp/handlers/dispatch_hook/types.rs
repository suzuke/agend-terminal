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
    /// Which tier of [`super::resolve_source_repo`] fired — exposes the
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
    /// `true` when the post-bind ci-watch arm failed (F7). The primary
    /// dispatch succeeded; callers surface a degraded warning.
    pub ci_watch_arm_failed: bool,
    /// The truthful result of an attempted dispatch-time ci-watch arm. `None`
    /// means that no watch was attempted (for example `bind:false` or an
    /// unresolved repository), while `Some` preserves the normalized chain
    /// targets echoed by the typed send response.
    pub ci_watch: Option<CiWatchOutcome>,
}

/// Result of an attempted dispatch-time ci-watch arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiWatchOutcome {
    /// Whether the watch sidecar was armed successfully.
    pub armed: bool,
    /// The normalized chain targets passed to the watch arm.
    pub next_after_ci: Vec<String>,
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

/// Which tier of [`super::resolve_source_repo`] fired. Observable via
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
    /// `expected_head` failed full-SHA or commit-resolution validation.
    ValidateExpectedHead,
    /// `from_ref` rejected by `validate_branch` charset / option-injection guard.
    ValidateFromRef,
    /// `branch` rejected by `validate_branch` (charset / option-injection) or by
    /// `is_protected_ref` (E4.5) — at the validation boundary, before any git
    /// subprocess runs (CR-2026-06-14 F1).
    ValidateBranch,
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
    /// Bound worktree HEAD differed from `expected_head`.
    VerifyExpectedHead,
}

/// Canonical `code` enum — stable across releases. Callers MUST match
/// on this rather than parsing `message` substrings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// `expected_head` was not a full 40/64-hex SHA.
    InvalidExpectedHead,
    /// `expected_head` did not resolve or differed from the bound HEAD.
    ExpectedHeadMismatch,
    /// `from_ref` arg rejected by `validate_branch` charset rules.
    InvalidFromRef,
    /// `branch` arg rejected by `validate_branch` charset / option-injection
    /// rules (CR-2026-06-14 F1).
    InvalidBranch,
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
