//! Review-repro tests (scope: daemon-ci-pr) for `pr_state::scanner`.
//!
//! Two findings, both verified by SOURCE-SCANNING `scanner.rs` (static
//! invariants) — the runtime paths they describe cannot be driven to RED
//! without the structural fix itself (a failing self-IPC enqueue under a real
//! registry; a head-advance timestamp field that does not yet exist). The
//! scans mirror the file's own `auto_release_not_called_under_pr_state_flock`
//! invariant: prod-slice off the `#[cfg(test)]` mod and brace-match / substring
//! against concat-built needles so the test can't self-satisfy.

#![allow(clippy::unwrap_used, clippy::expect_used)]

/// The production half of scanner.rs (everything before the in-file
/// `#[cfg(test)]` module), so a needle living in a test never self-satisfies.
fn prod_src() -> &'static str {
    let src = include_str!("../scanner.rs");
    let cfg_test = ["#[cfg(", "test)]"].concat();
    match src.find(&cfg_test) {
        Some(i) => &src[..i],
        None => src,
    }
}

/// Finding: "pr-ready-for-merge dedup flag set before deferred enqueue can fail
/// — signal lost on enqueue failure".
///
/// In the `[pr-ready-for-merge]` arm, `ready_emitted_for_sha` is set
/// OPTIMISTICALLY under the flock (the dedup ledger), but the actual
/// `enqueue_with_idle_hint` runs LATER, post-flock, in the `pending_emits`
/// drain. If that enqueue fails, the flag is already persisted → the next scan
/// tick sees `ready_emitted_for_sha == head_sha` and skips re-emit → the
/// pr-ready merge-handoff signal is permanently lost (only warn-logged). Unlike
/// the Merged/ClosedUnmerged arms, the pr-ready path has NO persistent-ledger
/// backstop.
///
/// CORRECT behavior: an enqueue failure in the post-flock drain must NOT leave
/// the signal unrecoverable — either the dedup flag is set only on enqueue
/// SUCCESS, or the failure path resets `ready_emitted_for_sha` (via a follow-up
/// `with_pr_state`) so the next tick retries. Either fix makes the post-flock
/// drain region reference the dedup flag and/or a `with_pr_state` recovery.
///
/// RED today: the drain region (from the `pending_emits` drain to the end of
/// the per-file loop body) references NEITHER `ready_emitted_for_sha` NOR a
/// `with_pr_state` recovery — the `Err` arm only `tracing::warn!`s. GREEN once
/// the fix wires a recovery / success-gated flag into that region.
#[test]
fn pr_ready_dedup_flag_recovers_on_enqueue_failure_daemon_ci_pr() {
    let prod = prod_src();

    // Bound the post-flock drain region: from the deferred-emit drain loop
    // header to the end of the per-file loop body (the `registry` reservation
    // line is the last statement before the loop closes).
    let drain_start_needle = ["for (author, msg) in ", "pending_emits"].concat();
    let region_end_needle = ["let _ = ", "registry;"].concat();

    let start = prod
        .find(&drain_start_needle)
        .expect("pending_emits drain loop present in scanner.rs");
    let end_rel = prod[start..]
        .find(&region_end_needle)
        .expect("end-of-loop-body marker present after the drain");
    let region = &prod[start..start + end_rel];

    let dedup_flag_needle = ["ready_emitted", "_for_sha"].concat();
    let recovery_needle = ["with_pr", "_state"].concat();

    let references_flag = region.contains(&dedup_flag_needle);
    let references_recovery = region.contains(&recovery_needle);

    assert!(
        references_flag || references_recovery,
        "pr-ready dedup flag `ready_emitted_for_sha` is set optimistically under \
         the flock, but the post-flock `enqueue_with_idle_hint` drain has no path \
         that ties an enqueue FAILURE back to the dedup flag — so a failed enqueue \
         permanently loses the [pr-ready-for-merge] signal (no ledger backstop, \
         unlike the terminal arms). The drain region must either set the flag only \
         on enqueue success or reset `ready_emitted_for_sha` (via `with_pr_state`) \
         on failure so the next tick retries."
    );
}

/// Finding: "Stale-snapshot freshness gate compares poll time to created_at but
/// not to the branch's last head advance".
///
/// `apply_gh_poll` gates state-changing gh observations on
/// `poll_is_fresh_for(&polled_at, &state.created_at)`. `created_at` is stamped
/// ONCE at PrState creation and never advances when the branch HEAD moves
/// (force-push / head-advance). So a snapshot polled AFTER `created_at` but
/// BEFORE a subsequent head advance is treated as fresh and applied — an old gh
/// observation (e.g. a `Closed` for a PR since reopened/re-pushed on a reused
/// head) can drive a sticky `ClosedUnmergedObserved` terminal transition →
/// false release. The `created_at` anchor only covers the cold-start race, not
/// the head-reuse race on a long-lived branch.
///
/// CORRECT behavior: freshness must be gated on the most recent HEAD-ADVANCE
/// timestamp (the suggestion: stamp/require a head-advance `updated_at` /
/// `head_observed_at`), not the immutable `created_at`. The fix changes the
/// freshness anchor argument away from `state.created_at`.
///
/// RED today: the call site is byte-for-byte `poll_is_fresh_for(&polled_at,
/// &state.created_at)`. GREEN once the second argument is a head-advance
/// timestamp instead of `created_at`.
#[test]
fn freshness_gate_not_anchored_on_immutable_created_at_daemon_ci_pr() {
    let prod = prod_src();

    // The bug: the freshness gate anchors on the immutable `created_at`.
    // `concat`-built so this assertion text can't self-satisfy the scan.
    let bug_needle = ["poll_is_fresh_for(&polled_at, &state.", "created_at)"].concat();

    assert!(
        !prod.contains(&bug_needle),
        "freshness gate anchors on the immutable `created_at` \
         (`poll_is_fresh_for(&polled_at, &state.created_at)`), which never \
         advances on a branch HEAD move — so a snapshot predating the current \
         head (head-reuse / force-push race) is wrongly applied as a terminal \
         transition. Gate on a head-advance timestamp (e.g. an `updated_at` \
         stamped when head_sha changes) instead of `created_at`."
    );
}
