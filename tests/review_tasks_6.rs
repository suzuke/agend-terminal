//! Review-repro static-invariant (SCOPEKEY: tasks) — FINDING #6.
//!
//! `handle_update`'s Claimed / InProgress / Done transition events take their
//! `by` actor from the OUT-OF-LOCK `record` snapshot
//! (`record.owner ... unwrap_or(caller.as_str())`). The in-lock
//! `append_batch_checked_at` precondition re-validates ONLY the status
//! transition — not the actor — so a concurrent `OwnerAssigned` between the
//! out-of-lock read and the append makes the persisted event attribute the
//! action to the PREVIOUS owner (audit/actor drift).
//!
//! The fix is architectural: the `by`/owner stamped on the emitted transition
//! events must be resolved from the FRESH state inside the precondition
//! closure, not from the stale `record`. That restructuring does not yet exist,
//! so this is an interim guard (see redesign_note in the manifest).
//!
//! The drift site is uniquely identified by `unwrap_or(caller.as_str())`, which
//! appears EXACTLY at the three transition arms (Claimed/InProgress/Done) of
//! `handle_update` that build `by` from the out-of-lock record. RED now
//! (present); GREEN once the actor is re-derived in-lock and the stale-record
//! `by` construction is removed.

use std::path::PathBuf;

fn read_handler() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/tasks/handler.rs");
    std::fs::read_to_string(&p).expect("read src/tasks/handler.rs")
}

/// Isolate the `handle_update` body (the drift site lives there).
fn handle_update_body(text: &str) -> String {
    let start = text
        .find("fn handle_update(")
        .expect("handle_update exists");
    let after = &text[start..];
    // End at the next top-level `fn ` after the body.
    let end = after[1..]
        .find("\nfn ")
        .map(|e| start + 1 + e)
        .unwrap_or(text.len());
    text[start..end].to_string()
}

#[test]
#[ignore = "tasks-update-actor-drift: red until fix; remove #[ignore] after fix to confirm"]
fn handle_update_resolves_actor_in_lock_not_from_stale_record_tasks() {
    let text = read_handler();
    let body = handle_update_body(&text);

    // BAD pattern: the transition events' `by` is built from the out-of-lock
    // `record.owner ... unwrap_or(caller.as_str())`. The fix re-derives the
    // actor from fresh in-lock state, eliminating these call sites.
    let stale_actor_uses = body.matches("unwrap_or(caller.as_str())").count();

    assert_eq!(
        stale_actor_uses, 0,
        "FINDING #6: handle_update builds the `by` actor of Claimed/InProgress/Done \
         transition events from the OUT-OF-LOCK record (found {stale_actor_uses} \
         `unwrap_or(caller.as_str())` site(s)). A concurrent OwnerAssigned makes the \
         persisted event attribute the action to the previous owner. Resolve the actor \
         from the FRESH state inside the append_batch_checked_at precondition closure."
    );
}
