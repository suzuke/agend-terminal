//! Exact-generation crash disposition authority.
//!
//! The PTY channel is only an advisory wake-up.  The owner-memory ledger below
//! is the authority that fences an old process from a replacement generation
//! and serialises crash/watchdog recovery claims.

use super::AgentCore;
use crate::sync_audit::CoreMutex;
use crate::types::{AgentName, InstanceId};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// Monotonic process-owner generation.  It is deliberately not persisted:
/// restarting the owner starts a fresh namespace, while a replacement within
/// one owner always receives a greater generation.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SpawnGeneration(u64);

#[allow(dead_code)]
impl SpawnGeneration {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub(crate) const fn value(self) -> u64 {
        self.0
    }
}

/// Owner-scoped allocator.  Relaxed arithmetic is sufficient: ordering is
/// carried by the ledger mutex and the only invariant is uniqueness/monotonicity.
pub(crate) struct GenerationSource {
    next: AtomicU64,
}

impl GenerationSource {
    pub(crate) const fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
        }
    }

    pub(crate) fn next(&self) -> SpawnGeneration {
        SpawnGeneration(self.next.fetch_add(1, Ordering::Relaxed))
    }
}

static OWNER_GENERATIONS: OnceLock<GenerationSource> = OnceLock::new();

pub(crate) fn owner_generation_source() -> &'static GenerationSource {
    OWNER_GENERATIONS.get_or_init(GenerationSource::new)
}

/// Disposition state for one exact `(InstanceId, SpawnGeneration)` record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Disposition {
    Pending,
    Claimed,
    Ready,
    Executing,
    Live,
    Failed,
    Discarded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Claimant {
    Crash,
    RespawnWatchdog,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct DispositionKey {
    pub(crate) instance_id: InstanceId,
    pub(crate) generation: SpawnGeneration,
}

/// Exact crash/startup-failure context captured before any name lookup can
/// observe a replacement handle.  `core` and `deleted` are the old handle's
/// own Arcs; the recovery sink must never substitute a by-name replacement.
#[derive(Clone)]
pub(crate) struct CrashObservation {
    pub(crate) instance_id: InstanceId,
    pub(crate) generation: SpawnGeneration,
    pub(crate) core: Arc<CoreMutex<AgentCore>>,
    pub(crate) deleted: Arc<AtomicBool>,
    pub(crate) owner_shutdown: Option<Arc<AtomicBool>>,
    pub(crate) name: AgentName,
}

impl fmt::Debug for CrashObservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CrashObservation")
            .field("instance_id", &self.instance_id)
            .field("generation", &self.generation)
            .field("deleted", &self.deleted.load(Ordering::SeqCst))
            .field(
                "owner_shutdown",
                &self
                    .owner_shutdown
                    .as_ref()
                    .map(|v| v.load(Ordering::SeqCst)),
            )
            .field("name", &self.name)
            .finish()
    }
}

impl CrashObservation {
    pub(crate) fn key(&self) -> DispositionKey {
        DispositionKey {
            instance_id: self.instance_id,
            generation: self.generation,
        }
    }

    fn valid(&self, current: Option<SpawnGeneration>) -> bool {
        current == Some(self.generation)
            && !self.deleted.load(Ordering::SeqCst)
            && !self
                .owner_shutdown
                .as_ref()
                .map(|v| v.load(Ordering::SeqCst))
                .unwrap_or(false)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ClaimToken {
    key: DispositionKey,
}

impl ClaimToken {
    pub(crate) const fn key(self) -> DispositionKey {
        self.key
    }
}

/// Opaque authority for one exact-generation recovery execution.  The permit
/// is intentionally non-`Copy`/non-`Clone`: once it is settled as Live or
/// Failed it cannot be replayed for a second ledger transition.
pub(crate) struct RecoveryExecutionPermit {
    key: DispositionKey,
    core: Arc<CoreMutex<AgentCore>>,
    restarting_admitted: bool,
}

impl RecoveryExecutionPermit {
    /// Publish Restarting on the exact old core immediately before spawning a
    /// replacement.  This is idempotent only as a guarded one-shot operation;
    /// callers cannot use the same permit to publish a second transition.
    pub(crate) fn admit_restarting(&mut self) -> bool {
        if self.restarting_admitted {
            return false;
        }
        self.core.lock().state.set_restarting_with_permit(self);
        self.restarting_admitted = true;
        true
    }

    fn key(&self) -> DispositionKey {
        self.key
    }
}

struct Entry {
    observation: CrashObservation,
    disposition: Disposition,
    claimant: Option<Claimant>,
}

struct LedgerInner {
    current: HashMap<InstanceId, SpawnGeneration>,
    entries: HashMap<DispositionKey, Entry>,
}

/// Owner-memory, single-lock disposition ledger.  All transitions and
/// replacement invalidation happen under this mutex; callers snapshot the
/// registry first and never hold a registry lock while entering here.
pub(crate) struct CrashDispositionLedger {
    inner: Mutex<LedgerInner>,
}

impl CrashDispositionLedger {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(LedgerInner {
                current: HashMap::new(),
                entries: HashMap::new(),
            }),
        }
    }

