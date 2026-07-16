//! PR-D · D1 — the single `terminal_disposition` classifier (janitor spike §1).
//!
//! Today the worktree-cleanup / GC-retention / auto-release paths each run their
//! OWN "is this terminal?" decision (spike §0: three decision systems over six
//! kill-paths). This module is the unified DECISION seam they will share: one
//! pure classifier that maps a worktree's resolved signals to one of four
//! dispositions. It is **pure by construction** — every I/O-derived signal is an
//! INPUT, computed by the caller. The impure signal-gathering stays in each
//! system; D2–D4 wire the call sites (this D1 slice is purely additive and has
//! ZERO production callers).
//!
//! The one invariant every layer preserves (spike §0): **every ambiguity fails
//! toward NOT destroying.** `Keep` is the safe default; the classifier only
//! reaches `Release`/`Delete`/`Archive` on a POSITIVE terminal signal, never on
//! the mere absence of a keep signal. The fail-direction of each row is pinned
//! by the tests below.
//!
//! Extract-and-delegate (spike §5): the classifier COMPOSES sub-verdicts already
//! produced by each system's existing fn — `ReleaseDecision` from
//! [`crate::daemon::auto_release::decide_release`], `releasable_by_invariant`
//! from `auto_release::releasable_by_invariant`, a [`ReclaimState`] from the GC
//! `evaluate_candidate` classification. It re-encodes NONE of that logic; it only
//! adds the cross-system routing that unifies the three decision systems.
//!
//! PR-D narrows D1's blanket `#![allow(dead_code)]` as each call site lands: D2
//! (#2713) wired the L0–L2 auto-release path
//! ([`crate::daemon::auto_release::should_release_now`]); D3 wired the shared L0
//! gate ([`l0_protected`]) + the L4 branch decision ([`branch_disposition`]) into
//! the sweep ([`crate::worktree_cleanup`]); D4 wired the L3 GC reclaim judgment
//! (the GC `evaluate_candidate` produces a [`ReclaimState`] + `agent_alive` and
//! delegates the Keep/Delete/Archive routing here). The ONLY remaining
//! `#[allow(dead_code)]` is on [`ReclaimState::CleanReleaseArchive`] — the
//! removal-time archive-belt outcome the disposition ladder (D5) still routes
//! natively, matched here but not yet constructed outside tests.

use crate::daemon::auto_release::ReleaseDecision;

/// The four terminal dispositions of a worktree (spike §1). Orthogonal to the
/// branch decision ([`BranchDisposition`], spike §1 L4, which is worktree-
/// independent per the CR-2026-06-14 dir-vs-branch decoupling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// Not terminal, or ambiguous → leave it alone (the fail-closed default).
    Keep,
    /// Binding present + terminal → full managed release (`release_full`:
    /// WIP-preserve → remove → unbind → branch cleanup).
    Release,
    /// No binding, confirmed-clean terminal → historical hard remove
    /// (CleanRelease, decision Q3 — ungated).
    Delete,
    /// Reclaim-worthy but the git state is untrustworthy/unverifiable → atomic
    /// archive to `.trash` (recoverable): ForceReclaim always, or a CleanRelease
    /// the hard-delete couldn't act on.
    Archive,
}

