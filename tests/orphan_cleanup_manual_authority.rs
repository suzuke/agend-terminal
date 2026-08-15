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
    apply, apply_with_barrier, candidate_id, confirmation_path, preview, ApplyOutcome,
    CandidateOutcome, Clock, Confirmation, ConsumeBarrier, ExitWaiter, IdentityOracle,
    PreSignalAudit, PreviewError, ProcIdentity, RefusalReason, SignalOutcome, Signaler, Support,
    GRACE_MS,
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
}

/// The waiter used by the authority-slice contracts, which never reach a
/// signal. Recorded anyway so an accidental wait would show up.
#[derive(Default)]
struct NeverWaits {
    waits: AtomicU32,
}

impl ExitWaiter for NeverWaits {
    fn wait_for_exit(&self, _pid: u32, _timeout_ms: u64) -> bool {
        self.waits.fetch_add(1, Ordering::SeqCst);
        true
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
    )
    .expect("preview must be issuable");

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
    )
    .expect("preview must be issuable");
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
    )
    .expect("preview must be issuable");
    let ids_next: Vec<_> = next_generation.candidates.iter().map(|c| &c.id).collect();
    assert_ne!(
        ids, ids_next,
        "a new snapshot generation must invalidate the previous ids"
    );

    assert_eq!(
        signaler.total(),
        0,
        "still zero signals after three previews"
    );
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
    )
    .expect("preview must be issuable");

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
fn previewed(
    tag: &str,
    now_ms: i64,
) -> (
    std::path::PathBuf,
    FakeOracle,
    CountingSignaler,
    agend_terminal::admin::orphan_cleanup::Preview,
) {
    let home = tmp_home(tag);
    let oracle = FakeOracle::new(&[(4242, 111_000, 501), (4343, 222_000, 501)], Some(501));
    let signaler = CountingSignaler::default();
    let clock = FixedClock(now_ms);
    let snapshot = preview(
        &home,
        ACTOR,
        REASON,
        &[4242, 4343],
        &oracle,
        &clock,
        3,
        Support::Supported,
    )
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
    assert!(
        !stored.nonce.is_empty(),
        "the snapshot nonce must be stored, not only hashed into ids"
    );
    assert!(!stored.consumed, "a fresh confirmation must be unconsumed");
    assert_eq!(
        ids_of(&snapshot),
        stored
            .candidates
            .iter()
            .map(|c| c.id.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        signaler.total(),
        0,
        "persisting a confirmation must not signal"
    );
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
    let outcome = apply(
        &home,
        ACTOR,
        REASON,
        "",
        &ids_of(&snapshot),
        &oracle,
        &signaler,
        &clock,
        &audit,
        &NeverWaits::default(),
        Support::Supported,
    );
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
    for bad in [
        "../../etc/passwd",
        "not-a-token",
        &"f".repeat(35),
        &"g".repeat(36),
    ] {
        let outcome = apply(
            &home,
            ACTOR,
            REASON,
            bad,
            &ids_of(&snapshot),
            &oracle,
            &signaler,
            &clock,
            &audit,
            &NeverWaits::default(),
            Support::Supported,
        );
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
    let outcome = apply(
        &home,
        ACTOR,
        REASON,
        orphan_token,
        &ids_of(&snapshot),
        &oracle,
        &signaler,
        &clock,
        &audit,
        &NeverWaits::default(),
        Support::Supported,
    );
    assert_eq!(
        outcome,
        ApplyOutcome::Refused(RefusalReason::ConfirmationUnavailable)
    );
    assert_eq!(signaler.total(), 0);
    std::fs::remove_dir_all(&home).ok();

    // --- corrupt sidecar --------------------------------------------------
    let (home, oracle, signaler, snapshot) = previewed("refuse-corrupt", 1_700_000_000_000);
    let path = confirmation_path(&home, &snapshot.token).unwrap();
    std::fs::write(&path, b"{ this is not json").unwrap();
    let outcome = apply(
        &home,
        ACTOR,
        REASON,
        &snapshot.token,
        &ids_of(&snapshot),
        &oracle,
        &signaler,
        &clock,
        &audit,
        &NeverWaits::default(),
        Support::Supported,
    );
    assert_eq!(
        outcome,
        ApplyOutcome::Refused(RefusalReason::ConfirmationUnavailable)
    );
    assert_eq!(signaler.total(), 0);
    std::fs::remove_dir_all(&home).ok();

    // --- empty audit reason ----------------------------------------------
    let (home, oracle, signaler, snapshot) = previewed("refuse-empty-reason", 1_700_000_000_000);
    let outcome = apply(
        &home,
        ACTOR,
        "",
        &snapshot.token,
        &ids_of(&snapshot),
        &oracle,
        &signaler,
        &clock,
        &audit,
        &NeverWaits::default(),
        Support::Supported,
    );
    assert_eq!(
        outcome,
        ApplyOutcome::Refused(RefusalReason::EmptyAuditReason)
    );
    assert_eq!(signaler.total(), 0);
    std::fs::remove_dir_all(&home).ok();

    // --- actor does not match the confirmation ----------------------------
    let (home, oracle, signaler, snapshot) = previewed("refuse-actor", 1_700_000_000_000);
    let outcome = apply(
        &home,
        "someone-else",
        REASON,
        &snapshot.token,
        &ids_of(&snapshot),
        &oracle,
        &signaler,
        &clock,
        &audit,
        &NeverWaits::default(),
        Support::Supported,
    );
    assert_eq!(outcome, ApplyOutcome::Refused(RefusalReason::ActorMismatch));
    assert_eq!(signaler.total(), 0);
    std::fs::remove_dir_all(&home).ok();

    // --- reason does not match the confirmation ---------------------------
    let (home, oracle, signaler, snapshot) = previewed("refuse-reason", 1_700_000_000_000);
    let outcome = apply(
        &home,
        ACTOR,
        "a different justification",
        &snapshot.token,
        &ids_of(&snapshot),
        &oracle,
        &signaler,
        &clock,
        &audit,
        &NeverWaits::default(),
        Support::Supported,
    );
    assert_eq!(
        outcome,
        ApplyOutcome::Refused(RefusalReason::AuditReasonMismatch)
    );
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
    let outcome = apply(
        &home,
        ACTOR,
        REASON,
        &snapshot.token,
        &ids_of(&snapshot),
        &oracle,
        &signaler,
        &expired,
        &audit,
        &NeverWaits::default(),
        Support::Supported,
    );
    assert_eq!(outcome, ApplyOutcome::Refused(RefusalReason::Expired));
    assert_eq!(signaler.total(), 0);
    std::fs::remove_dir_all(&home).ok();

    let (home, oracle, signaler, snapshot) = previewed("refuse-backwards", created);
    let backwards = FixedClock(created - 1);
    let outcome = apply(
        &home,
        ACTOR,
        REASON,
        &snapshot.token,
        &ids_of(&snapshot),
        &oracle,
        &signaler,
        &backwards,
        &audit,
        &NeverWaits::default(),
        Support::Supported,
    );
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
    assert_eq!(
        ids.len(),
        2,
        "premise: the snapshot must offer two candidates"
    );

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
        let outcome = apply(
            &home,
            ACTOR,
            REASON,
            &snapshot.token,
            &set,
            &oracle,
            &signaler,
            &clock,
            &audit,
            &NeverWaits::default(),
            Support::Supported,
        );
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

    let outcome = apply(
        &home,
        ACTOR,
        REASON,
        &snapshot.token,
        &ids_of(&snapshot),
        &oracle,
        &signaler,
        &clock,
        &audit,
        &NeverWaits::default(),
        Support::Supported,
    );
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
        &home,
        ACTOR,
        REASON,
        &snapshot.token,
        &ids_of(&snapshot),
        &oracle,
        &signaler,
        &clock,
        &audit,
        &NeverWaits::default(),
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

    let outcome = apply(
        &home,
        ACTOR,
        REASON,
        &snapshot.token,
        &ids_of(&snapshot),
        &oracle,
        &signaler,
        &clock,
        &audit,
        &NeverWaits::default(),
        Support::Supported,
    );
    assert_eq!(
        outcome,
        ApplyOutcome::Refused(RefusalReason::SchemaMismatch)
    );
    assert_eq!(signaler.total(), 0);
    assert_eq!(
        audit.writes(),
        0,
        "an unreadable confirmation must not produce a pre-signal audit"
    );
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

    let issued = preview(
        &home,
        ACTOR,
        REASON,
        &[4242],
        &oracle,
        &clock,
        1,
        Support::Supported,
    );

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
                &home,
                ACTOR,
                REASON,
                &token,
                &ids,
                &oracle,
                &signaler,
                &clock,
                &audit,
                &NeverWaits::default(),
                Support::Supported,
                &*barrier,
            );
            (outcome, signaler.total())
        }));
    }

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    // A "winner" is an apply that consumed the confirmation and went on to act —
    // not merely one that avoided the replay refusal. The distinction matters:
    // an unserialised implementation can also produce a TORN READ, and counting
    // that as a loser would let the race pass for the wrong reason.
    let winners = results
        .iter()
        .filter(|(outcome, _)| matches!(outcome, ApplyOutcome::Applied(_)))
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
    // The loser must have signalled nothing at all: losing the CAS has to mean
    // losing the authority to act, not merely losing the right to say so.
    for (outcome, signals) in &results {
        match outcome {
            ApplyOutcome::Applied(_) => assert_eq!(
                *signals, 2,
                "the winner acts on exactly the confirmed set, once each"
            ),
            _ => assert_eq!(
                *signals, 0,
                "the loser must not signal anything: {outcome:?}"
            ),
        }
    }

    // And the durable record must agree with the outcome: consumed, once.
    let path = confirmation_path(&home, &token).unwrap();
    let stored: Confirmation = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(stored.consumed, "the winner's consumption must be durable");
    std::fs::remove_dir_all(&*home).ok();
}

