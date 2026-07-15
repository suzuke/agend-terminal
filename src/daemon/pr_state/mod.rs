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
//! Two authoritative entry points fire from existing daemon code:
//!
//! - [`record_ci_result`] — called from
//!   `src/daemon/ci_watch/poller.rs` right after the existing
//!   `[ci-ready-for-action]` emission. Records `CiState::Green` or
//!   `CiState::Failed { conclusion }` against the observed head SHA.
//! - [`record_validated_receipt`] — called only by the unified messaging API
//!   after it has constructed an assignment-bound typed receipt. Legacy
//!   name/SHA verdict rows remain display-only and cannot open the merge gate.
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
mod freshness_populator;
pub mod gh_poll;
pub mod ready_gate;
mod remote_gc;
mod scanner;

// #2502: ci-ready emit-gate predicates live in `ready_gate` (extracted to keep
// this file under its anti-monolith ceiling); re-exported so existing call sites
// resolve unchanged.
pub use ready_gate::{is_ci_ready_merge_blocked, is_ci_ready_terminal_at_head};
pub(crate) mod verdict_buffer;
// #986: the production per-tick handler drives the scanner with an explicit
// (snapshot) poller via `scan_and_emit_with` — the old `scan_and_emit` wrapper
// (hardcoded `CliGhPoller`, synchronous on the scanner thread) is gone.
pub(crate) use scanner::scan_and_emit_with;

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
    /// task66: the only merge-authoritative review evidence. Legacy collapsed
    /// `verdict_state` remains display-compatible but cannot unlock the gate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) validated_review_receipts: Vec<crate::review_receipt::ReviewReceiptSummary>,
    pub merge_state: MergeState,
    pub draft_state: DraftState,
    pub review_class: ReviewClass,
    /// Debounce key for `[pr-ready-for-merge]` — when ready_emitted_for_sha
    /// equals current head_sha, the event was already fired and won't
    /// re-emit until head_sha changes.
    #[serde(default)]
    pub ready_emitted_for_sha: Option<String>,
    /// #2745 debounce key for the `[review-class-unresolved]` diagnostic — the
    /// fail-closed analogue of [`Self::ready_emitted_for_sha`]. Set to the
    /// head_sha once the "CI-green + VERIFIED but review_class is Unresolved"
    /// re-arm diagnostic has fired, so the scanner emits it at most once per sha
    /// (cleared on head advance). Kept SEPARATE from `ready_emitted_for_sha` so it
    /// never interferes with the terminal-replay suppression that field drives.
    #[serde(default)]
    pub diagnostic_emitted_for_sha: Option<String>,
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
    /// #2131: a `state=CLOSED + mergedAt=None` observation is AMBIGUOUS under
    /// squash-merge eventual consistency — gh transiently reports it before the
    /// merge-commit association lands (mergedAt flips). Set when the FIRST such
    /// observation is seen and the classification is DEFERRED one poll; a
    /// subsequent poll that STILL reports closed-unmerged confirms it (emit), while
    /// a `MERGED`/reopened observation clears it. Mirrors the merged-terminal
    /// "two consecutive observations" gate. `#[serde(default)]` = false for state
    /// files written before this field existed.
    #[serde(default)]
    pub closed_unmerged_pending: bool,
    /// #2749 (task t-…-9, decision d-20260712092257798199-17): deterministic
    /// latest-main ancestry freshness cache, stamped by the OFF-TICK ci-watch
    /// background poller (never on the scanner tick). The scanner reads this
    /// tuple READ-ONLY and trusts it ONLY when `freshness_checked_head_sha ==
    /// head_sha` AND `freshness_checked_base_sha == <current base>` AND it is
    /// within TTL AND `!freshness_error`. A head OR base move invalidates it
    /// (head-only keying was rejected: a base advance with the head unchanged is
    /// exactly the #2749 stale case). Unknown/stale/error ⇒ pr-ready is
    /// suppressed FAIL-CLOSED (never mislabeled pr-needs-rebase); #2747's
    /// exact-head merge gate remains the hard backstop. `#[serde(default)]` so
    /// pre-existing state files load with an empty (unknown) cache.
    #[serde(default)]
    pub freshness_checked_head_sha: Option<String>,
    #[serde(default)]
    pub freshness_checked_base_sha: Option<String>,
    #[serde(default)]
    pub freshness_checked_at: Option<String>,
    #[serde(default)]
    pub freshness_behind_by: Option<u64>,
    #[serde(default)]
    pub freshness_error: bool,
    /// #2749 correction (codex): a persisted 60s retry lease (RFC3339 deadline)
    /// for a FAILED ancestry compare on the exact observed tuple — bounds a
    /// persistently-failing forge to ONE compare per lease (not one per 15s worker
    /// cycle). Cleared on a successful compare, or when the observed (head/base)
    /// tuple changes (the errored tuple is then stale). `#[serde(default)]`.
    #[serde(default)]
    pub freshness_retry_after: Option<String>,
    /// #2749 CORRECTION 3 (codex R2): the ATOMIC observed (head, base) pair,
    /// written together in ONE `apply_gh_poll` from a single
    /// `gh pr view --json headRefOid,baseRefOid` response — never composed
    /// across two independent reads (that torn snapshot was the R2 defect).
    /// This is the INDEPENDENT base authority the read-only gate compares the
    /// populator's CHECKED tuple against: a main advance bumps
    /// `observed_base_sha` while `freshness_checked_base_sha` still holds the
    /// old base ⇒ mismatch ⇒ suppress-ready until the off-tick populator
    /// rechecks. The FINAL gate requires three heads to agree
    /// (`head_sha == observed_head_sha == freshness_checked_head_sha`) AND
    /// `observed_base_sha == freshness_checked_base_sha`. On gh_poll failure
    /// `observed_error` is set and `observed_at` is NOT advanced (the last-good
    /// pair is preserved, not clobbered) so the gate closes immediately.
    /// `#[serde(default)]` so pre-existing state files load with an empty
    /// (unknown) observation.
    #[serde(default)]
    pub observed_head_sha: Option<String>,
    #[serde(default)]
    pub observed_base_sha: Option<String>,
    #[serde(default)]
    pub observed_at: Option<String>,
    #[serde(default)]
    pub observed_error: bool,
    /// t-…-17 C13: required reviewers whose assignment is RESERVED-but-unverified,
    /// DERIVED by the per-tick reconciler (a LATER slice) from the assignment
    /// authority (`assignment_authority.rs`). While ANY entry is present
    /// [`is_merge_ready`] returns false — a required reviewer is reserved but not
    /// yet Verified — yet reserved entries are NEVER counted toward
    /// `required_verified_count` and NEVER pushed into `VerdictState::Verified`
    /// (plan §3(l)/I17). Additive serde (`default` + `skip_serializing_if`) ⇒
    /// legacy state files are byte-identical. Population is a LATER slice; this
    /// slice adds only the field + the gate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) reserved_assignments: Vec<ReservedAssignment>,
    /// t-…-17 B4 (codex m-…-322): FAIL-CLOSED merge-gate flag. SET by the
    /// [`record_ci_result`] A6 drain whenever the branch's assignment authority
    /// could NOT be read reliably — a corrupt/unreadable record, an
    /// exists-but-unreadable branch dir, or a required assignment-lock acquisition
    /// failure — so the reserved derivation could not be trusted. While set,
    /// [`is_merge_ready`] returns false unconditionally. This closes the
    /// sole-corrupt-record fail-open: a branch whose ONLY record is corrupt looks
    /// assignment-free to a lossy record read, so without the tri-state
    /// [`crate::daemon::assignment_authority::probe_branch_authority`] the A6 drain
    /// would derive an EMPTY reserved set on a fresh state and OPEN the gate. CLEARED
    /// only by a successful locked-or-genuinely-absent derive.
    /// Additive serde (`default` + skip-when-false) ⇒ legacy state files load
    /// byte-identical; an old file without the field defaults `false` and is
    /// re-derived on the next CI observation.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) authority_unknown: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// t-…-17 C13: one RESERVED-but-unverified required reviewer on a `PrState`. Carries
