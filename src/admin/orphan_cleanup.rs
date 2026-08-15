//! #3273 V2 — manual orphan cleanup with immutable confirmation authority.
//!
//! Consensus `d-20260814214253501210-22`: V1 candidates
//! ([`crate::admin::orphan_provenance`]) stay **report-only**. Nothing there —
//! not PPID, argv, cwd, age, CPU, SID, PGID, nor a suggested owner — grants
//! authority to signal anything. This module is the only place a signal can
//! originate, and it may act only after an operator confirms an immutable
//! snapshot.
//!
//! ## Why the authority triple is re-derived here
//!
//! [`crate::admin::orphan_provenance::OrphanCandidate`] carries `pid` and
//! `start_token` but no `uid`, and every field on it is an *observation*. So
//! this module does not read authority out of a V1 report: it re-derives the
//! whole `(pid, start_token, uid)` triple through its own [`IdentityOracle`]
//! and hashes that into the candidate id. A V1 report can only *propose* which
//! pids to look at.
//!
//! ## Why the signal seam is injected
//!
//! [`crate::process::terminate`] returns `()`. A caller cannot tell a delivered
//! SIGTERM from a silently-failed one, so "the signal was sent" would be an
//! assumption rather than a fact. [`Signaler`] returns a [`SignalOutcome`],
//! which is what makes the contracts in
//! `tests/orphan_cleanup_manual_authority.rs` assertable at all.

use std::path::Path;

/// Exact per-process identity. Both halves must match a snapshot before any
/// signal; either being unobtainable is `None`, which always fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcIdentity {
    pub start_token: u64,
    pub uid: u32,
}

/// Reads the exact identity of a PID *right now*. `None` means "unknown", and
/// unknown is never treated as "unchanged" — it stops the operation.
pub trait IdentityOracle {
    fn identity(&self, pid: u32) -> Option<ProcIdentity>;
    /// The uid this process runs as. `None` fails closed: without it a foreign
    /// uid cannot be ruled out.
    fn self_uid(&self) -> Option<u32>;
}

/// Result of one exact-PID signal. Unlike [`crate::process::terminate`] this
/// reports what happened, so a contract can assert delivery instead of assuming
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalOutcome {
    Delivered,
    NoSuchProcess,
    PermissionDenied,
    Failed(String),
}

/// Sends a signal to ONE exact pid. Implementations must never target a group,
/// a session or a tree — the consensus forbids inferring a blast radius from an
/// observation.
pub trait Signaler {
    fn term(&self, pid: ExactPid) -> SignalOutcome;
    fn kill(&self, pid: ExactPid) -> SignalOutcome;
}

/// Waits, with a hard bound, for one exact pid to exit after a TERM.
///
/// This is a seam rather than a sleep-and-poll loop inside the executor for two
/// reasons: a contract can then prove the wait actually happened and that it
/// carried the declared bound, and an unbounded wait cannot be introduced
/// later without changing this signature. Returns whether the pid exited within
/// `timeout_ms`.
pub trait ExitWaiter {
    fn wait_for_exit(&self, pid: ExactPid, timeout_ms: u64) -> bool;
}

/// How long a TERMed process is given before escalation is even considered.
/// Bounded and declared here so the contracts can assert the exact value that
/// reaches the waiter.
pub const GRACE_MS: u64 = 2_000;

/// Injected clock so TTL and grace windows are deterministic under test.
pub trait Clock {
    fn now_ms(&self) -> i64;
}

/// The pre-signal audit record. Written and fsynced BEFORE the first TERM: if
/// the machine dies mid-operation the record of intent must already be on disk,
/// which is the one ordering `decisions.rs::archive_batch` deliberately does
/// not model (it audits after the rename).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreSignalAudit {
    pub token: String,
    pub actor: String,
    pub audit_reason: String,
    pub candidate_id: String,
    pub pid: u32,
    pub start_token: u64,
    pub uid: u32,
    pub at_ms: i64,
}

/// Durable audit sink. `record_pre_signal` returning `Ok` is a promise that the
/// bytes are fsynced; an `Err` refuses the signal.
pub trait AuditStore {
    fn record_pre_signal(&self, record: &PreSignalAudit) -> anyhow::Result<()>;
}

/// Whether this platform/backend can support manual cleanup at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Support {
    Supported,
    /// Report-only. Preview still renders; apply always refuses with zero
    /// signals.
    Unsupported(String),
}

/// One immutable candidate. `id` binds the snapshot generation to the exact
/// identity triple, so an id cannot be replayed against a different process.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CandidateSnapshot {
    pub id: String,
    pub pid: u32,
    pub start_token: u64,
    pub uid: u32,
}

/// The dry-run result. Producing one performs zero signals, always.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub support: Support,
    pub token: String,
    pub generation: u64,
    pub created_ms: i64,
    pub candidates: Vec<CandidateSnapshot>,
}

/// How long a confirmation stays usable. Short by construction: the snapshot
/// asserts a liveness fact, and liveness decays.
pub const CONFIRM_TTL_MS: i64 = 120_000;

/// Build the immutable candidate id for one identity triple within one
/// snapshot generation.
pub fn candidate_id(generation: u64, pid: u32, start_token: u64, uid: u32) -> String {
    // Self-contained on purpose: this module is re-exported into the library
    // crate for the contract tests (the same `#[path]` trick V1 uses), so it
    // must not reach for binary-only helpers such as `daemon::utils`.
    // Field-separated with a byte that cannot appear in a decimal number, so
    // no two distinct triples can render to the same material.
    use sha2::Digest;
    let material = format!("{generation}:{pid}:{start_token}:{uid}");
    let mut hasher = sha2::Sha256::new();
    hasher.update(material.as_bytes());
    hex::encode(hasher.finalize())
}

/// The durable confirmation sidecar. Written by `preview`, consumed once by
/// `apply`. `nonce` is stored, not merely folded into the candidate ids, so a
/// snapshot can be identified even when its candidate list is empty.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Confirmation {
    pub schema_version: u8,
    pub actor: String,
    pub audit_reason: String,
    pub created_ms: i64,
    pub generation: u64,
    pub nonce: String,
    pub consumed: bool,
    pub candidates: Vec<CandidateSnapshot>,
}