/// L3 (binding-absent, GC) reclaim classification. Produced by the GC system's
/// `evaluate_candidate` (D4 wires it); the pure part the classifier routes on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReclaimState {
    /// Not a GC candidate (in grace, not aged, or spared) → Keep.
    NotEligible,
    /// `released_at` present, past the 24h grace, worktree clean → hard delete.
    CleanReleaseHardDelete,
    /// `released_at` present + past grace, but the hard delete is unactionable
    /// (lock contention / owning-repo unresolved / remove failed) and the archive
    /// gate is on → archive instead. PR-D·D4: this is a REMOVAL-time archive-belt
    /// outcome (`gc_remove_one`'s #2550 W5 `AGEND_WORKTREE_ARCHIVE_FALLBACK` fall-through), NOT an
    /// `evaluate_candidate` verdict — the candidate JUDGMENT D4 delegates only
    /// produces `NotEligible`/`CleanReleaseHardDelete`/`ForceReclaim`. This belt
    /// outcome routes natively still (the disposition LADDER is D5), so it is
    /// matched by [`terminal_disposition`] but not yet constructed outside tests →
    /// a targeted `dead_code` allow until D5 wires it.
    #[allow(dead_code)]
    CleanReleaseArchive,
    /// Never-released OR malformed `released_at`, dead agent, past the age cap →
    /// ForceReclaim: ALWAYS archived, never hard-deleted (t-worktree-leak PR-2).
    ForceReclaim,
}

/// Whether a branch's work is provably in `default` (spike §1 L4). This axis is
/// SEPARATE from the worktree-dir disposition: a remote-gone branch's worktree
/// may be reclaimed while its branch ref is KEPT (CR-2026-06-14 — remote-gone
/// alone is never a `branch -D` trigger).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BranchSignal {
    /// Merged-ancestor, squash-merged past the age floor, or an authoritative
    /// merged PR (#2698) → the work is provably in `default`.
    ProvablyInDefault,
    /// The remote-tracking ref is gone but the branch is NOT proven merged —
    /// may hold unpushed local work (CR-2026-06-14). Never a delete trigger.
    RemoteGoneOnly,
    /// Not merged and not remote-gone → keep.
    NotMerged,
}

/// Branch-ref disposition (spike §1 L4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BranchDisposition {
    KeepBranch,
    DeleteBranch,
}

/// Provenance of a branch that a lifecycle path is considering for removal.
/// Naming alone is never sufficient provenance: callers must resolve one of
/// these variants from the binding/task/SCM evidence they own, otherwise the
/// classifier fails closed with [`BranchLifecycleDisposition::Keep`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BranchProvenance {
    /// A review checkout carrying an authority-proven lease identity.
    ManagedReview,
    /// A normal branch whose tip is reachable from the default branch.
    Merged,
    /// A normal branch whose changes are proven squash-merged.
    SquashMerged,
    /// A terminal reviewer-checkout residue (for example an aged, unbound
    /// `review/*` checkout). This is distinct from a forged name: callers
    /// still need all the terminal/occupancy/task/PR gates below.
    ReviewerResidue,
    /// Evidence was missing or contradictory.
    Unknown,
}

/// Typed branch-ref lifecycle decision shared by release and cleanup paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BranchLifecycleDisposition {
    Keep,
    Delete,
}

/// Resolved evidence for the branch lifecycle decision. `None` on any
/// safety-critical dimension means that the caller could not prove the safe
/// answer; the classifier intentionally treats that as `Keep`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BranchLifecycleInput {
    pub provenance: BranchProvenance,
    pub terminal: bool,
    pub active_holder: Option<bool>,
    pub task_active: Option<bool>,
    pub open_pr: Option<bool>,
    pub unique_unpreserved_work: Option<bool>,
}

/// Decide whether a branch ref may be deleted after its worktree/lease
/// lifecycle has reached a terminal boundary. This is deliberately pure so
/// release and cleanup cannot grow subtly different ad-hoc branch-name rules.
/// Every unresolved or negative-preservation signal keeps the branch.
pub(crate) fn branch_lifecycle_disposition(
    input: &BranchLifecycleInput,
) -> BranchLifecycleDisposition {
    if !input.terminal
        || input.active_holder != Some(false)
        || input.task_active != Some(false)
        || input.open_pr != Some(false)
        || input.unique_unpreserved_work != Some(false)
    {
        return BranchLifecycleDisposition::Keep;
    }
    match input.provenance {
        BranchProvenance::ManagedReview
        | BranchProvenance::Merged
        | BranchProvenance::SquashMerged
        | BranchProvenance::ReviewerResidue => BranchLifecycleDisposition::Delete,
        BranchProvenance::Unknown => BranchLifecycleDisposition::Keep,
    }
}

