//! ci-ready emit-gate predicates (#2502): pure checks over a cached `PrState`
//! deciding whether the daemon should emit `[ci-ready-for-action]` on CI pass.
//! Extracted from `mod.rs` to keep that grandfathered file under its
//! anti-monolith ceiling; both predicates are re-exported from the parent module
//! so existing call sites (`crate::daemon::pr_state::is_ci_ready_*`,
//! `super::is_ci_ready_merge_blocked`) are unchanged.

use super::{DraftState, MergeState, PrState, VerdictState};

/// #t-92758: a PR whose `[ci-ready-for-action]` chain handoff is pointless right
/// now because the PR cannot be merged/acted-on — a REJECTED verdict (a reviewer
/// bounced it; it's being reworked) or a Draft PR (`gh pr merge` refuses drafts).
/// Used to (a) SUPPRESS a new ci-ready emission and (b) EVICT an existing
/// ci-handoff track so the re-nudge watchdog stops pinging the chain target for a
/// PR they can't move.
///
/// IRON RULE (regression-pinned): this is DELIBERATELY narrow — it returns
/// `false` for `Verified` / `Unverified` / `Pending` / `None` verdicts. A
/// VERIFIED+green PR is exactly the "your turn to merge" case the chain exists
/// for and MUST keep emitting + re-nudging; this predicate must never suppress
/// it. (Unverified/Pending/None are not merge-blocked verdicts — the reviewer
/// hasn't bounced the PR — so the chain handoff stays live.)
pub fn is_ci_ready_merge_blocked(state: &PrState) -> bool {
    matches!(state.verdict_state, VerdictState::Rejected { .. })
        || matches!(state.draft_state, DraftState::Draft)
}