/// Why an apply refused. The contracts assert the EXACT reason rather than
/// "something refused": a gate that fires for the wrong cause is a gate whose
/// evidence cannot be trusted, and a blanket refusal would let a stub pass
/// every refusal contract at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusalReason {
    MissingToken,
    MalformedToken,
    ConfirmationUnavailable,
    SchemaMismatch,
    ActorMismatch,
    EmptyAuditReason,
    AuditReasonMismatch,
    Expired,
    Replayed,
    ConfirmIdsNotExact,
    Unsupported,
    /// This process could not determine its own uid, so a foreign-uid process
    /// cannot be ruled out. Unknown ownership is never treated as own.
    MissingSelfUid,
    /// A confirmed candidate belongs to another user. Signalling it would be
    /// acting outside the operator's own authority.
    ForeignUid,
    /// The full-batch preflight found at least one confirmed candidate whose
    /// live identity no longer matches the snapshot. The WHOLE batch refuses:
    /// signalling the still-valid ones first would leave a partial mutation
    /// that the all-or-nothing confirmation was meant to prevent.
    StaleBatch,
    /// Lost the race for a single-use confirmation. Distinct from `Replayed`
    /// only in provenance — both mean "this confirmation is already spent" —
    /// but a contended loser has not yet observed the winner's write, so the
    /// two are reported separately rather than guessed at.
    Contended,
    /// A stored candidate's `id` does not hash to the identity triple stored
    /// beside it. The id is authority material, so it must be RE-DERIVED from
    /// the sidecar's own fields rather than trusted as a carried string:
    /// without that, editing `pid`/`start_token`/`uid` while leaving `id` alone
    /// lets a confirmed id ride onto a process the operator never saw.
    CandidateIdMismatch,
    /// Staging placeholder: every authority check passed but the executor stage
    /// is not implemented yet, so nothing was signalled. The TERM/KILL slice
    /// removes this variant.
    ExecutorUnavailable,
}

/// Why a preview could not be issued. A preview that cannot be persisted must
/// NOT hand back a token: the token would name a confirmation that apply can
/// never revalidate, which is an authority artefact with no authority behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewError {
    PersistFailed(String),
}

/// Test seam that lets a contract interleave two applies at exactly the point a
/// non-CAS implementation would race. Production uses [`NoBarrier`]; a
/// read-then-write sequence without serialization is not a CAS, and this is
/// what makes that difference observable rather than a matter of timing luck.
pub trait ConsumeBarrier {
    fn before_consume(&self) {}
}

/// The production barrier: does nothing.
pub struct NoBarrier;
impl ConsumeBarrier for NoBarrier {}

/// What happened to one confirmed candidate. Reported per candidate because a
/// batch is not atomic at the kernel: the operator needs to know which exact
/// process was signalled and which was refused, not a single aggregate verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateOutcome {
    /// TERM delivered and the exact identity was gone within the grace window.
    /// No KILL was sent.
    Terminated,
    /// TERM delivered, the SAME identity was still alive after the grace
    /// window, and exactly one KILL followed.
    Killed,
    /// The identity re-read immediately before signalling did not match the
    /// snapshot — PID reuse, a changed start token, or an unreadable process.
    /// Nothing was signalled.
    RefusedIdentityChanged,
    /// The pre-signal audit could not be made durable. Nothing was signalled:
    /// an action nobody can prove happened is one that must not happen.
    RefusedAuditFailed,
    /// A signal was attempted and the kernel refused it.
    SignalFailed(String),
    /// The confirmed pid cannot be converted into an exact, individually
    /// signallable target — 0, or above `i32::MAX`. Nothing was signalled, and
    /// the batch stops: a sidecar naming such a pid is not a snapshot this
    /// build produced.
    RefusedUnsafePid,
    /// An earlier candidate hit a mutation-path failure, so this one was never
    /// attempted. Fail-stop is deliberate: continuing after an unexplained
    /// failure would widen a blast radius nobody authorised.
    NotAttempted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    Refused(RefusalReason),
    /// Authority was fully established and each confirmed candidate has an
    /// outcome, in confirmation order.
    Applied(Vec<(String, CandidateOutcome)>),
}

/// Path of the confirmation sidecar for `token`, or `None` when the token is
/// not of the exact accepted shape. Mirrors `decisions.rs::confirmation_path`:
/// the shape check is what stops a token being spent as a path component.
pub fn confirmation_path(home: &Path, token: &str) -> Option<std::path::PathBuf> {
    if token.len() == 36
        && token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        Some(home.join("orphan-cleanup").join(format!("{token}.json")))
    } else {
        None
    }
}

/// Consume an operator confirmation and act on it.
///
/// Every authority check runs BEFORE anything is signalled, and each refusal
/// names its own cause.
#[allow(clippy::too_many_arguments)]
pub fn apply<O: IdentityOracle, S: Signaler, C: Clock, A: AuditStore, W: ExitWaiter>(
    home: &Path,
    actor: &str,
    audit_reason: &str,
    token: &str,
    confirm_ids: &[String],
    oracle: &O,
    signaler: &S,
    clock: &C,
    audit: &A,
    waiter: &W,
    support: Support,
) -> ApplyOutcome {
    apply_with_barrier(
        home,
        actor,
        audit_reason,
        token,
        confirm_ids,
        oracle,
        signaler,
        clock,
        audit,
        waiter,
        support,
        &NoBarrier,
    )
}

/// [`apply`] with an injected barrier. Only the contracts pass anything other
/// than [`NoBarrier`].
#[allow(clippy::too_many_arguments)]
pub fn apply_with_barrier<
    O: IdentityOracle,
    S: Signaler,
    C: Clock,
    A: AuditStore,
    W: ExitWaiter,
    B: ConsumeBarrier,
