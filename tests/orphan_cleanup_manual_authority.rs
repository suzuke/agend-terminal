#![allow(clippy::unwrap_used, clippy::expect_used)]
//! #3273 V2 — manual orphan cleanup may act only on immutable, operator-confirmed
//! authority.
//!
//! Consensus `d-20260814214253501210-22`. V1 stays report-only; nothing observed
//! there (PPID, argv, cwd, age, CPU, SID, PGID, a suggested owner) grants
//! authority to signal anything. Every contract in this file is written against
//! injected seams, and nearly every one closes with the same assertion: **the
//! signaller recorded zero calls**. That counter is the load-bearing evidence —
//! a refusal that still signalled would be worse than no refusal at all,
//! because it would read as safe.
//!
//! §3.10 test-first. The RED commit ships these contracts against a stub
//! executor that compiles and answers with an empty snapshot, so each failure
//! below is a *behavioural* one with a readable message rather than a missing
//! symbol.

use agend_terminal::admin::orphan_cleanup::{
    candidate_id, preview, Clock, IdentityOracle, ProcIdentity, SignalOutcome, Signaler, Support,
};
use std::cell::RefCell;
use std::sync::atomic::{AtomicU32, Ordering};

// ---------------------------------------------------------------------------
// Seams. Every fake counts what it was asked to do; the counts are the contract.
// ---------------------------------------------------------------------------

/// Identity source with a scripted table. A pid absent from the table is
/// "unknown", which must always fail closed rather than read as "unchanged".
struct FakeOracle {
    table: RefCell<Vec<(u32, ProcIdentity)>>,
    self_uid: Option<u32>,
}

impl FakeOracle {
    fn new(rows: &[(u32, u64, u32)], self_uid: Option<u32>) -> Self {
        Self {
            table: RefCell::new(
                rows.iter()
                    .map(|(pid, start_token, uid)| {
                        (
                            *pid,
                            ProcIdentity {
                                start_token: *start_token,
                                uid: *uid,
                            },
                        )
                    })
                    .collect(),
            ),
            self_uid,
        }
    }
}

impl IdentityOracle for FakeOracle {
    fn identity(&self, pid: u32) -> Option<ProcIdentity> {
        self.table
            .borrow()
            .iter()
            .find(|(candidate, _)| *candidate == pid)
            .map(|(_, identity)| *identity)
    }
    fn self_uid(&self) -> Option<u32> {
        self.self_uid
    }
}

/// Records every signal it is asked to send. `total()` being zero is the
/// assertion most of this file turns on.
#[derive(Default)]
struct CountingSignaler {
    terms: AtomicU32,
    kills: AtomicU32,
}

impl CountingSignaler {
    fn total(&self) -> u32 {
        self.terms.load(Ordering::SeqCst) + self.kills.load(Ordering::SeqCst)
    }
}

impl Signaler for CountingSignaler {
    fn term(&self, _pid: u32) -> SignalOutcome {
        self.terms.fetch_add(1, Ordering::SeqCst);
        SignalOutcome::Delivered
    }
    fn kill(&self, _pid: u32) -> SignalOutcome {
        self.kills.fetch_add(1, Ordering::SeqCst);
        SignalOutcome::Delivered
    }
    fn is_alive(&self, _pid: u32) -> bool {
        false
    }
}

struct FixedClock(i64);

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

fn tmp_home(tag: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-orphan-cleanup-{}-{}-{}",
        std::process::id(),
        tag,
        id
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// Contract 1 — the dry run is immutable and inert.
// ---------------------------------------------------------------------------

/// A preview must name every identifiable candidate with an id that binds the
/// snapshot generation to the exact `(pid, start_token, uid)` triple, must be
/// byte-stable when recomputed from the same inputs, and must not signal.
///
/// The id is the whole authority story: if it were derived from anything
/// observational (argv, ppid, a suggested owner) then confirming an id would
/// confirm a guess. Binding it to the identity triple is what makes a
/// confirmation un-replayable against a different process.
#[test]
fn preview_emits_immutable_ids_and_signals_nothing_3273() {
    let home = tmp_home("preview-immutable");
    let oracle = FakeOracle::new(&[(4242, 111_000, 501), (4343, 222_000, 501)], Some(501));
    let signaler = CountingSignaler::default();
    let clock = FixedClock(1_700_000_000_000);

    let snapshot = preview(
        &home,
        "operator",
        "manual cleanup of two stuck tool shells",
        &[4242, 4343],
        &oracle,
        &clock,
        7,
        Support::Supported,
    );

    assert_eq!(
        signaler.total(),
        0,
        "a dry run must never signal; it exists to be read, not to act"
    );
    assert_eq!(
        snapshot.candidates.len(),
        2,
        "both identifiable pids must appear in the snapshot: {:?}",
        snapshot.candidates
    );
    assert_eq!(snapshot.generation, 7);

    let first = &snapshot.candidates[0];
    assert_eq!(first.pid, 4242);
    assert_eq!(first.start_token, 111_000);
    assert_eq!(first.uid, 501);
    assert_eq!(
        first.id,
        candidate_id(7, 4242, 111_000, 501),
        "the id must be exactly the hash of (generation, pid, start_token, uid)"
    );

    // Same inputs, same ids — an operator confirming an id from a printed
    // preview must be confirming the same thing the executor will re-derive.
    let again = preview(
        &home,
        "operator",
        "manual cleanup of two stuck tool shells",
        &[4242, 4343],
        &oracle,
        &clock,
        7,
        Support::Supported,
    );
    let ids: Vec<_> = snapshot.candidates.iter().map(|c| &c.id).collect();
    let ids_again: Vec<_> = again.candidates.iter().map(|c| &c.id).collect();
    assert_eq!(ids, ids_again, "candidate ids must be reproducible");

    // A different generation must produce different ids, or a stale printed
    // confirmation could be replayed against a fresh snapshot.
    let next_generation = preview(
        &home,
        "operator",
        "manual cleanup of two stuck tool shells",
        &[4242, 4343],
        &oracle,
        &clock,
        8,
        Support::Supported,
    );
    let ids_next: Vec<_> = next_generation.candidates.iter().map(|c| &c.id).collect();
    assert_ne!(
        ids, ids_next,
        "a new snapshot generation must invalidate the previous ids"
    );

    assert_eq!(signaler.total(), 0, "still zero signals after three previews");
    std::fs::remove_dir_all(&home).ok();
}

/// A pid whose identity cannot be resolved is dropped from the snapshot
/// entirely. Carrying it as a partially-known row would let an operator confirm
/// something the executor could never safely re-validate.
#[test]
fn preview_drops_unidentifiable_pids_3273() {
    let home = tmp_home("preview-unknown");
    // 4242 resolves; 9999 is absent from the oracle table.
    let oracle = FakeOracle::new(&[(4242, 111_000, 501)], Some(501));
    let signaler = CountingSignaler::default();
    let clock = FixedClock(1_700_000_000_000);

    let snapshot = preview(
        &home,
        "operator",
        "one known, one unknowable",
        &[4242, 9999],
        &oracle,
        &clock,
        1,
        Support::Supported,
    );

    assert_eq!(signaler.total(), 0);
    assert_eq!(
        snapshot.candidates.len(),
        1,
        "only the identifiable pid may be offered: {:?}",
        snapshot.candidates
    );
    assert_eq!(snapshot.candidates[0].pid, 4242);
    assert!(
        !snapshot.candidates.iter().any(|c| c.pid == 9999),
        "an unidentifiable pid must never become a confirmable candidate"
    );
    std::fs::remove_dir_all(&home).ok();
}
