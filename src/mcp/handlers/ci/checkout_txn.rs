//! #2755: durable transaction journal + provisioning lock for
//! `repo action=checkout`, per decision d-20260713024125724636-10.
//!
//! `git worktree add` + marker + submodule init + `bind_full` is a multi-step
//! side-effecting provision. A crash / init failure between steps must never
//! leave a half-provisioned worktree that later reads as "leased". This module
//! makes the provision a **journalled transaction**:
//!
//! - Phases advance `Prepared → WorktreeAdded → MarkerDurable → SubmodulesReady
//!   → Committed`; each transition is durably persisted BEFORE the next
//!   side effect, so a replay after a crash knows exactly how far it got.
//! - **Committed is the durable linearization point**: the caller returns
//!   success ONLY after a `Committed` journal is durably written
//!   ([`store::atomic_write`]); a write failure aborts into rollback.
//! - The journal (keyed by the `<instance>-<source>` mangled name) and its
//!   provisioning lock (keyed by the NORMALIZED target PATH, so any consumer —
//!   checkout/bind/release/GC — derives the same domain from the path alone) live
//!   OUTSIDE the worktree in a daemon-owned area — so a `remove --force` of the
//!   worktree can never delete the recovery record, and a stable key lets a
//!   restart find pending work.
//! - **CAS-by-nonce**: each provisioning attempt stamps a unique `nonce`; a
//!   replayer compares it to distinguish "my in-flight attempt" from a stale
//!   record left by a previous process.
//! - Rollback that fails (Windows open-handle, transient FS) RETAINS intent:
//!   `rollback_pending` stays set with an exponential-backoff `next_attempt_at`;
//!   at the [`INTERVENTION_CEILING_SECS`] cap it enters operator-visible
//!   `intervention` and keeps retrying at ceiling cadence rather than orphaning
//!   a recoverable worktree.
//!
//! This module is pure state + filesystem journal (no live git), so every
//! invariant above is unit-testable without a real `git worktree add`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// Test seam: when set on the current thread, `Journal::save` fails ONLY the
// `Committed` write, so a real-entry test can exercise the Committed-write-failure
// abort (earlier phase saves still succeed).
// `all(test, unix)`: this seam is exercised only by the Unix-only
// `checkout_commit_write_failure_rolls_back_2755` real-entry test (the #2158
// source guard's absolute arm is `/`-prefixed — see checkout_submodule_tests.rs).
// Gating the whole seam (thread_local + setter + `save` check) to `unix` keeps it
// physically absent on Windows tests, so there is no dead-code to suppress.
#[cfg(all(test, unix))]
thread_local! {
    static FAIL_COMMITTED_SAVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test-only: arm/disarm the [`FAIL_COMMITTED_SAVE`] seam for the current thread.
#[cfg(all(test, unix))]
pub(crate) fn set_fail_committed_save(fail: bool) {
    FAIL_COMMITTED_SAVE.with(|c| c.set(fail));
}

/// Bumped if the on-disk journal shape changes incompatibly.
pub(crate) const JOURNAL_SCHEMA_VERSION: u32 = 1;

/// Rollback-retry backoff ceiling. Once the exponential backoff reaches this many
/// seconds the transaction is operator-visible (`intervention`) and keeps
/// retrying at this cadence — a recoverable open-handle worktree is never
/// permanently abandoned.
pub(crate) const INTERVENTION_CEILING_SECS: i64 = 300;

/// The five provisioning phases (durably advanced in order).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Phase {
    /// Journal written; NO filesystem side effect yet.
    Prepared,
    /// `git worktree add` succeeded.
    WorktreeAdded,
    /// `.agend-managed` marker written + durable.
    MarkerDurable,
    /// Recursive submodule init succeeded.
    SubmodulesReady,
    /// Durable linearization point — success may be returned.
    Committed,
}

