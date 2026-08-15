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
    fn term(&self, pid: u32) -> SignalOutcome;
    fn kill(&self, pid: u32) -> SignalOutcome;
    /// Liveness probe used to bound the grace window.
    fn is_alive(&self, pid: u32) -> bool;
}

/// Injected clock so TTL and grace windows are deterministic under test.
pub trait Clock {
    fn now_ms(&self) -> i64;
}

/// The pre-signal audit record. Written and fsynced BEFORE the first TERM: if
/// the machine dies mid-operation the record of intent must already be on disk,
/// which is the one ordering `decisions.rs::archive_batch` deliberately does
/// not model (it audits after the rename).
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Dry run: resolve each proposed pid's exact identity and emit immutable ids.
///
/// A pid whose identity cannot be resolved is dropped from the snapshot rather
/// than carried as a partially-known candidate — an unidentifiable process can
/// never become a confirmable one.
pub fn preview<O: IdentityOracle, C: Clock>(
    _home: &Path,
    _actor: &str,
    _audit_reason: &str,
    _proposed_pids: &[u32],
    _oracle: &O,
    clock: &C,
    generation: u64,
    support: Support,
) -> Preview {
    // RED STUB (#3273): the executor does not exist yet, so the snapshot is
    // empty and no id is ever emitted. `tests/orphan_cleanup_manual_authority.rs`
    // fails on the missing ids; it does NOT fail on a missing symbol. GREEN
    // replaces this body.
    Preview {
        support,
        token: String::new(),
        generation,
        created_ms: clock.now_ms(),
        candidates: Vec::new(),
    }
}
