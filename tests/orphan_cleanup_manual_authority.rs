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
    apply, apply_with_barrier, candidate_id, confirmation_path, preview, ApplyOutcome, Clock,
    Confirmation, ConsumeBarrier, IdentityOracle, PreSignalAudit, PreviewError, ProcIdentity,
    RefusalReason, SignalOutcome, Signaler, Support,
};
use std::sync::atomic::{AtomicU32, Ordering};

// ---------------------------------------------------------------------------
// Seams. Every fake counts what it was asked to do; the counts are the contract.
// ---------------------------------------------------------------------------

/// Identity source with a scripted table. A pid absent from the table is
/// "unknown", which must always fail closed rather than read as "unchanged".
struct FakeOracle {
    table: Vec<(u32, ProcIdentity)>,
    self_uid: Option<u32>,
}

impl FakeOracle {
    fn new(rows: &[(u32, u64, u32)], self_uid: Option<u32>) -> Self {
        Self {
            table: rows
                .iter()
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
            self_uid,
        }
    }
}

impl IdentityOracle for FakeOracle {
    fn identity(&self, pid: u32) -> Option<ProcIdentity> {
        self.table
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
    ).expect("preview must be issuable");

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
    ).expect("preview must be issuable");
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
    ).expect("preview must be issuable");
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
    ).expect("preview must be issuable");

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

// ---------------------------------------------------------------------------
// Seams needed from the apply side.
// ---------------------------------------------------------------------------

/// Audit sink that records what it was asked to persist, and can be armed to
/// fail so the "audit failure refuses the signal" contract has a seam.
#[derive(Default)]
struct RecordingAudit {
    writes: AtomicU32,
    fail: bool,
}

impl RecordingAudit {
    fn writes(&self) -> u32 {
        self.writes.load(Ordering::SeqCst)
    }
}

impl agend_terminal::admin::orphan_cleanup::AuditStore for RecordingAudit {
    fn record_pre_signal(&self, _record: &PreSignalAudit) -> anyhow::Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            anyhow::bail!("forced audit failure");
        }
        Ok(())
    }
}

const ACTOR: &str = "operator";
const REASON: &str = "two tool shells stuck after a crashed run";

/// Preview two live candidates and hand back (home, snapshot).
fn previewed(tag: &str, now_ms: i64) -> (std::path::PathBuf, FakeOracle, CountingSignaler,
    agend_terminal::admin::orphan_cleanup::Preview) {
    let home = tmp_home(tag);
    let oracle = FakeOracle::new(&[(4242, 111_000, 501), (4343, 222_000, 501)], Some(501));
    let signaler = CountingSignaler::default();
    let clock = FixedClock(now_ms);
    let snapshot = preview(&home, ACTOR, REASON, &[4242, 4343], &oracle, &clock, 3, Support::Supported)
        .expect("preview must be issuable");
    (home, oracle, signaler, snapshot)
}

fn ids_of(snapshot: &agend_terminal::admin::orphan_cleanup::Preview) -> Vec<String> {
    snapshot.candidates.iter().map(|c| c.id.clone()).collect()
}

// ---------------------------------------------------------------------------
// Contract 2 — the confirmation is durable before it can be spent.
// ---------------------------------------------------------------------------

/// A preview must leave a parseable sidecar bound to the actor, the reason, the
/// generation and a stored nonce, marked unconsumed. Without a durable record
/// there is nothing for `apply` to revalidate against, and "the operator saw
/// this exact list" would rest on the operator's memory.
#[test]
fn preview_persists_a_durable_unconsumed_confirmation_3273() {
    let (home, _oracle, signaler, snapshot) = previewed("sidecar-durable", 1_700_000_000_000);

    let path = confirmation_path(&home, &snapshot.token).expect("token must be path-safe");
    let raw = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("preview must persist {}: {e}", path.display()));
    let stored: Confirmation =
        serde_json::from_slice(&raw).expect("sidecar must parse as a Confirmation");

    assert_eq!(stored.schema_version, 1);
    assert_eq!(stored.actor, ACTOR);
    assert_eq!(stored.audit_reason, REASON);
    assert_eq!(stored.generation, 3);
    assert!(!stored.nonce.is_empty(), "the snapshot nonce must be stored, not only hashed into ids");
    assert!(!stored.consumed, "a fresh confirmation must be unconsumed");
    assert_eq!(ids_of(&snapshot), stored.candidates.iter().map(|c| c.id.clone()).collect::<Vec<_>>());
    assert_eq!(signaler.total(), 0, "persisting a confirmation must not signal");
    std::fs::remove_dir_all(&home).ok();
}