impl Phase {
    /// Rank for ordering assertions (monotonic advance only).
    pub(crate) fn rank(self) -> u8 {
        match self {
            Phase::Prepared => 0,
            Phase::WorktreeAdded => 1,
            Phase::MarkerDurable => 2,
            Phase::SubmodulesReady => 3,
            Phase::Committed => 4,
        }
    }
}

/// The durable transaction record for one checkout provisioning attempt.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct Journal {
    pub schema_version: u32,
    /// CAS-by-nonce: unique per provisioning attempt.
    pub nonce: String,
    pub phase: Phase,
    /// The mangled worktree directory being provisioned.
    pub worktree_path: String,
    /// Canonical source repo (the `git worktree remove` cwd on rollback).
    pub source_repo: String,
    pub branch: String,
    pub bind: bool,
    pub created_at: String,
    /// A phase failed and a `git worktree remove --force` rollback is owed.
    #[serde(default)]
    pub rollback_pending: bool,
    /// Count of rollback attempts so far (drives backoff + intervention).
    #[serde(default)]
    pub attempts: u32,
    /// Earliest rfc3339 time the next rollback retry may run.
    #[serde(default)]
    pub next_attempt_at: Option<String>,
    /// Backoff reached the ceiling — operator-visible, still retrying.
    #[serde(default)]
    pub intervention: bool,
}

impl Journal {
    /// A fresh `Prepared` journal for a new provisioning attempt.
    pub(crate) fn prepared(
        nonce: impl Into<String>,
        worktree_path: impl Into<String>,
        source_repo: impl Into<String>,
        branch: impl Into<String>,
        bind: bool,
        created_at: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: JOURNAL_SCHEMA_VERSION,
            nonce: nonce.into(),
            phase: Phase::Prepared,
            worktree_path: worktree_path.into(),
            source_repo: source_repo.into(),
            branch: branch.into(),
            bind,
            created_at: created_at.into(),
            rollback_pending: false,
            attempts: 0,
            next_attempt_at: None,
            intervention: false,
        }
    }

    /// Advance to `next` (must be a strictly higher rank — monotonic).
    pub(crate) fn advance(&mut self, next: Phase) {
        debug_assert!(
            next.rank() > self.phase.rank(),
            "phase must advance monotonically: {:?} -> {:?}",
            self.phase,
            next
        );
        self.phase = next;
    }

    /// Mark that rollback is owed and (re)compute the next backoff deadline from
    /// `now`. `attempts` increments; once the backoff hits the ceiling the record
    /// is flagged `intervention`.
    pub(crate) fn arm_rollback(&mut self, now: chrono::DateTime<chrono::Utc>) {
        self.rollback_pending = true;
        let backoff = backoff_secs(self.attempts);
        self.next_attempt_at = Some((now + chrono::Duration::seconds(backoff)).to_rfc3339());
        if backoff >= INTERVENTION_CEILING_SECS {
            self.intervention = true;
        }
        self.attempts = self.attempts.saturating_add(1);
    }

    /// Is a pending rollback due to retry at `now`? A deadline that is absent or
    /// unparseable is treated as due (fail-forward — never strand a recoverable
    /// worktree behind a corrupt timestamp).
    pub(crate) fn rollback_due(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        if !self.rollback_pending {
            return false;
        }
        match self
            .next_attempt_at
            .as_deref()
            .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        {
            Some(d) => now >= d.with_timezone(&chrono::Utc),
            None => true,
        }
    }

    /// Durably persist (temp+fsync+rename+dir-fsync via [`store::atomic_write`]).
    pub(crate) fn save(&self, home: &Path, mangled: &str) -> anyhow::Result<()> {
        #[cfg(all(test, unix))]
        if self.phase == Phase::Committed && FAIL_COMMITTED_SAVE.with(|c| c.get()) {
            return Err(anyhow::anyhow!(
                "test seam: forced Committed journal save failure"
            ));
        }
        crate::store::save_atomic(&journal_path(home, mangled), self)
    }

    /// Load the journal for `mangled`, or `None` if absent/corrupt. Production
    /// recovery uses [`load_typed`] (which distinguishes the two); this Option
    /// convenience is test-only.
    #[cfg(test)]
    pub(crate) fn load(home: &Path, mangled: &str) -> Option<Journal> {
        match load_typed(home, mangled) {
            JournalLoad::Loaded(j) => Some(j),
            JournalLoad::Absent | JournalLoad::Corrupt | JournalLoad::Unreadable => None,
        }
    }

    /// Delete the journal (transaction fully resolved — Committed cleanup or
    /// completed rollback). Best-effort.
    pub(crate) fn clear(home: &Path, mangled: &str) {
        let _ = std::fs::remove_file(journal_path(home, mangled));
    }
}

