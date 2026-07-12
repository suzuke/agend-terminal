//! #2453 R2 P0-2 witnesses for the app-restart commit-pending decision.
//!
//! Re-homed out of `app/mod.rs` (which sits at its anti-monolith grandfather ceiling
//! — `tests/src_file_size_invariant.rs`) into this sibling `*tests*.rs` file, exempt
//! by filename. Included from `app/mod.rs` via
//! `#[cfg(test)] #[path = "commit_pending_tests.rs"] mod commit_pending_tests;`, so
//! `use super::*` reaches `app`'s private `CommitPending` / `CommitPoll` /
//! `poll_commit_pending`.
//!
//! The NON-BLOCKING commit-pending decision the TUI polls each tick. Deterministic
//! (now-vs-deadline arithmetic, no wall-clock waiting): a buffered ack commits (even
//! past the deadline — `try_recv` is checked FIRST); a disconnect aborts; an empty
//! channel is `Pending` before the deadline and a watchdog `Abort` at/after it.
//! (RED with the `Pending`-stub / deadline-commit `poll_commit_pending`; GREEN once it
//! runs the real try_recv-first + watchdog logic.)

use super::*;
use std::time::{Duration, Instant};

fn cp(rx: crossbeam_channel::Receiver<()>, deadline: Instant) -> CommitPending {
    CommitPending {
        flush_ack: rx,
        deadline,
    }
}

#[test]
fn commits_on_buffered_ack() {
    let (tx, rx) = crossbeam_channel::bounded::<()>(1);
    tx.send(()).expect("send ack");
    let now = Instant::now();
    assert_eq!(
        poll_commit_pending(&cp(rx, now + Duration::from_secs(3600)), now),
        CommitPoll::Commit
    );
}

#[test]
fn aborts_on_disconnect() {
    let (tx, rx) = crossbeam_channel::bounded::<()>(1);
    drop(tx); // the post-flush action dropped un-run → the sender is gone
    let now = Instant::now();
    assert_eq!(
        poll_commit_pending(&cp(rx, now + Duration::from_secs(3600)), now),
        CommitPoll::Abort("flush_disconnected")
    );
}

#[test]
fn pending_before_deadline() {
    let (_tx, rx) = crossbeam_channel::bounded::<()>(1); // sender alive, no ack
    let now = Instant::now();
    assert_eq!(
        poll_commit_pending(&cp(rx, now + Duration::from_secs(3600)), now),
        CommitPoll::Pending
    );
}

#[test]
fn watchdog_aborts_at_deadline() {
    let (_tx, rx) = crossbeam_channel::bounded::<()>(1); // alive, no ack, no disconnect
    let now = Instant::now();
    assert_eq!(
        poll_commit_pending(&cp(rx, now), now + Duration::from_millis(1)),
        CommitPoll::Abort("flush_ack_watchdog")
    );
}

/// A buffered ack must WIN over the watchdog (`try_recv` checked first): even past the
/// deadline a delivered reply commits rather than aborting.
#[test]
fn buffered_ack_beats_watchdog() {
    let (tx, rx) = crossbeam_channel::bounded::<()>(1);
    tx.send(()).expect("send ack");
    let now = Instant::now();
    assert_eq!(
        poll_commit_pending(&cp(rx, now), now + Duration::from_secs(1)),
        CommitPoll::Commit,
        "a buffered ack must commit even past the watchdog deadline"
    );
}
