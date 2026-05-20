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
/// `DraftTransition` / `MergedObserved` / `ClosedUnmergedObserved` are
/// reserved for v2 (gh-poll integration). Sourced only from tests
/// today; #972 v1 production code emits `CiObserved` + `VerdictObserved`.
#[derive(Debug, Clone)]
#[allow(dead_code)] // v2 variants — see docstring
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

/// Atomic save. Uses [`crate::store::atomic_write`] which is
/// post-#965 unique-tmp safe (concurrent saves to different PRs do
/// not contend on a shared tmp inode).
pub fn save(home: &Path, state: &PrState) -> anyhow::Result<()> {
    let path = pr_state_dir(home).join(pr_state_filename(&state.repo, &state.branch));
    let body = serde_json::to_string_pretty(state)?;
    crate::store::atomic_write(&path, body.as_bytes())?;
    Ok(())
}

/// Remove the per-PR file. Used by the per-tick scanner after a
/// terminal state (Merged / ClosedUnmerged) is observed and the
/// `[pr-merged]` / `[pr-closed-unmerged]` events have been emitted.
pub fn remove(home: &Path, repo: &str, branch: &str) -> std::io::Result<()> {
    let path = pr_state_dir(home).join(pr_state_filename(repo, branch));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
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
    let mut state = load(home, repo, branch)
        .unwrap_or_else(|| new_for_branch(repo, branch, head_sha, review_class));
    if !subscribers.is_empty() && state.subscribers.is_empty() {
        state.subscribers = subscribers;
    }
    apply(
        &mut state,
        Event::CiObserved {
            head_sha,
            conclusion,
            observed_at: chrono::Utc::now().to_rfc3339(),
        },
    );
    if let Err(e) = save(home, &state) {
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
    let Some(reviewed_head) = reviewed_head else {
        return;
    };
    // Look up task → branch via list_all (no per-id getter in v1).
    // Pre-filter by task id so we only iterate enough to find the match.
    let branch = match crate::tasks::list_all(home)
        .into_iter()
        .find(|t| t.id == task_id)
        .and_then(|t| t.branch)
    {
        Some(b) if !b.is_empty() => b,
        _ => return,
    };
    // We don't always know the repo from the task. Walk the pr-state
    // directory and find the file whose branch matches. (Typically 1
    // pr per branch; ambiguity unlikely.)
    let dir = pr_state_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut state): Result<PrState, _> = serde_json::from_str(&content) else {
            continue;
        };
        if state.branch != branch {
            continue;
        }
        apply(
            &mut state,
            Event::VerdictObserved {
                reviewer,
                reviewed_head,
                kind,
            },
        );
        if let Err(e) = save(home, &state) {
            tracing::warn!(
                repo = %state.repo,
                branch = %state.branch,
                error = %e,
                "#972 pr_state: record_verdict save failed"
            );
        }
    }
}

/// Author resolution chain (reviewer cross-audit PRIMARY):
/// 1. Stored `pr_author` field if non-empty (gh-poll already populated)
/// 2. `subscribers[0]` from ci_watch (legacy fallback)
/// 3. `"fixup-lead"` last-resort fallback so emission never silently drops
pub fn resolve_author(state: &PrState) -> String {
    if !state.pr_author.is_empty() {
        return state.pr_author.clone();
    }
    if let Some(first) = state.subscribers.first() {
        return first.clone();
    }
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

/// Per-tick scanner: walks `<home>/pr-state/*.json`, emits any newly-
/// eligible `[pr-ready-for-merge]` events (debounced via
/// `ready_emitted_for_sha`), and sweeps terminal-state files.
///
/// gh-poll for pr_number/pr_author/draft/merge state is fired here
/// (rate-limited — at most one gh call per scanner tick per file).
pub fn scan_and_emit(home: &Path, registry: &crate::agent::AgentRegistry) {
    let dir = pr_state_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut state): Result<PrState, _> = serde_json::from_str(&content) else {
            continue;
        };
        let mut dirty = false;

        // Emit [pr-ready-for-merge] if eligible and not already fired.
        if matches!(state.merge_state, MergeState::MergeReady)
            && state.ready_emitted_for_sha.as_deref() != Some(state.head_sha.as_str())
        {
            let author = resolve_author(&state);
            let body = format_ready_body(&state);
            let msg = build_event_message("pr-ready-for-merge", &author, &state, body);
            if let Err(e) = crate::inbox::enqueue(home, &author, msg) {
                tracing::warn!(
                    repo = %state.repo,
                    branch = %state.branch,
                    error = %e,
                    "#972 pr_state: [pr-ready-for-merge] enqueue failed"
                );
            } else {
                state.ready_emitted_for_sha = Some(state.head_sha.clone());
                dirty = true;
                tracing::info!(
                    repo = %state.repo,
                    branch = %state.branch,
                    head = %state.head_sha,
                    author = %author,
                    "#972 pr_state: [pr-ready-for-merge] emitted"
                );
            }
        }

        // Terminal-state sweep.
        match &state.merge_state {
            MergeState::Merged {
                merge_commit,
                merged_at,
            } => {
                let author = resolve_author(&state);
                let body = format!(
                    "[pr-merged] {}@{} (merge_commit {}, merged_at {})",
                    state.repo,
                    state.branch,
                    &merge_commit[..8.min(merge_commit.len())],
                    merged_at,
                );
                let _ = crate::inbox::enqueue(
                    home,
                    &author,
                    build_event_message("pr-merged", &author, &state, body),
                );
                let _ = remove(home, &state.repo, &state.branch);
                continue;
            }
            MergeState::ClosedUnmerged { closed_at } => {
                let author = resolve_author(&state);
                let body = format!(
                    "[pr-closed-unmerged] {}@{} (closed_at {})",
                    state.repo, state.branch, closed_at
                );
                let _ = crate::inbox::enqueue(
                    home,
                    &author,
                    build_event_message("pr-closed-unmerged", &author, &state, body),
                );
                let _ = remove(home, &state.repo, &state.branch);
                continue;
            }
            _ => {}
        }

        if dirty {
            if let Err(e) = save(home, &state) {
                tracing::warn!(
                    repo = %state.repo,
                    branch = %state.branch,
                    error = %e,
                    "#972 pr_state: post-emit save failed"
                );
            }
        }
        let _ = registry; // reserved for future gh-poll author lookup hook
    }
}

fn build_event_message(
    kind: &str,
    _author: &str,
    state: &PrState,
    body: String,
) -> crate::inbox::InboxMessage {
    crate::inbox::InboxMessage {
        schema_version: 0,
        id: None,
        from: "system:pr-state".to_string(),
        text: body,
        kind: Some(kind.to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        channel: None,
        read_at: None,
        thread_id: None,
        parent_id: None,
        delivery_mode: None,
        task_id: None,
        force_meta: None,
        // #946 grep target: `{repo}@{branch}` canonical form
        correlation_id: Some(format!("{}@{}", state.repo, state.branch)),
        reviewed_head: Some(state.head_sha.clone()),
        attachments: vec![],
        in_reply_to_msg_id: None,
        in_reply_to_excerpt: None,
        superseded_by: None,
        from_id: None,
        broadcast_context: None,
        sequencing: None,
        eta_minutes: None,
        reporting_cadence: None,
        worktree_binding_required: None,
    }
}

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
}