/// The result of reading a journal file — recovery must distinguish a genuinely
/// ABSENT record from a CORRUPT one (a torn/partial write from a crash), since a
/// corrupt journal must not be silently treated as "nothing to recover".
pub(crate) enum JournalLoad {
    Absent,
    /// #2755 R4: the file EXISTS but could not be READ (permission / transient I/O).
    /// Recovery authority is UNCERTAIN, so consumers MUST fail closed — never conflate
    /// this with `Absent` (which is a genuine NotFound = nothing to recover).
    Unreadable,
    Corrupt,
    Loaded(Journal),
}

/// Read the journal for `mangled`, distinguishing Absent / Unreadable / Corrupt /
/// Loaded. #2755 R4: ONLY `NotFound` is `Absent`; any other read error is `Unreadable`
/// (fail-closed authority), so a permission/transient error can't silently bypass a
/// still-outstanding recovery record.
pub(crate) fn load_typed(home: &Path, mangled: &str) -> JournalLoad {
    match std::fs::read(journal_path(home, mangled)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => JournalLoad::Absent,
        Err(_) => JournalLoad::Unreadable,
        Ok(bytes) => match serde_json::from_slice::<Journal>(&bytes) {
            Ok(j) => JournalLoad::Loaded(j),
            Err(_) => JournalLoad::Corrupt,
        },
    }
}

/// The daemon-owned transaction area (sibling of `worktrees/`, NOT inside any
/// worktree — a `remove --force` can never delete the recovery record).
pub(crate) fn txn_root(home: &Path) -> PathBuf {
    home.join("checkout_txn")
}

/// Journal file for `mangled` (the `<instance>-<source>` worktree key).
pub(crate) fn journal_path(home: &Path, mangled: &str) -> PathBuf {
    txn_root(home).join(mangled).join("journal.json")
}

/// #2755 R3 (root + independent review): QUARANTINE a corrupt journal instead of
/// CLEARING it. A torn/disk-corrupted record still carries recovery AUTHORITY (a
/// managed worktree may remain at this path), so the ONLY durable source/path/nonce
/// evidence must be retained for operator intervention — never destroyed. Renames
/// `journal.json` → `journal.json.corrupt` (best-effort, idempotent): the evidence
/// survives AND a later sweep sees `load_typed` = Absent (no `journal.json`), so the
/// torn record is not reprocessed every tick. Safe WITHOUT a lock: journal writes are
/// atomic (`store::save_atomic` = temp+rename), so a *corrupt* `journal.json` is
/// always a genuine crash/corruption artifact, never a live checkout's mid-write.
pub(crate) fn quarantine_corrupt(home: &Path, mangled: &str) -> bool {
    let jp = journal_path(home, mangled);
    // #2755 R4: collision-safe evidence name (hash the corrupt bytes) so two distinct
    // torn records at this path don't clobber each other's evidence; OBSERVE the rename.
    let dest = match std::fs::read(&jp) {
        Ok(bytes) => corrupt_evidence_path(home, mangled, &bytes),
        Err(_) => jp.with_file_name("journal.json.corrupt"), // unreadable ⇒ stable fallback
    };
    std::fs::rename(&jp, &dest).is_ok()
}