>(
    home: &Path,
    actor: &str,
    audit_reason: &str,
    token: &str,
    confirm_ids: &[String],
    oracle: &O,
    signaler: &S,
    clock: &C,
    audit: &A,
    waiter: &W,
    support: Support,
    barrier: &B,
) -> ApplyOutcome {
    // An unsupported platform or backend stays report-only however correct the
    // confirmation is: there is no identity oracle to revalidate against, and
    // without that a signal would rest on the snapshot's word alone.
    if matches!(support, Support::Unsupported(_)) {
        return ApplyOutcome::Refused(RefusalReason::Unsupported);
    }
    // Pure input checks first — they need no disk and no lock.
    if audit_reason.trim().is_empty() {
        return ApplyOutcome::Refused(RefusalReason::EmptyAuditReason);
    }
    if token.is_empty() {
        return ApplyOutcome::Refused(RefusalReason::MissingToken);
    }
    let Some(path) = confirmation_path(home, token) else {
        // Shape alone, before the token is ever used to address a file. This is
        // what stops a token being spent as a path component.
        return ApplyOutcome::Refused(RefusalReason::MalformedToken);
    };

    // EVERYTHING below happens under one advisory lock on this confirmation.
    // A read of `consumed == false` followed by a later write of `true` is not
    // a compare-and-swap: two applies can both read `false` and both proceed.
    // Holding the lock across read → validate → durable consume is what makes
    // "single use" true under concurrency rather than only in sequence.
    let _guard = match acquire_confirmation_lock(&path) {
        Ok(guard) => guard,
        Err(_) => return ApplyOutcome::Refused(RefusalReason::Contended),
    };

    let Ok(raw) = std::fs::read(&path) else {
        return ApplyOutcome::Refused(RefusalReason::ConfirmationUnavailable);
    };
    let Ok(stored) = serde_json::from_slice::<Confirmation>(&raw) else {
        return ApplyOutcome::Refused(RefusalReason::ConfirmationUnavailable);
    };
    if stored.schema_version != 1 {
        // Not a weaker confirmation — an unreadable one.
        return ApplyOutcome::Refused(RefusalReason::SchemaMismatch);
    }
    if stored.actor != actor {
        return ApplyOutcome::Refused(RefusalReason::ActorMismatch);
    }
    if stored.audit_reason != audit_reason {
        return ApplyOutcome::Refused(RefusalReason::AuditReasonMismatch);
    }
    // A confirmation asserts a liveness fact and liveness decays, so the window
    // is closed on BOTH sides: a negative age means the clock moved backwards
    // relative to the record, and a confirmation from the future is not fresh,
    // it is unusable.
    let age_ms = clock.now_ms().saturating_sub(stored.created_ms);
    if !(0..=CONFIRM_TTL_MS).contains(&age_ms) {
        return ApplyOutcome::Refused(RefusalReason::Expired);
    }
    if stored.consumed {
        return ApplyOutcome::Refused(RefusalReason::Replayed);
    }
    // RE-DERIVE every id from the fields stored beside it, BEFORE the ids are
    // compared against the operator's and before anything is consumed or
    // signalled.
    //
    // Without this the id is merely CARRIED: `confirm_ids` would be checked
    // against a string in the sidecar, and editing `pid`/`start_token`/`uid`
    // while leaving `id` alone would let a confirmed id ride onto a process the
    // operator never saw. Everything downstream would then behave correctly on
    // the wrong target — the preflight matches the edited triple, the pre-signal
    // audit records it, and the TERM lands on it. The id only binds an identity
    // if it is recomputed from that identity.
    for candidate in &stored.candidates {
        let derived = candidate_id(
            stored.generation,
            candidate.pid,
            candidate.start_token,
            candidate.uid,
        );
        if derived != candidate.id {
            return ApplyOutcome::Refused(RefusalReason::CandidateIdMismatch);
        }
    }
    // All-or-nothing, and deliberately NOT deduplicated: a repeated id is not a
    // second confirmation of the same candidate, it is a set that does not equal
    // the snapshot's. Sorting makes the comparison order-insensitive, which is a
    // property of the comparison rather than a licence to accept a different set.
    let mut confirmed: Vec<&str> = confirm_ids.iter().map(String::as_str).collect();
    confirmed.sort_unstable();
    let mut expected: Vec<&str> = stored
        .candidates
        .iter()
        .map(|candidate| candidate.id.as_str())
        .collect();
    expected.sort_unstable();
    if confirmed != expected {
        return ApplyOutcome::Refused(RefusalReason::ConfirmIdsNotExact);
    }

    // FULL-BATCH preflight, before the token is spent and before anything is
    // written or signalled. Checking each candidate as we reach it would leave
    // candidate 1 already dead when candidate 2 turns out to be stale — a
    // partial mutation the all-or-nothing confirmation exists to prevent. The
    // operator authorised a set, not a prefix of it.
    let Some(self_uid) = oracle.self_uid() else {
        // Without our own uid a foreign-uid target cannot be ruled out, and
        // "probably mine" is not authority.
        return ApplyOutcome::Refused(RefusalReason::MissingSelfUid);
    };
    for candidate in &stored.candidates {
        let Some(live) = oracle.identity(candidate.pid) else {
            return ApplyOutcome::Refused(RefusalReason::StaleBatch);
        };
        if live.start_token != candidate.start_token || live.uid != candidate.uid {
            return ApplyOutcome::Refused(RefusalReason::StaleBatch);
        }
        if live.uid != self_uid {
            return ApplyOutcome::Refused(RefusalReason::ForeignUid);
        }
    }

    // The exact point a non-CAS implementation would interleave. Production
    // passes NoBarrier, so this is a no-op outside the contracts.
    barrier.before_consume();

    // Durable consumption BEFORE anything downstream can act. Consuming and
    // then failing later is acceptable fail-closed; acting on a confirmation
    // whose consumption never reached disk is not.
    let mut consumed = stored;
    consumed.consumed = true;
    if persist_confirmation(&path, &consumed).is_err() {
        return ApplyOutcome::Refused(RefusalReason::ConfirmationUnavailable);
    }
    // The confirmation is spent and no longer readable as authority by anyone
    // else, so the lock has done its job. Holding it across the signal path
    // would serialise unrelated confirmations behind a grace window.
    drop(_guard);

    let mut results: Vec<(String, CandidateOutcome)> = Vec::new();
    // Fail-stop: once the mutation path has failed in a way we cannot explain,
    // later candidates are not attempted. Continuing would widen a blast radius
    // nobody authorised on the strength of an operation that already went wrong.
    let mut stop = false;
    for candidate in &consumed.candidates {
        if stop {
            results.push((candidate.id.clone(), CandidateOutcome::NotAttempted));
            continue;
        }
        // Durable record of intent FIRST. An action nobody can prove happened
        // must not happen, so a failure here refuses the signal outright.
        let record = PreSignalAudit {
            token: token.to_string(),
            actor: actor.to_string(),
            audit_reason: audit_reason.to_string(),
            candidate_id: candidate.id.clone(),
            pid: candidate.pid,
            start_token: candidate.start_token,
            uid: candidate.uid,
            at_ms: clock.now_ms(),
        };
        if audit.record_pre_signal(&record).is_err() {
            results.push((candidate.id.clone(), CandidateOutcome::RefusedAuditFailed));
            stop = true;
            continue;
        }
        // The authority type is constructed HERE, once, and every syscall below
        // takes it. A pid that cannot become an ExactPid never reaches a
        // signal — the check cannot be skipped by an adapter that casts
        // internally, because there is no u32 path into `Signaler`.
        let Some(target) = ExactPid::new(candidate.pid) else {
            results.push((candidate.id.clone(), CandidateOutcome::RefusedUnsafePid));
            stop = true;
            continue;
        };
        // Preflight is not authority at signal time. A pid can be recycled in
        // the moment between the two, and this re-read is the only thing
        // standing between that and signalling a stranger. `None` counts as
        // changed: unknown is never "unchanged".
        if !identity_matches(oracle, candidate) {
            results.push((
                candidate.id.clone(),
                CandidateOutcome::RefusedIdentityChanged,
            ));
            // FAIL-STOP. Not because this candidate was mutated — it was not —
            // but because every candidate in this batch was validated at the
            // SAME preflight instant. Discovering that one pid was recycled
            // means the picture of the remaining ones is stale too, and
            // uncertainty about authority must stop the mutation widening
            // rather than merely skip a row.
            stop = true;
            continue;
        }
        match signaler.term(target) {
            SignalOutcome::Delivered => {}
            // ESRCH: it exited between the re-read and the signal. That is the
            // outcome we wanted, reached without us.
            SignalOutcome::NoSuchProcess => {
                results.push((candidate.id.clone(), CandidateOutcome::Terminated));
                continue;
            }
            SignalOutcome::PermissionDenied => {
                results.push((
                    candidate.id.clone(),
                    CandidateOutcome::SignalFailed("permission denied".to_string()),
                ));
                stop = true;
                continue;
            }
            SignalOutcome::Failed(error) => {
                results.push((candidate.id.clone(), CandidateOutcome::SignalFailed(error)));
                stop = true;
                continue;
            }
        }
        if waiter.wait_for_exit(target, GRACE_MS) {
            results.push((candidate.id.clone(), CandidateOutcome::Terminated));
            continue;
        }
        // Still alive after the bounded wait. Re-read AGAIN: the process that
        // ignored TERM and the process alive now are not proven to be the same
        // one, and a KILL cannot be taken back.
        if !identity_matches(oracle, candidate) {
            results.push((
                candidate.id.clone(),
                CandidateOutcome::RefusedIdentityChanged,
            ));
            // Same rule as the pre-TERM re-read: the world moved under a batch
            // that was validated all at once, so the rest of it is no longer
            // trustworthy.
            stop = true;
            continue;
        }
        match signaler.kill(target) {
            SignalOutcome::Delivered => {
                results.push((candidate.id.clone(), CandidateOutcome::Killed));
            }
            SignalOutcome::NoSuchProcess => {
                // It went during the re-read window; report what is true rather
                // than claiming a kill that did nothing.
                results.push((candidate.id.clone(), CandidateOutcome::Terminated));
            }
            SignalOutcome::PermissionDenied => {
                results.push((
                    candidate.id.clone(),
                    CandidateOutcome::SignalFailed("permission denied".to_string()),
                ));
                stop = true;
            }
            SignalOutcome::Failed(error) => {
                results.push((candidate.id.clone(), CandidateOutcome::SignalFailed(error)));
                stop = true;
            }
        }
    }
    ApplyOutcome::Applied(results)
}

