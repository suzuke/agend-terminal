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

    /// Has a worktree been created on disk at this phase (⇒ rollback must
    /// `git worktree remove --force`)? False only at `Prepared`.
    pub(crate) fn worktree_exists(self) -> bool {
        self.rank() >= Phase::WorktreeAdded.rank()
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
        crate::store::save_atomic(&journal_path(home, mangled), self)
    }

    /// Load the journal for `mangled`, or `None` if absent/unparseable.
    pub(crate) fn load(home: &Path, mangled: &str) -> Option<Journal> {
        let bytes = std::fs::read(journal_path(home, mangled)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Delete the journal (transaction fully resolved — Committed cleanup or
    /// completed rollback). Best-effort.
    pub(crate) fn clear(home: &Path, mangled: &str) {
        let _ = std::fs::remove_file(journal_path(home, mangled));
    }
}

/// The daemon-owned transaction area (sibling of `worktrees/`, NOT inside any
/// worktree — a `remove --force` can never delete the recovery record).
fn txn_root(home: &Path) -> PathBuf {
    home.join("checkout_txn")
}

/// Journal file for `mangled` (the `<instance>-<source>` worktree key).
pub(crate) fn journal_path(home: &Path, mangled: &str) -> PathBuf {
    txn_root(home).join(mangled).join("journal.json")
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

/// Roll back a provisioning attempt that failed AFTER `git worktree add`: arm the
/// journal with retained-intent + backoff (durably), release any partial lease
/// (`unbind`, a no-op when no binding was written), then attempt the worktree
/// `remove` (returns true on success). The journal is CLEARED only when the
/// worktree is actually gone; otherwise it survives with `rollback_pending` +
/// `next_attempt_at` so recovery retries (up to the INTERVENTION ceiling), never
/// orphaning a recoverable worktree. `remove`/`unbind` are injected for tests.
pub(crate) fn rollback_failed(
    home: &Path,
    mangled: &str,
    journal: &mut Journal,
    now: chrono::DateTime<chrono::Utc>,
    remove: impl Fn() -> bool,
    unbind: impl Fn(),
) {
    journal.arm_rollback(now);
    let _ = journal.save(home, mangled);
    unbind();
    if remove() {
        Journal::clear(home, mangled);
    }
}

/// Resolve any journal left by a CRASHED prior provisioning of THIS mangled path
/// (called under the INNER path-lock, before a fresh `git worktree add`).
///
/// - No journal → nothing to do.
/// - `Committed` → a completed attempt whose tombstone wasn't cleared: clear it.
/// - Any earlier phase WITH a worktree on disk → a crashed in-flight attempt:
///   roll it back via `remove` (returns true on success) then clear; if `remove`
///   FAILS, RETAIN intent (armed + exponential backoff persisted) and return
///   `Err` so the caller aborts rather than colliding with the stale worktree.
/// - `Prepared` (no worktree materialized) → just clear.
///
/// `remove` is injected so the recovery logic is unit-testable without live git.
pub(crate) fn recover_stale(
    home: &Path,
    mangled: &str,
    now: chrono::DateTime<chrono::Utc>,
    remove: impl Fn(&Journal) -> bool,
) -> Result<(), String> {
    let Some(mut j) = Journal::load(home, mangled) else {
        return Ok(());
    };
    if j.phase == Phase::Committed {
        Journal::clear(home, mangled);
        return Ok(());
    }
    if !j.phase.worktree_exists() {
        Journal::clear(home, mangled);
        return Ok(());
    }
    if remove(&j) {
        Journal::clear(home, mangled);
        Ok(())
    } else {
        j.arm_rollback(now);
        let _ = j.save(home, mangled);
        Err(format!(
            "a prior checkout of this path left a worktree that could not be rolled back \
             (retained for retry; attempts={}{})",
            j.attempts,
            if j.intervention { ", INTERVENTION" } else { "" }
        ))
    }
}

/// Drive every DUE pending rollback across all checkout-transaction journals.
///
/// The ONE shared callable for BOTH boot-repair and a periodic tick (there is no
/// dedicated worker — the caller decides cadence). For each journal that is
/// `rollback_pending` and past its `next_attempt_at`, re-attempt the worktree
/// `remove`: clear on success; otherwise re-arm (exponential backoff) and, the
/// FIRST time a journal crosses into the INTERVENTION ceiling, emit a deduped
/// operator-visible `audit` (subsequent sweeps of the same stuck journal do NOT
/// re-emit). `remove`/`audit` are injected so the sweep is unit-testable without
/// live git. Returns the count of journals resolved (worktree removed) this pass.
pub(crate) fn recover_pending_sweep(
    home: &Path,
    now: chrono::DateTime<chrono::Utc>,
    remove: impl Fn(&Journal) -> bool,
    mut audit: impl FnMut(&Journal),
) -> usize {
    let Ok(entries) = std::fs::read_dir(txn_root(home)) else {
        return 0; // no transaction area yet ⇒ nothing pending
    };
    let mut resolved = 0;
    for entry in entries.flatten() {
        // Journal lives at <root>/<mangled>/journal.json; the sibling
        // <mangled>.lock files load as None and are skipped.
        let Some(mangled) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(mut j) = Journal::load(home, &mangled) else {
            continue;
        };
        if !j.rollback_pending || !j.rollback_due(now) {
            continue;
        }
        if remove(&j) {
            Journal::clear(home, &mangled);
            resolved += 1;
        } else {
            let was_intervention = j.intervention;
            j.arm_rollback(now);
            if j.intervention && !was_intervention {
                audit(&j); // deduped: only on ENTERING intervention
            }
            let _ = j.save(home, &mangled);
        }
    }
    resolved
}

/// Production entry to [`recover_pending_sweep`] — the ONE shared callable invoked
/// from BOTH boot-repair (`bootstrap::boot_hygiene_sweeps`) and the per-tick
/// recovery handler (no dedicated worker). Supplies the real `git worktree remove
/// --force` (run in each journal's recorded source repo) and the operator-visible
/// INTERVENTION audit (`event_log`). Returns the count resolved this pass.
pub(crate) fn recover_pending_sweep_prod(home: &Path) -> usize {
    recover_pending_sweep(
        home,
        chrono::Utc::now(),
        |j| {
            crate::git_helpers::git_bypass(
                Path::new(&j.source_repo),
                &["worktree", "remove", "--force", &j.worktree_path],
            )
            .map(|o| o.status.success())
            .unwrap_or(false)
        },
        |j| {
            crate::event_log::log(
                home,
                "checkout_txn_intervention",
                "checkout_txn",
                &format!(
                    "stuck checkout-worktree rollback entered INTERVENTION after {} attempts: {}",
                    j.attempts, j.worktree_path
                ),
            );
        },
    )
}