/// #2755 R4: collision-safe sidecar path for a corrupt journal's retained evidence —
/// the corrupt BYTES are hashed so distinct torn records don't overwrite each other's
/// evidence, and re-quarantining the SAME bytes is idempotent.
fn corrupt_evidence_path(home: &Path, mangled: &str, bytes: &[u8]) -> PathBuf {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hasher::write(&mut h, bytes);
    journal_path(home, mangled).with_file_name(format!(
        "journal.json.corrupt-{:016x}",
        std::hash::Hasher::finish(&h)
    ))
}

/// Normalize an absolute worktree TARGET path into the stable identity used as
/// the CROSS-CONSUMER lock domain (checkout / bind / release / GC all key off the
/// same real path, regardless of naming scheme). Canonicalize when the path
/// EXISTS (release/GC — resolves symlinks / `.` / `..`); otherwise (checkout,
/// pre-creation) canonicalize the PARENT and re-append the basename, so the same
/// real path yields the same identity whether or not the worktree is
/// materialized yet.
pub(crate) fn normalize_target(target: &Path) -> String {
    if let Ok(c) = target.canonicalize() {
        return c.to_string_lossy().into_owned();
    }
    match (target.parent(), target.file_name()) {
        (Some(parent), Some(name)) => {
            let base = parent
                .canonicalize()
                .unwrap_or_else(|_| parent.to_path_buf());
            base.join(name).to_string_lossy().into_owned()
        }
        _ => target.to_string_lossy().into_owned(),
    }
}

/// A filesystem-safe, bounded lock-file NAME for a normalized target path —
/// STABLE (same across processes of one binary, so any consumer derives the same
/// lock file from the path alone) and collision-RESISTANT (a hash, not a
/// bijection — the AUTHORITATIVE path identity is the guard's `normalize_target`
/// string, revalidated on use; the hash only names the lock file). A hash reseed
/// across a binary upgrade merely orphans idle lock files; the DURABLE journal is
/// keyed separately by the human-readable `mangled`.
fn path_lock_key(normalized: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hasher::write(&mut h, normalized.as_bytes());
    format!("wtpath-{:016x}", std::hash::Hasher::finish(&h))
}