/// Does the live process at this pid still have the exact identity the operator
/// confirmed? `None` — unreadable, gone, or unknowable — is a mismatch, never a
/// pass.
fn identity_matches<O: IdentityOracle>(oracle: &O, candidate: &CandidateSnapshot) -> bool {
    oracle
        .identity(candidate.pid)
        .is_some_and(|live| live.start_token == candidate.start_token && live.uid == candidate.uid)
}

/// A pid proven safe to signal INDIVIDUALLY.
///
/// The whole consensus rests on "one exact PID, never a group, a session or a
/// tree", and on Unix the naive `pid as i32` quietly breaks that in two ways:
///
/// * `0` — `kill(0, sig)` signals every process in the CALLER's process group.
/// * `> i32::MAX` — the cast wraps NEGATIVE, and `kill(-n, sig)` signals process
///   group `n`.
///
/// Both turn an exact-pid contract into a blast radius, and neither is visible
/// at the call site. Constructing this type is the only way to reach a signal,
/// so the conversion cannot be skipped by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExactPid(i32);

impl ExactPid {
    pub fn new(pid: u32) -> Option<Self> {
        // Neither refusal is about plausibility — both are about `kill(2)`'s
        // sign convention, which turns these two values into group signals with
        // no diagnostic anywhere. Refusing here, rather than clamping or
        // warning, is what keeps "one exact PID" a property of the type.
        if pid == 0 || pid > i32::MAX as u32 {
            return None;
        }
        Some(ExactPid(pid as i32))
    }

    pub fn get(self) -> i32 {
        self.0
    }
}

/// Whether this build can support manual cleanup at all.
pub fn platform_support() -> Support {
    #[cfg(unix)]
    {
        Support::Supported
    }
    #[cfg(not(unix))]
    {
        // Report-only rather than best-effort: without a per-pid identity
        // oracle there is nothing to revalidate a snapshot against, and a
        // signal resting on the snapshot's word alone is the failure mode the
        // whole re-read discipline exists to prevent.
        Support::Unsupported(
            "manual orphan cleanup needs a per-pid identity oracle and an exact-pid signal, \
             which this platform does not provide"
                .to_string(),
        )
    }
}

/// Raw per-process facts, read straight from the OS. Split out from
/// [`IdentityOracle`] so the freshness rule — every question is answered by a
/// NEW read — is testable without a live process to recycle.
pub trait RawIdentityReader {
    fn read(&self, pid: u32) -> Option<ProcIdentity>;
    fn self_uid(&self) -> Option<u32>;
}

/// The production oracle: never caches. A cached identity is a stale identity,
/// and the entire re-read discipline exists because the answer can change
/// between two adjacent instants.
pub struct FreshIdentityOracle<R: RawIdentityReader> {
    reader: R,
}

impl<R: RawIdentityReader> FreshIdentityOracle<R> {
    pub fn new(reader: R) -> Self {
        Self { reader }
    }
}

impl<R: RawIdentityReader> IdentityOracle for FreshIdentityOracle<R> {
    fn identity(&self, pid: u32) -> Option<ProcIdentity> {
        // Every question is a new read, including a repeat of one just asked.
        // There is deliberately nowhere to put a cache: the preflight, the
        // pre-TERM re-read and the pre-KILL re-read exist precisely because the
        // answer can change between two adjacent instants, and serving the
        // second from the first would silently delete two of the three checks.
        self.reader.read(pid)
    }
    fn self_uid(&self) -> Option<u32> {
        self.reader.self_uid()
    }
}

/// Translate a raw `kill(2)` return into an outcome. A pure function so the
/// mapping is testable without sending a real signal.
pub fn signal_outcome_from_syscall(rc: i32, errno: i32) -> SignalOutcome {
    if rc == 0 {
        // The only success. `errno` is meaningless after a successful call.
        return SignalOutcome::Delivered;
    }
    // ESRCH and EPERM are separated because the executor treats them
    // differently: "already gone" is the outcome we wanted, "not permitted" is
    // a fail-stop. Collapsing them into one error would make that distinction
    // unavailable at the only point it can be acted on.
    if errno == errno_no_such_process() {
        SignalOutcome::NoSuchProcess
    } else if errno == errno_permission_denied() {
        SignalOutcome::PermissionDenied
    } else {
        SignalOutcome::Failed(format!("kill(2) failed with errno {errno}"))
    }
}