/// the full-typed authority (`target`, `review_author`, `assignment_id`) so the
/// gate/diagnostic can name the holder. `review_author` reuses the SINGLE shared
/// [`crate::mcp::handlers::comms_gates::ReviewAuthor`] principal (no shadow copy).
/// `pub(crate)` (not `pub`) because that principal is itself `pub(crate)` — a `pub`
/// wrapper would leak a more-private type (rustc `private_interfaces`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ReservedAssignment {
    pub target: String,
    pub review_author: crate::mcp::handlers::comms_gates::ReviewAuthor,
    pub assignment_id: uuid::Uuid,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ReviewClass {
    /// §3.6 — single VERIFIED triggers MergeReady.
    Single,
    /// §3.5 — two VERIFIED required.
    Dual,
    /// #2745: the merge-authority review class was ABSENT / UNKNOWN / MISMATCHED at
    /// arm/parse time — i.e. never explicitly resolved to Single|Dual. FAIL-CLOSED:
    /// never merge-ready; the emitter raises an actionable diagnostic instead of a
    /// premature `[pr-ready-for-merge]`. Distinguished from an *explicit* Single so an
    /// omitted intent can never silently take the least-ceremony path.
    Unresolved,
}

impl ReviewClass {
    pub fn required_verified_count(&self) -> usize {
        match self {
            ReviewClass::Single => 1,
            ReviewClass::Dual => 2,
            // #2745: `Unresolved` is never satisfiable — `is_merge_ready` rejects it
            // outright before this is consulted; `usize::MAX` is a defensive backstop
            // so even a forgotten guard can never meet the threshold.
            ReviewClass::Unresolved => usize::MAX,
        }
    }

    /// Display / wire token for this class: `single` / `dual` / `unresolved`.
    /// The `single`/`dual` tokens are also the exact watch `review_class` values
    /// [`parse_fail_closed`] round-trips.
    pub fn as_token(&self) -> &'static str {
        match self {
            ReviewClass::Single => "single",
            ReviewClass::Dual => "dual",
            ReviewClass::Unresolved => "unresolved",
        }
    }

    /// Parse a watch/dispatch `review_class` string to the typed class, FAIL-CLOSED:
    /// only the exact lowercased tokens `single`/`dual` resolve; anything else
    /// (absent → `None`, empty, or an unknown/typo'd value) is [`Unresolved`].
    /// The single source of truth for the watch/dispatch string → `ReviewClass`
    /// mapping — the poller (`record_ci_result` feed) and the test-only
    /// `parse_review_class(&Value)` wrapper both route through it.
    pub fn parse_fail_closed(raw: Option<&str>) -> ReviewClass {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("single") => ReviewClass::Single,
            Some("dual") => ReviewClass::Dual,
            _ => ReviewClass::Unresolved,
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

/// The reducer's 2-state view of a CI outcome — `Green` vs `Failed` — used
/// to drive `CiState` transitions, where a cancelled run and a failed run
/// are not actionably different (neither is "your turn").
///
/// Distinct from [`crate::daemon::ci_watch::poller::CiOutcome`], the
/// poller's 3-state view — it keeps a separate `Other` case because the
/// poller's notification/aggregation layer displays cancelled/timed_out/etc.
/// VERBATIM (`[ci-ended] …: {other}`) rather than reporting them as a
/// failure. Do NOT merge `CiOutcome` into this enum: collapsing `Other`
/// into `Failed` is correct for the reducer but would change the poller's
/// notification text. Derived from the Pattern-A follow-up spike, gapfix
/// task t-20260621072505708315-50793-7.
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
                state.diagnostic_emitted_for_sha = None;
                state
                    .validated_review_receipts
                    .retain(|r| r.reviewed_head == head_sha);
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

/// #2079: match a reviewer-asserted SHA against a full canonical head SHA,
/// tolerating an ABBREVIATED prefix. `full` is the canonical head (gh-poll /
/// CI always supply the 40-char SHA); `asserted` is what a reviewer's report
/// carried — gh and humans routinely abbreviate to 7–12 chars (the #2078 case:
/// `7e1d422` silently buffered to its 24h TTL because the exact `==` never met
/// the full-SHA-keyed drain).
///
/// Exact equality when the strings match. Otherwise `asserted` must be a HEX
/// prefix of `full`, ≥7 chars (git's abbreviation floor; collisions across a
/// single repo's PR set are negligible — noted in #2079). A non-hex or
/// <7-char `asserted` (e.g. a test's `"sha-A"`) gets NO loosening — it falls
/// back to exact equality, so this can never widen a non-SHA comparison.
#[cfg(test)]
pub(crate) fn sha_prefix_match(full: &str, asserted: &str) -> bool {
    if full == asserted {
        return true;
    }
    let n = asserted.len();
    (7..40).contains(&n)
        && asserted.bytes().all(|b| b.is_ascii_hexdigit())
        && full.len() > n
        && full.starts_with(asserted)
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
    // t-…-17 B4 (codex m-…-322): the assignment authority was UNREADABLE at the last
    // A6 drain (a corrupt/unreadable record, an exists-but-unreadable branch dir, or a
    // required assignment-lock failure), so the reserved derivation could NOT be
    // trusted. FAIL CLOSED. This is the explicit "authority unreadable ⇒ close" state
    // that closes the sole-corrupt-record fail-open: a branch whose only record is
    // corrupt looks assignment-free to the lossy `has_active`, so the drain would
    // otherwise derive an EMPTY reserved set on a fresh state and OPEN this gate.
    if state.authority_unknown {
        return false;
    }
    // t-…-17 C13: a required reviewer whose assignment is RESERVED-but-unverified
    // holds the PR closed. Reserved entries are DERIVED (excl. Satisfied) by the
    // reconciler and are NEVER counted toward `required_verified_count` / pushed
    // into `VerdictState::Verified` — they only gate here (plan §3(l)/I17).
    if !state.reserved_assignments.is_empty() {
        return false;
    }
    // #2745 fail-closed: an `Unresolved` review_class (intent ABSENT / UNKNOWN /
    // MISMATCHED at arm time) is NEVER merge-ready — no verdict count can satisfy
    // it. The scanner raises an actionable diagnostic in place of pr-ready.
    if matches!(state.review_class, ReviewClass::Unresolved) {
        return false;
    }
    if matches!(state.draft_state, DraftState::Draft) {
        return false;
    }
    let CiState::Green { sha: ci_sha, .. } = &state.ci_state else {
        return false;
    };
    if ci_sha != &state.head_sha {
        return false;
    }
    let current: Vec<_> = state
        .validated_review_receipts
        .iter()
        .filter(|r| r.matches_state(state))
        .collect();
    if current
        .iter()
        .any(|r| !matches!(r.verdict, crate::review_receipt::ReviewVerdict::Verified))
    {
        return false;
    }
    let distinct_reviewers: std::collections::HashSet<_> = current
        .iter()
        .filter(|r| matches!(r.verdict, crate::review_receipt::ReviewVerdict::Verified))
        .map(|r| r.reviewer_instance_id)
        .collect();
    if distinct_reviewers.len() < state.review_class.required_verified_count() {
        return false;
    }
    // Receipt validation is exact-full-head; prefixes are never authoritative.
    current.iter().all(|r| r.reviewed_head == state.head_sha)
}

/// #2749 read-only freshness gate outcome (decision d-20260712092257798199-17).
/// Derived PURELY from the PrState freshness cache tuple (`freshness_checked_*`)
/// plus the atomic observed pair (`observed_*`). Computing this NEVER runs a
/// `provider.compare` — that ancestry work is the OFF-TICK populator's job, so
/// the per-tick scanner path stays free of a compare storm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FreshnessGate {
    /// Ancestry PROVEN fresh at the current head (behind_by == 0): the PR may
    /// announce `[pr-ready-for-merge]`.
    Fresh,
    /// Ancestry PROVEN stale — a valid, agreeing tuple with behind_by > 0:
    /// suppress pr-ready and durably emit `[pr-needs-rebase]`.
    Behind { behind_by: u64 },
    /// Ancestry UNPROVEN — unknown (pre-populator), torn observation (the three
    /// heads disagree or the checked/observed bases disagree), stale past TTL, or
    /// a compare/observe error. FAIL CLOSED: suppress pr-ready and emit NOTHING
    /// (never mislabel as pr-needs-rebase). #2747's exact-head merge gate is the
    /// hard backstop; the off-tick populator refreshes the tuple next cycle.
    Suppress,
}