    /// Publish a source observation.  A deleted/shutdown source or stale
    /// generation publishes nothing.  Duplicate publication of the same live
    /// key is idempotent and leaves the existing disposition untouched.
    pub(crate) fn publish(&self, observation: CrashObservation) -> bool {
        let mut inner = self.inner.lock();
        if observation.deleted.load(Ordering::SeqCst)
            || observation
                .owner_shutdown
                .as_ref()
                .map(|v| v.load(Ordering::SeqCst))
                .unwrap_or(false)
        {
            return false;
        }
        let current = inner.current.get(&observation.instance_id).copied();
        if current.is_some() && !observation.valid(current) {
            return false;
        }
        if current.is_none() {
            inner
                .current
                .insert(observation.instance_id, observation.generation);
        }
        let key = observation.key();
        if let Some(entry) = inner.entries.get(&key) {
            return entry.disposition != Disposition::Discarded;
        }
        inner.entries.insert(
            key,
            Entry {
                observation,
                disposition: Disposition::Pending,
                claimant: None,
            },
        );
        true
    }

    /// Register a newly installed handle generation and remove all superseded
    /// records that are not currently executing.  The current-generation map
    /// remains the late-publication fence after the old entry is removed.
    pub(crate) fn register_generation(&self, instance_id: InstanceId, generation: SpawnGeneration) {
        let mut inner = self.inner.lock();
        if inner
            .current
            .get(&instance_id)
            .is_some_and(|old| *old >= generation)
        {
            return;
        }
        inner.current.insert(instance_id, generation);
        inner.entries.retain(|key, entry| {
            !(key.instance_id == instance_id
                && key.generation != generation
                && entry.disposition != Disposition::Executing)
        });
    }

    pub(crate) fn claim(&self, key: DispositionKey, claimant: Claimant) -> Option<ClaimToken> {
        let mut inner = self.inner.lock();
        let current = inner.current.get(&key.instance_id).copied();
        let entry = inner.entries.get_mut(&key)?;
        // State eligibility is checked before source validity.  An already
        // Executing record remains owned and must never be rewritten to
        // Discarded by a late delete/replacement observation.
        if entry.disposition != Disposition::Pending {
            return None;
        }
        if !entry.observation.valid(current) {
            entry.disposition = Disposition::Discarded;
            return None;
        }
        entry.disposition = Disposition::Claimed;
        entry.claimant = Some(claimant);
        Some(ClaimToken { key })
    }

    pub(crate) fn mark_ready(&self, token: ClaimToken) -> bool {
        self.transition(token.key(), Disposition::Claimed, Disposition::Ready)
    }

    /// Revalidate the old handle's deleted/shutdown Arcs and current generation
    /// while holding the same ledger lock that performs Discard invalidation.
    pub(crate) fn begin_execute(&self, token: ClaimToken) -> Option<RecoveryExecutionPermit> {
        let mut inner = self.inner.lock();
        let current = inner.current.get(&token.key.instance_id).copied();
        let entry = inner.entries.get_mut(&token.key)?;
        // Only Ready can enter execution.  In particular, a copied token or a
        // late invalidation must not rewrite an already Executing record.
        if entry.disposition != Disposition::Ready {
            return None;
        }
        if !entry.observation.valid(current) {
            entry.disposition = Disposition::Discarded;
            return None;
        }
        let core = Arc::clone(&entry.observation.core);
        entry.disposition = Disposition::Executing;
        Some(RecoveryExecutionPermit {
            key: token.key,
            core,
            restarting_admitted: false,
        })
    }