/// `ESRCH` for this platform.
fn errno_no_such_process() -> i32 {
    #[cfg(unix)]
    {
        libc::ESRCH
    }
    #[cfg(not(unix))]
    {
        // `libc` is a Unix-only dependency of this crate, so the two errno
        // values this mapping needs are named directly. Both are fixed by C89
        // and match the values the `libc` crate reports on Windows.
        3
    }
}

/// `EPERM` for this platform.
fn errno_permission_denied() -> i32 {
    #[cfg(unix)]
    {
        libc::EPERM
    }
    #[cfg(not(unix))]
    {
        1
    }
}

/// Injected sleep so a bounded wait is testable without spending the bound.
pub trait Sleeper {
    fn sleep_ms(&self, ms: u64);
}

/// Injected liveness probe. Takes an [`ExactPid`] so an unchecked pid cannot
/// reach a syscall through this path either.
pub trait LivenessProbe {
    fn is_alive(&self, pid: ExactPid) -> bool;
}

/// The production waiter: polls until the pid is gone or the bound is spent,
/// whichever comes first, and never longer.
pub struct BoundedWaiter<P: LivenessProbe, S: Sleeper> {
    pub probe: P,
    pub sleeper: S,
    pub poll_ms: u64,
}

impl<P: LivenessProbe, S: Sleeper> ExitWaiter for BoundedWaiter<P, S> {
    fn wait_for_exit(&self, pid: ExactPid, timeout_ms: u64) -> bool {
        // Ask before sleeping: a process that has already gone must not cost a
        // poll interval, and the grace window is time the operator is blocked
        // for.
        if !self.probe.is_alive(pid) {
            return true;
        }
        // A zero poll interval would spin; one millisecond is the smallest step
        // the sleeper can express.
        let step = self.poll_ms.max(1);
        let mut slept_ms: u64 = 0;
        while slept_ms < timeout_ms {
            // Clamp the LAST step to what is left of the bound. Without this a
            // 50ms poll against a 120ms bound would sleep 150ms — overshooting
            // a promise the operator was given.
            let this_step = step.min(timeout_ms - slept_ms);
            self.sleeper.sleep_ms(this_step);
            slept_ms += this_step;
            if !self.probe.is_alive(pid) {
                return true;
            }
        }
        false
    }
}

/// Append-only JSONL audit, one record per line, fsynced before it returns.
pub struct JsonlAuditStore {
    pub path: std::path::PathBuf,
}

impl AuditStore for JsonlAuditStore {
    fn record_pre_signal(&self, record: &PreSignalAudit) -> anyhow::Result<()> {
        use std::io::Write;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("audit path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        harden_private(parent, 0o700)?;
        // One record, one line, serialised BEFORE the file is opened so a
        // serialisation failure cannot leave a half-written line behind.
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        // The audit names exact pids and the operator who confirmed them; it is
        // as much authority material as the confirmation itself, and
        // `OpenOptions::create` honours the umask.
        harden_private(&self.path, 0o600)?;
        file.write_all(&line)?;
        // `Ok` from this function is a promise the bytes are on disk, because a
        // signal is licensed by it. A buffered write would make that promise
        // false in exactly the crash this ordering exists for.
        file.sync_all()?;
        // The first record also creates the directory entry, which needs its own
        // fsync to survive. Same tolerance as `persist_confirmation`: some
        // filesystems refuse fsync on a directory handle, but a failure to OPEN
        // the directory means durability cannot be established at all.
        std::fs::File::open(parent)?.sync_all().or_else(|error| {
            if error.kind() == std::io::ErrorKind::InvalidInput {
                Ok(())
            } else {
                Err(error)
            }
        })?;
        Ok(())
    }
}

/// The raw `kill(2)` seam. Injected so the concrete Unix signaler's target and
/// signal number are observable without sending anything to a real process.
/// Returns `(rc, errno)`.
pub trait KillSyscall {
    fn kill(&self, pid: i32, signal: i32) -> (i32, i32);
}

/// The production Unix signaler. Every target comes from an [`ExactPid`], and
/// the pid is passed through POSITIVE — a negative argument to `kill(2)` means
/// "the process group", which is the blast radius the consensus forbids.
pub struct UnixSignaler<K: KillSyscall> {
    pub syscall: K,
}

impl<K: KillSyscall> Signaler for UnixSignaler<K> {
    fn term(&self, pid: ExactPid) -> SignalOutcome {
        // POSITIVE, and it is `ExactPid` that guarantees it stays positive:
        // `kill(-n, …)` is "process group n", which is the blast radius the
        // consensus forbids and which no call site would show.
        let (rc, errno) = self.syscall.kill(pid.get(), signal_term());
        signal_outcome_from_syscall(rc, errno)
    }
    fn kill(&self, pid: ExactPid) -> SignalOutcome {
        let (rc, errno) = self.syscall.kill(pid.get(), signal_kill());
        signal_outcome_from_syscall(rc, errno)
    }
}

/// The production `kill(2)`. Injected through [`KillSyscall`] so the concrete
/// signaler's target and signal number stay observable, and so this — the only
/// place in the module that can actually reach a process — is three lines long.
pub struct LibcKill;

#[cfg(unix)]
impl KillSyscall for LibcKill {
    fn kill(&self, pid: i32, signal: i32) -> (i32, i32) {
        // SAFETY: `kill(2)` takes two scalars and touches none of our memory.
        let rc = unsafe { libc::kill(pid, signal) };
        // Read errno IMMEDIATELY: any intervening libc call may overwrite it,
        // and a stale errno would be mapped as though the kernel had said it.
        let errno = if rc == 0 {
            0
        } else {
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
        };
        (rc, errno)
    }
}

#[cfg(not(unix))]
impl KillSyscall for LibcKill {
    fn kill(&self, _pid: i32, _signal: i32) -> (i32, i32) {
        // No exact-pid signal exists here. Reporting a failure rather than a
        // success keeps the "assume it worked" behaviour out of the one place
        // that could reintroduce it; `platform_support` already refuses apply
        // before this can be reached.
        (-1, 0)
    }
}

/// SIGTERM for this platform.
pub fn signal_term() -> i32 {
    #[cfg(unix)]
    {
        libc::SIGTERM
    }
    #[cfg(not(unix))]
    {
        15
    }
}

/// SIGKILL for this platform.
pub fn signal_kill() -> i32 {
    #[cfg(unix)]
    {
        libc::SIGKILL
    }
    #[cfg(not(unix))]
    {
        9
    }
}

/// Liveness by `kill(pid, 0)`. EPERM means the process EXISTS but is not ours,
/// which is "alive" for the purpose of a grace window — treating it as gone
/// would escalate against something we already know we cannot touch.
pub struct KillProbe<K: KillSyscall> {
    pub syscall: K,
}

impl<K: KillSyscall> LivenessProbe for KillProbe<K> {
    fn is_alive(&self, pid: ExactPid) -> bool {
        // Signal 0 asks the kernel the question without sending anything.
        let (rc, errno) = self.syscall.kill(pid.get(), 0);
        // EPERM is the load-bearing case, and the easy one to drop: it means
        // the process EXISTS and is not ours. Reading it as dead would end the
        // grace window early and escalate to a KILL against something we
        // already know we cannot signal.
        rc == 0 || errno == errno_permission_denied()
    }
}

/// The production reader: asks the OS on every call.
pub struct LiveIdentityReader;

#[cfg(unix)]
impl RawIdentityReader for LiveIdentityReader {
    fn read(&self, pid: u32) -> Option<ProcIdentity> {
        read_identity(pid)
    }
    fn self_uid(&self) -> Option<u32> {
        // SAFETY: `geteuid(2)` takes no arguments, touches no memory of ours
        // and is documented as always succeeding.
        Some(unsafe { libc::geteuid() })
    }
}

#[cfg(not(unix))]
impl RawIdentityReader for LiveIdentityReader {
    fn read(&self, _pid: u32) -> Option<ProcIdentity> {
        None
    }
    fn self_uid(&self) -> Option<u32> {
        // Unknown, which every caller treats as fail-closed. There is no
        // half-answer here: a uid we cannot read cannot rule out a foreign one.
        None
    }
}

/// ONE coherent observation of the identity triple.
///
/// Deliberately not `process::process_start_token` followed by a separate uid
/// lookup: two independent reads can straddle a PID recycle and yield a triple
/// whose halves describe two different processes — which is exactly the
/// confusion the triple exists to detect. Each arm below takes both halves from
/// a single kernel observation, so an incoherent triple cannot be constructed.
///
/// The start-token formula matches `process::process_start_token` so the two
/// agree about the same process.
#[cfg(target_os = "macos")]
fn read_identity(pid: u32) -> Option<ProcIdentity> {
    if pid == 0 {
        return None;
    }
    // SAFETY: proc_pidinfo writes at most `size_of::<proc_bsdinfo>()` bytes
    // into our stack buffer; we pass that exact size and check the return.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if read != size {
        return None;
    }
    Some(ProcIdentity {
        start_token: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
        // The EFFECTIVE uid, which is what `geteuid()` reports for ourselves.
        uid: info.pbi_uid,
    })
}