/// #2749 freshness cache TTL — the ε staleness bound. A checked tuple or an
/// observation older than this (relative to the gate's `now`) is treated as
/// stale and the gate fails closed. Sized generously above the off-tick
/// populator cadence so a single skipped refresh cycle does not needlessly
/// suppress a genuinely-fresh pr-ready. Documented ε = populator cadence +
/// FRESHNESS_TTL_SECS.
pub const FRESHNESS_TTL_SECS: i64 = 600;

/// #2749 the read-only three-way freshness gate (CORRECTION 3 / codex R2). A
/// MergeReady PR may announce pr-ready ONLY when deterministic latest-main
/// ancestry is PROVEN fresh at the current head. Requires ALL of:
/// - three heads agree: `head_sha == observed_head_sha == freshness_checked_head_sha`
///   (ci_head == the atomically-observed head == the head the compare used);
/// - the CHECKED base equals the INDEPENDENTLY-observed base
///   (`freshness_checked_base_sha == observed_base_sha`). A main advance bumps
///   `observed_base_sha` (via gh_poll) while the checked base still lags ⇒
///   mismatch ⇒ Suppress until the populator rechecks. This is what makes a
///   stale-but-self-consistent tuple fail closed — the core #2749 fix; a
///   tuple-only check (no independent base) cannot detect the main advance.
/// - neither the observation nor the compare errored;
/// - both the observation (`observed_at`) and the compare
///   (`freshness_checked_at`) are at-or-before `now` AND within `ttl_secs` of it
///   (a future timestamp fails closed — never treated as fresh).
///
/// Then `behind_by == 0 ⇒ Fresh`, `> 0 ⇒ Behind`. Any missing / mismatched /
/// stale / errored input ⇒ Suppress.
pub fn freshness_gate(
    state: &PrState,
    now: chrono::DateTime<chrono::Utc>,
    ttl_secs: i64,
) -> FreshnessGate {
    // Three heads must agree.
    let (Some(checked_head), Some(observed_head)) = (
        state.freshness_checked_head_sha.as_deref(),
        state.observed_head_sha.as_deref(),
    ) else {
        return FreshnessGate::Suppress;
    };
    if checked_head != state.head_sha || observed_head != state.head_sha {
        return FreshnessGate::Suppress;
    }
    // The checked base must equal the independently-observed base.
    let (Some(checked_base), Some(observed_base)) = (
        state.freshness_checked_base_sha.as_deref(),
        state.observed_base_sha.as_deref(),
    ) else {
        return FreshnessGate::Suppress;
    };
    if checked_base != observed_base {
        return FreshnessGate::Suppress;
    }
    // Neither signal errored.
    if state.observed_error || state.freshness_error {
        return FreshnessGate::Suppress;
    }
    // Both the observation and the compare must be at-or-before `now` AND within
    // TTL. #2749 review-fix R2 (codex): a FUTURE observed_at / freshness_checked_at
    // yields a NEGATIVE age, which a `<= ttl_secs` check accepted — a fail-OPEN that
    // let a clock-skewed / forged-future stamp read Fresh indefinitely. Compare at
    // FULL `chrono::Duration` precision: `num_seconds()` truncates toward zero, so a
    // SUB-second future stamp (+1..999ms) would collapse to age 0 and slip through
    // `0 <= age`. Require `Duration::zero() <= age <= Duration::seconds(ttl_secs)`
    // (strict fail-closed, no skew allowance). A negative `ttl_secs` short-circuits
    // false — an empty window can never admit Fresh.
    let within_ttl = |ts: Option<&str>| -> bool {
        if ttl_secs < 0 {
            return false;
        }
        let Some(ts) = ts else { return false };
        let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(ts) else {
            return false;
        };
        let age = now.signed_duration_since(parsed.with_timezone(&chrono::Utc));
        age >= chrono::Duration::zero() && age <= chrono::Duration::seconds(ttl_secs)
    };
    if !within_ttl(state.observed_at.as_deref())
        || !within_ttl(state.freshness_checked_at.as_deref())
    {
        return FreshnessGate::Suppress;
    }
    match state.freshness_behind_by {
        Some(0) => FreshnessGate::Fresh,
        Some(n) => FreshnessGate::Behind { behind_by: n },
        None => FreshnessGate::Suppress,
    }
}

// ─── storage ───────────────────────────────────────────────────────────

/// Canonical path to the PR-state directory.
pub fn pr_state_dir(home: &Path) -> PathBuf {
    home.join("pr-state")
}