/// #2502: a PR whose cached terminal merge_state (Merged / ClosedUnmerged) was
/// observed at the SAME head CI just passed on. Emitting `[ci-ready-for-action]`
/// here hands "your turn" on a PR that is already merged/closed, spawning a
/// re-nudge loop the chain target can't resolve — the proactive emit-time half of
/// a suppression whose reactive half is the scanner's terminal track-evict
/// (`pr_state::scanner`, the green-then-terminal ordering).
///
/// HEAD-GUARDED (load-bearing): a terminal state at a DIFFERENT head — a
/// force-push / branch-reuse that opened a fresh PR on the same branch — returns
/// `false`, so the live handoff still fires. This is sound because
/// [`super::record_ci_result`] SKIPS `CiObserved` on terminal states (#1314),
/// freezing a terminal file's `head_sha` at the merge/close head;
/// `state.head_sha == ci_head` is therefore a reliable proxy for "CI ran on the
/// head that was merged/closed", never a later reused head.
///
/// Distinct from [`is_ci_ready_merge_blocked`] (REJECTED/Draft, still-OPEN): that
/// predicate is IRON-RULE-narrow and MUST NOT be widened to terminal states. The
/// emit site ORs the two. Both fail OPEN — a missing sidecar (caller passes
/// `None`), a non-terminal merge_state, an empty `head_sha`, or a head mismatch
/// all return `false` and the normal emit proceeds.
pub fn is_ci_ready_terminal_at_head(state: &PrState, ci_head: &str) -> bool {
    matches!(
        state.merge_state,
        MergeState::Merged { .. } | MergeState::ClosedUnmerged { .. }
    ) && !state.head_sha.is_empty()
        && state.head_sha == ci_head
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::{
        new_for_branch, DraftState, MergeState, PrState, ReviewClass, VerdictState,
    };
    use super::{is_ci_ready_merge_blocked, is_ci_ready_terminal_at_head};

    /// A fresh PrState at `head` (verdict None, draft Ready, merge NotReady) —
    /// the production constructor, so these tests never drift from the real
    /// default shape.
    fn state_at(head: &str) -> PrState {
        new_for_branch("owner/repo", "feat/test", head, ReviewClass::Single)
    }

    /// #t-92758 IRON RULE: `is_ci_ready_merge_blocked` blocks ONLY a REJECTED
    /// verdict or a Draft PR — never VERIFIED / Unverified / Pending / None. A
    /// VERIFIED+green PR is the "your turn to merge" case the ci-ready chain
    /// exists for and MUST keep emitting + re-nudging; a regression that made the
    /// predicate true for VERIFIED would silently kill legitimate merge handoffs.
    #[test]
    fn is_ci_ready_merge_blocked_only_rejected_or_draft() {
        let mut s = state_at("sha-A");

        // Non-blocking verdicts (Ready draft state):
        s.verdict_state = VerdictState::None;
        assert!(!is_ci_ready_merge_blocked(&s), "None must not block");
        s.verdict_state = VerdictState::Pending;
        assert!(!is_ci_ready_merge_blocked(&s), "Pending must not block");
        s.verdict_state = VerdictState::Unverified {
            reviewer: "r".into(),
            reviewed_head: "sha-A".into(),
        };
        assert!(!is_ci_ready_merge_blocked(&s), "Unverified must not block");
        s.verdict_state = VerdictState::Verified {
            reviewers: vec![("r".into(), "sha-A".into())],
        };
        assert!(
            !is_ci_ready_merge_blocked(&s),
            "IRON RULE: VERIFIED must NEVER be suppressed/evicted"
        );

        // Blocking: REJECTED verdict.
        s.verdict_state = VerdictState::Rejected {
            reviewer: "r".into(),
            reviewed_head: "sha-A".into(),
            reason: None,
        };
        assert!(is_ci_ready_merge_blocked(&s), "REJECTED must block");

        // Blocking: Draft — even with an otherwise-mergeable VERIFIED verdict.
        s.verdict_state = VerdictState::Verified {
            reviewers: vec![("r".into(), "sha-A".into())],
        };
        s.draft_state = DraftState::Draft;
        assert!(
            is_ci_ready_merge_blocked(&s),
            "Draft must block even with a VERIFIED verdict"
        );
    }

    /// #2502: `is_ci_ready_terminal_at_head` suppresses ONLY a terminal
    /// (Merged / ClosedUnmerged) PR observed at the SAME head CI passed on, and
    /// is head-GUARDED — a terminal state at a different head (force-push /
    /// branch-reuse) MUST NOT suppress, or a fresh PR on a reused branch would
    /// silently lose its handoff. Non-terminal states never suppress.
    #[test]
    fn is_ci_ready_terminal_at_head_only_same_head_terminal() {
        let mut s = state_at("sha-A");

        // Non-terminal states never suppress, even at the matching head.
        s.merge_state = MergeState::NotReady;
        assert!(
            !is_ci_ready_terminal_at_head(&s, "sha-A"),
            "NotReady must not suppress"
        );
        s.merge_state = MergeState::MergeReady;
        assert!(
            !is_ci_ready_terminal_at_head(&s, "sha-A"),
            "MergeReady must not suppress"
        );

        // Merged at the SAME head → suppress.
        s.merge_state = MergeState::Merged {
            merge_commit: "mc".into(),
            merged_at: "t".into(),
        };
        assert!(
            is_ci_ready_terminal_at_head(&s, "sha-A"),
            "Merged at same head must suppress"
        );
        // Merged at a DIFFERENT head (force-push / reuse) → fail open. This is the
        // anti-false-suppression nail: #1314 freezes a terminal head_sha at the
        // merge head, so a green at sha-B is a fresh PR on the reused branch.
        assert!(
            !is_ci_ready_terminal_at_head(&s, "sha-B"),
            "Merged at a different head must NOT suppress (branch reuse stays live)"
        );

        // ClosedUnmerged at the SAME head → suppress; different head → fail open.
        s.merge_state = MergeState::ClosedUnmerged {
            closed_at: "t".into(),
        };
        assert!(
            is_ci_ready_terminal_at_head(&s, "sha-A"),
            "ClosedUnmerged at same head must suppress"
        );
        assert!(
            !is_ci_ready_terminal_at_head(&s, "sha-B"),
            "ClosedUnmerged at a different head must NOT suppress"
        );

        // Empty head_sha → fail open (never match an empty ci_head either).
        s.head_sha = String::new();
        assert!(
            !is_ci_ready_terminal_at_head(&s, ""),
            "empty head_sha must fail open (no spurious suppression)"
        );
    }
}