/// A confirmation token and its sidecar are authority material: whatever can
/// read them learns the exact triple an operator confirmed, and whatever can
/// write them can forge one. `File::create` honours the umask, so on a
/// permissive umask these would be group- or world-readable by default.
#[cfg(unix)]
#[test]
fn confirmation_material_is_private_to_the_owner_3273() {
    use std::os::unix::fs::PermissionsExt;

    let (home, oracle, signaler, snapshot) = previewed("private-modes", 1_700_000_000_000);
    let path = confirmation_path(&home, &snapshot.token).unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "sidecar must be owner-only, got {mode:o}");

    let dir_mode = std::fs::metadata(path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        dir_mode, 0o700,
        "confirmation dir must be owner-only, got {dir_mode:o}"
    );

    // The lock is created by apply, and names a confirmation by token, so it is
    // held to the same standard. This apply is deliberately made to refuse
    // before the executor — an oracle with no self uid — so the contract stays
    // about file modes and cannot start depending on signal behaviour.
    let audit = RecordingAudit::default();
    let clock = FixedClock(1_700_000_000_000);
    let uidless = FakeOracle::new(&[(4242, 111_000, 501), (4343, 222_000, 501)], None);
    let refusal = apply(
        &home,
        ACTOR,
        REASON,
        &snapshot.token,
        &ids_of(&snapshot),
        &uidless,
        &signaler,
        &clock,
        &audit,
        &NeverWaits::default(),
        Support::Supported,
    );
    assert_eq!(
        refusal,
        ApplyOutcome::Refused(RefusalReason::MissingSelfUid)
    );
    let _ = &oracle;
    let lock_mode = std::fs::metadata(path.with_extension("lock"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        lock_mode, 0o600,
        "lock must be owner-only, got {lock_mode:o}"
    );

    assert_eq!(signaler.total(), 0);
    std::fs::remove_dir_all(&home).ok();
}

