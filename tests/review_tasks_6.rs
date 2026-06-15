//! Review-repro static-invariant (SCOPEKEY: tasks) — FINDING #6 (#231),
//! RESOLVED-BY-INTERIM-GUARD (t-71 design decision).
//!
//! Original premise: `handle_update`'s Claimed/InProgress/Done transition events
//! built their `by` actor from the OUT-OF-LOCK `record` snapshot, so a concurrent
//! `OwnerAssigned` between the read and the append could persist an event that
//! attributes the action to the PREVIOUS owner.
//!
//! Resolution (KISS, NOT the Option-A append-API rewrite — see t-71 spike): the
//! events keep their out-of-lock `by`, but `handle_update` captures the
//! out-of-lock `stale_owner` and `update_batch_precondition` re-checks it against
//! FRESH committed state under the append lock — a FAIL-CLOSED guard that REJECTS
//! the write (retryable) if the owner drifted, so a stale-`by` event can never
//! commit. Rebuilding the actor inside the append closure (Option A) was judged
//! over-engineering for this human-driven, low-contention path.
//!
//! ⚠ This is a STRUCTURAL gate ONLY: it asserts the drift guard is WIRED. The
//! actual BEHAVIORAL safety — reject on owner drift, including the
//! system-identity (ACL-bypassed) by-drift case — is verified by `handler.rs`'s
//! `inlock_precond_*_231` unit tests, which call `update_batch_precondition`
//! directly against a crafted fresh-state-with-changed-owner. Per #2018, a source
//! scan is a CLAIM, not behavioral proof; those unit tests are the proof, and
//! this gate exists only to catch a regression that silently REMOVES the guard.

use std::path::PathBuf;

fn read_handler() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/tasks/handler.rs");
    std::fs::read_to_string(&p).expect("read src/tasks/handler.rs")
}

#[test]
fn handle_update_has_inlock_actor_drift_guard_tasks() {
    let text = read_handler();

    // (1) `handle_update` captures the out-of-lock owner and threads it into the
    //     in-lock precondition for re-validation.
    let captures_stale_owner =
        text.contains("let stale_owner = record.owner.clone();") && text.contains("&stale_owner");

    // (2) `update_batch_precondition` rejects (fail-closed) when the fresh owner
    //     has drifted from the stale one on an attribution-bearing transition.
    let drift_check =
        text.contains("fresh.owner != *stale_owner") && text.contains("attribution would be stale");

    assert!(
        captures_stale_owner && drift_check,
        "#231 in-lock actor-drift guard must remain wired: handle_update must capture \
         the out-of-lock owner (`stale_owner`) and pass it to update_batch_precondition, \
         which must reject fail-closed when the fresh owner drifted \
         (`fresh.owner != *stale_owner` → \"attribution would be stale\"). Behavioral \
         coverage lives in handler.rs `inlock_precond_*_231`. \
         captures_stale_owner={captures_stale_owner} drift_check={drift_check}"
    );
}