/// #2059: true iff `path` is a per-branch `PrState` document — a `*.json` file
/// that is NOT a dotfile. The pr-state dir also holds the `.emitted-terminal.json`
/// terminal-latch ledger (a different schema) and `.lock` files; both share the
/// `.json`/no-extension space but are not `PrState`. Parsing `.emitted-terminal.json`
/// as `PrState` spammed a `missing field 'repo'` WARN on every gh-poll + scan tick
/// (~every 10 s). Every dir-scan that deserializes entries as `PrState` routes
/// through this predicate so the ledger/locks are skipped uniformly.
pub(crate) fn is_pr_state_file(path: &Path) -> bool {
    let is_dotfile = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.'));
    !is_dotfile && path.extension().and_then(|e| e.to_str()) == Some("json")
}

/// t-…-17 B4 (codex m-…-416): enumerate every LIVE `PrState`'s `{repo, branch}`
/// identity, read from the DESERIALIZED file CONTENT — NEVER from the lossy/hashed
/// filename. The per-tick reconciler UNIONs this with
/// [`crate::daemon::assignment_authority::active_branches`], which discovers a branch
/// only via its FIRST PARSEABLE authority record: a branch whose authority records are
/// ALL corrupt is invisible to `active_branches`, so it would VANISH from the workset
/// and `redrive_reserved` would never run — leaving `authority_unknown` stale-false and
/// the merge gate OPEN. Rediscovering it here via its readable PrState routes it back
/// through `redrive_reserved` (→ probe `Unreadable` → SET the fail-closed flag).
///
/// Mirrors the scanner's file filtering ([`scanner::scan_and_emit_with`]): only
/// `*.json`, and the `.emitted-terminal.json` ledger / `.lock` sidecars are skipped
/// via [`is_pr_state_file`]. A malformed/unreadable PrState is SURFACED
/// (`tracing::warn!` with the path) — observable, NEVER silently dropped. It is
/// inherently fail-closed: an unreadable PrState cannot be deserialized, so it can
/// never be declared merge-ready and cannot contribute an identity here either. Lock-free.
pub(crate) fn list_state_identities(home: &Path) -> Vec<(String, String)> {
    let dir = pr_state_dir(home);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!(
                dir = %dir.display(),
                error = %e,
                "t-…-17 B4: list_state_identities read_dir failed — reconciler PrState-identity source empty this tick"
            );
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_pr_state_file(&path) {
            continue; // #2059: skip .emitted-terminal.json ledger + .lock sidecars
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "t-…-17 B4: list_state_identities read failed — malformed/unreadable PrState SURFACED, not discovered this tick (fail-closed: an unreadable PrState is never merge-ready)"
                );
                continue;
            }
        };
        match serde_json::from_str::<PrState>(&content) {
            Ok(state) => out.push((state.repo, state.branch)),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "t-…-17 B4: list_state_identities parse failed — malformed PrState SURFACED, not discovered this tick (fail-closed: an unreadable PrState is never merge-ready)"
                );
            }
        }
    }
    out
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
    let Ok(content) = std::fs::read_to_string(&path) else {
        return None;
    };
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

/// #2800: ensure a PrState exists for a cold PR whose CI has not yet
/// reached terminal. Two-phase: (1) check local file; (2) if missing,
/// confirm PR identity against the SCM provider, then CAS-create with
/// `CiState::Pending`. The pr-state/file lock is NOT held across the
/// network call — the provider query runs unlocked, and
/// `with_pr_state_or_create` does the CAS afterwards. On a concurrent
/// creator the CAS converges: if the file already exists with matching
/// identity, the caller gets the existing state; a mismatch fails closed.
pub fn ensure_from_scm(
    home: &std::path::Path,
    repo: &str,
    branch: &str,
    pr_number: u64,
    expected_head: &str,
    review_class: ReviewClass,
) -> anyhow::Result<PrState> {
    // Phase 1: fast path — file already exists. Return it as-is and let
    // the caller do the identity check (preserves existing error codes).
    if let Some(existing) = load(home, repo, branch) {
        return Ok(existing);
    }

    // Phase 2: no local file — confirm identity against SCM provider.
    let provider = crate::scm::make_scm_provider(repo, None);
    let pr = provider
        .pr_view(repo, pr_number, &["number", "headRefOid", "headRefName"])
        .map_err(|e| anyhow::anyhow!("SCM pr_view failed for PR #{pr_number}: {e}"))?;
    let scm_head = pr
        .head_ref_oid
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("SCM returned no headRefOid for PR #{pr_number}"))?;
    if !scm_head.eq_ignore_ascii_case(expected_head) {
        anyhow::bail!(
            "SCM head mismatch for PR #{pr_number}: expected {expected_head}, SCM reports {scm_head}"
        );
    }

    // Phase 3: CAS-create with Pending CI.
    let state = with_pr_state_or_create(
        home,
        repo,
        branch,
        || {
            let mut s = new_for_branch(repo, branch, expected_head, review_class);
            s.pr_number = pr_number;
            s.pr_author = pr.author_login.unwrap_or_default();
            s
        },
        |s| s.clone(),
    )?;
    if state.head_sha != expected_head || state.pr_number != pr_number {
        anyhow::bail!(
            "concurrent pr-state creator wrote mismatching identity \
             (head={}, pr={}); expected head={expected_head}, pr={pr_number}",
            state.head_sha,
            state.pr_number
        );
    }
    Ok(state)
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

/// #1907 teardown audit: scrub a deleted instance's name from every pr_state
/// file's `subscribers` list. A deleted agent left in a subscriber array would
/// route PR events ([ci-ready]/[pr-ready]) at a vacant or same-name-redeployed
/// slot — the same per-instance-residual class the other cascade cleanups
/// (ci_watch / dispatch_tracking) already close. This store had NO per-instance
/// cleanup before. Best-effort, flock-serialized per file (via [`with_pr_state`]).
/// Returns the number of files mutated.
pub fn cleanup_subscribers_for_instance(home: &Path, name: &str) -> usize {
    let dir = pr_state_dir(home);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let mut mutated = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_pr_state_file(&path) {
            continue;
        }
        // Value-based, NOT typed `PrState` parse: an audit/cleanup that silently
        // skips a file it cannot fully deserialize is itself a leak-hiding bug (the
        // #1902 "the oracle was leaky" lesson) — a schema-drifted or partially-
        // written pr_state file that still names the deleted instance MUST be
        // scrubbed. Flock-serialized on the same `<file>.lock` path `with_json_state`
        // uses, so a concurrent gh-poll RMW cannot race this.
        let lock_path = path.with_extension("lock");
        let _lock = match crate::store::acquire_file_lock(&lock_path) {
            Ok(l) => l,
            Err(_) => continue,
        };
        let Some(mut v) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        else {
            continue;
        };
        let Some(subs) = v.get_mut("subscribers").and_then(|s| s.as_array_mut()) else {
            continue;
        };
        let before = subs.len();
        subs.retain(|s| s.as_str() != Some(name));
        if subs.len() == before {
            continue;
        }
        if crate::store::atomic_write(
            &path,
            serde_json::to_string_pretty(&v)
                .unwrap_or_default()
                .as_bytes(),
        )
        .is_ok()
        {
            mutated += 1;
        }
    }
    if mutated > 0 {
        tracing::info!(%name, count = mutated, "#1907: scrubbed deleted instance from pr_state subscribers");
    }
    mutated
}