/// Recognize only the exact lock-file namespace emitted by [`lock_path`].
/// Other journal-key artifacts must continue through typed fail-closed
/// handling rather than being silently classified as coordination locks.
pub(crate) fn is_canonical_path_lock_name(name: &str) -> bool {
    let Some(hex) = name
        .strip_prefix("wtpath-")
        .and_then(|suffix| suffix.strip_suffix(".lock"))
    else {
        return false;
    };
    hex.len() == 16
        && hex
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// The CANONICAL per-worktree-path lock file, keyed by the NORMALIZED target
/// PATH (not the naming scheme). Kept outside the journal subdir so it is not
/// swept with the journal. The single shared lock file for any op that mutates
/// the worktree at this path — checkout today; a follow-up repo-release reuses it
/// to serialize delete against checkout on the SAME path. Do not fork a private
/// per-op lock file.
pub(crate) fn lock_path(home: &Path, normalized_target: &str) -> PathBuf {
    txn_root(home).join(format!("{}.lock", path_lock_key(normalized_target)))
}

/// A held per-worktree-path provisioning lock PLUS the NORMALIZED target-path
/// identity it guards — a TYPED proof, not a bare flock, so a holder can
/// REVALIDATE that the lock it holds matches the exact path it is about to
/// mutate. The flock releases when this guard drops (RAII), so simply holding it
/// serializes the path. `mangled` is retained as `(instance, source)` metadata
/// (the journal key), but the lock DOMAIN is the normalized path (`target`).
///
/// #2755's checkout only HOLDS this guard. The identity/revalidate accessors are
/// the forward API that Slice A (t-20260713082228621780-70517-26) will require on
/// `bind_full` (pass the proof, assert identity before writing the binding);
/// `allow(dead_code)` stands until that consumer lands.
#[allow(dead_code)] // fields/accessors consumed by Slice A (t-…70517-26)
pub(crate) struct PathLockGuard {
    target: String,
    mangled: String,
    _flock: crate::store::FileFlockGuard,
}

#[allow(dead_code)] // accessors consumed by Slice A (t-…70517-26); #2755 holds only
impl PathLockGuard {
    /// The normalized absolute target-path lock domain this guard holds.
    pub(crate) fn target(&self) -> &str {
        &self.target
    }

    /// The `(instance, source)` mangled journal key (metadata; NOT the lock domain).
    pub(crate) fn mangled(&self) -> &str {
        &self.mangled
    }

    /// Re-resolve / revalidate: does this held lock guard the worktree at
    /// `target_path`? A holder normalizes the path it is about to mutate and
    /// asserts identity before mutating.
    pub(crate) fn guards(&self, target_path: &Path) -> bool {
        normalize_target(target_path) == self.target
    }
}

/// Acquire the canonical per-worktree-path provisioning lock (blocking), keyed by
/// the NORMALIZED `worktree_dir` PATH — crate-reusable across worktree-mutating
/// ops on the same real path (checkout now; repo-release next — do NOT duplicate
/// this API privately). `mangled` is carried as the journal metadata key.
///
/// Fleet lock order (acquire outer→inner, release inner→outer):
/// branch-lease flock (`binding::acquire_branch_lease_lock`, OUTER, bind-only) →
/// THIS per-path flock (serializes one worktree PATH for bind AND non-bind; the
/// SOLE lock when `bind:false`) → binding-write lock (`.binding.json.lock`,
/// inside `bind_full`, INNER). Held across the whole provision; the caller
/// declares this guard AFTER the branch-lease guard so it drops (releases) first
/// — inner-first.
pub(crate) fn acquire_path_lock(
    home: &Path,
    worktree_dir: &Path,
    mangled: &str,
) -> anyhow::Result<PathLockGuard> {
    let target = normalize_target(worktree_dir);
    let flock = crate::store::acquire_file_lock(&lock_path(home, &target))?;
    Ok(PathLockGuard {
        target,
        mangled: mangled.to_string(),
        _flock: flock,
    })
}

/// NON-BLOCKING variant for the recovery sweep: `None` when the path-lock is held
/// (an ACTIVE checkout owns this path — leave its journal alone) instead of
/// blocking a daemon tick. `Some` only when the lock is free (⇒ any non-Committed
/// journal here is from a CRASHED, not a live, provision — safe to recover).
pub(crate) fn try_acquire_path_lock(
    home: &Path,
    worktree_dir: &Path,
    mangled: &str,
) -> Option<PathLockGuard> {
    let target = normalize_target(worktree_dir);
    let flock = crate::store::try_acquire_file_lock(&lock_path(home, &target))
        .ok()
        .flatten()?;
    Some(PathLockGuard {
        target,
        mangled: mangled.to_string(),
        _flock: flock,
    })
}

/// Exponential rollback backoff for retry `attempts` (0-based): 2^attempts
/// seconds, capped at [`INTERVENTION_CEILING_SECS`] so a persistently-stuck
/// worktree retries forever at the ceiling cadence rather than growing
/// unbounded.
pub(crate) fn backoff_secs(attempts: u32) -> i64 {
    let doubled = 1i64.checked_shl(attempts.min(31)).unwrap_or(i64::MAX);
    doubled.min(INTERVENTION_CEILING_SECS)
}

/// A unique-per-attempt nonce (pid + wall-clock nanos) for CAS-by-nonce.
pub(crate) fn new_nonce() -> String {
    let now = chrono::Utc::now();
    format!(
        "{}-{}",
        std::process::id(),
        now.timestamp_nanos_opt()
            .unwrap_or_else(|| now.timestamp_millis())
    )
}

/// The observable outcome of a post-`git worktree add` rollback, so a caller reports
/// the ACTUAL cleanup state and never claims the worktree was removed while it is
/// still present awaiting the recovery sweep (#2755 R3 — root + independent review).
#[must_use = "the checkout response must reflect Removed vs RollbackPending — never claim rolled back while cleanup is pending"]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RollbackOutcome {
    /// Worktree removed and the journal cleared — cleanup complete.
    Removed,
    /// Worktree remove FAILED (Windows open-handle / transient FS); the retained-
    /// intent journal is armed for the recovery sweep to retry. `intent_durable` is
    /// whether that armed journal was durably persisted — `false` means the save
    /// ALSO failed, a worse state the response must surface for intervention.
    RollbackPending { intent_durable: bool },
}

