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
    /// Lost the race for a single-use confirmation. Distinct from `Replayed`
    /// only in provenance — both mean "this confirmation is already spent" —
    /// but a contended loser has not yet observed the winner's write, so the
    /// two are reported separately rather than guessed at.
    Contended,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    Refused(RefusalReason),
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
pub fn apply<O: IdentityOracle, S: Signaler, C: Clock, A: AuditStore>(
    home: &Path,
    actor: &str,
    audit_reason: &str,
    token: &str,
    confirm_ids: &[String],
    oracle: &O,
    signaler: &S,
    clock: &C,
    audit: &A,
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
        support,
        &NoBarrier,
    )
}

/// [`apply`] with an injected barrier. Only the contracts pass anything other
/// than [`NoBarrier`].
#[allow(clippy::too_many_arguments)]
pub fn apply_with_barrier<O: IdentityOracle, S: Signaler, C: Clock, A: AuditStore, B: ConsumeBarrier>(
    _home: &Path,
    _actor: &str,
    _audit_reason: &str,
    _token: &str,
    _confirm_ids: &[String],
    _oracle: &O,
    _signaler: &S,
    _clock: &C,
    _audit: &A,
    _support: Support,
    _barrier: &B,
) -> ApplyOutcome {
    // RED STUB (#3273): no gate is implemented yet, so every call lands on the
    // staging refusal. It signals nothing — which is why the contracts assert
    // the exact reason: a stub that merely "refuses" would otherwise satisfy
    // every refusal contract without a single gate existing.
    ApplyOutcome::Refused(RefusalReason::ExecutorUnavailable)
}

/// Dry run: resolve each proposed pid's exact identity and emit immutable ids.
///
/// A pid whose identity cannot be resolved is dropped from the snapshot rather
/// than carried as a partially-known candidate — an unidentifiable process can
/// never become a confirmable one.
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
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "confirmation path has no parent")
    })?;
    std::fs::create_dir_all(parent)?;
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
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
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