/// #1907 teardown audit: does any pr_state file still list `name` as a
/// subscriber? Value-based to mirror [`cleanup_subscribers_for_instance`] — it
/// must detect the name even in a file that no longer parses as a full `PrState`,
/// otherwise a malformed-but-name-bearing file is a silent residual.
pub fn has_subscriber(home: &Path, name: &str) -> bool {
    let dir = pr_state_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_pr_state_file(&path) {
            continue;
        }
        let listed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .and_then(|v| {
                v.get("subscribers")
                    .and_then(|s| s.as_array())
                    .map(|arr| arr.iter().any(|s| s.as_str() == Some(name)))
            })
            .unwrap_or(false);
        if listed {
            return true;
        }
    }
    false
}

/// Fixed 1h upper bound for "stale terminal state" classification at daemon
/// boot — see [`suppress_stale_terminal_replay`]. (#env-cleanup: was
/// env-overridable via `AGEND_PR_STATE_REPLAY_AGE_HOURS`; demoted to YAGNI.)
fn replay_age_threshold() -> std::time::Duration {
    const DEFAULT_HOURS: u64 = 1;
    std::time::Duration::from_secs(DEFAULT_HOURS.saturating_mul(3600))
}

/// #1017: at daemon boot, mark terminal-state pr-state files whose
/// mtime is older than the fixed 1h replay-age threshold
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
        if !is_pr_state_file(&path) {
            continue; // #2059: skip .emitted-terminal.json ledger + .lock sidecars
        }
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
        validated_review_receipts: Vec::new(),
        merge_state: MergeState::NotReady,
        draft_state: DraftState::Ready,
        review_class,
        ready_emitted_for_sha: None,
        diagnostic_emitted_for_sha: None,
        auto_armed: false,
        auto_armed_for_sha: None,
        auto_armed_at: None,
        // #986 gh-poll observation fields — populated on first
        // scanner pass post-creation by gh_poll::CliGhPoller.
        last_gh_poll_at: None,
        gh_poll_failures: 0,
        last_gh_state: None,
        closed_unmerged_pending: false,
        // #2749: fresh state has an empty (unknown) freshness cache — the
        // off-tick ci-watch poller stamps it on a later background cycle.
        freshness_checked_head_sha: None,
        freshness_checked_base_sha: None,
        freshness_checked_at: None,
        freshness_behind_by: None,
        freshness_error: false,
        freshness_retry_after: None,
        // #2749: fresh state has an empty (unknown) observation — gh_poll
        // stamps observed_head_sha + observed_base_sha atomically on a later
        // pass; until then the read-only gate fails closed.
        observed_head_sha: None,
        observed_base_sha: None,
        observed_at: None,
        observed_error: false,
        reserved_assignments: Vec::new(),
        authority_unknown: false,
        created_at: now.clone(),
        updated_at: now,
    }
}