/// Roll back a provisioning attempt that failed AFTER `git worktree add`: arm the
/// journal with retained-intent + backoff (durably), release any partial lease
/// (`unbind`, a no-op when no binding was written), then attempt the worktree
/// `remove`. Returns [`RollbackOutcome`]: `Removed` (journal cleared) ONLY when the
/// worktree is actually gone; otherwise `RollbackPending` — the journal survives with
/// `rollback_pending` + `next_attempt_at` so recovery retries (up to the INTERVENTION
/// ceiling), never orphaning a recoverable worktree, and the caller MUST report
/// pending, not "rolled back". The armed-intent SAVE result is surfaced as
/// `intent_durable` (a save failure is a durability regression the response flags).
/// `remove`/`unbind` are injected for tests.
pub(crate) fn rollback_failed(
    home: &Path,
    mangled: &str,
    journal: &mut Journal,
    now: chrono::DateTime<chrono::Utc>,
    remove: impl Fn() -> bool,
    unbind: impl Fn(),
) -> RollbackOutcome {
    journal.arm_rollback(now);
    let intent_durable = journal.save(home, mangled).is_ok();
    unbind();
    if remove() {
        Journal::clear(home, mangled);
        RollbackOutcome::Removed
    } else {
        RollbackOutcome::RollbackPending { intent_durable }
    }
}