// ---------------------------------------------------------------------------
// Executor seams — ONE ordered event log shared by every seam.
//
// Counters cannot express ordering, and ordering is most of what these
// contracts are about: "the audit was written" and "the audit was written
// BEFORE the TERM" are different claims, and only the second one is the
// consensus's requirement. Everything the executor does against the outside
// world therefore appends to a single log, and the contracts compare that log
// literally.
// ---------------------------------------------------------------------------

use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct EventLog(Arc<Mutex<Vec<String>>>);

impl EventLog {
    fn push(&self, event: String) {
        self.0.lock().unwrap().push(event);
    }
    fn events(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
    fn signals(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter(|e| e.starts_with("TERM:") || e.starts_with("KILL:"))
            .collect()
    }
}

/// An oracle whose answers change between reads, so a contract can model a
/// process exiting, a PID being recycled, or an identity becoming unreadable at
/// the exact read the executor depends on. The last scripted answer repeats.
struct ScriptedOracle {
    script: Mutex<Vec<(u32, Vec<Option<ProcIdentity>>)>>,
    self_uid: Option<u32>,
}

impl ScriptedOracle {
    fn new(script: &[(u32, Vec<Option<ProcIdentity>>)], self_uid: Option<u32>) -> Self {
        Self {
            script: Mutex::new(script.to_vec()),
            self_uid,
        }
    }
}

impl IdentityOracle for ScriptedOracle {
    fn identity(&self, pid: u32) -> Option<ProcIdentity> {
        let mut script = self.script.lock().unwrap();
        let row = script.iter_mut().find(|(candidate, _)| *candidate == pid)?;
        if row.1.len() > 1 {
            row.1.remove(0)
        } else {
            *row.1.first()?
        }
    }
    fn self_uid(&self) -> Option<u32> {
        self.self_uid
    }
}

#[derive(Default)]
struct SignalScript {
    /// TERM is refused for this pid.
    term_fails: Option<u32>,
    /// TERM finds the process already gone (ESRCH).
    term_gone: Vec<u32>,
    /// KILL is refused for this pid.
    kill_fails: Option<u32>,
    /// KILL finds the process already gone.
    kill_gone: Vec<u32>,
}

struct LoggingSignaler {
    log: EventLog,
    script: SignalScript,
}

impl Signaler for LoggingSignaler {
    fn term(&self, pid: u32) -> SignalOutcome {
        self.log.push(format!("TERM:{pid}"));
        if self.script.term_fails == Some(pid) {
            return SignalOutcome::PermissionDenied;
        }
        if self.script.term_gone.contains(&pid) {
            return SignalOutcome::NoSuchProcess;
        }
        SignalOutcome::Delivered
    }
    fn kill(&self, pid: u32) -> SignalOutcome {
        self.log.push(format!("KILL:{pid}"));
        if self.script.kill_fails == Some(pid) {
            return SignalOutcome::PermissionDenied;
        }
        if self.script.kill_gone.contains(&pid) {
            return SignalOutcome::NoSuchProcess;
        }
        SignalOutcome::Delivered
    }
}

struct LoggingAudit {
    log: EventLog,
    fail: bool,
}

impl agend_terminal::admin::orphan_cleanup::AuditStore for LoggingAudit {
    fn record_pre_signal(&self, record: &PreSignalAudit) -> anyhow::Result<()> {
        // Logged BEFORE the failure branch: an attempt that failed still
        // happened, and the ordering contract is about when it was attempted.
        self.log.push(format!("audit:{}", record.pid));
        if self.fail {
            anyhow::bail!("forced audit failure");
        }
        Ok(())
    }
}

/// Records the exact bound it was asked to wait for, and whether the pid
/// exited. `survives` lists pids that outlive the grace window.
struct LoggingWaiter {
    log: EventLog,
    survives: Vec<u32>,
}

impl ExitWaiter for LoggingWaiter {
    fn wait_for_exit(&self, pid: u32, timeout_ms: u64) -> bool {
        self.log.push(format!("wait:{pid}:{timeout_ms}"));
        !self.survives.contains(&pid)
    }
}

fn identity(start_token: u64, uid: u32) -> Option<ProcIdentity> {
    Some(ProcIdentity { start_token, uid })
}

struct Stage {
    home: std::path::PathBuf,
    oracle: ScriptedOracle,
    snapshot: agend_terminal::admin::orphan_cleanup::Preview,
    log: EventLog,
}

fn staged(tag: &str, script: &[(u32, Vec<Option<ProcIdentity>>)], self_uid: Option<u32>) -> Stage {
    let home = tmp_home(tag);
    let pids: Vec<u32> = script.iter().map(|(pid, _)| *pid).collect();
    let oracle = ScriptedOracle::new(script, self_uid);
    let clock = FixedClock(1_700_000_000_000);
    let snapshot = preview(
        &home,
        ACTOR,
        REASON,
        &pids,
        &oracle,
        &clock,
        5,
        Support::Supported,
    )
    .expect("preview must be issuable");
    Stage {
        home,
        oracle,
        snapshot,
        log: EventLog::default(),
    }
}

fn run(
    stage: &Stage,
    term_fails: Option<u32>,
    survives: &[u32],
    audit_fails: bool,
) -> ApplyOutcome {
    run_scripted(
        stage,
        SignalScript {
            term_fails,
            ..Default::default()
        },
        survives,
        audit_fails,
    )
}

fn run_scripted(
    stage: &Stage,
    script: SignalScript,
    survives: &[u32],
    audit_fails: bool,
) -> ApplyOutcome {
    let signaler = LoggingSignaler {
        log: stage.log.clone(),
        script,
    };
    let audit = LoggingAudit {
        log: stage.log.clone(),
        fail: audit_fails,
    };
    let waiter = LoggingWaiter {
        log: stage.log.clone(),
        survives: survives.to_vec(),
    };
    let clock = FixedClock(1_700_000_000_000);
    apply(
        &stage.home,
        ACTOR,
        REASON,
        &stage.snapshot.token,
        &ids_of(&stage.snapshot),
        &stage.oracle,
        &signaler,
        &clock,
        &audit,
        &waiter,
        Support::Supported,
    )
}

// ---------------------------------------------------------------------------
// Contract 5 — ownership. Unknown ownership is never own.
// ---------------------------------------------------------------------------

#[test]
fn apply_without_a_self_uid_refuses_3273() {
    let stage = staged("no-self-uid", &[(4242, vec![identity(111_000, 501)])], None);
    let outcome = run(&stage, None, &[], false);
    assert_eq!(
        outcome,
        ApplyOutcome::Refused(RefusalReason::MissingSelfUid)
    );
    assert!(
        stage.log.events().is_empty(),
        "nothing may happen without proven ownership: {:?}",
        stage.log.events()
    );
    std::fs::remove_dir_all(&stage.home).ok();
}

#[test]
fn apply_to_a_foreign_uid_refuses_3273() {
    let stage = staged(
        "foreign-uid",
        &[(4242, vec![identity(111_000, 999)])],
        Some(501),
    );
    let outcome = run(&stage, None, &[], false);
    assert_eq!(outcome, ApplyOutcome::Refused(RefusalReason::ForeignUid));
    assert!(stage.log.events().is_empty());
    std::fs::remove_dir_all(&stage.home).ok();
}

// ---------------------------------------------------------------------------
// Contract 6 — full-batch preflight, and the re-read that preflight does not
// replace.
// ---------------------------------------------------------------------------

/// One stale candidate refuses the WHOLE batch, before any signal. Signalling
/// the still-valid ones first would leave the partial mutation the
/// all-or-nothing confirmation exists to prevent: the operator authorised a
/// set, not a prefix of it.
#[test]
fn a_single_stale_candidate_refuses_the_whole_batch_3273() {
    let stage = staged(
        "stale-batch",
        &[
            (4242, vec![identity(111_000, 501)]),
            (4343, vec![identity(222_000, 501), identity(999_999, 501)]),
        ],
        Some(501),
    );
    let outcome = run(&stage, None, &[], false);
    assert_eq!(outcome, ApplyOutcome::Refused(RefusalReason::StaleBatch));
    assert!(
        stage.log.events().is_empty(),
        "the valid candidate must not be signalled ahead of the stale one, and no audit may be written: {:?}",
        stage.log.events()
    );
    std::fs::remove_dir_all(&stage.home).ok();
}

/// Preflight is NOT authority at signal time. A candidate that passed preview
/// and passed the full-batch preflight can still be recycled in the moment
/// before TERM, and the immediate re-read is the only thing standing between
/// that and signalling a stranger.
#[test]
fn pid_reuse_between_preflight_and_term_signals_nothing_3273() {
    // Reads: preview, preflight, immediate pre-TERM — the third differs.
    let stage = staged(
        "reuse-before-term",
        &[(
            4242,
            vec![
                identity(111_000, 501),
                identity(111_000, 501),
                identity(888_888, 501),
            ],
        )],
        Some(501),
    );
    let outcome = run(&stage, None, &[], false);
    assert_eq!(
        outcome,
        ApplyOutcome::Applied(vec![(
            stage.snapshot.candidates[0].id.clone(),
            CandidateOutcome::RefusedIdentityChanged
        )])
    );
    assert!(
        stage.log.signals().is_empty(),
        "a recycled pid must never be signalled: {:?}",
        stage.log.events()
    );
    std::fs::remove_dir_all(&stage.home).ok();
}

/// Same shape, but the identity becomes unreadable rather than different.
/// Unknown is not "unchanged".
#[test]
fn unreadable_identity_before_term_signals_nothing_3273() {
    let stage = staged(
        "unknown-before-term",
        &[(
            4242,
            vec![identity(111_000, 501), identity(111_000, 501), None],
        )],
        Some(501),
    );
    let outcome = run(&stage, None, &[], false);
    assert_eq!(
        outcome,
        ApplyOutcome::Applied(vec![(
            stage.snapshot.candidates[0].id.clone(),
            CandidateOutcome::RefusedIdentityChanged
        )])
    );
    assert!(stage.log.signals().is_empty(), "{:?}", stage.log.events());
    std::fs::remove_dir_all(&stage.home).ok();
}

// ---------------------------------------------------------------------------
// Contract 7 — the signal path, asserted as an ORDERED sequence.
// ---------------------------------------------------------------------------

/// TERM delivered, the process gone within a bounded wait, no KILL. The
/// sequence is asserted whole: `writes() == 1` would not have ruled out an
/// implementation that signalled first and audited afterwards, which is exactly
/// the ordering the consensus forbids.
#[test]
fn a_successful_term_audits_first_waits_bounded_and_never_kills_3273() {
    let stage = staged(
        "term-works",
        &[(4242, vec![identity(111_000, 501)])],
        Some(501),
    );
    let outcome = run(&stage, None, &[], false);

    assert_eq!(
        outcome,
        ApplyOutcome::Applied(vec![(
            stage.snapshot.candidates[0].id.clone(),
            CandidateOutcome::Terminated
        )])
    );
    assert_eq!(
        stage.log.events(),
        vec![
            "audit:4242".to_string(),
            "TERM:4242".to_string(),
            format!("wait:4242:{GRACE_MS}"),
        ],
        "durable audit strictly before TERM, then one bounded wait, and no KILL"
    );
    std::fs::remove_dir_all(&stage.home).ok();
}

/// A process that ignores TERM and is still the SAME identity after the bounded
/// wait earns exactly one KILL. The wait must appear in the log with the
/// declared bound: without it, an implementation could escalate instantly and
/// still satisfy a "one KILL" count.
#[test]
fn term_ignored_by_the_same_identity_earns_exactly_one_kill_after_the_grace_3273() {
    let stage = staged(
        "term-ignored",
        &[(
            4242,
            vec![
                identity(111_000, 501),
                identity(111_000, 501),
                identity(111_000, 501),
                identity(111_000, 501),
            ],
        )],
        Some(501),
    );
    let outcome = run(&stage, None, &[4242], false);

    assert_eq!(
        outcome,
        ApplyOutcome::Applied(vec![(
            stage.snapshot.candidates[0].id.clone(),
            CandidateOutcome::Killed
        )])
    );
    assert_eq!(
        stage.log.events(),
        vec![
            "audit:4242".to_string(),
            "TERM:4242".to_string(),
            format!("wait:4242:{GRACE_MS}"),
            "KILL:4242".to_string(),
        ],
        "the KILL must come after a bounded wait, not instead of one"
    );
    std::fs::remove_dir_all(&stage.home).ok();
}

/// If the PID is recycled DURING the grace window, escalation stops. The thing
/// that ignored TERM and the thing alive now are not proven to be the same
/// process, and a KILL cannot be taken back.
#[test]
fn pid_reuse_during_the_grace_window_cancels_the_kill_3273() {
    let stage = staged(
        "reuse-in-grace",
        &[(
            4242,
            vec![
                identity(111_000, 501),
                identity(111_000, 501),
                identity(111_000, 501),
                identity(777_777, 501),
            ],
        )],
        Some(501),
    );
    let outcome = run(&stage, None, &[4242], false);

    assert_eq!(
        outcome,
        ApplyOutcome::Applied(vec![(
            stage.snapshot.candidates[0].id.clone(),
            CandidateOutcome::RefusedIdentityChanged
        )])
    );
    assert_eq!(
        stage.log.events(),
        vec![
            "audit:4242".to_string(),
            "TERM:4242".to_string(),
            format!("wait:4242:{GRACE_MS}"),
        ],
        "no KILL may follow an identity that changed under us"
    );
    std::fs::remove_dir_all(&stage.home).ok();
}

/// An action nobody can prove happened must not happen: an undurable pre-signal
/// audit blocks the signal. The log proves the audit was ATTEMPTED and that
/// nothing followed it.
#[test]
fn an_undurable_pre_signal_audit_blocks_the_signal_3273() {
    let stage = staged(
        "audit-fails",
        &[(4242, vec![identity(111_000, 501)])],
        Some(501),
    );
    let outcome = run(&stage, None, &[], true);

    assert_eq!(
        outcome,
        ApplyOutcome::Applied(vec![(
            stage.snapshot.candidates[0].id.clone(),
            CandidateOutcome::RefusedAuditFailed
        )])
    );
    assert_eq!(
        stage.log.events(),
        vec!["audit:4242".to_string()],
        "the audit was attempted and nothing followed it"
    );
    std::fs::remove_dir_all(&stage.home).ok();
}

/// Fail-stop. A refused signal ends the batch; later candidates are never
/// attempted, because continuing past an unexplained failure widens a blast
/// radius nobody authorised.
#[test]
fn a_signal_failure_stops_the_batch_3273() {
    let stage = staged(
        "fail-stop",
        &[
            (4242, vec![identity(111_000, 501)]),
            (4343, vec![identity(222_000, 501)]),
        ],
        Some(501),
    );
    let outcome = run(&stage, Some(4242), &[], false);

    match outcome {
        ApplyOutcome::Applied(results) => {
            assert_eq!(
                results.len(),
                2,
                "every confirmed candidate must be accounted for"
            );
            assert!(
                matches!(results[0].1, CandidateOutcome::SignalFailed(_)),
                "first candidate must report the refused signal: {results:?}"
            );
            assert_eq!(
                results[1].1,
                CandidateOutcome::NotAttempted,
                "the batch must stop, not carry on to the next process"
            );
        }
        other => panic!("expected per-candidate results, got {other:?}"),
    }
    assert_eq!(
        stage.log.events(),
        vec!["audit:4242".to_string(), "TERM:4242".to_string()],
        "the second candidate must leave no trace at all: {:?}",
        stage.log.events()
    );
    std::fs::remove_dir_all(&stage.home).ok();
}

// ---------------------------------------------------------------------------
// Contract 8 — authority uncertainty stops the batch, and the signal mapping
// is pinned rather than merely documented.
// ---------------------------------------------------------------------------

/// An identity that changed under us is not just a fact about ONE candidate.
/// Every candidate in this batch was validated at the same preflight instant,
/// so discovering that the world moved means our picture of the remaining ones
/// is stale too. Uncertainty about authority must stop the batch widening, not
/// merely skip a row.
#[test]
fn an_identity_change_before_term_stops_the_batch_3273() {
    let stage = staged(
        "identity-change-stops",
        &[
            // preview, preflight, then recycled at the pre-TERM re-read
            (
                4242,
                vec![
                    identity(111_000, 501),
                    identity(111_000, 501),
                    identity(888_888, 501),
                ],
            ),
            (4343, vec![identity(222_000, 501)]),
        ],
        Some(501),
    );
    let outcome = run(&stage, None, &[], false);

    match outcome {
        ApplyOutcome::Applied(results) => {
            assert_eq!(results[0].1, CandidateOutcome::RefusedIdentityChanged);
            assert_eq!(
                results[1].1,
                CandidateOutcome::NotAttempted,
                "a stale picture of one candidate makes the rest of the batch unsafe: {results:?}"
            );
        }
        other => panic!("expected per-candidate results, got {other:?}"),
    }
    assert!(
        stage.log.signals().is_empty(),
        "nothing may be signalled once authority is uncertain: {:?}",
        stage.log.events()
    );
    std::fs::remove_dir_all(&stage.home).ok();
}

/// Same rule when the identity changes DURING the grace window: the first
/// candidate has already been TERMed, but the discovery still invalidates our
/// picture of the rest.
#[test]
fn an_identity_change_during_grace_stops_the_batch_3273() {
    let stage = staged(
        "grace-change-stops",
        &[
            (
                4242,
                vec![
                    identity(111_000, 501),
                    identity(111_000, 501),
                    identity(111_000, 501),
                    identity(777_777, 501),
                ],
            ),
            (4343, vec![identity(222_000, 501)]),
        ],
        Some(501),
    );
    let outcome = run(&stage, None, &[4242], false);

    match outcome {
        ApplyOutcome::Applied(results) => {
            assert_eq!(results[0].1, CandidateOutcome::RefusedIdentityChanged);
            assert_eq!(results[1].1, CandidateOutcome::NotAttempted);
        }
        other => panic!("expected per-candidate results, got {other:?}"),
    }
    assert_eq!(
        stage.log.events(),
        vec![
            "audit:4242".to_string(),
            "TERM:4242".to_string(),
            format!("wait:4242:{GRACE_MS}"),
        ],
        "no KILL, and the second candidate must leave no trace"
    );
    std::fs::remove_dir_all(&stage.home).ok();
}

/// A TERM that finds the process already gone reached the outcome we wanted
/// without us. It is not a failure, so the batch continues — and there is
/// nothing to wait for, so no wait is issued.
#[test]
fn term_on_an_already_gone_process_needs_no_wait_3273() {
    let stage = staged(
        "term-esrch",
        &[
            (4242, vec![identity(111_000, 501)]),
            (4343, vec![identity(222_000, 501)]),
        ],
        Some(501),
    );
    let outcome = run_scripted(
        &stage,
        SignalScript {
            term_gone: vec![4242],
            ..Default::default()
        },
        &[],
        false,
    );

    match outcome {
        ApplyOutcome::Applied(results) => {
            assert_eq!(results[0].1, CandidateOutcome::Terminated);
            assert_eq!(
                results[1].1,
                CandidateOutcome::Terminated,
                "an already-gone process is not a failure; the batch continues: {results:?}"
            );
        }
        other => panic!("expected per-candidate results, got {other:?}"),
    }
    assert_eq!(
        stage.log.events(),
        vec![
            "audit:4242".to_string(),
            "TERM:4242".to_string(),
            "audit:4343".to_string(),
            "TERM:4343".to_string(),
            format!("wait:4343:{GRACE_MS}"),
        ],
        "no wait may be issued for a process that was already gone"
    );
    std::fs::remove_dir_all(&stage.home).ok();
}

/// A KILL that finds the process already gone reports Terminated, not Killed.
/// Claiming a kill that did nothing would misreport what happened.
#[test]
fn kill_on_an_already_gone_process_reports_termination_3273() {
    let stage = staged(
        "kill-esrch",
        &[(
            4242,
            vec![
                identity(111_000, 501),
                identity(111_000, 501),
                identity(111_000, 501),
                identity(111_000, 501),
            ],
        )],
        Some(501),
    );
    let outcome = run_scripted(
        &stage,
        SignalScript {
            kill_gone: vec![4242],
            ..Default::default()
        },
        &[4242],
        false,
    );

    assert_eq!(
        outcome,
        ApplyOutcome::Applied(vec![(
            stage.snapshot.candidates[0].id.clone(),
            CandidateOutcome::Terminated
        )]),
        "a kill that found nothing to kill did not kill anything"
    );
    std::fs::remove_dir_all(&stage.home).ok();
}

/// A refused KILL is a mutation-path failure exactly as a refused TERM is, and
/// stops the batch.
#[test]
fn a_refused_kill_stops_the_batch_3273() {
    let stage = staged(
        "kill-refused",
        &[
            (
                4242,
                vec![
                    identity(111_000, 501),
                    identity(111_000, 501),
                    identity(111_000, 501),
                    identity(111_000, 501),
                ],
            ),
            (4343, vec![identity(222_000, 501)]),
        ],
        Some(501),
    );
    let outcome = run_scripted(
        &stage,
        SignalScript {
            kill_fails: Some(4242),
            ..Default::default()
        },
        &[4242],
        false,
    );

    match outcome {
        ApplyOutcome::Applied(results) => {
            assert!(
                matches!(results[0].1, CandidateOutcome::SignalFailed(_)),
                "{results:?}"
            );
            assert_eq!(results[1].1, CandidateOutcome::NotAttempted);
        }
        other => panic!("expected per-candidate results, got {other:?}"),
    }
    assert_eq!(
        stage.log.events(),
        vec![
            "audit:4242".to_string(),
            "TERM:4242".to_string(),
            format!("wait:4242:{GRACE_MS}"),
            "KILL:4242".to_string(),
        ],
        "the second candidate must leave no trace"
    );
    std::fs::remove_dir_all(&stage.home).ok();
}