    pub(crate) fn mark_live(&self, permit: RecoveryExecutionPermit) -> bool {
        if !permit.restarting_admitted {
            return false;
        }
        self.transition(permit.key(), Disposition::Executing, Disposition::Live)
    }

    pub(crate) fn mark_failed(&self, permit: RecoveryExecutionPermit) -> bool {
        if !permit.restarting_admitted {
            return false;
        }
        let key = permit.key();
        let core = Arc::clone(&permit.core);
        let settled = self.transition(key, Disposition::Executing, Disposition::Failed);
        if settled {
            core.lock().state.set_crashed_from_recovery();
        }
        settled
    }

    /// Publish a raw crash observation, then record Crashed outside the ledger
    /// mutex.  This keeps producer state mutation separate from ledger locking
    /// and makes the crash funnel explicit for PTY exits and API kills.
    pub(crate) fn publish_crashed(&self, observation: CrashObservation) -> bool {
        let published = self.publish(observation.clone());
        if published {
            observation.core.lock().state.set_crashed_from_observation();
        }
        published
    }

    pub(crate) fn discard(&self, key: DispositionKey) -> bool {
        let mut inner = self.inner.lock();
        let Some(entry) = inner.entries.get_mut(&key) else {
            return false;
        };
        if !matches!(
            entry.disposition,
            Disposition::Pending | Disposition::Claimed | Disposition::Ready
        ) {
            return false;
        }
        entry.disposition = Disposition::Discarded;
        true
    }

    pub(crate) fn disposition(&self, key: DispositionKey) -> Option<Disposition> {
        self.inner
            .lock()
            .entries
            .get(&key)
            .map(|entry| entry.disposition)
    }

    /// Return pending observations for the per-tick recovery sweep.  Claimed or
    /// executing work remains owned by its claimant; terminal Discarded records
    /// are never returned.
    pub(crate) fn pending(&self) -> Vec<CrashObservation> {
        self.inner
            .lock()
            .entries
            .values()
            .filter(|entry| entry.disposition == Disposition::Pending)
            .map(|entry| entry.observation.clone())
            .collect()
    }

    fn transition(&self, key: DispositionKey, from: Disposition, to: Disposition) -> bool {
        let mut inner = self.inner.lock();
        let current = inner.current.get(&key.instance_id).copied();
        let Some(entry) = inner.entries.get_mut(&key) else {
            return false;
        };
        if entry.disposition != from {
            return false;
        }
        entry.disposition = to;
        if matches!(to, Disposition::Live | Disposition::Failed) && current != Some(key.generation)
        {
            inner.entries.remove(&key);
        }
        true
    }

    #[cfg(test)]
    pub(crate) fn entry_count(&self) -> usize {
        self.inner.lock().entries.len()
    }
}

static OWNER_LEDGER: OnceLock<CrashDispositionLedger> = OnceLock::new();
static OWNER_CRASH_WAKE: OnceLock<crossbeam_channel::Sender<super::AgentExitEvent>> =
    OnceLock::new();

pub(crate) fn owner_ledger() -> &'static CrashDispositionLedger {
    OWNER_LEDGER.get_or_init(CrashDispositionLedger::new)
}

pub(crate) fn install_owner_crash_wake(tx: crossbeam_channel::Sender<super::AgentExitEvent>) {
    let _ = OWNER_CRASH_WAKE.set(tx);
}

