//! #972: daemon-side PR-state aggregator.
//!
//! Joins two signals that previously lived in disjoint daemon-side
//! state stores (`ci_watch` and `auto_release`) into a single
//! per-PR state machine. When `(CI green ∧ N×VERIFIED)` converge at
//! the same `head_sha`, the daemon emits `[pr-ready-for-merge]` to
//! the PR author's inbox — eliminating the lead-kick deadlock that
//! operator caught twice on 2026-05-20 (Incident A — primary→cross
//! handoff; Incident B — CI+VERIFIED→self-merge handoff).
//!
//! ## Storage layout
//!
//! Per-PR file at `<home>/pr-state/<repo-slug>-<pr_number>.json`.
//! `<repo-slug>` is the canonical [`dispatch_hook::canonicalize_repo_slug`]
//! form with `/` replaced by `_`. Per-file (not single index) so
//! independent PR updates don't contend on one mutex.
//!
//! Writes go through [`crate::store::atomic_write`] which has the
//! post-#965 unique-tmp-filename safety (concurrent multi-PR updates
//! cannot corrupt each other).
//!
//! ## Ingestion
//!
//! Two entry points, both fire from existing daemon code:
//!
//! - [`record_ci_result`] — called from
//!   `src/daemon/ci_watch/poller.rs` right after the existing
//!   `[ci-ready-for-action]` emission. Records `CiState::Green` or
//!   `CiState::Failed { conclusion }` against the observed head SHA.
//! - [`record_verdict`] — called from
//!   `src/api/handlers/messaging.rs` right after
//!   `auto_release::enqueue_intent`. Records the verdict variant
//!   (Verified / Rejected / Unverified) with reviewer + reviewed_head.
//!
//! Both call [`scan_and_emit_for_pr`] internally to recompute derived
//! state and fire any newly-eligible events.
//!
//! ## §4.2 stale-head invariant (LOAD-BEARING)
//!
//! [`is_merge_ready`] requires `head_sha == ci_sha == every reviewer's
//! reviewed_head`. Without this invariant the aggregator would emit
//! `[pr-ready-for-merge]` for an unreviewed commit after a
//! push-after-VERIFIED → self-merge of unreviewed code = critical
//! regression. Pinned by reducer tests `repro_972_*_sha_mismatch_*`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub mod auto_arm;
pub mod gh_poll;
mod remote_gc;
mod scanner;
pub use scanner::scan_and_emit;
#[cfg(test)]
pub use scanner::scan_and_emit_with;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrState {
    pub repo: String,
    pub pr_number: u64,
    pub branch: String,
    /// Current PR HEAD SHA. Updated when ci_watch observes a head advance.
    pub head_sha: String,
    /// Explicit PR author — populated at first observation via
    /// `gh pr view --json author,headRefName`. Falls through to
    /// subscribers[0] → task assignee → "lead" if the lookup fails.
    /// Reviewer cross-audit PRIMARY: this MUST be set, not derived
    /// from subscriber order alone — subscribers mutate during long
    /// PR lifecycles.
    pub pr_author: String,
    /// ci_watch's subscriber list at the time of first observation.
    /// Used only as fallback for [`pr_author`] resolution.
    #[serde(default)]
    pub subscribers: Vec<String>,
    pub ci_state: CiState,
    pub verdict_state: VerdictState,
    pub merge_state: MergeState,
    pub draft_state: DraftState,
    pub review_class: ReviewClass,
    /// Debounce key for `[pr-ready-for-merge]` — when ready_emitted_for_sha
    /// equals current head_sha, the event was already fired and won't
    /// re-emit until head_sha changes.
    #[serde(default)]
    pub ready_emitted_for_sha: Option<String>,
    /// #973 cross-audit Pushback C: tracks whether implementer armed
    /// `gh pr merge --auto` against the current head. Cleared on
    /// head_sha advance (force-push cancels GitHub's auto-merge).
    #[serde(default)]
    pub auto_armed: bool,
    #[serde(default)]
    pub auto_armed_for_sha: Option<String>,
    #[serde(default)]
    pub auto_armed_at: Option<String>,
    /// #986: last successful gh-poll observation (RFC3339). `None`
    /// pre-first-poll. Drives tiered cadence — when `auto_armed=true`
    /// we re-poll every 15s; otherwise every 60s.
    #[serde(default)]
    pub last_gh_poll_at: Option<String>,
    /// #986: consecutive failed gh-poll attempts (rate-limit / CLI
    /// absent / network error). Drives exponential backoff:
    /// `2^failures × tick` capped at 300s. Cleared on first success.
    #[serde(default)]
    pub gh_poll_failures: u32,
    /// #986: snapshot of the last gh-poll response for diff detection.
    /// Drives transition observation (state transitions / isDraft
    /// toggle / mergedAt landing). `None` pre-first-poll.
    #[serde(default)]
    pub last_gh_state: Option<gh_poll::GhPrMetadata>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CiState {
    Pending,
    Green {
        sha: String,
        observed_at: String,
    },
    Failed {
        sha: String,
        observed_at: String,
        conclusion: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum VerdictState {
    /// No verdict signal yet.
    None,
    /// Reviewer dispatched (we infer this from `next_after_ci` being
    /// armed); no report received yet.
    Pending,
    /// One or more reviewers reported VERIFIED. For §3.5 dual-review
    /// the threshold is 2; for §3.6 single it's 1.
    Verified {
        /// (reviewer_agent_name, reviewed_head_sha)
        reviewers: Vec<(String, String)>,
    },
    Rejected {
        reviewer: String,
        reviewed_head: String,
        reason: Option<String>,
    },
    Unverified {
        reviewer: String,
        reviewed_head: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MergeState {
    NotReady,
    MergeReady,
    Merged {
        merge_commit: String,
        merged_at: String,
    },
    /// PR was closed without merging (close-without-merge).
    ClosedUnmerged {
        closed_at: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DraftState {
    /// PR is a regular (non-draft) PR.
    Ready,
    /// PR is in draft mode — `gh pr merge` will refuse. We emit a
    /// note in [`pr-ready-for-merge`] body and the merge cannot fire.
    Draft,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReviewClass {
    /// §3.6 — single VERIFIED triggers MergeReady.
    Single,
    /// §3.5 — two VERIFIED required.
    Dual,
}

impl ReviewClass {
    pub fn required_verified_count(&self) -> usize {
        match self {
            ReviewClass::Single => 1,
            ReviewClass::Dual => 2,
        }
    }
}

// ─── reducer ───────────────────────────────────────────────────────────

/// One ingestion event for the reducer. Pure data — no side effects.
/// Both production ingestion entry points ([`record_ci_result`] and
/// [`record_verdict`]) translate their inputs into one of these.
///
#[derive(Debug, Clone)]
pub enum Event<'a> {
    /// CI's poll observed a head transition + conclusion.
    CiObserved {
        head_sha: &'a str,
        conclusion: CiConclusion<'a>,
        observed_at: String,
    },
    /// `kind=report` arrived with VERIFIED / REJECTED / UNVERIFIED.
    VerdictObserved {
        reviewer: &'a str,
        reviewed_head: &'a str,
        kind: VerdictKind<'a>,
    },
    /// `gh pr view` reported draft transition.
    DraftTransition { is_draft: bool },
    /// `gh pr view` reported merged.
    MergedObserved {
        merge_commit: &'a str,
        merged_at: String,
    },
    /// `gh pr view` reported closed-unmerged.
    ClosedUnmergedObserved { closed_at: String },
}

#[derive(Debug, Clone, Copy)]
pub enum CiConclusion<'a> {
    Pending,
    Green,
    Failed { conclusion: &'a str },
}

#[derive(Debug, Clone, Copy)]
pub enum VerdictKind<'a> {
    Verified,
    Rejected { reason: Option<&'a str> },
    Unverified,
}

/// Pure reducer — applies an event to a PrState. Returns the new
/// state. Side effects (event emission, file write) live in the
/// callers; the reducer is pure so the test matrix can drive it with
/// synthetic events without touching disk.
///
/// Invariants enforced here (LOAD-BEARING):
/// - On `CiObserved` with new head_sha: head_sha advances, any
///   accumulated verdicts for the old sha are cleared (back to Pending
///   if reviewer was working, None otherwise), `auto_armed` clears,
///   `ready_emitted_for_sha` clears.
/// - On `VerdictObserved` with stale `reviewed_head` (≠ current
///   head_sha): record verdict against its observed sha but the
///   §4.2 staleness check in [`is_merge_ready`] will refuse to
///   transition merge_state.
/// - On `DraftTransition(true)`: merge_state stays NotReady regardless
///   of ci/verdict.
/// - On `MergedObserved` / `ClosedUnmergedObserved`: terminal state.
pub fn apply(state: &mut PrState, event: Event<'_>) {
    state.updated_at = chrono::Utc::now().to_rfc3339();
    match event {
        Event::CiObserved {
            head_sha,
            conclusion,
            observed_at,
        } => {
            // Head advance invalidates prior verdicts (§4.2) + clears
            // auto_armed + clears ready_emitted_for_sha.
            if state.head_sha != head_sha {
                state.head_sha = head_sha.to_string();
                state.auto_armed = false;
                state.auto_armed_for_sha = None;
                state.ready_emitted_for_sha = None;
                // Drop verdicts whose reviewed_head no longer matches.
                // Verified gets dropped per-reviewer; Rejected/Unverified
                // collapse to None since they were about an old commit.
                state.verdict_state = match &state.verdict_state {
                    VerdictState::Verified { reviewers } => {
                        let kept: Vec<(String, String)> = reviewers
                            .iter()
                            .filter(|(_, sha)| sha == head_sha)
                            .cloned()
                            .collect();
                        if kept.is_empty() {
                            VerdictState::Pending
                        } else {
                            VerdictState::Verified { reviewers: kept }
                        }
                    }
                    VerdictState::Rejected { reviewed_head, .. }
                    | VerdictState::Unverified { reviewed_head, .. }
                        if reviewed_head != head_sha =>
                    {
                        VerdictState::None
                    }
                    other => other.clone(),
                };
            }
            state.ci_state = match conclusion {
                CiConclusion::Pending => CiState::Pending,
                CiConclusion::Green => CiState::Green {
                    sha: head_sha.to_string(),
                    observed_at,
                },
                CiConclusion::Failed { conclusion } => CiState::Failed {
                    sha: head_sha.to_string(),
                    observed_at,
                    conclusion: conclusion.to_string(),
                },
            };
        }
        Event::VerdictObserved {
            reviewer,
            reviewed_head,
            kind,
        } => {
            match kind {
                VerdictKind::Verified => {
                    let new_entry = (reviewer.to_string(), reviewed_head.to_string());
                    state.verdict_state =
                        match std::mem::replace(&mut state.verdict_state, VerdictState::None) {
                            VerdictState::Verified { mut reviewers } => {
                                // Replace existing entry from same reviewer
                                // (the reviewer may have re-reviewed at a
                                // different sha). Otherwise append.
                                if let Some(idx) = reviewers.iter().position(|(r, _)| r == reviewer)
                                {
                                    reviewers[idx] = new_entry;
                                } else {
                                    reviewers.push(new_entry);
                                }
                                VerdictState::Verified { reviewers }
                            }
                            _ => VerdictState::Verified {
                                reviewers: vec![new_entry],
                            },
                        };
                }
                VerdictKind::Rejected { reason } => {
                    state.verdict_state = VerdictState::Rejected {
                        reviewer: reviewer.to_string(),
                        reviewed_head: reviewed_head.to_string(),
                        reason: reason.map(String::from),
                    };
                }
                VerdictKind::Unverified => {
                    state.verdict_state = VerdictState::Unverified {
                        reviewer: reviewer.to_string(),
                        reviewed_head: reviewed_head.to_string(),
                    };
                }
            }
        }
        Event::DraftTransition { is_draft } => {
            state.draft_state = if is_draft {
                DraftState::Draft
            } else {
                DraftState::Ready
            };
        }
        Event::MergedObserved {
            merge_commit,
            merged_at,
        } => {
            state.merge_state = MergeState::Merged {
                merge_commit: merge_commit.to_string(),
                merged_at,
            };
        }
        Event::ClosedUnmergedObserved { closed_at } => {
            state.merge_state = MergeState::ClosedUnmerged { closed_at };
        }
    }
    // Recompute derived merge_state. Terminal states (Merged /
    // ClosedUnmerged) are sticky — never re-derived from CI/verdict.
    if !matches!(
        state.merge_state,
        MergeState::Merged { .. } | MergeState::ClosedUnmerged { .. }
    ) {
        state.merge_state = if is_merge_ready(state) {
            MergeState::MergeReady
        } else {
            MergeState::NotReady
        };
    }
}

/// §4.2 stale-head invariant — CI's green SHA AND every reviewer's
/// reviewed_head MUST equal the current PR head_sha. If `head_sha`
/// advanced after VERIFIED, the verdict is stale; refuse to fire
/// merge-ready.
///
/// Also gates on:
/// - Draft state — `gh pr merge` rejects drafts; refuse to mark ready
/// - Threshold per `review_class` (Single=1 / Dual=2)
pub fn is_merge_ready(state: &PrState) -> bool {
    if matches!(state.draft_state, DraftState::Draft) {
        return false;
    }
    let CiState::Green { sha: ci_sha, .. } = &state.ci_state else {
        return false;
    };
    if ci_sha != &state.head_sha {
        return false;
    }
    let VerdictState::Verified { reviewers } = &state.verdict_state else {
        return false;
    };
    if reviewers.len() < state.review_class.required_verified_count() {
        return false;
    }
    reviewers
        .iter()
        .all(|(_, reviewed)| reviewed == &state.head_sha)
}

// ─── storage ───────────────────────────────────────────────────────────

/// Canonical path to the PR-state directory.
pub fn pr_state_dir(home: &Path) -> PathBuf {
    home.join("pr-state")
}

/// Canonical filename for a (repo, branch) PR state file. Keyed by
/// branch (not pr_number) because ci_watch — the primary writer —
/// knows the branch but not the PR number; `pr_number` is filled in
/// via gh-poll on first observation. GitHub enforces one open PR per
/// (head_ref, base_ref) combo, so (repo, branch) is unique for an
/// open PR within a session. `/` in repo and branch is replaced by
/// `_` so the filename stays single-component.
pub fn pr_state_filename(repo: &str, branch: &str) -> String {
    let repo_slug = repo.replace('/', "_");
    let branch_slug = branch.replace('/', "_");
    format!("{repo_slug}__{branch_slug}.json")
}

/// Load a PR state from disk (returns `None` if file missing or
/// malformed — never panics).
pub fn load(home: &Path, repo: &str, branch: &str) -> Option<PrState> {
    let path = pr_state_dir(home).join(pr_state_filename(repo, branch));
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// PR-3 (t-ci-ready-pr3-arm-not-armed): is the branch a KNOWN-open PR per the
/// last gh-poll observation? Used by the ci-watch age-cap GC to exempt open PRs
/// (an open PR should keep notifying on CI; aging its watch out would only let
/// the auto-arm re-create it next poll — churn). Conservative: an untracked /
/// never-polled branch returns `false` (ages out normally).
pub fn is_branch_open(home: &Path, repo: &str, branch: &str) -> bool {
    load(home, repo, branch)
        .and_then(|s| s.last_gh_state)
        .map(|m| matches!(m.state, gh_poll::GhPrState::Open))
        .unwrap_or(false)
}

/// Atomic save — used by tests for setup. Production mutation paths
/// go through [`with_pr_state`] which serializes under flock.
#[cfg_attr(not(test), allow(dead_code))]
pub fn save(home: &Path, state: &PrState) -> anyhow::Result<()> {
    let path = pr_state_dir(home).join(pr_state_filename(&state.repo, &state.branch));
    let body = serde_json::to_string_pretty(state)?;
    crate::store::atomic_write(&path, body.as_bytes())?;
    Ok(())
}

/// #1342: flock-serialized read-modify-write for pr_state files.
/// All mutation paths MUST go through this helper to prevent lost-update
/// races (e.g. gh-poll save overwriting scanner's `ready_emitted_for_sha`).
/// The closure receives a fresh `&mut PrState` loaded under an exclusive
/// lock; save happens automatically after the closure returns.
/// Returns `Ok(None)` when the file does not exist (closure not called).
pub fn with_pr_state<F, R>(
    home: &std::path::Path,
    repo: &str,
    branch: &str,
    mutate: F,
) -> anyhow::Result<Option<R>>
where
    F: FnOnce(&mut PrState) -> R,
{
    let dir = pr_state_dir(home);
    std::fs::create_dir_all(&dir)?;
    let data_path = dir.join(pr_state_filename(repo, branch));
    crate::store::with_json_state(&data_path, mutate)
}

pub fn with_pr_state_or_create<F, D, R>(
    home: &std::path::Path,
    repo: &str,
    branch: &str,
    default_fn: D,
    mutate: F,
) -> anyhow::Result<R>
where
    D: FnOnce() -> PrState,
    F: FnOnce(&mut PrState) -> R,
{
    let dir = pr_state_dir(home);
    std::fs::create_dir_all(&dir)?;
    let data_path = dir.join(pr_state_filename(repo, branch));
    crate::store::with_json_state_or_create(&data_path, default_fn, mutate)
}

/// Remove the per-PR file. Used by the per-tick scanner after a
/// terminal state (Merged / ClosedUnmerged) is observed and the
/// `[pr-merged]` / `[pr-closed-unmerged]` events have been emitted.
pub fn remove(home: &Path, repo: &str, branch: &str) -> std::io::Result<()> {
    let dir = pr_state_dir(home);
    let filename = pr_state_filename(repo, branch);
    let path = dir.join(&filename);
    let lock_path = dir.join(format!("{filename}.lock"));
    let _ = std::fs::remove_file(&lock_path);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Read the `AGEND_PR_STATE_REPLAY_AGE_HOURS` env var (default 1) and
/// return as a `Duration`. Tunable upper bound for "stale terminal
/// state" classification at daemon boot — see
/// [`suppress_stale_terminal_replay`].
fn replay_age_threshold() -> std::time::Duration {
    const DEFAULT_HOURS: u64 = 1;
    let hours = std::env::var("AGEND_PR_STATE_REPLAY_AGE_HOURS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_HOURS);
    std::time::Duration::from_secs(hours.saturating_mul(3600))
}

/// #1017: at daemon boot, mark terminal-state pr-state files whose
/// mtime is older than `AGEND_PR_STATE_REPLAY_AGE_HOURS` (default 1h)
/// as already-emitted. Without this, a fresh daemon process replays
/// the [pr-merged] / [pr-closed-unmerged] events for every stale
/// Merged / ClosedUnmerged file on the first `scan_and_emit_with`
/// tick — operator gets a flood of "PR merged" inbox events for
/// merges that happened many hours / days ago.
///
/// Mechanism: load each file. If `merge_state in {Merged,
/// ClosedUnmerged}` AND file mtime is older than the threshold,
/// set `ready_emitted_for_sha = Some(head_sha)` and save. The
/// terminal-state branch of [`scan_and_emit_with`] checks this
/// gate and skips the event emit while still removing the file.
///
/// Idempotent: re-running over a tree where the gate is already set
/// is a no-op. Fresh merges (mtime within the threshold) are left
/// untouched so the legitimate post-restart event still fires.
pub fn suppress_stale_terminal_replay(home: &Path) {
    suppress_stale_terminal_replay_with(home, replay_age_threshold());
}

/// Inner implementation of [`suppress_stale_terminal_replay`] that
/// takes an explicit threshold. Used by tests to bypass the env-var
/// reader + run deterministically without needing a `filetime` /
/// `utimensat` mtime-mutator dev-dependency. Production callers use
/// the public wrapper above.
pub fn suppress_stale_terminal_replay_with(home: &Path, threshold: std::time::Duration) {
    let dir = pr_state_dir(home);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(
                dir = %dir.display(),
                error = %e,
                "#1017 pr_state: suppress_stale_terminal_replay read_dir failed — skipping"
            );
            return;
        }
    };
    let mut suppressed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        let Ok(age) = mtime.elapsed() else { continue };
        if age < threshold {
            continue; // fresh — let scan_and_emit fire the event
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(state) = serde_json::from_str::<PrState>(&content) else {
            continue;
        };
        if !matches!(
            state.merge_state,
            MergeState::Merged { .. } | MergeState::ClosedUnmerged { .. }
        ) {
            continue;
        }
        if state.ready_emitted_for_sha.as_deref() == Some(state.head_sha.as_str()) {
            continue;
        }
        let repo = state.repo.clone();
        let branch = state.branch.clone();
        match with_pr_state(home, &repo, &branch, |s| {
            if s.ready_emitted_for_sha.as_deref() == Some(s.head_sha.as_str()) {
                return false;
            }
            s.ready_emitted_for_sha = Some(s.head_sha.clone());
            true
        }) {
            Ok(Some(true)) => {
                suppressed += 1;
                tracing::debug!(
                    repo = %repo,
                    branch = %branch,
                    age_hours = age.as_secs() / 3600,
                    "#1017 pr_state: stale terminal replay suppressed"
                );
            }
            Err(e) => {
                tracing::warn!(
                    repo = %repo,
                    branch = %branch,
                    error = %e,
                    "#1017 pr_state: suppress_stale save failed"
                );
            }
            _ => {}
        }
    }
    if suppressed > 0 {
        tracing::info!(
            count = suppressed,
            threshold_hours = threshold.as_secs() / 3600,
            "#1017 pr_state: suppressed stale terminal replays at boot"
        );
    }
}

// ─── ingestion + emission ──────────────────────────────────────────────

/// Build a fresh PrState for a newly-observed (repo, branch) pair.
/// `pr_number` and `pr_author` default to placeholders; the per-tick
/// scanner fills them in via `gh pr view` on next pass.
///
/// `review_class` is the §3.5 / §3.6 threshold; sourced from the
/// ci-watch file (operator-set via `ci action=watch review_class=…`).
/// Default `ReviewClass::Single` when the watch file omits the field.
pub fn new_for_branch(
    repo: &str,
    branch: &str,
    head_sha: &str,
    review_class: ReviewClass,
) -> PrState {
    let now = chrono::Utc::now().to_rfc3339();
    PrState {
        repo: repo.to_string(),
        pr_number: 0,
        branch: branch.to_string(),
        head_sha: head_sha.to_string(),
        pr_author: String::new(),
        subscribers: Vec::new(),
        ci_state: CiState::Pending,
        verdict_state: VerdictState::None,
        merge_state: MergeState::NotReady,
        draft_state: DraftState::Ready,
        review_class,
        ready_emitted_for_sha: None,
        auto_armed: false,
        auto_armed_for_sha: None,
        auto_armed_at: None,
        // #986 gh-poll observation fields — populated on first
        // scanner pass post-creation by gh_poll::CliGhPoller.
        last_gh_poll_at: None,
        gh_poll_failures: 0,
        last_gh_state: None,
        created_at: now.clone(),
        updated_at: now,
    }
}

/// CI ingestion entry point — called from `ci_watch::poller` after
/// the existing `[ci-ready-for-action]` emission. Loads-or-creates
/// the pr_state file, applies the event, saves. Failures are
/// `tracing::warn`-logged but never propagated (must not block CI
/// poll — same discipline as #870 `auto_release::enqueue_intent`).
///
/// `review_class` is sourced from the ci-watch file's `review_class`
/// field (see [`crate::daemon::ci_watch::poller::parse_review_class`]).
/// Applied on FIRST observation (file creation); subsequent
/// observations preserve the existing `review_class` to avoid
/// flapping if the watch file mutates mid-PR. Operator who needs
/// to change review_class mid-flight should `remove` the pr_state
/// file before re-running `ci action=watch`.
pub fn record_ci_result(
    home: &Path,
    repo: &str,
    branch: &str,
    head_sha: &str,
    conclusion: CiConclusion<'_>,
    subscribers: Vec<String>,
    review_class: ReviewClass,
) {
    if let Err(e) = with_pr_state_or_create(
        home,
        repo,
        branch,
        || new_for_branch(repo, branch, head_sha, review_class),
        |state| {
            // #1314: skip CiObserved on terminal states to prevent
            // stale write over scanner's ready_emitted_for_sha.
            if matches!(
                state.merge_state,
                MergeState::Merged { .. } | MergeState::ClosedUnmerged { .. }
            ) {
                return;
            }
            if !subscribers.is_empty() && state.subscribers.is_empty() {
                state.subscribers = subscribers;
            }
            apply(
                state,
                Event::CiObserved {
                    head_sha,
                    conclusion,
                    observed_at: chrono::Utc::now().to_rfc3339(),
                },
            );
        },
    ) {
        tracing::warn!(
            repo = %repo,
            branch = %branch,
            error = %e,
            "#972 pr_state: record_ci_result save failed"
        );
    }
}

/// Verdict ingestion entry point — called from
/// `api::handlers::messaging::handle_send` after the existing
/// `auto_release::enqueue_intent` hook. `task_id` is the verdict's
/// correlation_id. We look up the task's branch on the task board
/// and apply the verdict to the matching pr_state file.
///
/// Best-effort: if the task can't be found, or no pr_state file
/// exists for the task's branch yet, the verdict is silently
/// dropped (auto_release still handles it; pr_state will pick up
/// the verdict on next CI tick when the file is created — TODO
/// for v2: persist orphan verdicts in a sidecar so they apply on
/// next file-create).
pub fn record_verdict(
    home: &Path,
    task_id: &str,
    reviewer: &str,
    reviewed_head: Option<&str>,
    kind: VerdictKind<'_>,
) {
    // #1002 Phase 1: tracing on every silent gate (A-E) so the next
    // #982-style "verdict_state stuck at None" bisect can identify
    // which gate fired without code spelunking. Levels are chosen so
    // default daemon filter (`agend_terminal=info`) surfaces every
    // operator-actionable miss — debug-level would be invisible at
    // default and re-create the silent-failure class #1002 was filed
    // against.
    let Some(reviewed_head) = reviewed_head else {
        tracing::info!(
            task_id,
            reviewer,
            "#1002 record_verdict skipped (gate A) — reviewed_head is None; \
             reviewer kind=report did not carry reviewed_head field"
        );
        return;
    };
    let Some(task) = crate::tasks::load_by_id(home, task_id) else {
        tracing::info!(
            task_id,
            reviewer,
            "#1002 record_verdict skipped (gate B) — task not found in task board; \
             correlation_id likely mismatched (e.g. used review-task id instead of impl-task id)"
        );
        return;
    };
    let branch = match task.branch {
        Some(b) if !b.is_empty() => b,
        _ => {
            tracing::info!(
                task_id,
                reviewer,
                "#1002 record_verdict skipped (gate C) — task.branch field empty; \
                 task was created without a branch hint"
            );
            return;
        }
    };
    // We don't always know the repo from the task. Walk the pr-state
    // directory and find the file whose branch matches. (Typically 1
    // pr per branch; ambiguity unlikely.)
    let dir = pr_state_dir(home);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                task_id,
                dir = %dir.display(),
                error = %e,
                "#1002 record_verdict skipped (gate D) — pr-state dir read failed"
            );
            return;
        }
    };
    let mut matched_any = false;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(state): Result<PrState, _> = serde_json::from_str(&content) else {
            continue;
        };
        if state.branch != branch {
            continue;
        }
        matched_any = true;
        let repo = state.repo.clone();
        if let Err(e) = with_pr_state(home, &repo, &branch, |s| {
            apply(
                s,
                Event::VerdictObserved {
                    reviewer,
                    reviewed_head,
                    kind,
                },
            );
        }) {
            tracing::warn!(
                repo = %repo,
                branch = %branch,
                error = %e,
                "#972 pr_state: record_verdict save failed"
            );
        }
    }
    if !matched_any {
        tracing::info!(
            task_id,
            branch = %branch,
            reviewer,
            "#1002 record_verdict noop (gate E) — no pr-state file matched task branch; \
             CI watch may not have created the file yet for this branch"
        );
    }
}

/// Author resolution chain (reviewer cross-audit PRIMARY):
/// 1. Stored `pr_author` field if non-empty (gh-poll already populated
///    it via the 4-tier chain in [`gh_poll::resolve_author_with_gh`])
/// 2. `subscribers[0]` from ci_watch (pre-gh-poll fallback)
/// 3. `"fixup-lead"` last-resort fallback (with `tracing::warn`)
pub fn resolve_author(state: &PrState) -> String {
    if !state.pr_author.is_empty() {
        return state.pr_author.clone();
    }
    if let Some(first) = state.subscribers.first() {
        return first.clone();
    }
    tracing::warn!(
        repo = %state.repo,
        branch = %state.branch,
        "#986 resolve_author: no pr_author + no subscribers → \
         fixup-lead fallback (gh-poll may not have run yet)"
    );
    "fixup-lead".to_string()
}

/// Build the `[pr-ready-for-merge]` event body. Pulled out for
/// unit testability + future #973 `--auto`-aware reformulation.
pub fn format_ready_body(state: &PrState) -> String {
    let sha_short = &state.head_sha[..8.min(state.head_sha.len())];
    let pr_id = if state.pr_number > 0 {
        format!("{}#{}", state.repo, state.pr_number)
    } else {
        format!("{}@{}", state.repo, state.branch)
    };
    let reviewers_csv = if let VerdictState::Verified { reviewers } = &state.verdict_state {
        reviewers
            .iter()
            .map(|(r, _)| r.as_str())
            .collect::<Vec<_>>()
            .join(",")
    } else {
        String::new()
    };
    format!(
        "[pr-ready-for-merge] {pr_id} (head {sha_short}): \
         CI green ∧ VERIFIED ({reviewers_csv}). \
         §3.12 self-merge gate open — `gh pr merge {pr_or_branch} --squash --delete-branch` \
         (or post-#973 `--auto`).",
        pr_or_branch = if state.pr_number > 0 {
            state.pr_number.to_string()
        } else {
            state.branch.clone()
        }
    )
}

// ─── scanner functions moved to scanner.rs ────────────────────────────

// ─── reducer test matrix ───────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn now() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    fn new_state(head: &str, class: ReviewClass) -> PrState {
        PrState {
            repo: "owner/repo".to_string(),
            pr_number: 100,
            branch: "feat/test".to_string(),
            head_sha: head.to_string(),
            pr_author: "dev".to_string(),
            subscribers: vec!["dev".to_string()],
            ci_state: CiState::Pending,
            verdict_state: VerdictState::None,
            merge_state: MergeState::NotReady,
            draft_state: DraftState::Ready,
            review_class: class,
            ready_emitted_for_sha: None,
            auto_armed: false,
            auto_armed_for_sha: None,
            auto_armed_at: None,
            last_gh_poll_at: None,
            gh_poll_failures: 0,
            last_gh_state: None,
            created_at: now(),
            updated_at: now(),
        }
    }

    /// T1: CI green at head_sha + Verified at same head_sha → MergeReady.
    #[test]
    fn t1_ci_then_verdict_at_same_sha_yields_merge_ready() {
        let mut s = new_state("sha-A", ReviewClass::Single);
        apply(
            &mut s,
            Event::CiObserved {
                head_sha: "sha-A",
                conclusion: CiConclusion::Green,
                observed_at: now(),
            },
        );
        apply(
            &mut s,
            Event::VerdictObserved {
                reviewer: "rev-1",
                reviewed_head: "sha-A",
                kind: VerdictKind::Verified,
            },
        );
        assert_eq!(s.merge_state, MergeState::MergeReady);
    }

    /// T2 / Reviewer must-have T_sha_mismatch: CI at sha-A, verdict
    /// at sha-B (b != a) → NotReady. §4.2 invariant.
    #[test]
    fn t2_sha_mismatch_refuses_merge_ready() {
        let mut s = new_state("sha-A", ReviewClass::Single);
        apply(
            &mut s,
            Event::CiObserved {
                head_sha: "sha-A",
                conclusion: CiConclusion::Green,
                observed_at: now(),
            },
        );
        apply(
            &mut s,
            Event::VerdictObserved {
                reviewer: "rev-1",
                reviewed_head: "sha-B-OLD",
                kind: VerdictKind::Verified,
            },
        );
        assert_eq!(s.merge_state, MergeState::NotReady);
        assert!(!is_merge_ready(&s));
    }

    /// T3 / Reviewer must-have T_force_push: Verified at sha-A; head
    /// advances to sha-B; verdict invalidated; back to NotReady.
    #[test]
    fn t3_head_advance_invalidates_verdict() {
        let mut s = new_state("sha-A", ReviewClass::Single);
        apply(
            &mut s,
            Event::CiObserved {
                head_sha: "sha-A",
                conclusion: CiConclusion::Green,
                observed_at: now(),
            },
        );
        apply(
            &mut s,
            Event::VerdictObserved {
                reviewer: "rev-1",
                reviewed_head: "sha-A",
                kind: VerdictKind::Verified,
            },
        );
        assert_eq!(s.merge_state, MergeState::MergeReady);
        // Head advances (force-push).
        apply(
            &mut s,
            Event::CiObserved {
                head_sha: "sha-B",
                conclusion: CiConclusion::Pending,
                observed_at: now(),
            },
        );
        assert_eq!(s.head_sha, "sha-B");
        assert_eq!(s.merge_state, MergeState::NotReady);
        // Auto-armed (if any) cleared.
        assert!(!s.auto_armed);
        assert_eq!(s.ready_emitted_for_sha, None);
        // Verdict cleared (was for sha-A).
        assert_eq!(s.verdict_state, VerdictState::Pending);
    }

    /// T4: idempotent debounce — once `ready_emitted_for_sha` matches
    /// head_sha, the reducer doesn't mutate ready_emitted_for_sha
    /// (that field is updated by the emitter, not the reducer). The
    /// reducer can still recompute MergeReady on every event; the
    /// emitter is responsible for one-fire-per-sha.
    #[test]
    fn t4_reducer_recomputes_merge_ready_every_event() {
        let mut s = new_state("sha-A", ReviewClass::Single);
        apply(
            &mut s,
            Event::CiObserved {
                head_sha: "sha-A",
                conclusion: CiConclusion::Green,
                observed_at: now(),
            },
        );
        apply(
            &mut s,
            Event::VerdictObserved {
                reviewer: "rev-1",
                reviewed_head: "sha-A",
                kind: VerdictKind::Verified,
            },
        );
        assert_eq!(s.merge_state, MergeState::MergeReady);
        // No-op event (re-record same CI). MergeReady should stay.
        apply(
            &mut s,
            Event::CiObserved {
                head_sha: "sha-A",
                conclusion: CiConclusion::Green,
                observed_at: now(),
            },
        );
        assert_eq!(s.merge_state, MergeState::MergeReady);
    }

    /// T5 / Reviewer must-have T_dual_review: Dual review_class
    /// requires 2 VERIFIED at the same head_sha.
    #[test]
    fn t5_dual_review_requires_two_verified() {
        let mut s = new_state("sha-A", ReviewClass::Dual);
        apply(
            &mut s,
            Event::CiObserved {
                head_sha: "sha-A",
                conclusion: CiConclusion::Green,
                observed_at: now(),
            },
        );
        apply(
            &mut s,
            Event::VerdictObserved {
                reviewer: "rev-1",
                reviewed_head: "sha-A",
                kind: VerdictKind::Verified,
            },
        );
        // Only 1 of 2 — not ready.
        assert_eq!(s.merge_state, MergeState::NotReady);
        apply(
            &mut s,
            Event::VerdictObserved {
                reviewer: "rev-2",
                reviewed_head: "sha-A",
                kind: VerdictKind::Verified,
            },
        );
        assert_eq!(s.merge_state, MergeState::MergeReady);
    }

    /// T6 / Reviewer must-have T_reject: Rejected verdict at any sha
    /// → NotReady. (No MergeReady possible from Rejected variant.)
    #[test]
    fn t6_rejected_keeps_not_ready() {
        let mut s = new_state("sha-A", ReviewClass::Single);
        apply(
            &mut s,
            Event::CiObserved {
                head_sha: "sha-A",
                conclusion: CiConclusion::Green,
                observed_at: now(),
            },
        );
        apply(
            &mut s,
            Event::VerdictObserved {
                reviewer: "rev-1",
                reviewed_head: "sha-A",
                kind: VerdictKind::Rejected {
                    reason: Some("LGTM after addressing X"),
                },
            },
        );
        assert_eq!(s.merge_state, MergeState::NotReady);
        assert!(matches!(s.verdict_state, VerdictState::Rejected { .. }));
    }

    /// T7 / Reviewer must-have T_unverified: Unverified verdict → NotReady.
    #[test]
    fn t7_unverified_keeps_not_ready() {
        let mut s = new_state("sha-A", ReviewClass::Single);
        apply(
            &mut s,
            Event::CiObserved {
                head_sha: "sha-A",
                conclusion: CiConclusion::Green,
                observed_at: now(),
            },
        );
        apply(
            &mut s,
            Event::VerdictObserved {
                reviewer: "rev-1",
                reviewed_head: "sha-A",
                kind: VerdictKind::Unverified,
            },
        );
        assert_eq!(s.merge_state, MergeState::NotReady);
        assert!(matches!(s.verdict_state, VerdictState::Unverified { .. }));
    }

    /// T8 / dev-2 T_draft: Draft state refuses MergeReady regardless
    /// of CI + verdict.
    #[test]
    fn t8_draft_refuses_merge_ready() {
        let mut s = new_state("sha-A", ReviewClass::Single);
        apply(&mut s, Event::DraftTransition { is_draft: true });
        apply(
            &mut s,
            Event::CiObserved {
                head_sha: "sha-A",
                conclusion: CiConclusion::Green,
                observed_at: now(),
            },
        );
        apply(
            &mut s,
            Event::VerdictObserved {
                reviewer: "rev-1",
                reviewed_head: "sha-A",
                kind: VerdictKind::Verified,
            },
        );
        assert_eq!(s.merge_state, MergeState::NotReady);
        // Draft → Ready transition unblocks.
        apply(&mut s, Event::DraftTransition { is_draft: false });
        assert_eq!(s.merge_state, MergeState::MergeReady);
    }

    /// T9 / dev-2 T_invalidate: MergeReady → head_sha advance →
    /// MergeReady cleared + auto_armed cleared.
    #[test]
    fn t9_post_merge_ready_force_push_invalidates() {
        let mut s = new_state("sha-A", ReviewClass::Single);
        apply(
            &mut s,
            Event::CiObserved {
                head_sha: "sha-A",
                conclusion: CiConclusion::Green,
                observed_at: now(),
            },
        );
        apply(
            &mut s,
            Event::VerdictObserved {
                reviewer: "rev-1",
                reviewed_head: "sha-A",
                kind: VerdictKind::Verified,
            },
        );
        assert_eq!(s.merge_state, MergeState::MergeReady);
        // Simulate implementer armed --auto.
        s.auto_armed = true;
        s.auto_armed_for_sha = Some("sha-A".to_string());
        s.auto_armed_at = Some(now());
        // Force-push.
        apply(
            &mut s,
            Event::CiObserved {
                head_sha: "sha-B",
                conclusion: CiConclusion::Pending,
                observed_at: now(),
            },
        );
        assert!(!s.auto_armed);
        assert_eq!(s.auto_armed_for_sha, None);
        assert_eq!(s.merge_state, MergeState::NotReady);
    }

    /// T10 / dev-2 T_closed_unmerged: ClosedUnmerged is sticky and
    /// does not get downgraded by subsequent CI/verdict events.
    #[test]
    fn t10_closed_unmerged_is_sticky() {
        let mut s = new_state("sha-A", ReviewClass::Single);
        apply(&mut s, Event::ClosedUnmergedObserved { closed_at: now() });
        assert!(matches!(s.merge_state, MergeState::ClosedUnmerged { .. }));
        // Subsequent CI/verdict noise must not flip back to MergeReady.
        apply(
            &mut s,
            Event::CiObserved {
                head_sha: "sha-A",
                conclusion: CiConclusion::Green,
                observed_at: now(),
            },
        );
        apply(
            &mut s,
            Event::VerdictObserved {
                reviewer: "rev-1",
                reviewed_head: "sha-A",
                kind: VerdictKind::Verified,
            },
        );
        assert!(matches!(s.merge_state, MergeState::ClosedUnmerged { .. }));
    }

    /// T11: Merged is sticky.
    #[test]
    fn t11_merged_is_sticky() {
        let mut s = new_state("sha-A", ReviewClass::Single);
        apply(
            &mut s,
            Event::MergedObserved {
                merge_commit: "merge-sha",
                merged_at: now(),
            },
        );
        assert!(matches!(s.merge_state, MergeState::Merged { .. }));
        apply(
            &mut s,
            Event::CiObserved {
                head_sha: "sha-B",
                conclusion: CiConclusion::Pending,
                observed_at: now(),
            },
        );
        assert!(matches!(s.merge_state, MergeState::Merged { .. }));
    }

    /// T12: CI Failed → NotReady. Subsequent VERIFIED at same sha
    /// is honored but merge_state stays NotReady (ci is failed).
    #[test]
    fn t12_ci_failed_blocks_merge_ready() {
        let mut s = new_state("sha-A", ReviewClass::Single);
        apply(
            &mut s,
            Event::CiObserved {
                head_sha: "sha-A",
                conclusion: CiConclusion::Failed {
                    conclusion: "failure",
                },
                observed_at: now(),
            },
        );
        apply(
            &mut s,
            Event::VerdictObserved {
                reviewer: "rev-1",
                reviewed_head: "sha-A",
                kind: VerdictKind::Verified,
            },
        );
        assert_eq!(s.merge_state, MergeState::NotReady);
    }

    /// T13: same reviewer reporting Verified twice doesn't double-count
    /// (dual-review requires DISTINCT reviewers).
    #[test]
    fn t13_same_reviewer_twice_counts_as_one() {
        let mut s = new_state("sha-A", ReviewClass::Dual);
        apply(
            &mut s,
            Event::CiObserved {
                head_sha: "sha-A",
                conclusion: CiConclusion::Green,
                observed_at: now(),
            },
        );
        apply(
            &mut s,
            Event::VerdictObserved {
                reviewer: "rev-1",
                reviewed_head: "sha-A",
                kind: VerdictKind::Verified,
            },
        );
        // Same reviewer re-reports — must NOT bump to 2.
        apply(
            &mut s,
            Event::VerdictObserved {
                reviewer: "rev-1",
                reviewed_head: "sha-A",
                kind: VerdictKind::Verified,
            },
        );
        assert_eq!(s.merge_state, MergeState::NotReady);
        if let VerdictState::Verified { reviewers } = &s.verdict_state {
            assert_eq!(reviewers.len(), 1, "dedup by reviewer name");
        } else {
            panic!("expected Verified state, got {:?}", s.verdict_state);
        }
    }

    /// T14: storage round-trip — serialize, deserialize, structural eq.
    #[test]
    fn t14_storage_roundtrip() {
        let dir = std::env::temp_dir().join(format!("agend-972-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let s = new_state("sha-A", ReviewClass::Single);
        save(&dir, &s).unwrap();
        let loaded = load(&dir, &s.repo, &s.branch).expect("reload");
        assert_eq!(loaded, s);
        // Remove leaves no file.
        remove(&dir, &s.repo, &s.branch).unwrap();
        assert!(load(&dir, &s.repo, &s.branch).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T15: resolve_author chain — explicit pr_author wins, then
    /// subscribers[0], then "fixup-lead" fallback.
    #[test]
    fn t15_resolve_author_chain() {
        let mut s = new_state("sha-A", ReviewClass::Single);
        // Default state has pr_author="dev" + subscribers=[dev]
        assert_eq!(resolve_author(&s), "dev");
        s.pr_author = String::new();
        assert_eq!(resolve_author(&s), "dev"); // falls to subscribers[0]
        s.subscribers.clear();
        assert_eq!(resolve_author(&s), "fixup-lead"); // last-resort fallback
    }

    /// T16: format_ready_body uses pr_number when known, branch otherwise.
    #[test]
    fn t16_format_ready_body_with_and_without_pr_number() {
        let mut s = new_state("sha-A0001234567890", ReviewClass::Single);
        s.verdict_state = VerdictState::Verified {
            reviewers: vec![("rev-1".to_string(), "sha-A0001234567890".to_string())],
        };
        s.pr_number = 0;
        let body = format_ready_body(&s);
        assert!(body.contains("owner/repo@feat/test"), "branch form: {body}");
        s.pr_number = 970;
        let body = format_ready_body(&s);
        assert!(body.contains("owner/repo#970"), "pr-number form: {body}");
        assert!(body.contains("sha-A000"), "sha short: {body}");
        assert!(body.contains("rev-1"), "reviewers: {body}");
    }

    /// T_integration: scan_and_emit fires `[pr-ready-for-merge]` to
    /// author's inbox once per MergeReady transition. Subsequent
    /// scans (same head_sha) do NOT re-emit (debounce). Hits
    /// production scanner + inbox enqueue path end-to-end (no
    /// network, no ci_watch — synthetic PrState on disk).
    #[test]
    fn t18_scan_and_emit_fires_once_per_sha() {
        use parking_lot::Mutex;
        use std::collections::HashMap;
        use std::sync::Arc;

        let dir = std::env::temp_dir().join(format!("agend-972-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Inbox needs the inbox dir to exist.
        std::fs::create_dir_all(dir.join("inbox")).ok();

        // Build a MergeReady state on disk.
        let mut s = new_state("sha-A", ReviewClass::Single);
        s.ci_state = CiState::Green {
            sha: "sha-A".to_string(),
            observed_at: now(),
        };
        s.verdict_state = VerdictState::Verified {
            reviewers: vec![("rev-1".to_string(), "sha-A".to_string())],
        };
        s.merge_state = MergeState::MergeReady;
        s.pr_author = "dev".to_string();
        save(&dir, &s).unwrap();

        let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));

        // First scan: emit.
        scan_and_emit(&dir, &registry);
        let inbox_msgs = crate::inbox::drain(&dir, "dev");
        assert_eq!(inbox_msgs.len(), 1, "expected one [pr-ready-for-merge]");
        assert_eq!(inbox_msgs[0].kind.as_deref(), Some("pr-ready-for-merge"));
        // Default fixture has pr_number=100 — body uses `owner/repo#100` form.
        assert!(
            inbox_msgs[0].text.contains("owner/repo#100"),
            "body shape: {}",
            inbox_msgs[0].text
        );
        assert!(
            inbox_msgs[0].text.contains("rev-1"),
            "reviewer surfacing: {}",
            inbox_msgs[0].text
        );
        // #946 correlation_id grep target.
        assert_eq!(
            inbox_msgs[0].correlation_id.as_deref(),
            Some("owner/repo@feat/test")
        );
        assert_eq!(inbox_msgs[0].reviewed_head.as_deref(), Some("sha-A"));

        // Second scan: must NOT re-emit (debounce per ready_emitted_for_sha).
        scan_and_emit(&dir, &registry);
        let inbox_msgs = crate::inbox::drain(&dir, "dev");
        assert!(
            inbox_msgs.is_empty(),
            "second scan must not re-emit; got {} message(s)",
            inbox_msgs.len()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T17: record_ci_result creates the file on first observation;
    /// subsequent calls update in-place.
    #[test]
    fn t17_record_ci_result_creates_then_updates() {
        let dir = std::env::temp_dir().join(format!("agend-972-rcr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // First observation: file did not exist.
        assert!(load(&dir, "owner/repo", "feat/x").is_none());
        record_ci_result(
            &dir,
            "owner/repo",
            "feat/x",
            "sha-A",
            CiConclusion::Pending,
            vec!["dev".to_string()],
            ReviewClass::Single,
        );
        let s = load(&dir, "owner/repo", "feat/x").expect("created");
        assert_eq!(s.head_sha, "sha-A");
        assert_eq!(s.subscribers, vec!["dev".to_string()]);

        // Second observation: file updates to Green.
        record_ci_result(
            &dir,
            "owner/repo",
            "feat/x",
            "sha-A",
            CiConclusion::Green,
            vec![],
            ReviewClass::Single,
        );
        let s = load(&dir, "owner/repo", "feat/x").expect("reloaded");
        assert!(matches!(s.ci_state, CiState::Green { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T19 (reviewer-rejection fix coverage): `record_ci_result` honors
    /// the `review_class` argument on FIRST observation, persists it to
    /// the pr_state file. This is the production code path that was
    /// missing pre-#972-rejection-fix — without it the pr_state file
    /// always defaulted to Single regardless of ci-watch's
    /// `review_class` field.
    #[test]
    fn t19_record_ci_result_propagates_review_class_dual() {
        let dir = std::env::temp_dir().join(format!("agend-972-dual-prop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // First observation with Dual. File MUST have Dual.
        record_ci_result(
            &dir,
            "owner/repo",
            "feat/dual",
            "sha-A",
            CiConclusion::Pending,
            vec!["dev".to_string()],
            ReviewClass::Dual,
        );
        let s = load(&dir, "owner/repo", "feat/dual").expect("created");
        assert_eq!(
            s.review_class,
            ReviewClass::Dual,
            "first-observation review_class must propagate from ci-watch"
        );

        // Subsequent observation: existing review_class preserved (no
        // mid-flight flapping if the watch file mutates).
        record_ci_result(
            &dir,
            "owner/repo",
            "feat/dual",
            "sha-A",
            CiConclusion::Green,
            vec![],
            ReviewClass::Single,
        );
        let s = load(&dir, "owner/repo", "feat/dual").expect("reloaded");
        assert_eq!(
            s.review_class,
            ReviewClass::Dual,
            "subsequent observation must NOT override the initial review_class"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T20 (reviewer-rejection fix end-to-end): full pipeline for a
    /// dual-review PR. CI green + ONE VERIFIED → NotReady. Second
    /// VERIFIED from a distinct reviewer at the same SHA → MergeReady.
    /// scan_and_emit fires `[pr-ready-for-merge]` only on the second
    /// verdict, not the first.
    #[test]
    fn t20_dual_review_does_not_merge_until_two_verdicts_e2e() {
        use parking_lot::Mutex;
        use std::collections::HashMap;
        use std::sync::Arc;

        let dir = std::env::temp_dir().join(format!("agend-972-dual-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(dir.join("inbox")).ok();

        // First CI observation arms the file with Dual.
        record_ci_result(
            &dir,
            "owner/repo",
            "feat/dual-e2e",
            "sha-A",
            CiConclusion::Green,
            vec!["dev".to_string()],
            ReviewClass::Dual,
        );

        // ONE verdict arrives. State must NOT transition to MergeReady.
        let mut s = load(&dir, "owner/repo", "feat/dual-e2e").unwrap();
        apply(
            &mut s,
            Event::VerdictObserved {
                reviewer: "rev-1",
                reviewed_head: "sha-A",
                kind: VerdictKind::Verified,
            },
        );
        assert_eq!(
            s.merge_state,
            MergeState::NotReady,
            "dual-review with one verdict must stay NotReady"
        );
        save(&dir, &s).unwrap();

        // Scanner pass: NO event emitted because state is NotReady.
        let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        scan_and_emit(&dir, &registry);
        assert!(
            crate::inbox::drain(&dir, "dev").is_empty(),
            "no [pr-ready-for-merge] until second verdict"
        );

        // SECOND verdict from distinct reviewer. Now MergeReady.
        let mut s = load(&dir, "owner/repo", "feat/dual-e2e").unwrap();
        apply(
            &mut s,
            Event::VerdictObserved {
                reviewer: "rev-2",
                reviewed_head: "sha-A",
                kind: VerdictKind::Verified,
            },
        );
        assert_eq!(s.merge_state, MergeState::MergeReady);
        save(&dir, &s).unwrap();

        // Scanner now fires [pr-ready-for-merge].
        scan_and_emit(&dir, &registry);
        let msgs = crate::inbox::drain(&dir, "dev");
        assert_eq!(msgs.len(), 1, "second verdict unlocks the merge gate");
        assert_eq!(msgs[0].kind.as_deref(), Some("pr-ready-for-merge"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T21 (reviewer-rejection fix coverage): `parse_review_class` (in
    /// `ci_watch::poller`) is the production source of the
    /// `ReviewClass` value passed into `record_ci_result`. Pin the
    /// parser contract: "dual" (case-insensitive) → Dual; everything
    /// else (absent / null / unknown string / wrong type) → Single.
    #[test]
    fn t21_parse_review_class_contract() {
        use crate::daemon::ci_watch::parse_review_class;
        use serde_json::json;

        assert_eq!(
            parse_review_class(&json!({"review_class": "dual"})),
            ReviewClass::Dual
        );
        assert_eq!(
            parse_review_class(&json!({"review_class": "DUAL"})),
            ReviewClass::Dual
        );
        assert_eq!(
            parse_review_class(&json!({"review_class": "Dual"})),
            ReviewClass::Dual
        );
        assert_eq!(
            parse_review_class(&json!({"review_class": "single"})),
            ReviewClass::Single
        );
        assert_eq!(
            parse_review_class(&json!({"review_class": "unknown"})),
            ReviewClass::Single,
            "unknown strings default to Single (safe fallback)"
        );
        assert_eq!(
            parse_review_class(&json!({"review_class": null})),
            ReviewClass::Single
        );
        assert_eq!(
            parse_review_class(&json!({})),
            ReviewClass::Single,
            "absent field defaults to Single"
        );
        assert_eq!(
            parse_review_class(&json!({"review_class": 42})),
            ReviewClass::Single,
            "wrong type defaults to Single"
        );
    }

    // ─── #986 caller-path integration tests (T2/T4/T5/T6/T9/T10) ─────

    use crate::daemon::pr_state::gh_poll::tests::MockGhPoller;
    use crate::daemon::pr_state::gh_poll::{GhPrMetadata, GhPrState};

    fn home_with_state(tag: &str, state: PrState) -> std::path::PathBuf {
        let home =
            std::env::temp_dir().join(format!("agend-986-int-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(crate::fleet::fleet_yaml_path(&home), "instances: {}\n").unwrap();
        std::fs::create_dir_all(home.join("inbox")).ok();
        save(&home, &state).unwrap();
        home
    }

    fn empty_registry() -> crate::agent::AgentRegistry {
        use parking_lot::Mutex;
        use std::collections::HashMap;
        use std::sync::Arc;
        Arc::new(Mutex::new(HashMap::new()))
    }

    fn gh_meta_open(number: u64, branch: &str, author: &str) -> GhPrMetadata {
        GhPrMetadata {
            number,
            author_login: author.to_string(),
            head_ref: branch.to_string(),
            is_cross_repository: false,
            is_draft: false,
            state: GhPrState::Open,
            merged_at: None,
        }
    }

    /// #986 T2 — first observation populates pr_number + pr_author.
    /// Before scan: state.pr_number=0, pr_author="". After scan with
    /// gh-poll returning the matching PR: both fields populated.
    #[test]
    fn t9_first_gh_observation_populates_pr_identity() {
        let mut s = new_state("sha-A", ReviewClass::Single);
        s.pr_author = String::new(); // simulate freshly-created state
        s.pr_number = 0;
        let home = home_with_state("first-obs", s);
        let poller = MockGhPoller::new(vec![Ok(vec![gh_meta_open(970, "feat/test", "dev")])]);

        scan_and_emit_with(&home, &empty_registry(), &poller);

        let loaded = load(&home, "owner/repo", "feat/test").unwrap();
        assert_eq!(loaded.pr_number, 970);
        assert_eq!(loaded.pr_author, "dev"); // tier 2 direct name match (no fleet entries)
        assert_eq!(loaded.gh_poll_failures, 0);
        assert!(loaded.last_gh_poll_at.is_some());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// PR-3 (t-ci-ready-pr3-arm-not-armed) — INTEGRATION (codex re-verify): a
    /// BOUND branch with NO ci-watch AND NO pr-state file must STILL be
    /// discovered and auto-armed. This exercises the binding-seeded discovery
    /// path through the real scanner — the exact structural hole codex's first
    /// pass found, which the `auto_arm` unit tests (injecting `prs` directly)
    /// could not reach. Without the bound-branch seed, the repo never enters the
    /// poll list (no pr-state) → the open PR is never discovered → #1782 unfixed.
    #[test]
    #[cfg(unix)]
    fn pr3_bound_branch_with_no_seed_is_discovered_and_armed() {
        let parent = std::env::temp_dir().join(format!("agend-pr3-integ-{}", std::process::id()));
        let home = parent.join("home");
        std::fs::create_dir_all(&home).unwrap();

        // Source repo whose origin remote resolves to the slug "owner/repo".
        let repo_path = parent.join("source-repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec![
                "remote",
                "add",
                "origin",
                "https://github.com/owner/repo.git",
            ],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&repo_path)
                .env("AGEND_GIT_BYPASS", "1")
                .output()
                .unwrap();
        }

        // Bind "dev-x" → branch "feat/x" in that repo. NO pr-state, NO ci-watch.
        let bdir = crate::paths::runtime_dir(&home).join("dev-x");
        std::fs::create_dir_all(&bdir).unwrap();
        std::fs::write(
            bdir.join("binding.json"),
            serde_json::to_string(&serde_json::json!({
                "version": 1, "agent": "dev-x", "task_id": "t",
                "branch": "feat/x", "worktree": "/tmp/wt-dev-x",
                "source_repo": repo_path.display().to_string(),
                "issued_at": "2026-06-05T00:00:00Z",
            }))
            .unwrap(),
        )
        .unwrap();

        // gh-poll observes one OPEN PR on that branch.
        let poller = MockGhPoller::new(vec![Ok(vec![gh_meta_open(700, "feat/x", "suzuke")])]);

        scan_and_emit_with(&home, &empty_registry(), &poller);

        // The bound-branch discovery path must have auto-armed a watch.
        let watch = crate::daemon::ci_watch::ci_watches_dir(&home).join(
            crate::daemon::ci_watch::watch_filename("owner/repo", "feat/x"),
        );
        assert!(
            watch.exists(),
            "a bound branch with no pr-state/watch seed must be discovered + auto-armed"
        );
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&watch).unwrap()).unwrap();
        let subs: Vec<&str> = v["subscribers"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["instance"].as_str())
            .collect();
        assert!(
            subs.contains(&"dev-x"),
            "subscriber must be the BOUND agent (not the gh author login): {subs:?}"
        );
        std::fs::remove_dir_all(&parent).ok();
    }

    /// #986 T4 — `gh state=MERGED + mergedAt!=None` fires
    /// MergedObserved → reducer transitions to Merged terminal state
    /// → scanner emits `[pr-merged]` to author inbox + sweeps file.
    #[test]
    fn t10_merged_observation_fires_pr_merged_event() {
        let mut s = new_state("sha-A", ReviewClass::Single);
        s.pr_author = String::new();
        let home = home_with_state("merged-obs", s);
        let merged_meta = GhPrMetadata {
            number: 970,
            author_login: "dev".into(),
            head_ref: "feat/test".into(),
            is_cross_repository: false,
            is_draft: false,
            state: GhPrState::Merged,
            merged_at: Some("2026-05-20T04:17:09Z".to_string()),
        };
        let poller = MockGhPoller::new(vec![Ok(vec![merged_meta.clone()])]);

        scan_and_emit_with(&home, &empty_registry(), &poller);

        // #1287: first scan emits but persists with dedup flag — file
        // survives until next scan confirms already_emitted.
        let persisted = load(&home, "owner/repo", "feat/test")
            .expect("#1287: file must survive first scan with dedup flag");
        assert_eq!(
            persisted.ready_emitted_for_sha.as_deref(),
            Some("sha-A"),
            "#1287: ready_emitted_for_sha must be set after emit"
        );
        // [pr-merged] in dev's inbox.
        let msgs = crate::inbox::drain(&home, "dev");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].kind.as_deref(), Some("pr-merged"));
        assert!(msgs[0].text.contains("pr-merged"));

        // Second scan: already_emitted → remove without re-emitting.
        let poller2 = MockGhPoller::new(vec![Ok(vec![merged_meta])]);
        scan_and_emit_with(&home, &empty_registry(), &poller2);
        assert!(
            load(&home, "owner/repo", "feat/test").is_none(),
            "second scan must sweep terminal file"
        );
        let msgs2 = crate::inbox::drain(&home, "dev");
        assert!(msgs2.is_empty(), "#1287: no duplicate emit on second scan");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// #986 T5 — `gh state=CLOSED + mergedAt=None` fires
    /// ClosedUnmergedObserved → reducer transitions → scanner emits
    /// `[pr-closed-unmerged]`.
    #[test]
    fn t11_closed_unmerged_observation_fires_event() {
        let mut s = new_state("sha-A", ReviewClass::Single);
        s.pr_author = String::new();
        let home = home_with_state("closed-obs", s);
        let closed_meta = GhPrMetadata {
            number: 970,
            author_login: "dev".into(),
            head_ref: "feat/test".into(),
            is_cross_repository: false,
            is_draft: false,
            state: GhPrState::Closed,
            merged_at: None,
        };
        let poller = MockGhPoller::new(vec![Ok(vec![closed_meta.clone()])]);

        scan_and_emit_with(&home, &empty_registry(), &poller);

        // #1287: first scan emits + persists with dedup flag.
        let persisted = load(&home, "owner/repo", "feat/test")
            .expect("#1287: file must survive first scan with dedup flag");
        assert_eq!(persisted.ready_emitted_for_sha.as_deref(), Some("sha-A"));
        let msgs = crate::inbox::drain(&home, "dev");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].kind.as_deref(), Some("pr-closed-unmerged"));

        // Second scan: already_emitted → remove without re-emitting.
        let poller2 = MockGhPoller::new(vec![Ok(vec![closed_meta])]);
        scan_and_emit_with(&home, &empty_registry(), &poller2);
        assert!(load(&home, "owner/repo", "feat/test").is_none());
        let msgs2 = crate::inbox::drain(&home, "dev");
        assert!(msgs2.is_empty(), "#1287: no duplicate emit on second scan");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// #986 T7 — gh-poll failure increments `gh_poll_failures` and
    /// updates `last_gh_poll_at` so backoff math kicks in next tick.
    #[test]
    fn t12_gh_poll_failure_increments_backoff_counter() {
        let s = new_state("sha-A", ReviewClass::Single);
        let home = home_with_state("backoff", s);
        let poller = MockGhPoller::new(vec![Err(anyhow::anyhow!("simulated rate limit"))]);

        scan_and_emit_with(&home, &empty_registry(), &poller);

        let loaded = load(&home, "owner/repo", "feat/test").unwrap();
        assert_eq!(loaded.gh_poll_failures, 1);
        assert!(loaded.last_gh_poll_at.is_some());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// #986 T-load-bearing (reviewer #990 BLOCKING #1) — the actual
    /// regression path that motivated #986: PrState in MergeReady
    /// state but `pr_author=""` / `pr_number=0` (placeholder values
    /// from `new_for_branch` when ci_watch arms before any gh-poll).
    /// After scan_and_emit_with applies gh-poll:
    /// - pr_author populated via 4-tier resolution chain
    /// - pr_number populated from gh metadata
    /// - `[pr-ready-for-merge]` event enqueued to RESOLVED author with
    ///   the gh-discovered PR number in the body
    ///
    /// Pre-#986: pre-poll state sat MergeReady forever, ready event
    /// fired to subscribers[0] (fallback) with `repo@branch` body
    /// instead of `repo#N`. Operator-visible: lead manual kick still
    /// needed even with #972 aggregator merged.
    #[test]
    fn t14_gh_poll_promotes_unknown_author_to_ready_event() {
        // Build a state ALREADY MergeReady (CI green + 1×VERIFIED at
        // same sha) but with placeholder pr_author / pr_number.
        let mut s = new_state("sha-A", ReviewClass::Single);
        s.pr_author = String::new();
        s.pr_number = 0;
        s.ci_state = CiState::Green {
            sha: "sha-A".to_string(),
            observed_at: now(),
        };
        s.verdict_state = VerdictState::Verified {
            reviewers: vec![("rev-1".to_string(), "sha-A".to_string())],
        };
        s.merge_state = MergeState::MergeReady;
        // subscribers[0] is "dev" from fixture — but we want gh-poll
        // to win via tier 2 name match. Set up a fleet.yaml with a
        // "suzuke" instance that matches the gh author.login.
        let home = home_with_state("ready-promote", s);
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  suzuke:\n    backend: claude\n",
        )
        .unwrap();
        std::fs::create_dir_all(home.join("inbox")).ok();

        // MockGhPoller returns metadata: PR #990, author "suzuke"
        // (matches the fleet instance via tier 2 name match), state=OPEN.
        let poller = MockGhPoller::new(vec![Ok(vec![gh_meta_open(990, "feat/test", "suzuke")])]);

        scan_and_emit_with(&home, &empty_registry(), &poller);

        // Post-scan state: pr_number/author populated, ready event swept
        // (file persists because state is OPEN not Merged), ready event
        // enqueued.
        let loaded = load(&home, "owner/repo", "feat/test").expect("state persists post-scan");
        assert_eq!(loaded.pr_number, 990, "pr_number populated from gh-poll");
        assert_eq!(
            loaded.pr_author, "suzuke",
            "pr_author resolved via tier-2 name match against fleet.yaml"
        );

        // [pr-ready-for-merge] enqueued to the RESOLVED author (suzuke,
        // NOT subscribers[0]'s "dev"). Body must include the gh-poll
        // PR number (repo#990) NOT the placeholder repo@branch form.
        let msgs = crate::inbox::drain(&home, "suzuke");
        assert_eq!(
            msgs.len(),
            1,
            "exactly one [pr-ready-for-merge] to resolved author"
        );
        assert_eq!(msgs[0].kind.as_deref(), Some("pr-ready-for-merge"));
        assert!(
            msgs[0].text.contains("owner/repo#990"),
            "event body must use gh-discovered PR number, not @branch placeholder: {}",
            msgs[0].text
        );

        // Subscriber["dev"] inbox should be empty — gh-poll's resolved
        // author won over the legacy subscribers[0] fallback.
        let dev_msgs = crate::inbox::drain(&home, "dev");
        assert!(
            dev_msgs.is_empty(),
            "subscribers[0] fallback must NOT fire when gh-poll resolves a different author"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    /// #986 T10 (reviewer MANDATORY idempotency) — same gh-poll output
    /// applied twice does NOT double-emit / double-transition. Reducer
    /// recomputes derived state; Merged terminal already swept on
    /// first pass, so second pass has nothing to do.
    #[test]
    fn t13_idempotent_same_observation_no_double_emit() {
        let mut s = new_state("sha-A", ReviewClass::Single);
        s.pr_author = String::new();
        let home = home_with_state("idempotent", s);
        let merged_meta = GhPrMetadata {
            number: 970,
            author_login: "dev".into(),
            head_ref: "feat/test".into(),
            is_cross_repository: false,
            is_draft: false,
            state: GhPrState::Merged,
            merged_at: Some("2026-05-20T04:17:09Z".to_string()),
        };
        // Two consecutive polls return the same metadata.
        let poller = MockGhPoller::new(vec![Ok(vec![merged_meta.clone()]), Ok(vec![merged_meta])]);

        scan_and_emit_with(&home, &empty_registry(), &poller);
        let msgs1 = crate::inbox::drain(&home, "dev");
        assert_eq!(msgs1.len(), 1, "first scan emits [pr-merged]");

        // Second scan — file already swept; no PrState files to poll.
        // Even if a stale file existed, the terminal state would be
        // sticky and the scanner wouldn't re-emit.
        scan_and_emit_with(&home, &empty_registry(), &poller);
        let msgs2 = crate::inbox::drain(&home, "dev");
        assert_eq!(msgs2.len(), 0, "second scan: file swept, no re-emit");
        let _ = std::fs::remove_dir_all(&home);
    }

    fn tmp_home_for_1002(tag: &str) -> std::path::PathBuf {
        let home = std::env::temp_dir().join(format!("agend-1002-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(crate::fleet::fleet_yaml_path(&home), "instances: {}\n").unwrap();
        std::fs::create_dir_all(home.join("inbox")).ok();
        home
    }

    /// #1002 Phase 1 observability pin: a malformed pr-state JSON
    /// file MUST emit a tracing::debug! message identifying the path
    /// and parse error, rather than silently continuing.
    ///
    /// Pre-fix code at `scan_and_emit_with` had:
    ///   let Ok(state): Result<PrState, _> = serde_json::from_str(&content)
    ///       else { continue; };
    /// — the silent `continue` meant a corrupt file or a schema-skew
    /// PrState was indistinguishable from "no files at all". This pin
    /// catches the next regression where a silent skip masks a real
    /// issue.
    #[test]
    #[tracing_test::traced_test]
    fn t15_malformed_pr_state_file_emits_observability_trace() {
        let home = tmp_home_for_1002("t15-malformed");
        let dir = pr_state_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        // Write a deliberately malformed pr-state JSON file.
        let bad_path = dir.join("malformed.json");
        std::fs::write(&bad_path, "{this is not json").unwrap();

        // MockGhPoller with no responses — apply_gh_poll's
        // read_dir/parse layer is exercised first and emits its own
        // trace; the scanner-loop layer also reads the same dir.
        let poller = MockGhPoller::new(vec![]);
        scan_and_emit_with(&home, &empty_registry(), &poller);

        // The malformed file path appears in tracing output via the
        // #1002 debug line. We don't pin the exact format (impl-detail)
        // but require:
        //   1. The new "#1002" tracing marker is present
        //   2. The malformed file's name is referenced
        assert!(
            logs_contain("#1002"),
            "scanner must emit a #1002-tagged observability trace on malformed file"
        );
        assert!(
            logs_contain("malformed.json"),
            "trace must identify the malformed file by name"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// #1002 Phase 1 observability pin: record_verdict's gate A
    /// (reviewed_head is None) MUST emit a tracing::debug! identifying
    /// the silent skip with the gate marker. Pre-fix, the early-return
    /// at `let Some(reviewed_head) = reviewed_head else { return };`
    /// silently swallowed the call; #982's verdict_state stuck at
    /// None could not be bisected without code spelunking.
    #[test]
    #[tracing_test::traced_test]
    fn t16_record_verdict_gate_a_emits_observability_trace() {
        let home = tmp_home_for_1002("t16-gate-a");
        // record_verdict with reviewed_head=None hits gate A
        // immediately — no fleet or pr-state setup required.
        record_verdict(
            &home,
            "t-fake-task-id",
            "fixup-reviewer",
            None,
            VerdictKind::Verified,
        );
        assert!(
            logs_contain("#1002"),
            "record_verdict must emit a #1002 observability trace on gate A"
        );
        assert!(logs_contain("gate A"), "trace must identify gate A by name");
        let _ = std::fs::remove_dir_all(&home);
    }

    // ─── #1017 startup-replay suppression ─────────────────────────────

    /// #1017 T17: stale Merged terminal-state files (mtime older than
    /// `AGEND_PR_STATE_REPLAY_AGE_HOURS`, default 1h) MUST be marked
    /// as already-emitted by `suppress_stale_terminal_replay` at boot.
    /// The next `scan_and_emit_with` tick then sweeps (removes) the
    /// file without firing the [pr-merged] event — closing the
    /// daemon-restart noise flood the operator hit on 2026-05-20.
    #[test]
    fn t17_1017_stale_merged_suppressed_then_swept_without_emit() {
        let mut s = new_state("sha-A", ReviewClass::Single);
        s.merge_state = MergeState::Merged {
            merge_commit: "merge-sha-A".to_string(),
            merged_at: "2026-05-20T04:00:00Z".to_string(),
        };
        let home = home_with_state("1017-stale-merged", s);

        // Threshold ZERO simulates "any age counts as stale". Tests
        // would otherwise need an mtime-mutator dev-dep (filetime crate)
        // to age the file; using the test seam avoids that.
        suppress_stale_terminal_replay_with(&home, std::time::Duration::ZERO);

        // Verify the file body now has ready_emitted_for_sha == head.
        let after_suppress = load(&home, "owner/repo", "feat/test")
            .expect("file persists after suppress (only flag mutated)");
        assert_eq!(
            after_suppress.ready_emitted_for_sha.as_deref(),
            Some("sha-A"),
            "stale Merged must have ready_emitted_for_sha set by suppress hook"
        );

        // First scan after boot: file should be swept (removed) but
        // NO [pr-merged] event emitted to the inbox.
        let poller = MockGhPoller::new(vec![Ok(vec![])]);
        scan_and_emit_with(&home, &empty_registry(), &poller);
        assert!(
            load(&home, "owner/repo", "feat/test").is_none(),
            "stale Merged file MUST be swept post-scan"
        );
        let msgs = crate::inbox::drain(&home, "dev");
        assert!(
            msgs.iter().all(|m| m.kind.as_deref() != Some("pr-merged")),
            "stale Merged MUST NOT emit [pr-merged] — got: {:?}",
            msgs.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// #1017 T18: a FRESH Merged terminal-state file (mtime within
    /// the replay-age threshold) is NOT touched by the suppress hook,
    /// and the next `scan_and_emit_with` tick fires the [pr-merged]
    /// event normally. Anti-regression: makes sure the suppression
    /// doesn't over-rotate into masking legitimate post-restart
    /// merges.
    #[test]
    fn t18_1017_fresh_merged_still_emits_normally() {
        let mut s = new_state("sha-B", ReviewClass::Single);
        s.merge_state = MergeState::Merged {
            merge_commit: "merge-sha-B".to_string(),
            merged_at: "2026-05-20T22:00:00Z".to_string(),
        };
        let home = home_with_state("1017-fresh-merged", s);

        // Threshold u32::MAX simulates "nothing is stale" — pin that
        // the suppress hook leaves fresh terminal files untouched.
        suppress_stale_terminal_replay_with(&home, std::time::Duration::from_secs(u32::MAX as u64));
        let after_suppress =
            load(&home, "owner/repo", "feat/test").expect("file persists after suppress");
        assert_eq!(
            after_suppress.ready_emitted_for_sha, None,
            "fresh Merged MUST NOT have ready_emitted_for_sha set by suppress hook"
        );

        // #1287: first scan emits + persists dedup flag (no removal).
        let poller = MockGhPoller::new(vec![Ok(vec![])]);
        scan_and_emit_with(&home, &empty_registry(), &poller);
        let persisted = load(&home, "owner/repo", "feat/test")
            .expect("#1287: file survives first scan with dedup flag");
        assert_eq!(persisted.ready_emitted_for_sha.as_deref(), Some("sha-B"));
        let msgs = crate::inbox::drain(&home, "dev");
        assert_eq!(msgs.len(), 1, "fresh Merged MUST emit [pr-merged]");
        assert_eq!(msgs[0].kind.as_deref(), Some("pr-merged"));

        // Second scan: already_emitted → swept, no re-emit.
        let poller2 = MockGhPoller::new(vec![Ok(vec![])]);
        scan_and_emit_with(&home, &empty_registry(), &poller2);
        assert!(
            load(&home, "owner/repo", "feat/test").is_none(),
            "second scan must sweep terminal file"
        );
        let msgs2 = crate::inbox::drain(&home, "dev");
        assert!(msgs2.is_empty(), "#1287: no duplicate emit on second scan");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// #1017 T19: ClosedUnmerged stale terminal state follows the
    /// same suppression contract as Merged. Symmetry pin.
    #[test]
    fn t19_1017_stale_closed_unmerged_suppressed_without_emit() {
        let mut s = new_state("sha-C", ReviewClass::Single);
        s.merge_state = MergeState::ClosedUnmerged {
            closed_at: "2026-05-20T05:00:00Z".to_string(),
        };
        let home = home_with_state("1017-stale-closed", s);

        // Threshold ZERO = anything counts as stale (see T17 rationale).
        suppress_stale_terminal_replay_with(&home, std::time::Duration::ZERO);
        let poller = MockGhPoller::new(vec![Ok(vec![])]);
        scan_and_emit_with(&home, &empty_registry(), &poller);
        let msgs = crate::inbox::drain(&home, "dev");
        assert!(
            msgs.iter()
                .all(|m| m.kind.as_deref() != Some("pr-closed-unmerged")),
            "stale ClosedUnmerged MUST NOT emit [pr-closed-unmerged]"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// #1017 T20: `AGEND_PR_STATE_REPLAY_AGE_HOURS` env var overrides
    /// the default 1h threshold. Operator-tunable.
    #[test]
    #[serial_test::serial]
    fn t20_1017_replay_age_threshold_env_override() {
        // SAFETY: set + remove around test; serial_test ensures no race.
        unsafe { std::env::set_var("AGEND_PR_STATE_REPLAY_AGE_HOURS", "24") };
        let got = replay_age_threshold();
        assert_eq!(got, std::time::Duration::from_secs(24 * 3600));
        unsafe { std::env::remove_var("AGEND_PR_STATE_REPLAY_AGE_HOURS") };

        // Default fallback when unset.
        let default = replay_age_threshold();
        assert_eq!(default, std::time::Duration::from_secs(3600));
    }
}