/// Pre-computed, caller-gathered signals for [`terminal_disposition`]. Ambiguity
/// is preserved as `Option::None` on the dimensions where the DECISION (not the
/// I/O) owns the fail-direction, so the classifier can route it to `Keep`.
pub(crate) struct DispositionInput {
    // ── L0: protection layer (any hit → Keep) ──
    /// `.agend-managed` marker present. `false` = operator-created, not ours.
    pub daemon_managed: bool,
    /// `.agend-pinned` present.
    pub pinned: bool,
    /// In-use / occupancy. `None` = could not be determined (corrupt
    /// binding.json, un-canonicalizable ancestor) → fail-closed Keep;
    /// `Some(true)` = in use → Keep.
    pub in_use: Option<bool>,

    // ── L1/L2: binding present — the auto-release system ──
    /// Whether a binding is present. Drives the L2 (present) vs L3 (absent)
    /// split — kept explicit because `ReleaseDecision::SkipNotBound` conflates
    /// "no binding" with "dirty undetermined".
    pub binding_present: bool,
    /// The auto-release gate verdict (`decide_release`: task/opt-out/bound/dirty).
    pub release_decision: ReleaseDecision,
    /// The PR-invariant half (`releasable_by_invariant`): PR-terminal OR
    /// (no-PR ∧ branch tasks done). Ambiguity (Unknown / open PR / pending
    /// task) already collapses to `false` upstream → the classifier keeps.
    pub releasable_by_invariant: bool,

    // ── L3: binding absent — the GC system ──
    /// Agent liveness. `None` = liveness could not be read → fail-toward-alive
    /// Keep; `Some(true)` = alive → Keep.
    pub agent_alive: Option<bool>,
    /// GC reclaim classification.
    pub reclaim: ReclaimState,
}

/// Spike §1 L0 — the SHARED protection gate: a worktree is protected from ALL
/// reclaim paths when it is operator-created (not `.agend-managed`), pinned,
/// in-use, or its occupancy is unresolvable. This is the fail-closed prefix every
/// terminal-detection system shares; extracting it (PR-D·D3) lets a system that
/// owns its OWN terminal trigger — e.g. the sweep's `merged‖gone` — delegate the
/// SAME occupancy/marker fail-direction instead of re-deriving it (the RCA
/// "no-man's-land between policies" fix). `in_use == None` (unresolvable) fails
/// CLOSED → protected, mirroring the sweep's `list_worktrees Err → skip`.
pub(crate) fn l0_protected(daemon_managed: bool, pinned: bool, in_use: Option<bool>) -> bool {
    !daemon_managed || pinned || !matches!(in_use, Some(false))
}

/// The unified worktree-disposition decision (spike §1, layers L0–L3). Pure:
/// routes over pre-computed sub-verdicts, duplicating none of their logic.
pub(crate) fn terminal_disposition(input: &DispositionInput) -> Disposition {
    // ── L0 — protection. Any ambiguity or protection hit → Keep. The shared
    // fail-direction lives in `l0_protected` so a system that owns its own
    // terminal trigger (the sweep's merged‖gone, PR-D·D3) delegates the SAME
    // occupancy/marker gate rather than re-deriving it. ──
    if l0_protected(input.daemon_managed, input.pinned, input.in_use) {
        return Disposition::Keep;
    }

    if input.binding_present {
        // ── L1/L2 — binding present. Delegate the gate to `decide_release`'s
        // verdict; release only on a POSITIVE PR-invariant. Every other case
        // (dirty-not-releasable, undetermined dirty, opt-out, open PR, …) keeps. ──
        match input.release_decision {
            // Clean + bound + PR-releasable → release.
            ReleaseDecision::Release if input.releasable_by_invariant => Disposition::Release,
            // #2697: a dirty-but-releasable worktree still releases (release_full
            // WIP-preserves the dirty tree first) rather than living forever.
            ReleaseDecision::SkipDirtyWorktree if input.releasable_by_invariant => {
                Disposition::Release
            }
            _ => Disposition::Keep,
        }
    } else {
        // ── L3 — binding absent (GC). Liveness fails toward alive. ──
        if input.agent_alive != Some(false) {
            return Disposition::Keep; // alive, or liveness unreadable → spare
        }
        match input.reclaim {
            ReclaimState::NotEligible => Disposition::Keep,
            ReclaimState::CleanReleaseHardDelete => Disposition::Delete,
            ReclaimState::CleanReleaseArchive | ReclaimState::ForceReclaim => Disposition::Archive,
        }
    }
}