/// Called at checkout START, UNDER the freshly-acquired path-lock, to resolve a
/// journal left by a CRASHED prior attempt at THIS path so the fresh provision (or
/// idempotent reuse) can proceed without colliding with a stale worktree.
///
/// - Absent → nothing to do.
/// - `Committed` tombstone → leave any (valid) worktree for the caller's reuse;
///   just drop the stale record.
/// - Corrupt → a torn/disk-corrupted record still carries recovery AUTHORITY:
///   QUARANTINE it (retain evidence — #2755 R3), never silently drop it, then treat
///   the path as a crashed attempt below. The caller-known `source_repo` lets a
///   remove-failure arm a SYNTHESIZED replacement record the sweep can retry.
/// - Any non-Committed phase → a crashed attempt: remove a real worktree if one
///   EXISTS at `worktree_dir` (covers the Prepared-with-real-worktree window — a
///   crash after `git worktree add` but before the WorktreeAdded save), then clear.
///   If `remove` FAILS, arm+retain a DURABLE journal (loaded, or synthesized from
///   `source_repo`+`worktree_dir` for a corrupt/absent record) and return `Err` so
///   the sweep retries and the caller aborts rather than collide.
///
/// `remove` (prod: `git worktree remove --force worktree_dir`) is injected so the
/// logic is unit-testable without live git.
pub(crate) fn recover_stale(
    home: &Path,
    mangled: &str,
    worktree_dir: &Path,
    source_repo: &str,
    now: chrono::DateTime<chrono::Utc>,
    remove: impl Fn() -> bool,
) -> Result<(), String> {
    // #2755 R4 (item 3a): NotFound ⇒ Absent (nothing to do); UNREADABLE (permission /
    // transient I/O) leaves recovery authority UNCERTAIN ⇒ abort fail-closed, never
    // silently proceed as if there were nothing to recover.
    let existing = match load_typed(home, mangled) {
        JournalLoad::Absent => return Ok(()),
        JournalLoad::Unreadable => {
            return Err(
                "checkout journal storage is unreadable — recovery authority \
                 uncertain, aborting fail-closed"
                    .into(),
            )
        }
        JournalLoad::Corrupt => None, // evidence-retain + fail-closed arm handled below
        JournalLoad::Loaded(j) => Some(j),
    };
    let is_corrupt = existing.is_none();
    // For a corrupt record, retire it to a collision-safe evidence sidecar whenever the
    // path is otherwise resolved (no worktree / adopted / removed).
    let retire = |home: &Path| {
        if is_corrupt {
            quarantine_corrupt(home, mangled);
        } else {
            Journal::clear(home, mangled);
        }
    };
    if matches!(&existing, Some(j) if j.phase == Phase::Committed) {
        Journal::clear(home, mangled); // completed attempt's tombstone
        return Ok(());
    }
    if !worktree_dir.exists() {
        retire(home); // no worktree ⇒ nothing to reconcile
        return Ok(());
    }
    // #2755 R4 (item 1): NEVER delete a still-BOUND worktree (a crash after `bind_full`
    // but before the Committed journal leaves the binding written yet the journal
    // non-Committed). Under the caller's path lock, ADOPT a bound worktree as committed;
    // an UNCERTAIN binding fails closed.
    match crate::binding::worktree_binding_state(home, worktree_dir) {
        crate::binding::WorktreeBindingState::Bound => {
            retire(home); // provision effectively committed — keep the worktree + binding
            return Ok(());
        }
        crate::binding::WorktreeBindingState::Uncertain => {
            return Err(
                "a prior checkout left a worktree whose binding could not be read \
                 — refusing to remove a possibly-bound worktree (fail closed)"
                    .into(),
            )
        }
        crate::binding::WorktreeBindingState::Unbound => {}
    }
    if remove() {
        retire(home);
        return Ok(());
    }
    // Remove FAILED, worktree on disk, UNBOUND ⇒ arm a DURABLE recovery record.
    // #2755 R4 (item 3c): the arm-save is OBSERVED. For a corrupt record, COPY the
    // evidence aside FIRST, then the atomic replacement save OVERWRITES journal.json; if
    // that save FAILS, journal.json stays the (corrupt or loaded) record — a durable
    // BLOCKING state — so the next attempt never sees Absent (never fail-open).
    if is_corrupt {
        if let Ok(bytes) = std::fs::read(journal_path(home, mangled)) {
            let _ = std::fs::copy(
                journal_path(home, mangled),
                corrupt_evidence_path(home, mangled, &bytes),
            );
        }
    }
    let mut j = existing.unwrap_or_else(|| {
        Journal::prepared(
            new_nonce(),
            worktree_dir.to_string_lossy().into_owned(),
            source_repo.to_string(),
            String::new(),
            false,
            now.to_rfc3339(),
        )
    });
    j.arm_rollback(now);
    if j.save(home, mangled).is_err() {
        return Err(
            "a prior checkout left an un-removable worktree AND its recovery \
             record could not be persisted — the original journal is retained as a durable \
             blocking record; aborting fail-closed"
                .into(),
        );
    }
    Err(
        "a prior checkout of this path left a worktree that could not be rolled \
         back (retained for recovery)"
            .into(),
    )
}

// #2755 R4: the crash-recovery SWEEP (boot + per-tick driver, `recover_pending_sweep`
// [`_prod`]) lives in the sibling `checkout_recovery` module to keep both files under the
// LOC ceiling; re-exported so every `checkout_txn::recover_pending_sweep[_prod]` caller
// and test path is unchanged.
pub(crate) use super::checkout_recovery::recover_pending_sweep_prod;
// `recover_pending_sweep` (the injected-closure core) is only reached from tests;
// `recover_pending_sweep_prod` is its sole production caller (inside checkout_recovery).
#[cfg(test)]
pub(crate) use super::checkout_recovery::recover_pending_sweep;