// ---------------------------------------------------------------------------
// Contract 3 — every apply refusal names its own cause, and signals nothing.
// ---------------------------------------------------------------------------

/// Each case below asserts the EXACT refusal. A blanket "it refused" assertion
/// would be satisfied by an executor that refuses everything for the wrong
/// reason — including one that would later stop refusing for the right one.
#[test]
fn apply_refusals_name_their_cause_and_signal_nothing_3273() {
    let audit = RecordingAudit::default();

    // --- no token at all -------------------------------------------------
    let (home, oracle, signaler, snapshot) = previewed("refuse-missing-token", 1_700_000_000_000);
    let clock = FixedClock(1_700_000_000_000);
    let outcome = apply(&home, ACTOR, REASON, "", &ids_of(&snapshot), &oracle, &signaler, &clock, &audit, Support::Supported);
    assert_eq!(outcome, ApplyOutcome::Refused(RefusalReason::MissingToken));
    assert_eq!(signaler.total(), 0);
    assert_eq!(
        audit.writes(),
        0,
        "an unauthorized attempt must not emit a pre-signal audit record"
    );
    std::fs::remove_dir_all(&home).ok();

    // --- a token that is not of the accepted shape ------------------------
    // Includes a traversal attempt: the shape check is what stops a token
    // being spent as a path component.
    let (home, oracle, signaler, snapshot) = previewed("refuse-malformed", 1_700_000_000_000);
    for bad in ["../../etc/passwd", "not-a-token", &"f".repeat(35), &"g".repeat(36)] {
        let outcome = apply(&home, ACTOR, REASON, bad, &ids_of(&snapshot), &oracle, &signaler, &clock, &audit, Support::Supported);
        assert_eq!(
            outcome,
            ApplyOutcome::Refused(RefusalReason::MalformedToken),
            "token {bad:?} must be refused on shape alone"
        );
    }
    assert_eq!(signaler.total(), 0);
    std::fs::remove_dir_all(&home).ok();

    // --- well-formed token with no sidecar behind it ----------------------
    let (home, oracle, signaler, snapshot) = previewed("refuse-no-sidecar", 1_700_000_000_000);
    let orphan_token = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let outcome = apply(&home, ACTOR, REASON, orphan_token, &ids_of(&snapshot), &oracle, &signaler, &clock, &audit, Support::Supported);
    assert_eq!(outcome, ApplyOutcome::Refused(RefusalReason::ConfirmationUnavailable));
    assert_eq!(signaler.total(), 0);
    std::fs::remove_dir_all(&home).ok();

    // --- corrupt sidecar --------------------------------------------------
    let (home, oracle, signaler, snapshot) = previewed("refuse-corrupt", 1_700_000_000_000);
    let path = confirmation_path(&home, &snapshot.token).unwrap();
    std::fs::write(&path, b"{ this is not json").unwrap();
    let outcome = apply(&home, ACTOR, REASON, &snapshot.token, &ids_of(&snapshot), &oracle, &signaler, &clock, &audit, Support::Supported);
    assert_eq!(outcome, ApplyOutcome::Refused(RefusalReason::ConfirmationUnavailable));
    assert_eq!(signaler.total(), 0);
    std::fs::remove_dir_all(&home).ok();

    // --- empty audit reason ----------------------------------------------
    let (home, oracle, signaler, snapshot) = previewed("refuse-empty-reason", 1_700_000_000_000);
    let outcome = apply(&home, ACTOR, "", &snapshot.token, &ids_of(&snapshot), &oracle, &signaler, &clock, &audit, Support::Supported);
    assert_eq!(outcome, ApplyOutcome::Refused(RefusalReason::EmptyAuditReason));
    assert_eq!(signaler.total(), 0);
    std::fs::remove_dir_all(&home).ok();

    // --- actor does not match the confirmation ----------------------------
    let (home, oracle, signaler, snapshot) = previewed("refuse-actor", 1_700_000_000_000);
    let outcome = apply(&home, "someone-else", REASON, &snapshot.token, &ids_of(&snapshot), &oracle, &signaler, &clock, &audit, Support::Supported);
    assert_eq!(outcome, ApplyOutcome::Refused(RefusalReason::ActorMismatch));
    assert_eq!(signaler.total(), 0);
    std::fs::remove_dir_all(&home).ok();

    // --- reason does not match the confirmation ---------------------------
    let (home, oracle, signaler, snapshot) = previewed("refuse-reason", 1_700_000_000_000);
    let outcome = apply(&home, ACTOR, "a different justification", &snapshot.token, &ids_of(&snapshot), &oracle, &signaler, &clock, &audit, Support::Supported);
    assert_eq!(outcome, ApplyOutcome::Refused(RefusalReason::AuditReasonMismatch));
    assert_eq!(signaler.total(), 0);
    assert_eq!(
        audit.writes(),
        0,
        "no refusal above may have emitted a pre-signal audit record"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// The snapshot asserts a liveness fact and liveness decays, so a confirmation
/// has a short shelf life. A clock that has moved past the TTL must refuse, and
/// a clock that has moved BACKWARDS must refuse too — a negative age is not a
/// fresh confirmation, it is an unusable one.
#[test]
fn apply_outside_the_ttl_window_refuses_3273() {
    let audit = RecordingAudit::default();
    let created = 1_700_000_000_000_i64;

    let (home, oracle, signaler, snapshot) = previewed("refuse-expired", created);
    let expired = FixedClock(created + agend_terminal::admin::orphan_cleanup::CONFIRM_TTL_MS + 1);
    let outcome = apply(&home, ACTOR, REASON, &snapshot.token, &ids_of(&snapshot), &oracle, &signaler, &expired, &audit, Support::Supported);
    assert_eq!(outcome, ApplyOutcome::Refused(RefusalReason::Expired));
    assert_eq!(signaler.total(), 0);
    std::fs::remove_dir_all(&home).ok();

    let (home, oracle, signaler, snapshot) = previewed("refuse-backwards", created);
    let backwards = FixedClock(created - 1);
    let outcome = apply(&home, ACTOR, REASON, &snapshot.token, &ids_of(&snapshot), &oracle, &signaler, &backwards, &audit, Support::Supported);
    assert_eq!(
        outcome,
        ApplyOutcome::Refused(RefusalReason::Expired),
        "a confirmation from the future is not fresh, it is unusable"
    );
    assert_eq!(signaler.total(), 0);
    std::fs::remove_dir_all(&home).ok();
}

/// All-or-nothing. A proper subset, a superset, an unknown id, and a duplicate
/// must each refuse outright: the operator confirmed one exact list, and
/// signalling a part of it would be acting on authority nobody granted.
#[test]
fn apply_with_non_exact_confirm_ids_refuses_3273() {
    let audit = RecordingAudit::default();
    let clock = FixedClock(1_700_000_000_000);
    let (home, oracle, signaler, snapshot) = previewed("refuse-ids", 1_700_000_000_000);
    let ids = ids_of(&snapshot);
    assert_eq!(ids.len(), 2, "premise: the snapshot must offer two candidates");

    let subset = vec![ids[0].clone()];
    let superset = {
        let mut v = ids.clone();
        v.push(candidate_id(3, 5555, 333_000, 501));
        v
    };
    let unknown = vec![candidate_id(3, 5555, 333_000, 501), ids[1].clone()];
    let empty: Vec<String> = Vec::new();

    // A duplicate is not a second confirmation of the same candidate; it is a
    // set that does not equal the snapshot's, and equality is the whole rule.
    let duplicated = vec![ids[0].clone(), ids[1].clone(), ids[1].clone()];

    for (label, set) in [
        ("proper subset", subset),
        ("superset", superset),
        ("unknown id swapped in", unknown),
        ("empty", empty),
        ("duplicated id", duplicated),
    ] {
        let outcome = apply(&home, ACTOR, REASON, &snapshot.token, &set, &oracle, &signaler, &clock, &audit, Support::Supported);
        assert_eq!(
            outcome,
            ApplyOutcome::Refused(RefusalReason::ConfirmIdsNotExact),
            "{label} must refuse the whole batch"
        );
        assert_eq!(signaler.total(), 0, "{label} must signal nothing");
    }
    std::fs::remove_dir_all(&home).ok();
}

/// A confirmation is single-use. Once consumed it can never authorise anything
/// again, however well-formed the replay looks.
#[test]
fn apply_with_a_consumed_confirmation_refuses_3273() {
    let audit = RecordingAudit::default();
    let clock = FixedClock(1_700_000_000_000);
    let (home, oracle, signaler, snapshot) = previewed("refuse-replay", 1_700_000_000_000);

    // Mark the sidecar consumed directly: this contract is about the replay
    // gate, not about what consumed it.
    let path = confirmation_path(&home, &snapshot.token).unwrap();
    let mut stored: Confirmation = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    stored.consumed = true;
    std::fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();

    let outcome = apply(&home, ACTOR, REASON, &snapshot.token, &ids_of(&snapshot), &oracle, &signaler, &clock, &audit, Support::Supported);
    assert_eq!(outcome, ApplyOutcome::Refused(RefusalReason::Replayed));
    assert_eq!(signaler.total(), 0);
    std::fs::remove_dir_all(&home).ok();
}

/// An unsupported platform or backend stays report-only: preview may still
/// render, but apply refuses and signals nothing regardless of how correct the
/// confirmation is.
#[test]
fn apply_on_an_unsupported_platform_refuses_3273() {
    let audit = RecordingAudit::default();
    let clock = FixedClock(1_700_000_000_000);
    let (home, oracle, signaler, snapshot) = previewed("refuse-unsupported", 1_700_000_000_000);

    let outcome = apply(
        &home, ACTOR, REASON, &snapshot.token, &ids_of(&snapshot), &oracle, &signaler, &clock, &audit,
        Support::Unsupported("no identity oracle on this platform".to_string()),
    );
    assert_eq!(outcome, ApplyOutcome::Refused(RefusalReason::Unsupported));
    assert_eq!(signaler.total(), 0);
    std::fs::remove_dir_all(&home).ok();
}

// ---------------------------------------------------------------------------
// Contract 4 — findings from the lead's source review of this slice.
// ---------------------------------------------------------------------------

/// A sidecar whose schema this build does not understand is not a weaker
/// confirmation, it is an unreadable one. Refuse on the exact cause.
#[test]
fn apply_with_an_unknown_sidecar_schema_refuses_3273() {
    let audit = RecordingAudit::default();
    let clock = FixedClock(1_700_000_000_000);
    let (home, oracle, signaler, snapshot) = previewed("refuse-schema", 1_700_000_000_000);

    let path = confirmation_path(&home, &snapshot.token).unwrap();
    let mut stored: Confirmation = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    stored.schema_version = 2;
    std::fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();

    let outcome = apply(&home, ACTOR, REASON, &snapshot.token, &ids_of(&snapshot), &oracle, &signaler, &clock, &audit, Support::Supported);
    assert_eq!(outcome, ApplyOutcome::Refused(RefusalReason::SchemaMismatch));
    assert_eq!(signaler.total(), 0);
    assert_eq!(audit.writes(), 0, "an unreadable confirmation must not produce a pre-signal audit");
    std::fs::remove_dir_all(&home).ok();
}

/// A preview that cannot durably persist its sidecar must NOT hand back a
/// token. A token naming a confirmation that apply can never revalidate is an
/// authority artefact with no authority behind it — and the operator would have
/// no way to tell it apart from a real one.
#[test]
fn preview_that_cannot_persist_issues_no_token_3273() {
    let home = tmp_home("preview-persist-fail");
    // Occupy the directory path with a regular file so the sidecar cannot be
    // created. Deterministic, no permissions assumptions, works as any user.
    std::fs::write(home.join("orphan-cleanup"), b"not a directory").unwrap();

    let oracle = FakeOracle::new(&[(4242, 111_000, 501)], Some(501));
    let signaler = CountingSignaler::default();
    let clock = FixedClock(1_700_000_000_000);

    let issued = preview(&home, ACTOR, REASON, &[4242], &oracle, &clock, 1, Support::Supported);

    match issued {
        Err(PreviewError::PersistFailed(_)) => {}
        Ok(snapshot) => panic!(
            "preview handed out token {:?} despite failing to persist its sidecar",
            snapshot.token
        ),
    }
    assert_eq!(signaler.total(), 0);
    std::fs::remove_dir_all(&home).ok();
}

/// Single-use must survive concurrency, not merely sequence. A
/// `read(consumed==false)` followed by `save(consumed=true)` is not a CAS: two
/// applies can both read `false` and both proceed. Exactly one may win; the
/// loser must refuse and must never signal.
///
/// The barrier below is what makes that race deterministic instead of a matter
/// of timing luck. It waits with a bounded timeout on purpose: an
/// implementation that serialises properly will have the second caller blocked
/// outside the critical section, so the first would otherwise wait forever for
/// a partner that cannot arrive.
#[test]
fn concurrent_applies_consume_a_confirmation_exactly_once_3273() {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    struct RendezvousBarrier {
        state: Mutex<u32>,
        cv: Condvar,
    }
    impl ConsumeBarrier for RendezvousBarrier {
        fn before_consume(&self) {
            let mut arrived = self.state.lock().unwrap();
            *arrived += 1;
            if *arrived >= 2 {
                self.cv.notify_all();
                return;
            }
            // Bounded: a correctly serialised implementation keeps the second
            // caller outside the critical section, so no partner can arrive.
            let _ = self
                .cv
                .wait_timeout(arrived, Duration::from_millis(250))
                .unwrap();
        }
    }

    let (home, _oracle, _signaler, snapshot) = previewed("cas-race", 1_700_000_000_000);
    let home = Arc::new(home);
    let token = Arc::new(snapshot.token.clone());
    let ids = Arc::new(ids_of(&snapshot));
    let barrier = Arc::new(RendezvousBarrier {
        state: Mutex::new(0),
        cv: Condvar::new(),
    });

    let mut handles = Vec::new();
    for _ in 0..2 {
        let home = Arc::clone(&home);
        let token = Arc::clone(&token);
        let ids = Arc::clone(&ids);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            // fire-and-forget: joined below before any assertion.
            let oracle = FakeOracle::new(&[(4242, 111_000, 501), (4343, 222_000, 501)], Some(501));
            let signaler = CountingSignaler::default();
            let audit = RecordingAudit::default();
            let clock = FixedClock(1_700_000_000_000);
            let outcome = apply_with_barrier(
                &home, ACTOR, REASON, &token, &ids, &oracle, &signaler, &clock, &audit,
                Support::Supported, &*barrier,
            );
            (outcome, signaler.total())
        }));
    }

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    // A "winner" is an apply that got past every gate to the executor stage —
    // not merely one that avoided the replay refusal. The distinction matters:
    // an unserialised implementation can also produce a TORN READ, and counting
    // that as a loser would let the race pass for the wrong reason.
    let winners = results
        .iter()
        .filter(|(outcome, _)| {
            matches!(
                outcome,
                ApplyOutcome::Refused(RefusalReason::ExecutorUnavailable)
            )
        })
        .count();
    assert_eq!(
        winners, 1,
        "exactly one apply may consume a single-use confirmation: {results:?}"
    );
    // And the other one must have lost cleanly — a torn read or an unreadable
    // confirmation is a serialization failure wearing a refusal's clothes.
    let clean_losers = results
        .iter()
        .filter(|(outcome, _)| {
            matches!(
                outcome,
                ApplyOutcome::Refused(RefusalReason::Replayed)
                    | ApplyOutcome::Refused(RefusalReason::Contended)
            )
        })
        .count();
    assert_eq!(
        clean_losers, 1,
        "the loser must refuse as already-spent, not stumble over a half-written file: {results:?}"
    );
    for (_, signals) in &results {
        assert_eq!(*signals, 0, "no signal may be emitted in this slice");
    }

    // And the durable record must agree with the outcome: consumed, once.
    let path = confirmation_path(&home, &token).unwrap();
    let stored: Confirmation = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(stored.consumed, "the winner's consumption must be durable");
    std::fs::remove_dir_all(&*home).ok();
}