/// The branch-ref decision (spike §1 L4), independent of the worktree dir.
/// Delete only on provable-in-default; remote-gone alone never deletes.
pub(crate) fn branch_disposition(signal: BranchSignal) -> BranchDisposition {
    match signal {
        BranchSignal::ProvablyInDefault => BranchDisposition::DeleteBranch,
        BranchSignal::RemoteGoneOnly | BranchSignal::NotMerged => BranchDisposition::KeepBranch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Base input: daemon-managed, not pinned, not in use, no binding, not a GC
    /// candidate, agent dead. A no-op `Keep` — each test perturbs ONE dimension.
    fn base() -> DispositionInput {
        DispositionInput {
            daemon_managed: true,
            pinned: false,
            in_use: Some(false),
            binding_present: false,
            release_decision: ReleaseDecision::SkipNotBound,
            releasable_by_invariant: false,
            agent_alive: Some(false),
            reclaim: ReclaimState::NotEligible,
        }
    }

    // ── L0 protection — fail-direction pins ──

    #[test]
    fn l0_unmanaged_worktree_is_kept() {
        // A worktree WITHOUT the `.agend-managed` marker is operator-created —
        // never ours to reclaim, whatever the other signals say.
        let input = DispositionInput {
            daemon_managed: false,
            binding_present: true,
            release_decision: ReleaseDecision::Release,
            releasable_by_invariant: true,
            ..base()
        };
        assert_eq!(terminal_disposition(&input), Disposition::Keep);
    }

    #[test]
    fn l0_pinned_worktree_is_kept() {
        let input = DispositionInput {
            pinned: true,
            ..base()
        };
        assert_eq!(terminal_disposition(&input), Disposition::Keep);
    }

    #[test]
    fn l0_in_use_unresolvable_is_kept() {
        // Occupancy could not be determined (corrupt binding.json / un-
        // canonicalizable ancestor) → AMBIGUITY, not "not in use" → fail-closed.
        let input = DispositionInput {
            in_use: None,
            ..base()
        };
        assert_eq!(terminal_disposition(&input), Disposition::Keep);
    }

    #[test]
    fn l0_in_use_is_kept() {
        let input = DispositionInput {
            in_use: Some(true),
            ..base()
        };
        assert_eq!(terminal_disposition(&input), Disposition::Keep);
    }

    // ── PR-D·D3: the shared L0 protection predicate ──

    #[test]
    fn l0_protected_holds_the_l0_fail_directions() {
        // `l0_protected` is the extracted L0 gate `terminal_disposition` now calls —
        // it must hold the SAME fail directions (spike §1 L0).
        assert!(
            l0_protected(false, false, Some(false)),
            "unmanaged → protected"
        );
        assert!(l0_protected(true, true, Some(false)), "pinned → protected");
        assert!(l0_protected(true, false, Some(true)), "in-use → protected");
        assert!(
            l0_protected(true, false, None),
            "occupancy unresolvable → fail-CLOSED protected"
        );
        assert!(
            !l0_protected(true, false, Some(false)),
            "managed + unpinned + not-in-use → NOT protected (the only reclaimable case)"
        );
    }

    #[test]
    fn l0_protected_is_the_sweep_occupancy_gate() {
        // PR-D·D3: the sweep passes marker/pin as pass-through (it never consulted
        // them) + `Some(is_in_use)`, so the delegated gate reduces EXACTLY to the
        // pre-D3 `is_in_use` skip — byte-identical.
        for in_use in [true, false] {
            assert_eq!(
                l0_protected(true, false, Some(in_use)),
                in_use,
                "sweep occupancy delegation must equal is_in_use (in_use={in_use})"
            );
        }
    }

    // ── L1/L2 binding-present — fail-direction pins ──

    #[test]
    fn l1_undetermined_dirty_is_kept() {
        // `decide_release` maps dirty=None → SkipNotBound (fail-safe). With a
        // binding present that is NOT "no binding" — it is "dirty undetermined",
        // and an undetermined dirty state must never release.
        let input = DispositionInput {
            binding_present: true,
            release_decision: ReleaseDecision::SkipNotBound,
            releasable_by_invariant: true,
            ..base()
        };
        assert_eq!(terminal_disposition(&input), Disposition::Keep);
    }

    #[test]
    fn l1_dirty_not_releasable_is_kept() {
        // Dirty worktree, PR not terminal → WIP protection, keep.
        let input = DispositionInput {
            binding_present: true,
            release_decision: ReleaseDecision::SkipDirtyWorktree,
            releasable_by_invariant: false,
            ..base()
        };
        assert_eq!(terminal_disposition(&input), Disposition::Keep);
    }

    #[test]
    fn l2_bound_clean_not_releasable_is_kept() {
        // Clean + bound but the PR is open / Unknown / tasks pending →
        // releasable_by_invariant=false upstream → keep (sweeper retries).
        let input = DispositionInput {
            binding_present: true,
            release_decision: ReleaseDecision::Release,
            releasable_by_invariant: false,
            ..base()
        };
        assert_eq!(terminal_disposition(&input), Disposition::Keep);
    }

    #[test]
    fn l2_opt_out_is_kept() {
        let input = DispositionInput {
            binding_present: true,
            release_decision: ReleaseDecision::SkipOptOut,
            releasable_by_invariant: true,
            ..base()
        };
        assert_eq!(terminal_disposition(&input), Disposition::Keep);
    }

    #[test]
    fn l2_bound_clean_releasable_releases() {
        // The one release path: clean + bound + PR-releasable.
        let input = DispositionInput {
            binding_present: true,
            release_decision: ReleaseDecision::Release,
            releasable_by_invariant: true,
            ..base()
        };
        assert_eq!(terminal_disposition(&input), Disposition::Release);
    }

    #[test]
    fn l2_dirty_but_releasable_releases_2697() {
        // #2697: a releasable dirty worktree is no longer immortal — it releases
        // (release_full WIP-preserves first), not retains forever.
        let input = DispositionInput {
            binding_present: true,
            release_decision: ReleaseDecision::SkipDirtyWorktree,
            releasable_by_invariant: true,
            ..base()
        };
        assert_eq!(terminal_disposition(&input), Disposition::Release);
    }

    // ── L3 binding-absent (GC) — fail-direction pins ──

    #[test]
    fn l3_liveness_unreadable_fails_toward_alive() {
        // Liveness could not be read → treat as alive → spare (fail-toward-alive),
        // even for an otherwise force-reclaimable candidate.
        let input = DispositionInput {
            agent_alive: None,
            reclaim: ReclaimState::ForceReclaim,
            ..base()
        };
        assert_eq!(terminal_disposition(&input), Disposition::Keep);
    }

    #[test]
    fn l3_agent_alive_is_kept() {
        let input = DispositionInput {
            agent_alive: Some(true),
            reclaim: ReclaimState::ForceReclaim,
            ..base()
        };
        assert_eq!(terminal_disposition(&input), Disposition::Keep);
    }

    #[test]
    fn l3_not_eligible_is_kept() {
        let input = DispositionInput {
            reclaim: ReclaimState::NotEligible,
            ..base()
        };
        assert_eq!(terminal_disposition(&input), Disposition::Keep);
    }

    #[test]
    fn l3_clean_release_hard_deletes() {
        let input = DispositionInput {
            reclaim: ReclaimState::CleanReleaseHardDelete,
            ..base()
        };
        assert_eq!(terminal_disposition(&input), Disposition::Delete);
    }

    #[test]
    fn l3_clean_release_archive_archives() {
        let input = DispositionInput {
            reclaim: ReclaimState::CleanReleaseArchive,
            ..base()
        };
        assert_eq!(terminal_disposition(&input), Disposition::Archive);
    }

    #[test]
    fn l3_force_reclaim_archives_never_deletes() {
        // t-worktree-leak PR-2: ForceReclaim is ALWAYS archived, never hard-deleted.
        let input = DispositionInput {
            reclaim: ReclaimState::ForceReclaim,
            ..base()
        };
        assert_eq!(terminal_disposition(&input), Disposition::Archive);
    }

    // ── L4 branch — fail-direction pins ──

    #[test]
    fn l4_remote_gone_alone_keeps_branch() {
        // CR-2026-06-14: remote-gone alone is NEVER a `branch -D` trigger — the
        // branch may hold unpushed local work.
        assert_eq!(
            branch_disposition(BranchSignal::RemoteGoneOnly),
            BranchDisposition::KeepBranch
        );
    }

    #[test]
    fn l4_not_merged_keeps_branch() {
        assert_eq!(
            branch_disposition(BranchSignal::NotMerged),
            BranchDisposition::KeepBranch
        );
    }

    #[test]
    fn l4_provably_in_default_deletes_branch() {
        assert_eq!(
            branch_disposition(BranchSignal::ProvablyInDefault),
            BranchDisposition::DeleteBranch
        );
    }

    fn branch_input(provenance: BranchProvenance) -> BranchLifecycleInput {
        BranchLifecycleInput {
            provenance,
            terminal: true,
            active_holder: Some(false),
            task_active: Some(false),
            open_pr: Some(false),
            unique_unpreserved_work: Some(false),
        }
    }

    #[test]
    fn branch_lifecycle_deletes_only_proven_terminal_dispositions() {
        for provenance in [
            BranchProvenance::ManagedReview,
            BranchProvenance::Merged,
            BranchProvenance::SquashMerged,
            BranchProvenance::ReviewerResidue,
        ] {
            assert_eq!(
                branch_lifecycle_disposition(&branch_input(provenance)),
                BranchLifecycleDisposition::Delete,
                "terminal proven branch should be deletable: {provenance:?}"
            );
        }
        assert_eq!(
            branch_lifecycle_disposition(&branch_input(BranchProvenance::Unknown)),
            BranchLifecycleDisposition::Keep
        );
    }

    #[test]
    fn branch_lifecycle_fails_closed_for_active_open_or_unknown_work() {
        for mutate in [
            |i: &mut BranchLifecycleInput| i.active_holder = Some(true),
            |i: &mut BranchLifecycleInput| i.task_active = Some(true),
            |i: &mut BranchLifecycleInput| i.open_pr = Some(true),
            |i: &mut BranchLifecycleInput| i.unique_unpreserved_work = Some(true),
            |i: &mut BranchLifecycleInput| i.open_pr = None,
            |i: &mut BranchLifecycleInput| i.task_active = None,
        ] {
            let mut input = branch_input(BranchProvenance::ReviewerResidue);
            mutate(&mut input);
            assert_eq!(
                branch_lifecycle_disposition(&input),
                BranchLifecycleDisposition::Keep,
                "unsafe or unknown branch evidence must keep the ref"
            );
        }
    }
}