/// #2745 R2/R3 (root R1 + R2 findings): reconcile a persisted `review_class` with
/// the current watch-resolved class on each CI observation. FAIL-CLOSED, and Dual
/// is a MONOTONIC FLOOR so no watch SEQUENCE can weaken it:
/// - persisted `Dual` → stays `Dual` for ANY ordinary watch (Single / Unresolved
///   included). Closes the R2 two-observation bypass (Dual→Unresolved→Single):
///   a stale/missing obs can't launder the Dual floor so a later Single re-arm
///   drops the gate to one reviewer. An intentional downgrade is a separate
///   audited reset, never an ordinary observation.
/// - persisted `Unresolved` → adopt the watch class (operator re-arm recovery —
///   what makes the "re-arm with review_class=…" diagnostic close the loop).
/// - persisted `Single` + watch `Unresolved` → `Unresolved` (legacy/typo CURRENT
///   watch → fail-closed inventory; makes the migration bite pre-existing `Single`
///   state files the old poller collapsed).
/// - persisted `Single` + watch `Single`/`Dual` → adopt (strengthen or unchanged).
///
/// Head advance does NOT reset the class (the review threshold is stable across a
/// force-push); the next observation re-reconciles from the (unchanged) watch, so
/// stale head-advance input cannot weaken the gate either.
pub(crate) fn reconcile_review_class(persisted: ReviewClass, watch: ReviewClass) -> ReviewClass {
    match (persisted, watch) {
        // R2 finding 1 — Dual is a MONOTONIC FLOOR: once Dual, ANY ordinary watch
        // (Single / Unresolved) keeps Dual. This closes the two-observation bypass
        // (Dual → Unresolved → Single) where a stale/missing obs launders the floor
        // so a later Single re-arm downgrades a 2-reviewer gate to 1. An intentional
        // downgrade is a separate audited reset, never an ordinary observation.
        (ReviewClass::Dual, _) => ReviewClass::Dual,
        // Recovery: a not-yet-resolved gate adopts whatever the (re-arm) watch declares.
        (ReviewClass::Unresolved, w) => w,
        // A Single gate whose CURRENT watch can't resolve → Unresolved (legacy/typo
        // inventory; makes the migration bite pre-existing Single state files).
        (ReviewClass::Single, ReviewClass::Unresolved) => ReviewClass::Unresolved,
        // Single → Single/Dual: adopt (strengthen or unchanged).
        (ReviewClass::Single, w) => w,
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
/// #2745 R2: RECONCILED onto the existing state on EVERY observation via
/// [`reconcile_review_class`] (fail-closed, no-weaken) — NOT create-only — so an
/// operator re-arm actually recovers a persisted `Unresolved`, and a legacy
/// `Single` state gets inventoried when the current watch resolves `Unresolved`.
pub fn record_ci_result(
    home: &Path,
    repo: &str,
    branch: &str,
    head_sha: &str,
    conclusion: CiConclusion<'_>,
    subscribers: Vec<String>,
    review_class: ReviewClass,
) {
    // t-…-17 A6 (I11/I15/I16): hold the reviewer-assignment branch lock as the OUTER
    // lock of the replay below (the pr_state flock `with_pr_state_or_create` takes is
    // the INNER lock — the mandated assignment-OUTER / pr_state-INNER order), so the
    // `reserved_assignments` derivation is consistent with a concurrent
    // revoke/transfer/tombstone. Acquired when the branch is ACTIVE, or when a
    // typed-buffer hint requires safe select/persist/commit revalidation. Ordinary
    // assignment-free branches still create no empty store dir. If acquisition
    // fails the replay FAILS CLOSED; the reconciler retries later.
    // t-…-17 B4 (codex m-…-322): probe the branch authority with the TRI-STATE probe,
    // NOT the lossy `has_active`. `has_active` DROPS a corrupt record's `Err`, so a
    // branch whose SOLE record is corrupt looks assignment-free — replay then
    // derives an EMPTY reserved set on a fresh state and OPENs the merge gate. The
    // probe reports `Unreadable` for that case so replay fails closed (below).
    let initial_authority =
        crate::daemon::assignment_authority::probe_branch_authority(home, repo, branch);
    let typed_buffer_hint =
        verdict_buffer::has_validated_subject_hint(home, repo, branch, head_sha);
    // A typed buffered receipt requires the lock even when the first probe says
    // Absent: acquire before selection, then re-probe under lock.
    // This consumes revoked receipts safely without creating lock sidecars for
    // ordinary assignment-free branches.
    let _assignment_lock = if matches!(
        initial_authority,
        crate::daemon::assignment_authority::BranchAuthority::Active
    ) || typed_buffer_hint
    {
        crate::daemon::assignment_authority::lock_branch_for_drain(home, repo, branch)
    } else {
        None
    };
    // Whether the OUTER assignment lock is actually held — captured (Copy) so the replay
    // closure need not borrow the guard. A lock-free derivation on an `Active` branch
    // could read a torn reserved set that CLEARS the gate, so it is refused fail-closed.
    let lock_acquired = _assignment_lock.is_some();
    let authority = if lock_acquired {
        crate::daemon::assignment_authority::probe_branch_authority(home, repo, branch)
    } else {
        initial_authority
    };
    let mut selected_receipts = Vec::new();
    let save_result = with_pr_state_or_create(
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
            // #2745 R2 (root R1 finding): reconcile the persisted class from the
            // current watch-resolved authority (fail-closed, no-weaken) so re-arm
            // recovery + legacy-file inventory actually close the loop — this was
            // create-only before, stranding persisted Unresolved/legacy-Single. On a
            // transition OUT of Unresolved, clear the diagnostic debounce so the
            // pr-ready flow takes over at this head.
            let was_unresolved = matches!(state.review_class, ReviewClass::Unresolved);
            state.review_class = reconcile_review_class(state.review_class, review_class);
            if was_unresolved && !matches!(state.review_class, ReviewClass::Unresolved) {
                state.diagnostic_emitted_for_sha = None;
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
            // t-…-17 A6 (I13/I16): DRAIN — after the head is applied (above) and
            // BEFORE the typed-receipt buffer replay (below), REPLACE the whole
            // `reserved_assignments` vec with the full-typed derivation from the
            // authority store: every active record whose stored pr_number equals THIS
            // state's pr_number and whose evidence is NOT SatisfiedExactHead. This is
            // DECLARATIVE + convergent (a re-run yields the same set) and reads the
            // store lock-free under the OUTER assignment lock held above. Reserved
            // entries are NEVER counted toward `required_verified_count` / pushed into
            // `VerdictState::Verified` — they only hold `is_merge_ready` closed
            // (I17). A freshly-created state still carries pr_number 0 (gh-poll fills
            // it later), matching no record (persist rejects pr_number 0), so the
            // reconciler backstops the reservation until the generation is known.
            // t-…-17 B4 (codex m-…-322 / m-…-378): set/clear the fail-closed
            // `authority_unknown` gate flag from the probe + derive outcome, via the
            // SHARED `apply_authority_transition` helper — the SAME transition the A10b
            // reconciler's `redrive_reserved` applies (codex m-…-378 closed the divergence
            // where the two paths differed). SET on corrupt/unreadable/required-lock-
            // failure; CLEARED only on a successful locked-or-absent derive. This is the
            // explicit "authority unreadable ⇒ close" state that closes the sole-corrupt-
            // record fail-open (`is_merge_ready` gates on it).
            crate::daemon::assignment_authority::apply_authority_transition(
                state,
                repo,
                branch,
                authority,
                lock_acquired,
                |state| {
                    crate::daemon::assignment_authority::derive_reserved_for_prstate(
                        home, repo, branch, state,
                    )
                },
            );
            // A typed receipt may have preceded creation of this exact PR state.
            // Select only while the assignment lock is held, persist the PR state,
            // then exact-content commit below. A failed lock or save leaves the row
            // retryable, and revoke/transfer cannot cross revalidation→mutation.
            // The legacy name+SHA namespace is never read here.
            if lock_acquired
                && !matches!(
                    authority,
                    crate::daemon::assignment_authority::BranchAuthority::Unreadable
                )
            {
                let selections = verdict_buffer::select_validated_for_subject(
                    home,
                    repo,
                    branch,
                    state.pr_number,
                    head_sha,
                );
                for selection in &selections {
                    let receipt = selection.receipt();
                    if !crate::review_receipt::assignment_still_authorizes(home, receipt)
                        || !receipt.matches_state(state)
                        || receipt_seen(state, receipt)
                    {
                        continue;
                    }
                    tracing::info!(
                        repo = %repo,
                        branch = %branch,
                        head = %head_sha,
                        reviewer = %receipt.reviewer_name,
                        receipt_id = %receipt.receipt_id,
                        "task66 verdict_buffer: replaying validated receipt onto exact subject"
                    );
                    apply_receipt_to_state(state, receipt.clone());
                }
                selected_receipts = selections;
            }
        },
    );
    match save_result {
        Ok(()) => verdict_buffer::commit_validated_selections(&selected_receipts),
        Err(e) => {
            tracing::warn!(
                repo = %repo,
                branch = %branch,
                error = %e,
                "#972 pr_state: record_ci_result save failed"
            );
        }
    }
    // Keep assignment-OUTER held through the post-save exact-content commit.
    drop(_assignment_lock);
}

fn receipt_seen(state: &PrState, receipt: &crate::review_receipt::ReviewReceiptSummary) -> bool {
    state.validated_review_receipts.iter().any(|existing| {
        existing.receipt_id == receipt.receipt_id || existing.source_id == receipt.source_id
    })
}

fn apply_receipt_to_state(
    state: &mut PrState,
    receipt: crate::review_receipt::ReviewReceiptSummary,
) {
    let reviewer = receipt.reviewer_name.clone();
    let head = receipt.reviewed_head.clone();
    let verdict = receipt.verdict;
    let assignment_id = receipt.assignment_id;
    // Containment (not task68's append-only/worst-verdict ledger): one current
    // authoritative receipt per assignment/slot. A later independently validated
    // generation replaces the collapsed current view.
    state
        .validated_review_receipts
        .retain(|old| old.assignment_id != receipt.assignment_id && old.slot != receipt.slot);
    state.validated_review_receipts.push(receipt);
    if matches!(verdict, crate::review_receipt::ReviewVerdict::Verified) {
        state
            .reserved_assignments
            .retain(|reserved| reserved.assignment_id != assignment_id);
    }
    let kind = match verdict {
        crate::review_receipt::ReviewVerdict::Verified => VerdictKind::Verified,
        crate::review_receipt::ReviewVerdict::Rejected => VerdictKind::Rejected { reason: None },
        crate::review_receipt::ReviewVerdict::Unverified => VerdictKind::Unverified,
    };
    // Legacy collapsed state remains display-compatible only; is_merge_ready and
    // assignment evidence consume validated_review_receipts instead.
    apply(
        state,
        Event::VerdictObserved {
            reviewer: &reviewer,
            reviewed_head: &head,
            kind,
        },
    );
}

/// Typed verdict ingestion entry. Exact repo/branch/PR/full-head selection comes
/// only from the server-validated assignment receipt; there is no SHA scan.
/// Returns true only for the first accepted receipt/source identity.
pub(crate) fn record_validated_receipt(
    home: &Path,
    receipt: &crate::review_receipt::ValidatedCodeReviewReceipt,
) -> bool {
    let summary = receipt.summary();
    // Keep the same assignment-OUTER / pr_state-INNER lock order used by the
    // CI drain. Authorization at the API sink precedes inbox delivery; a revoke
    // may race after that check. Re-acquire the exact subject's assignment lock
    // here and revalidate while it is held so revoke/transfer cannot cross the
    // final check→PR-state mutation boundary.
    let Some(_assignment_lock) = crate::daemon::assignment_authority::lock_branch_for_drain(
        home,
        &summary.repo,
        &summary.branch,
    ) else {
        tracing::warn!(
            repo = %summary.repo,
            branch = %summary.branch,
            "task66 validated receipt could not acquire assignment lock; failing closed"
        );
        return false;
    };
    if !crate::review_receipt::assignment_still_authorizes(home, summary) {
        return false;
    }
    let mut applied = false;
    let mut subject_found = false;
    let mut pending_notify: Option<(String, crate::inbox::InboxMessage)> = None;
    match with_pr_state(home, &summary.repo, &summary.branch, |state| {
        subject_found = true;
        if !summary.matches_state(state) || receipt_seen(state, summary) {
            return;
        }
        apply_receipt_to_state(state, summary.clone());
        let label = match summary.verdict {
            crate::review_receipt::ReviewVerdict::Verified => "VERIFIED",
            crate::review_receipt::ReviewVerdict::Rejected => "REJECTED",
            crate::review_receipt::ReviewVerdict::Unverified => "UNVERIFIED",
        };
        if !is_merge_ready(state) {
            let recipient = resolve_notify_recipient(home, state);
            let body = format_verdict_body(state, &summary.reviewer_name, label);
            let msg =
                crate::inbox::InboxMessage::new_system("system:pr-state", "review-verdict", body)
                    .with_correlation_id(format!("{}@{}", state.repo, state.branch))
                    .with_reviewed_head(state.head_sha.clone());
            pending_notify = Some((recipient, msg));
        }
        applied = true;
    }) {
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(
                repo = %summary.repo,
                branch = %summary.branch,
                error = %e,
                "task66 validated receipt state update failed closed"
            );
            return false;
        }
    }
    if !subject_found {
        return verdict_buffer::buffer_validated(home, summary);
    }
    drop(_assignment_lock);
    if let Some((recipient, msg)) = pending_notify {
        if let Err(e) = crate::inbox::enqueue_with_idle_hint(home, &recipient, msg) {
            tracing::warn!(recipient, error = %e, "task66 review-verdict notify enqueue failed");
        }
    }
    applied
}

/// Legacy test-only ingestion helper. Production has no raw name+SHA verdict
/// entry after task66; legacy durable evidence is display-only and cannot open
/// the merge gate.
#[cfg(test)]
/// Pre-task66 name+SHA ingestion model retained only for compatibility tests.
/// `task_id` is the old correlation id, kept only for logging. Production
/// messaging never calls this function and `VerdictState` is display-only.
///
/// #2059 #2(c): keyed on `reviewed_head` (the SHA the reviewer asserts they
/// reviewed), not the task→branch chain. Applies the verdict to the pr-state
/// whose `head_sha == reviewed_head`; if none exists yet (the verdict preceded
/// the first CI/gh-poll observation — the #2058 dead zone), the verdict is
/// BUFFERED in the legacy TTL namespace. Production never replays that namespace
/// after task66. Best-effort throughout — a failure never propagates into this
/// compatibility-only path.
pub(crate) fn record_verdict(
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
    // #2059 #2(c): key on `reviewed_head` (the SHA the reviewer asserts they
    // reviewed), NOT the task→branch chain. Gates B (task lookup) and C
    // (task.branch) are GONE: a review task usually carries no branch (the
    // #2058 dead zone), and the SHA is self-describing + branch-independent
    // (survives fork PRs, missing tasks, multi-reviewer). The owned kind
    // metadata is extracted up front so it can both drive the live apply and
    // be buffered verbatim if no pr-state exists at this SHA yet.
    let (kind_str, kind_reason): (&str, Option<&str>) = match kind {
        VerdictKind::Verified => ("verified", None),
        VerdictKind::Rejected { reason } => ("rejected", reason),
        VerdictKind::Unverified => ("unverified", None),
    };
    // Walk the pr-state directory and find the file whose head_sha matches the
    // reviewed_head (typically 0 or 1; one PR per branch). A missing/unreadable
    // dir (no pr-state has ever been created — the verdict-before-anything case)
    // is NOT a hard error: it just means zero matches, so we fall through to the
    // buffer below rather than dropping the verdict (the gate-D early-return was
    // the second half of the #2058 dead zone — the dir often doesn't exist yet
    // when an early reviewer verdicts).
    let dir = pr_state_dir(home);
    let mut matched_any = false;
    let read = std::fs::read_dir(&dir);
    if let Err(ref e) = read {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::debug!(
                task_id,
                dir = %dir.display(),
                error = %e,
                "#2059 record_verdict: pr-state dir read failed — treating as no match, buffering"
            );
        }
    }
    for entry in read.into_iter().flatten().flatten() {
        let path = entry.path();
        if !is_pr_state_file(&path) {
            continue; // #2059: skip .emitted-terminal.json ledger + .lock sidecars
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(state): Result<PrState, _> = serde_json::from_str(&content) else {
            continue;
        };
        // #2079: prefix-tolerant — a reviewer's abbreviated `reviewed_head`
        // (e.g. `7e1d422`) matches the PR state's full canonical head_sha,
        // instead of silently falling through to the 24h-TTL buffer.
        if !sha_prefix_match(&state.head_sha, reviewed_head) {
            continue;
        }
        matched_any = true;
        let repo = state.repo.clone();
        let branch = state.branch.clone();
        let label = verdict_label(&kind);
        // #2 (t-verdict-to-author-routing): capture the verdict notification under
        // the flock, enqueue it AFTER `with_pr_state` returns (self-IPC safety —
        // mirrors the scanner's #1629 deferred-emit pattern).
        let mut pending_notify: Option<(String, crate::inbox::InboxMessage)> = None;
        if let Err(e) = with_pr_state(home, &repo, &branch, |s| {
            apply(
                s,
                Event::VerdictObserved {
                    reviewer,
                    reviewed_head,
                    kind,
                },
            );
            // The legacy display projection cannot make the PR merge-ready, so
            // surface it as an ordinary author notification.
            if !is_merge_ready(s) {
                let recipient = resolve_notify_recipient(home, s);
                let body = format_verdict_body(s, reviewer, label);
                let msg = crate::inbox::InboxMessage::new_system(
                    "system:pr-state",
                    "review-verdict",
                    body,
                )
                .with_correlation_id(format!("{}@{}", s.repo, s.branch))
                .with_reviewed_head(s.head_sha.clone());
                pending_notify = Some((recipient, msg));
            }
        }) {
            tracing::warn!(
                repo = %repo,
                branch = %branch,
                error = %e,
                "#972 pr_state: record_verdict save failed"
            );
        }
        // Enqueue OUTSIDE the with_pr_state flock (self-IPC via loopback api::call).
        if let Some((recipient, msg)) = pending_notify {
            if let Err(e) = crate::inbox::enqueue_with_idle_hint(home, &recipient, msg) {
                tracing::warn!(
                    repo = %repo,
                    branch = %branch,
                    recipient = %recipient,
                    error = %e,
                    "#2 review-verdict notify enqueue failed"
                );
            }
        }
    }
    if !matched_any {
        // Preserve the old row in its TTL-bounded compatibility namespace for
        // migration visibility. `record_ci_result` never drains this namespace;
        // only assignment-bound typed receipts may replay.
        verdict_buffer::buffer(home, reviewed_head, reviewer, kind_str, kind_reason);
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

/// t-verdict-to-author-routing-design (#2): resolve the AGENT to notify for an
/// author-facing pr-state signal (`[review-verdict]`, `[pr-ready-for-merge]`).
///
/// BINDING-first (shared-account-proof, reusing PR-3 #1799): the fleet shares one
/// GitHub account, so `resolve_author`'s gh-login chain can mis-route (and its
/// last resort is a hard-coded `"fixup-lead"`). The agent BOUND to the branch is
/// the dispatchee/author who is waiting; resolve that first, then fall back to the
/// existing `resolve_author` chain (`pr_author` → `subscribers[0]` → `fixup-lead`)
/// unchanged. The binding lookup is a plain FS read (no flock / subprocess), so it
/// is safe inside a `with_pr_state` closure.
pub fn resolve_notify_recipient(home: &Path, state: &PrState) -> String {
    // #2117 P3b: branch-only scan (source_repo="") — "who holds this branch" for
    // notify routing; cross-repo precision is not needed here.
    crate::binding::scan_existing_branch_binding(home, "", &state.branch, "")
        .unwrap_or_else(|| resolve_author(state))
}

/// #2059-#3: resolve the MERGE AUTHORITY for `[pr-ready-for-merge]` — a
/// distinct audience from the author-facing `[review-verdict]` signal.
/// Ready-for-MERGE must reach whoever merges (the team orchestrator), NOT the
/// last CI-chain hop or the author.
///
/// Resolution is via the DURABLE fleet.yaml teams config, deliberately NOT the
/// branch binding: `resolve_notify_recipient` is binding-first, but the
/// implementer RELEASES their worktree right after pushing, so by merge-ready
/// time the binding is usually gone and that resolver falls through to the
/// author — the exact mis-route that left PR #2058 stranded (#2059). Map a
/// known fleet member on this PR → its team's orchestrator. fleet.yaml survives
/// the binding release, so the route is stable across the whole PR lifetime.
///
/// Candidate members, fleet-name sources first (the gh-login `pr_author` is the
/// least reliable under the shared account, so it's last): the reviewers (from
/// the recorded verdict), then the watch subscribers, then `pr_author`. The
/// first whose team has an orchestrator wins; else (no team for anyone — a
/// single-agent deployment) the author self-notifies, since they merge their
/// own PR; `fixup-lead` remains only as the last-ditch when the author is
/// unknown.
pub fn resolve_merge_authority(home: &Path, state: &PrState) -> String {
    let mut candidates: Vec<&str> = Vec::new();
    match &state.verdict_state {
        VerdictState::Verified { reviewers } => {
            candidates.extend(reviewers.iter().map(|(r, _)| r.as_str()));
        }
        VerdictState::Rejected { reviewer, .. } | VerdictState::Unverified { reviewer, .. } => {
            candidates.push(reviewer.as_str());
        }
        VerdictState::None | VerdictState::Pending => {}
    }
    candidates.extend(state.subscribers.iter().map(String::as_str));
    if !state.pr_author.is_empty() {
        candidates.push(state.pr_author.as_str());
    }
    for member in candidates {
        if let Some(orch) = crate::teams::find_team_for(home, member)
            .and_then(|t| t.orchestrator)
            .filter(|o| !o.is_empty())
        {
            return orch;
        }
    }
    // No candidate belongs to a team with an orchestrator — a single-agent /
    // no-team deployment. There is no separate merge authority to route to, so
    // self-notify the AUTHOR (they merge their own PR). A literal "fixup-lead"
    // here would route into the void on any deployment that has no fixup-lead
    // instance — the #2058 dead-zone recurring on someone else's machine
    // (de-hardcode follow-up to #2063). "fixup-lead" stays only as the
    // last-ditch when even the author is unknown.
    if !state.pr_author.is_empty() {
        return state.pr_author.clone();
    }
    "fixup-lead".to_string()
}

/// t-verdict-to-author-routing-design (#2): the wire label for a verdict kind.
#[cfg(test)]
fn verdict_label(kind: &VerdictKind) -> &'static str {
    match kind {
        VerdictKind::Verified => "VERIFIED",
        VerdictKind::Rejected { .. } => "REJECTED",
        VerdictKind::Unverified => "UNVERIFIED",
    }
}

/// t-verdict-to-author-routing-design (#2): the `[review-verdict]` body surfaced
/// to the author when a verdict lands but the PR is not (yet) merge-ready.
fn format_verdict_body(state: &PrState, reviewer: &str, label: &str) -> String {
    let pr_id = if state.pr_number > 0 {
        format!("{}@{} (PR #{})", state.repo, state.branch, state.pr_number)
    } else {
        format!("{}@{}", state.repo, state.branch)
    };
    let mut body = format!("[review-verdict] {pr_id}: {label} by {reviewer}");
    if label == "REJECTED" {
        body.push_str(" — fix and re-push");
    }
    body
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
    let (reviewers_csv, verified_count) =
        if let VerdictState::Verified { reviewers } = &state.verdict_state {
            (
                reviewers
                    .iter()
                    .map(|(r, _)| r.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                reviewers.len(),
            )
        } else {
            (String::new(), 0)
        };
    // #2745: surface the review_class + distinct-VERIFIED tally so the merge
    // authority can see WHY the gate opened (single vs dual, N-of-required).
    let required = state.review_class.required_verified_count();
    format!(
        "[pr-ready-for-merge] {pr_id} (head {sha_short}): \
         CI green ∧ VERIFIED [{class} {verified_count}/{required} distinct] ({reviewers_csv}). \
         §3.12 self-merge gate open — `gh pr merge {pr_or_branch} --squash --delete-branch` \
         (or post-#973 `--auto`).",
        class = state.review_class.as_token(),
        pr_or_branch = if state.pr_number > 0 {
            state.pr_number.to_string()
        } else {
            state.branch.clone()
        }
    )
}

// ─── scanner functions moved to scanner.rs ────────────────────────────

// ─── reducer test matrix — re-homed to the sibling `tests.rs` to keep this
//     production file under the `src_file_size_invariant` ceiling (#2745 R3). ───
#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "tests.rs"]
mod tests;