pub(crate) fn owner_crash_wake() -> Option<crossbeam_channel::Sender<super::AgentExitEvent>> {
    OWNER_CRASH_WAKE.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::HealthTracker;
    use crate::state::StateTracker;
    use crate::vterm::VTerm;
    use std::sync::atomic::AtomicBool;

    fn observation(
        id: InstanceId,
        generation: SpawnGeneration,
        deleted: &Arc<AtomicBool>,
        shutdown: Option<&Arc<AtomicBool>>,
    ) -> CrashObservation {
        let core = Arc::new(CoreMutex::new(AgentCore {
            vterm: VTerm::new(80, 24),
            subscribers: Vec::new(),
            state: StateTracker::new(None),
            health: HealthTracker::new(),
            api_activity: crate::agent::ApiActivity::default(),
            observed_status: None,
        }));
        CrashObservation {
            instance_id: id,
            generation,
            core,
            deleted: Arc::clone(deleted),
            owner_shutdown: shutdown.cloned(),
            name: "ledger-test".into(),
        }
    }

    fn admit(ledger: &CrashDispositionLedger, token: ClaimToken) -> RecoveryExecutionPermit {
        let mut permit = ledger.begin_execute(token).expect("execution permit");
        assert!(permit.admit_restarting());
        permit
    }

    #[test]
    fn replacement_removes_old_pending_before_execution() {
        let ledger = CrashDispositionLedger::new();
        let id = InstanceId::new();
        let deleted = Arc::new(AtomicBool::new(false));
        let old = observation(id, SpawnGeneration::new(1), &deleted, None);
        ledger.register_generation(id, old.generation);
        assert!(ledger.publish(old.clone()));
        let token = ledger.claim(old.key(), Claimant::Crash).expect("claim");
        assert!(ledger.mark_ready(token));

        ledger.register_generation(id, SpawnGeneration::new(2));
        assert_eq!(ledger.disposition(old.key()), None);
        assert!(ledger.begin_execute(token).is_none());
        assert!(ledger.pending().is_empty());
    }

    #[test]
    fn delete_and_shutdown_fence_publication_and_execution() {
        let ledger = CrashDispositionLedger::new();
        let id = InstanceId::new();
        let deleted = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let obs = observation(id, SpawnGeneration::new(3), &deleted, Some(&shutdown));
        ledger.register_generation(id, obs.generation);
        assert!(ledger.publish(obs.clone()));
        let token = ledger.claim(obs.key(), Claimant::Crash).expect("claim");
        assert!(ledger.mark_ready(token));
        deleted.store(true, Ordering::SeqCst);
        assert!(ledger.begin_execute(token).is_none());
        assert_eq!(ledger.disposition(obs.key()), Some(Disposition::Discarded));

        let id2 = InstanceId::new();
        let deleted2 = Arc::new(AtomicBool::new(false));
        let shutdown2 = Arc::new(AtomicBool::new(true));
        let obs2 = observation(id2, SpawnGeneration::new(4), &deleted2, Some(&shutdown2));
        assert!(!ledger.publish(obs2));
    }

    #[test]
    fn one_atomic_claim_wins_between_crash_and_watchdog() {
        let ledger = Arc::new(CrashDispositionLedger::new());
        let id = InstanceId::new();
        let deleted = Arc::new(AtomicBool::new(false));
        let obs = observation(id, SpawnGeneration::new(5), &deleted, None);
        ledger.register_generation(id, obs.generation);
        assert!(ledger.publish(obs.clone()));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let a = {
            let ledger = Arc::clone(&ledger);
            let barrier = Arc::clone(&barrier);
            let key = obs.key();
            std::thread::spawn(move || {
                barrier.wait();
                ledger.claim(key, Claimant::Crash).is_some()
            })
        };
        let b = {
            let ledger = Arc::clone(&ledger);
            let barrier = Arc::clone(&barrier);
            let key = obs.key();
            std::thread::spawn(move || {
                barrier.wait();
                ledger.claim(key, Claimant::RespawnWatchdog).is_some()
            })
        };
        barrier.wait();
        assert_ne!(
            a.join().expect("crash claim thread"),
            b.join().expect("watchdog claim thread")
        );
    }

    #[test]
    fn eligible_attempt_reaches_live_or_failed_only_after_execute() {
        let ledger = CrashDispositionLedger::new();
        let id = InstanceId::new();
        let deleted = Arc::new(AtomicBool::new(false));
        let live = observation(id, SpawnGeneration::new(6), &deleted, None);
        ledger.register_generation(id, live.generation);
        assert!(ledger.publish(live.clone()));
        let live_token = ledger.claim(live.key(), Claimant::Crash).expect("claim");
        assert!(ledger.mark_ready(live_token));
        let live_permit = admit(&ledger, live_token);
        assert!(ledger.mark_live(live_permit));
        assert_eq!(ledger.disposition(live.key()), Some(Disposition::Live));

        let id2 = InstanceId::new();
        let failed = observation(id2, SpawnGeneration::new(7), &deleted, None);
        ledger.register_generation(id2, failed.generation);
        assert!(ledger.publish(failed.clone()));
        let failed_token = ledger.claim(failed.key(), Claimant::Crash).expect("claim");
        assert!(ledger.mark_ready(failed_token));
        let failed_permit = admit(&ledger, failed_token);
        assert!(ledger.mark_failed(failed_permit));
        assert_eq!(ledger.disposition(failed.key()), Some(Disposition::Failed));
    }

    #[test]
    fn executing_cannot_be_mutated_by_late_claim_or_begin_or_discard() {
        let ledger = CrashDispositionLedger::new();

        let id_claim = InstanceId::new();
        let deleted_claim = Arc::new(AtomicBool::new(false));
        let claim_obs = observation(id_claim, SpawnGeneration::new(8), &deleted_claim, None);
        ledger.register_generation(id_claim, claim_obs.generation);
        assert!(ledger.publish(claim_obs.clone()));
        let claim_token = ledger
            .claim(claim_obs.key(), Claimant::Crash)
            .expect("claim");
        assert!(ledger.mark_ready(claim_token));
        let mut claim_permit = ledger.begin_execute(claim_token).expect("permit");
        assert!(claim_permit.admit_restarting());
        ledger.register_generation(id_claim, SpawnGeneration::new(9));
        assert!(ledger
            .claim(claim_obs.key(), Claimant::RespawnWatchdog)
            .is_none());
        assert_eq!(
            ledger.disposition(claim_obs.key()),
            Some(Disposition::Executing)
        );

        let id_begin = InstanceId::new();
        let deleted_begin = Arc::new(AtomicBool::new(false));
        let begin_obs = observation(id_begin, SpawnGeneration::new(10), &deleted_begin, None);
        ledger.register_generation(id_begin, begin_obs.generation);
        assert!(ledger.publish(begin_obs.clone()));
        let begin_token = ledger
            .claim(begin_obs.key(), Claimant::Crash)
            .expect("claim");
        assert!(ledger.mark_ready(begin_token));
        let mut begin_permit = ledger.begin_execute(begin_token).expect("permit");
        assert!(begin_permit.admit_restarting());
        deleted_begin.store(true, Ordering::SeqCst);
        let copied_begin_token = begin_token;
        assert!(ledger.begin_execute(copied_begin_token).is_none());
        assert!(ledger.begin_execute(begin_token).is_none());
        assert_eq!(
            ledger.disposition(begin_obs.key()),
            Some(Disposition::Executing)
        );

        let id_discard = InstanceId::new();
        let deleted_discard = Arc::new(AtomicBool::new(false));
        let discard_obs = observation(id_discard, SpawnGeneration::new(11), &deleted_discard, None);
        ledger.register_generation(id_discard, discard_obs.generation);
        assert!(ledger.publish(discard_obs.clone()));
        let discard_token = ledger
            .claim(discard_obs.key(), Claimant::Crash)
            .expect("claim");
        assert!(ledger.mark_ready(discard_token));
        let discard_permit = admit(&ledger, discard_token);
        assert!(!ledger.discard(discard_obs.key()));
        assert_eq!(
            ledger.disposition(discard_obs.key()),
            Some(Disposition::Executing)
        );
        assert!(ledger.mark_live(discard_permit));
        assert_eq!(
            ledger.disposition(discard_obs.key()),
            Some(Disposition::Live)
        );
    }

    #[test]
    fn superseded_terminal_entries_release_core_and_fence_late_publish() {
        let ledger = CrashDispositionLedger::new();
        let id = InstanceId::new();
        let deleted = Arc::new(AtomicBool::new(false));
        let old = observation(id, SpawnGeneration::new(12), &deleted, None);
        let weak_core = Arc::downgrade(&old.core);
        ledger.register_generation(id, old.generation);
        assert!(ledger.publish(old.clone()));
        let token = ledger.claim(old.key(), Claimant::Crash).expect("claim");
        assert!(ledger.mark_ready(token));
        let token_permit = admit(&ledger, token);
        assert!(ledger.mark_failed(token_permit));

        ledger.register_generation(id, SpawnGeneration::new(13));
        drop(old);
        assert!(
            weak_core.upgrade().is_none(),
            "superseded terminal entries must not retain the old core Arc"
        );
        let late = observation(id, SpawnGeneration::new(12), &deleted, None);
        assert!(!ledger.publish(late));
        assert_eq!(
            ledger.disposition(DispositionKey {
                instance_id: id,
                generation: SpawnGeneration::new(12)
            }),
            None
        );
        assert_eq!(ledger.entry_count(), 0);
    }

    /// Slice-2 RED: execution admission must return an opaque, one-use permit
    /// rather than a copyable boolean.  The permit is the only authority that
    /// may cross the ledger boundary into the Restarting transition.
    #[test]
    fn begin_execute_returns_typed_one_use_permit_slice2_red() {
        let ledger = CrashDispositionLedger::new();
        let id = InstanceId::new();
        let deleted = Arc::new(AtomicBool::new(false));
        let obs = observation(id, SpawnGeneration::new(90), &deleted, None);
        ledger.register_generation(id, obs.generation);
        assert!(ledger.publish(obs.clone()));
        let token = ledger.claim(obs.key(), Claimant::Crash).expect("claim");
        assert!(ledger.mark_ready(token));

        let mut permit = ledger
            .begin_execute(token)
            .expect("exact-generation admission must mint a permit");
        assert!(permit.admit_restarting());
        assert!(ledger.mark_live(permit));
    }

    #[test]
    fn execution_permit_admits_restarting_once_and_failed_returns_exact_core_to_crashed() {
        let ledger = CrashDispositionLedger::new();
        let id = InstanceId::new();
        let deleted = Arc::new(AtomicBool::new(false));
        let obs = observation(id, SpawnGeneration::new(91), &deleted, None);
        ledger.register_generation(id, obs.generation);
        assert!(ledger.publish_crashed(obs.clone()));
        assert_eq!(
            obs.core.lock().state.get_state(),
            crate::state::AgentState::Crashed
        );
        let token = ledger.claim(obs.key(), Claimant::Crash).expect("claim");
        assert!(ledger.mark_ready(token));

        let mut permit = ledger.begin_execute(token).expect("permit");
        assert!(permit.admit_restarting());
        assert!(!permit.admit_restarting());
        assert_eq!(
            obs.core.lock().state.get_state(),
            crate::state::AgentState::Restarting
        );
        assert!(ledger.mark_failed(permit));
        assert_eq!(
            obs.core.lock().state.get_state(),
            crate::state::AgentState::Crashed
        );
        assert_eq!(ledger.disposition(obs.key()), Some(Disposition::Failed));
    }

    #[test]
    fn superseded_generations_are_removed_and_count_stays_bounded() {
        let ledger = CrashDispositionLedger::new();
        let id = InstanceId::new();
        let deleted = Arc::new(AtomicBool::new(false));

        for value in 20..=83 {
            let generation = SpawnGeneration::new(value);
            let obs = observation(id, generation, &deleted, None);
            ledger.register_generation(id, generation);
            assert!(ledger.publish(obs.clone()));
            let token = ledger.claim(obs.key(), Claimant::Crash).expect("claim");
            assert!(ledger.mark_ready(token));
            assert!(ledger.discard(obs.key()));
            assert!(
                ledger.entry_count() <= 1,
                "superseded entries must be removed, count={}",
                ledger.entry_count()
            );
        }

        let late = observation(id, SpawnGeneration::new(20), &deleted, None);
        assert!(!ledger.publish(late));
        assert!(ledger.entry_count() <= 1);
    }

    #[test]
    fn superseded_executing_entry_is_removed_after_terminal_settlement() {
        let ledger = CrashDispositionLedger::new();
        let id = InstanceId::new();
        let deleted = Arc::new(AtomicBool::new(false));
        let old = observation(id, SpawnGeneration::new(84), &deleted, None);
        let weak_core = Arc::downgrade(&old.core);
        ledger.register_generation(id, old.generation);
        assert!(ledger.publish(old.clone()));
        let token = ledger.claim(old.key(), Claimant::Crash).expect("claim");
        assert!(ledger.mark_ready(token));
        let permit = admit(&ledger, token);
        ledger.register_generation(id, SpawnGeneration::new(85));
        assert_eq!(ledger.entry_count(), 1, "Executing remains owned");
        assert!(ledger.mark_live(permit));
        drop(old);
        assert_eq!(ledger.entry_count(), 0);
        assert!(weak_core.upgrade().is_none());
        assert_eq!(ledger.disposition(token.key()), None);
    }
}