#[cfg(target_os = "linux")]
fn read_identity(pid: u32) -> Option<ProcIdentity> {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    if pid == 0 {
        return None;
    }
    // ONE open handle answers both halves: the owner from the fd's own
    // metadata, the start time from its contents. Opening `/proc/<pid>/stat`
    // twice — or stat-ing the directory separately — could describe two
    // different processes if the pid were recycled in between.
    let mut file = std::fs::File::open(format!("/proc/{pid}/stat")).ok()?;
    let uid = file.metadata().ok()?.uid();
    let mut stat = String::new();
    file.read_to_string(&mut stat).ok()?;
    // `pid (comm) state ppid …` — `comm` may embed spaces and parens, so parse
    // the tail after the FINAL ')'. That tail begins at field 3, which puts
    // field 22 (`starttime`) at index 19.
    let tail = &stat[stat.rfind(')')? + 1..];
    let start_token = tail.split_whitespace().nth(19)?.parse::<u64>().ok()?;
    Some(ProcIdentity { start_token, uid })
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn read_identity(_pid: u32) -> Option<ProcIdentity> {
    // No coherent single-read implementation for this Unix. Unknown, which
    // always fails closed rather than degrading to a partial identity.
    None
}

/// What the operator asked for, as one value. Grouped so a handler cannot
/// forward three of four fields and still typecheck.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRequest<'a> {
    pub actor: &'a str,
    pub audit_reason: &'a str,
    pub proposed_pids: &'a [u32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyRequest<'a> {
    pub token: &'a str,
    pub actor: &'a str,
    pub audit_reason: &'a str,
    pub confirm_ids: &'a [String],
}

/// The seam between the CLI and the executor. The CLI's only job is to turn
/// operator input into one of these calls and render what comes back; all
/// authority lives behind this trait, so `cli.rs` can never grow a second,
/// weaker copy of the rules.
pub trait OrphanCleanupService {
    fn preview(&self, request: PreviewRequest<'_>) -> Result<Preview, PreviewError>;
    fn apply(&self, request: ApplyRequest<'_>) -> ApplyOutcome;
}

/// The manual command handlers. They live here, beside the seam, rather than in
/// `cli.rs`: the CLI's whole job is to parse operator input and print what comes
/// back, and keeping the forwarding here is what stops a second, weaker copy of
/// the authority rules growing next to the argument parser.
///
/// `run_doctor` is untouched and remains a report.
pub fn handle_preview_command<S: OrphanCleanupService>(
    service: &S,
    actor: &str,
    audit_reason: &str,
    proposed_pids: &[u32],
) -> Result<String, String> {
    // Exactly one call, with exactly what the operator typed. Anything the
    // handler added, dropped or reordered here would make the printed snapshot
    // describe something other than what was asked for.
    let preview = service
        .preview(PreviewRequest {
            actor,
            audit_reason,
            proposed_pids,
        })
        .map_err(|error| match error {
            PreviewError::PersistFailed(why) => format!(
                "orphan cleanup preview failed: the confirmation could not be persisted ({why}). \
                 No token was issued."
            ),
        })?;
    Ok(render_preview(&preview, proposed_pids.len()))
}

pub fn handle_apply_command<S: OrphanCleanupService>(
    service: &S,
    token: &str,
    actor: &str,
    audit_reason: &str,
    confirm_ids: &[String],
) -> Result<String, String> {
    match service.apply(ApplyRequest {
        token,
        actor,
        audit_reason,
        confirm_ids,
    }) {
        // A refusal must reach the operator AS a refusal. Rendering it as
        // success is the worst outcome available: the operator believes the
        // processes are gone and stops looking.
        ApplyOutcome::Refused(reason) => Err(format!(
            "orphan cleanup apply refused: {}",
            refusal_message(&reason)
        )),
        ApplyOutcome::Applied(results) => Ok(render_outcomes(&results)),
    }
}

/// The operator-facing text for a refusal. Each variant names its OWN cause:
/// a single "refused" would tell the operator nothing about whether to retry,
/// re-preview, or stop.
fn refusal_message(reason: &RefusalReason) -> String {
    match reason {
        RefusalReason::MissingToken => "no confirmation token was supplied".to_string(),
        RefusalReason::MalformedToken => {
            "the confirmation token is not of the accepted shape".to_string()
        }
        RefusalReason::ConfirmationUnavailable => {
            "the confirmation is unavailable or unreadable".to_string()
        }
        RefusalReason::SchemaMismatch => {
            "the confirmation was written by a different version and cannot be read".to_string()
        }
        RefusalReason::ActorMismatch => {
            "this confirmation was issued to a different actor".to_string()
        }
        RefusalReason::EmptyAuditReason => "an audit reason is required".to_string(),
        RefusalReason::AuditReasonMismatch => {
            "the audit reason does not match the one the confirmation records".to_string()
        }
        RefusalReason::Expired => {
            format!(
                "the confirmation is expired or dated in the future (it is usable for {}s); \
                 take a new preview",
                CONFIRM_TTL_MS / 1_000
            )
        }
        RefusalReason::Replayed => {
            "the confirmation has already been consumed; it is single-use".to_string()
        }
        RefusalReason::ConfirmIdsNotExact => {
            "the confirmed ids are not exactly the snapshot's set".to_string()
        }
        RefusalReason::Unsupported => {
            "manual orphan cleanup is report-only on this platform".to_string()
        }
        RefusalReason::MissingSelfUid => {
            "this process cannot read its own uid, so a foreign-owned process cannot be ruled out"
                .to_string()
        }
        RefusalReason::ForeignUid => "a confirmed candidate belongs to another user".to_string(),
        RefusalReason::StaleBatch => {
            "at least one confirmed candidate no longer matches the snapshot; take a new preview"
                .to_string()
        }
        RefusalReason::Contended => "another apply is consuming this confirmation".to_string(),
        RefusalReason::CandidateIdMismatch => {
            "a confirmed candidate's id does not match the identity recorded beside it; \
             the confirmation has been altered since it was issued"
                .to_string()
        }
        RefusalReason::ExecutorUnavailable => "the executor is unavailable".to_string(),
    }
}

/// The dry-run rendering. The token is printed as its own word so an operator —
/// or a script — can lift it without parsing.
fn render_preview(preview: &Preview, proposed: usize) -> String {
    let mut out = String::new();
    if let Support::Unsupported(why) = &preview.support {
        out.push_str(&format!(
            "note: this platform is report-only, so apply will refuse ({why})\n"
        ));
    }
    out.push_str(&format!("confirmation token: {}\n", preview.token));
    out.push_str(&format!("usable for: {}s\n", CONFIRM_TTL_MS / 1_000));
    out.push_str(&format!("candidates: {}\n", preview.candidates.len()));
    for candidate in &preview.candidates {
        out.push_str(&format!(
            "  candidate {} pid={} uid={} start_token={}\n",
            candidate.id, candidate.pid, candidate.uid, candidate.start_token
        ));
    }
    if preview.candidates.len() < proposed {
        // Silence here would read as "all of them were fine". A pid whose
        // identity could not be resolved is dropped from the snapshot, and the
        // operator has to know their list was narrowed before confirming it.
        out.push_str(&format!(
            "note: {} of {proposed} proposed pids could not be identified and were dropped\n",
            proposed - preview.candidates.len()
        ));
    }
    out.push_str(
        "to act on this snapshot, confirm every id above with:\n  \
         agend-terminal doctor orphans apply --token <token> --actor <actor> \
         --audit-reason <reason> --confirm-id <id> [--confirm-id <id>…]\n",
    );
    out
}

/// The per-candidate rendering. A batch is not atomic at the kernel, so the
/// operator is told which exact process was signalled and which was refused
/// rather than given one aggregate verdict.
fn render_outcomes(results: &[(String, CandidateOutcome)]) -> String {
    let mut out = String::from("orphan cleanup applied\n");
    for (id, outcome) in results {
        let described = match outcome {
            CandidateOutcome::Terminated => "terminated within the grace window".to_string(),
            CandidateOutcome::Killed => "survived the grace window and was killed".to_string(),
            CandidateOutcome::RefusedIdentityChanged => {
                "refused: its identity changed before it could be signalled".to_string()
            }
            CandidateOutcome::RefusedAuditFailed => {
                "refused: the pre-signal audit could not be made durable".to_string()
            }
            CandidateOutcome::SignalFailed(error) => format!("signal failed: {error}"),
            CandidateOutcome::RefusedUnsafePid => {
                "refused: the pid is not individually signallable".to_string()
            }
            CandidateOutcome::NotAttempted => {
                "not attempted: an earlier candidate failed and the batch stopped".to_string()
            }
        };
        out.push_str(&format!("  {id}: {described}\n"));
    }
    out
}

/// The production service. It owns no rules: it builds the real adapters and
/// calls [`preview`] / [`apply`], so there is exactly ONE copy of the authority
/// logic and no second, weaker one can grow beside the CLI.
pub struct LiveOrphanCleanupService {
    pub home: std::path::PathBuf,
}

impl LiveOrphanCleanupService {
    fn oracle() -> FreshIdentityOracle<LiveIdentityReader> {
        FreshIdentityOracle::new(LiveIdentityReader)
    }

    fn audit(&self) -> JsonlAuditStore {
        JsonlAuditStore {
            path: self.home.join("orphan-cleanup").join("audit.jsonl"),
        }
    }
}

impl OrphanCleanupService for LiveOrphanCleanupService {
    fn preview(&self, request: PreviewRequest<'_>) -> Result<Preview, PreviewError> {
        preview(
            &self.home,
            request.actor,
            request.audit_reason,
            request.proposed_pids,
            &Self::oracle(),
            &SystemClock,
            fresh_generation(),
            platform_support(),
        )
    }

    fn apply(&self, request: ApplyRequest<'_>) -> ApplyOutcome {
        apply(
            &self.home,
            request.actor,
            request.audit_reason,
            request.token,
            request.confirm_ids,
            &Self::oracle(),
            &UnixSignaler { syscall: LibcKill },
            &SystemClock,
            &self.audit(),
            &BoundedWaiter {
                probe: KillProbe { syscall: LibcKill },
                sleeper: ThreadSleeper,
                poll_ms: 50,
            },
            platform_support(),
        )
    }
}

/// Wall-clock milliseconds — the same clock that stamps `created_ms`, so the
/// TTL compares like with like. A pre-epoch clock yields 0, which makes every
/// confirmation read as expired: fail-closed, not fail-open.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// The production sleeper.
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// A per-snapshot generation. Random rather than a counter or a timestamp: two
/// previews taken within the same millisecond must not mint the same candidate
/// id for the same process, or an id would stop naming one snapshot.
fn fresh_generation() -> u64 {
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ])
}

/// A confirmation token and its sidecar ARE authority material: anything that
/// can read them can name the exact triple an operator confirmed, and anything
/// that can write them can forge one. On Unix `File::create` honours the
/// process umask, which on a permissive umask leaves them group- or
/// world-readable, so the modes are set explicitly rather than inherited.
///
/// Failing to tighten is fail-closed, not a warning: an authority file we could
/// not make private is one we should not have written.
#[cfg(unix)]
fn harden_private(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn harden_private(_path: &Path, _mode: u32) -> std::io::Result<()> {
    // Windows ACLs are inherited from the parent directory, which the daemon's
    // own home already constrains; there is no umask equivalent to correct.
    Ok(())
}

/// Guard holding the advisory lock for one confirmation.
///
/// #1629: this is a raw `fs4` flock rather than a `store::acquire_file_lock`
/// call because this module is re-exported into the LIBRARY crate for the
/// contract tests, where `store` does not exist. It therefore does the
/// chokepoint's `FLOCK_DEPTH` bookkeeping itself — `sync_audit` IS lib-visible —
/// so this site is TRACKED rather than exempt: the self-IPC deadlock guard
/// (`assert_no_registry_lock_for_self_ipc`) still sees the flock tier while this
/// lock is held. `tests/flock_depth_invariant.rs` exempts this file only while
/// both bookkeeping calls are present.
pub struct ConfirmationLock {
    file: std::fs::File,
}

impl Drop for ConfirmationLock {
    fn drop(&mut self) {
        // Unlock explicitly before the depth decrement, so depth > 0 always
        // implies the OS lock is still held — the same ordering `FileFlockGuard`
        // uses, and the reason cloned descriptors cannot leave it held.
        let _ = fs4::FileExt::unlock(&self.file);
        crate::sync_audit::flock_exited();
    }
}

/// Take the advisory lock for one confirmation. Keyed on the confirmation's own
/// path so two different tokens never serialise against each other.
fn acquire_confirmation_lock(path: &Path) -> std::io::Result<ConfirmationLock> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "confirmation path has no parent",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    harden_private(parent, 0o700)?;
    let lock_path = path.with_extension("lock");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    // The lock names a confirmation by token; keep it as private as the
    // confirmation itself.
    harden_private(&lock_path, 0o600)?;
    // Explicit trait syntax: Rust 1.89 stabilised an inherent `File::lock` with
    // the same name, and selecting it would trip the MSRV gate (rust-version is
    // below that). Same reasoning as `store::acquire_file_lock`.
    fs4::FileExt::lock(&file)?;
    // Bump AFTER the OS lock is held, so depth > 0 always implies "lock held" —
    // the ordering `store::acquire_file_lock` uses, mirrored here because this
    // module cannot reach that chokepoint from the library crate.
    crate::sync_audit::flock_entered();
    Ok(ConfirmationLock { file })
}

/// Dry run: resolve each proposed pid's exact identity and emit immutable ids.
///
/// A pid whose identity cannot be resolved is dropped from the snapshot rather
/// than carried as a partially-known candidate — an unidentifiable process can
/// never become a confirmable one.
#[allow(clippy::too_many_arguments)]
pub fn preview<O: IdentityOracle, C: Clock>(
    home: &Path,
    actor: &str,
    audit_reason: &str,
    proposed_pids: &[u32],
    oracle: &O,
    clock: &C,
    generation: u64,
    support: Support,
) -> Result<Preview, PreviewError> {
    let mut candidates = Vec::new();
    for pid in proposed_pids {
        // Identity is re-derived HERE, from this module's own oracle — never
        // read out of a V1 report. A pid whose identity is unobtainable is
        // dropped outright: a partially-known row could be confirmed by an
        // operator but could never be safely revalidated at signal time, and
        // offering it would be offering authority over an unknown process.
        let Some(identity) = oracle.identity(*pid) else {
            continue;
        };
        candidates.push(CandidateSnapshot {
            id: candidate_id(generation, *pid, identity.start_token, identity.uid),
            pid: *pid,
            start_token: identity.start_token,
            uid: identity.uid,
        });
    }
    // The token names this snapshot; apply binds the sidecar to it. Shaped to
    // pass the same 36-char hex/dash validation `decisions.rs::confirmation_path`
    // uses, so a token can never be spent as a path component.
    let token = uuid::Uuid::new_v4().to_string();
    let created_ms = clock.now_ms();
    let confirmation = Confirmation {
        schema_version: 1,
        actor: actor.to_string(),
        audit_reason: audit_reason.to_string(),
        created_ms,
        generation,
        // Stored, not merely folded into the candidate ids: a snapshot must
        // remain identifiable even when its candidate list is empty.
        nonce: uuid::Uuid::new_v4().to_string(),
        consumed: false,
        candidates: candidates.clone(),
    };
    let Some(path) = confirmation_path(home, &token) else {
        return Err(PreviewError::PersistFailed(
            "minted token is not path-safe".to_string(),
        ));
    };
    // Durable BEFORE the token is handed out. The ordering is the whole point:
    // a token whose sidecar never reached disk names a confirmation `apply` can
    // never revalidate, and the operator has no way to tell it from a real one.
    if let Err(error) = persist_confirmation(&path, &confirmation) {
        return Err(PreviewError::PersistFailed(error.to_string()));
    }
    Ok(Preview {
        support,
        token,
        generation,
        created_ms,
        candidates,
    })
}

/// Write a confirmation so that a crash cannot leave a half-written one behind:
/// same-directory temp file, fsync the file, rename over the target, then fsync
/// the directory so the rename itself is durable. The last step is the one most
/// often skipped — without it the rename can be lost even though the file's own
/// bytes were synced.
///
/// Self-contained on std for the same reason `candidate_id` is: this module is
/// re-exported into the library crate for the contract tests, where the
/// binary's store helpers do not exist.
fn persist_confirmation(path: &Path, confirmation: &Confirmation) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "confirmation path has no parent",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    harden_private(parent, 0o700)?;
    let bytes = serde_json::to_vec_pretty(confirmation)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("confirmation")
    ));
    {
        let mut file = std::fs::File::create(&tmp)?;
        // Tighten BEFORE the bytes land: a window in which the confirmation is
        // both complete and world-readable is exactly the window that matters.
        harden_private(&tmp, 0o600)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // The rename carries the temp file's mode, but re-assert it: an existing
    // target replaced by rename must not be able to donate looser modes, and a
    // second write over a hand-loosened file must retighten it.
    harden_private(path, 0o600)?;
    // Directory fsync is best-effort by platform, but a failure to OPEN the
    // directory is not: that means we cannot establish durability at all.
    std::fs::File::open(parent)?.sync_all().or_else(|error| {
        // Some filesystems refuse fsync on a directory handle. The rename is
        // still ordered on every platform this runs on; treat only that
        // specific refusal as tolerable.
        if error.kind() == std::io::ErrorKind::InvalidInput {
            Ok(())
        } else {
            Err(error)
        }
    })
}
