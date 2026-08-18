//! AUDIT2-006: bounded delivery worker.
//!
//! The daemon's main tick / `run_core` loop emits events and, via the event bus
//! subscribers, delivers notifications by injecting into agent PTYs, using a
//! structured backend protocol, and sending Telegram messages. All are
//! BLOCKING I/O: a Telegram network black-hole (no local request timeout), a
//! slow PTY readback, or a backend handshake/RPC wait would otherwise park the
//! tick thread — stalling the hang-detection, recovery-dispatcher and crash
//! handling that share that one thread.
//!
//! This module offloads the blocking delivery effect (physical PTY poke,
//! structured backend delivery, and Telegram send) off the tick thread. Durable
//! source-of-truth writes (inbox JSONL,
//! `notification_queue`, schedule `run_history`) stay SYNCHRONOUS on the caller
//! — a notification is a wakeup, not a commit barrier. `event_bus::emit`'s
//! handled-count is unaffected: it is decided by the synchronous kind-match
//! inside each subscriber, BEFORE any delivery is enqueued (see `event_bus.rs`).
//!
//! Backpressure: Telegram and tick-stall `DeliveryJob` kinds use a bounded
//! `sync_channel(QUEUE_CAP)` and `try_send`, while transport uses a separate
//! bounded fixed-worker scheduler with per-key FIFO queues. Neither enqueue
//! path blocks. On a full queue the job is dropped and the caller is told so it
//! can record the drop where it owns a durable status (cron → `drop_queue_full`;
//! Telegram → evict the dedup claim so a later identical emit isn't suppressed
//! for the whole TTL; transport → a typed queue-full result and receipt).
//!
//! Shutdown is best-effort by design: the worker is a daemon-lifetime thread
//! reaped by the OS at process exit. There is no graceful join (Rust std threads
//! can't be safely cancelled mid-`block_on`); a queued-but-undrained job is
//! explained by its synchronous record (e.g. cron's `ok_queued`).
//!
//! Telegram and the other delivery kinds remain on the bounded worker.
//! Transport delivery has a separate fixed-size scheduler because its adapter
//! calls can block; the scheduler preserves FIFO among pending jobs for a key
//! while allowing unrelated keys to progress without an unbounded thread fanout.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};

/// Queue depth. Large enough to absorb a burst of watchdog / cron / crash
/// notifications, small enough that a wedged delivery path (post request-timeout)
/// surfaces as drops quickly rather than unbounded memory growth.
const QUEUE_CAP: usize = 256;
const TRANSPORT_WORKERS: usize = 4;

/// A unit of blocking delivery work offloaded off the tick / main-loop thread.
enum DeliveryJob {
    /// A Telegram send whose dedup claim was already recorded on the caller
    /// thread (see `channel::telegram::notify`). On terminal send failure the
    /// worker evicts that claim.
    TelegramSend(TelegramSendJob),
    /// PR4: an out-of-band tick-stall page. The stall monitor thread `try_send`s
    /// this (never blocking the tick host it watches); the worker — off that
    /// thread — owns the escalation fan-out + `event_log` write. `host` / `phase`
    /// / `handler` are the captured stall identity, `generation` the seqlock
    /// progress marker at the time of the page.
    TickStallAlert {
        home: std::path::PathBuf,
        host: String,
        phase: String,
        handler: String,
        generation: u64,
    },
}

/// Payload for an offloaded Telegram send. Carries the already-resolved channel
/// coordinates and the dedup key claimed on the caller thread, so the worker
/// reproduces exactly what the synchronous path would have sent.
pub(crate) struct TelegramSendJob {
    pub home: std::path::PathBuf,
    pub instance: String,
    pub text: String,
    pub disable_notification: bool,
    pub token: String,
    pub group_id: i64,
    pub topic_id: Option<i32>,
    pub dedup_key: crate::channel::dedup::DedupKey,
}

struct DeliveryWorker {
    tx: SyncSender<DeliveryJob>,
}

struct TransportDelivery {
    home: PathBuf,
    agent: String,
    notification: String,
    epoch: u64,
}

struct TransportScheduler {
    state: Arc<TransportSchedulerState>,
}

struct TransportSchedulerState {
    queue: parking_lot::Mutex<TransportSchedulerQueue>,
    wake: parking_lot::Condvar,
}

struct TransportSchedulerQueue {
    pending: std::collections::VecDeque<TransportDelivery>,
    active: std::collections::HashSet<TransportKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportEnqueueError {
    QueueFull { epoch: u64 },
    Fenced,
}

static TRANSPORT_SCHEDULER: OnceLock<TransportScheduler> = OnceLock::new();

type TransportKey = (PathBuf, String);

struct TransportCoordinator {
    lanes: parking_lot::Mutex<HashMap<TransportKey, Arc<parking_lot::Mutex<()>>>>,
    epochs: parking_lot::Mutex<HashMap<TransportKey, Arc<TransportEpochEntry>>>,
}

struct TransportEpochState {
    epoch: u64,
    cleanup_active: bool,
}

struct TransportEpochEntry {
    state: parking_lot::Mutex<TransportEpochState>,
    admission_snapshot: AtomicU64,
}

const CLEANUP_SNAPSHOT_BIT: u64 = 1 << 63;
const EPOCH_SNAPSHOT_MASK: u64 = !CLEANUP_SNAPSHOT_BIT;

impl TransportEpochEntry {
    fn publish_admission_snapshot(&self, state: &TransportEpochState) {
        let epoch = state.epoch.min(EPOCH_SNAPSHOT_MASK);
        let snapshot = epoch
            | if state.cleanup_active {
                CLEANUP_SNAPSHOT_BIT
            } else {
                0
            };
        self.admission_snapshot.store(snapshot, Ordering::Release);
    }
}

impl Default for TransportEpochEntry {
    fn default() -> Self {
        Self {
            state: parking_lot::Mutex::new(TransportEpochState {
                epoch: 0,
                cleanup_active: false,
            }),
            admission_snapshot: AtomicU64::new(0),
        }
    }
}

fn transport_coordinator() -> &'static TransportCoordinator {
    static COORDINATOR: OnceLock<TransportCoordinator> = OnceLock::new();
    COORDINATOR.get_or_init(|| TransportCoordinator {
        lanes: parking_lot::Mutex::new(HashMap::new()),
        epochs: parking_lot::Mutex::new(HashMap::new()),
    })
}

fn transport_scheduler() -> &'static TransportScheduler {
    TRANSPORT_SCHEDULER.get_or_init(|| {
        let state = Arc::new(TransportSchedulerState {
            queue: parking_lot::Mutex::new(TransportSchedulerQueue {
                pending: std::collections::VecDeque::new(),
                active: std::collections::HashSet::new(),
            }),
            wake: parking_lot::Condvar::new(),
        });
        for worker_id in 0..TRANSPORT_WORKERS {
            let worker_state = Arc::clone(&state);
            // fire-and-forget: these fixed scheduler workers live for the daemon
            // lifetime and isolate blocking adapter I/O without unbounded fanout.
            if let Err(error) = std::thread::Builder::new()
                .name(format!("agend-transport-worker-{worker_id}"))
                .spawn(move || transport_worker_loop(worker_state))
            {
                tracing::error!(
                    worker_id,
                    error = %error,
                    "delivery_worker: failed to spawn transport scheduler worker"
                );
            }
        }
        TransportScheduler { state }
    })
}

fn transport_worker_loop(state: Arc<TransportSchedulerState>) {
    loop {
        let (job, lane) = {
            let mut queue = state.queue.lock();
            loop {
                let mut selected = None;
                for (index, candidate) in queue.pending.iter().enumerate() {
                    let key = (candidate.home.clone(), candidate.agent.clone());
                    if queue.active.contains(&key) {
                        continue;
                    }
                    if let Some(lane) =
                        transport_lane(&candidate.home, &candidate.agent).try_lock_arc()
                    {
                        selected = Some((index, key, lane));
                        break;
                    }
                }
                if let Some((index, key, lane)) = selected {
                    let job = queue
                        .pending
                        .remove(index)
                        .expect("selected transport job must remain queued");
                    queue.active.insert(key);
                    break (job, lane);
                }
                state.wake.wait(&mut queue);
            }
        };

        let key = (job.home.clone(), job.agent.clone());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dispatch_transport(job, lane);
        }));
        if result.is_err() {
            #[cfg(test)]
            test_support::note_transport_dispatch_complete(&key.0, &key.1);
            tracing::error!(agent = %key.1, "delivery_worker: transport delivery panicked");
        }
        let mut queue = state.queue.lock();
        queue.active.remove(&key);
        state.wake.notify_all();
    }
}

fn transport_lane(home: &Path, agent: &str) -> Arc<parking_lot::Mutex<()>> {
    let key = (home.to_path_buf(), agent.to_string());
    let mut lanes = transport_coordinator().lanes.lock();
    Arc::clone(
        lanes
            .entry(key)
            .or_insert_with(|| Arc::new(parking_lot::Mutex::new(()))),
    )
}

fn transport_epoch(home: &Path, agent: &str) -> u64 {
    transport_epoch_state(home, agent).state.lock().epoch
}

/// Read the generation token while the caller owns the keyed transport lane.
/// Self-kick uses this immediately after its physical LegacyPty delivery and
/// before releasing that lane, so the verification arm belongs to the same
/// generation as the delivery.
pub(crate) fn current_transport_epoch(home: &Path, agent: &str) -> u64 {
    transport_epoch_state(home, agent)
        .admission_snapshot
        .load(Ordering::Acquire)
        & EPOCH_SNAPSHOT_MASK
}

fn transport_epoch_for_enqueue(home: &Path, agent: &str) -> Result<u64, TransportEnqueueError> {
    let state = transport_epoch_state(home, agent);
    // Admission must remain non-blocking even when another same-key operation
    // holds the state mutex for a short linearization section. Only the packed
    // lifecycle snapshot makes an enqueue Fenced; ordinary mutex contention is
    // not teardown and must not silently drop a healthy wake.
    let snapshot = state.admission_snapshot.load(Ordering::Acquire);
    if snapshot & CLEANUP_SNAPSHOT_BIT != 0 {
        return Err(TransportEnqueueError::Fenced);
    }
    Ok(snapshot & EPOCH_SNAPSHOT_MASK)
}

fn transport_epoch_state(home: &Path, agent: &str) -> Arc<TransportEpochEntry> {
    let key = (home.to_path_buf(), agent.to_string());
    let mut epochs = transport_coordinator().epochs.lock();
    Arc::clone(
        epochs
            .entry(key)
            .or_insert_with(|| Arc::new(TransportEpochEntry::default())),
    )
}

pub(crate) struct TransportGenerationGuard {
    _serial: TransportLaneGuard,
    _key: (PathBuf, String),
    state: Arc<TransportEpochEntry>,
    cleanup: bool,
}

struct TransportLaneGuard {
    guard: Option<parking_lot::ArcMutexGuard<parking_lot::RawMutex, ()>>,
}

impl TransportLaneGuard {
    fn acquire(home: &Path, agent: &str) -> Self {
        Self {
            guard: Some(transport_lane(home, agent).lock_arc()),
        }
    }

    fn release(&mut self) {
        if let Some(guard) = self.guard.take() {
            drop(guard);
            notify_transport_scheduler();
        }
    }
}

impl Drop for TransportLaneGuard {
    fn drop(&mut self) {
        self.release();
    }
}

fn notify_transport_scheduler() {
    if let Some(scheduler) = TRANSPORT_SCHEDULER.get() {
        // Synchronize the lane release with the queue scan. This closes the
        // lost-wakeup window between try_lock_arc() and condvar wait(): either
        // the worker sees the free lane, or it is waiting before this notify
        // runs while holding the same queue mutex.
        let queue = scheduler.state.queue.lock();
        scheduler.state.wake.notify_all();
        drop(queue);
    }
}

/// Execute synchronous transport delivery on the same serialized lane as the
/// background worker. Callers may perform registry validation before the
/// delivery, but must not hold the registry lock across the closure/I/O.
pub(crate) fn with_transport_serial<T>(home: &Path, agent: &str, f: impl FnOnce() -> T) -> T {
    let lane = TransportLaneGuard::acquire(home, agent);
    #[cfg(test)]
    test_support::run_direct_transport_admission_hook(home, agent);
    let state = transport_epoch_state(home, agent);
    let state_guard = state.state.lock();
    drop(state_guard);
    let result = f();
    drop(lane);
    result
}

/// Invalidate queued transport work and hold the transport lane through
/// teardown. The epoch bump at drop also invalidates jobs enqueued while the
/// deletion guard was held, so a late worker dequeue cannot recreate receipts.
pub(crate) fn begin_transport_generation_transition(
    home: &Path,
    agent: &str,
) -> TransportGenerationGuard {
    let serial = TransportLaneGuard::acquire(home, agent);
    let key = (home.to_path_buf(), agent.to_string());
    let state = transport_epoch_state(home, agent);
    {
        let mut state_guard = state.state.lock();
        state_guard.epoch = state_guard.epoch.saturating_add(1);
        state.publish_admission_snapshot(&state_guard);
    }
    TransportGenerationGuard {
        _serial: serial,
        _key: key,
        state,
        cleanup: false,
    }
}

pub(crate) fn begin_transport_cleanup(home: &Path, agent: &str) -> TransportGenerationGuard {
    // Fixed test seam: the marker must already exist when cleanup is about to
    // acquire this lane. Keep this boundary separate from mark_deleting so an
    // ordering mutation cannot move the observation with the marker.
    #[cfg(test)]
    test_support::run_cleanup_before_lane_acquire_hook(home, agent);
    let serial = TransportLaneGuard::acquire(home, agent);
    let key = (home.to_path_buf(), agent.to_string());
    let state = transport_epoch_state(home, agent);
    {
        let mut state_guard = state.state.lock();
        state_guard.cleanup_active = true;
        state_guard.epoch = state_guard.epoch.saturating_add(1);
        state.publish_admission_snapshot(&state_guard);
    }
    TransportGenerationGuard {
        _serial: serial,
        _key: key,
        state,
        cleanup: true,
    }
}

fn global() -> &'static DeliveryWorker {
    static WORKER: OnceLock<DeliveryWorker> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (tx, rx) = sync_channel::<DeliveryJob>(QUEUE_CAP);
        // fire-and-forget: the delivery worker drains for the whole daemon
        // lifetime; there is no graceful join — shutdown is best-effort by design
        // (AUDIT2-006). The OS reaps the thread at process exit, and any queued-
        // but-undrained job is explained by its synchronous record.
        if let Err(e) = std::thread::Builder::new()
            .name("agend-delivery".into())
            .spawn(move || worker_loop(rx))
        {
            // Spawn failure is exceptional (OS thread exhaustion). Without the
            // worker the queue would silently fill; log loudly so the operator
            // sees why deliveries stop. The `tx` is still returned, so callers get
            // `Err(())` on every enqueue (queue never drained) and record drops.
            tracing::error!(error = %e, "AUDIT2-006: failed to spawn delivery worker thread");
        }
        DeliveryWorker { tx }
    })
}

fn worker_loop(rx: Receiver<DeliveryJob>) {
    while let Ok(job) = rx.recv() {
        // Isolate each job: one panicking delivery must not kill the worker
        // (mirrors the event_bus #1745 per-subscriber panic isolation).
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(job)));
    }
}

fn dispatch(job: DeliveryJob) {
    match job {
        DeliveryJob::TelegramSend(job) => {
            crate::channel::telegram::notify::send_telegram_job(job);
        }
        DeliveryJob::TickStallAlert {
            home,
            host,
            phase,
            handler,
            generation,
        } => {
            // PR4: the worker (NOT the monitor sampler) owns the escalation +
            // event-log side effects, so the sampler never blocks on channel /
            // disk I/O while the tick host it watches is wedged.
            //
            // Observe the routed identity FIRST (tests only): the assertion is
            // "the correct TickStallAlert reached the worker via the real
            // monitor→enqueue path", which must not hinge on the downstream
            // escalation/event-log I/O succeeding in a bare test $HOME.
            #[cfg(test)]
            crate::daemon::tick_stall::test_probe::emit(&host, &handler);
            let msg = format!(
                "[tick-stall] {host} made no progress for the configured threshold \
                 while in phase '{phase}' (handler={handler}, generation={generation}). \
                 The tick thread is wedged — hang-detection, recovery-dispatch and \
                 crash handling on this host are stalled until it clears; \
                 investigate the named handler."
            );
            let dispatched = crate::channel::notify_all_escalation_channels(
                &host,
                crate::channel::NotifySeverity::Error,
                &msg,
                false,
            );
            crate::event_log::log(&home, "tick_stall", &host, &msg);
            tracing::error!(
                host = %host,
                phase = %phase,
                handler = %handler,
                generation,
                channels = dispatched,
                "tick_stall: tick host wedged — out-of-band page dispatched"
            );
        }
    }
}

fn dispatch_transport(
    job: TransportDelivery,
    _serial: parking_lot::ArcMutexGuard<parking_lot::RawMutex, ()>,
) {
    let TransportDelivery {
        home,
        agent,
        notification,
        epoch,
    } = job;
    if crate::agent::deleting::is_deleting(&home, &agent) || transport_epoch(&home, &agent) != epoch
    {
        #[cfg(test)]
        test_support::note_transport_dispatch_complete(&home, &agent);
        tracing::debug!(
            agent = %agent,
            "delivery_worker: discarded stale transport delivery during teardown"
        );
        return;
    }
    let result = crate::transport::deliver_notification(
        &home,
        &agent,
        &notification,
        |home, agent, text| {
            #[cfg(test)]
            test_support::note_legacy_wake(agent);
            crate::inbox::notify::inject_with_submit_direct(home, agent, text)
        },
    );
    if let Err(error) = result {
        tracing::debug!(
            agent = %agent,
            error = %error,
            "delivery_worker: structured transport delivery failed"
        );
    }
    #[cfg(test)]
    test_support::note_transport_dispatch_complete(&home, &agent);
}

/// Enqueue a complete backend transport delivery. The bounded queue is the
/// caller-facing non-blocking boundary; structured handshakes and protocol
/// waits happen only in [`dispatch_transport`] on a keyed transport scheduler worker.
pub(crate) fn enqueue_transport_delivery(
    home: &Path,
    agent: &str,
    notification: &str,
) -> Result<(), TransportEnqueueError> {
    enqueue_transport_delivery_with_epoch(home, agent, notification).map(|_| ())
}

pub(crate) fn enqueue_transport_delivery_with_epoch(
    home: &Path,
    agent: &str,
    notification: &str,
) -> Result<u64, TransportEnqueueError> {
    let epoch = transport_epoch_for_enqueue(home, agent)?;
    schedule_transport_delivery(TransportDelivery {
        home: home.to_path_buf(),
        agent: agent.to_string(),
        notification: notification.to_string(),
        epoch,
    })?;
    Ok(epoch)
}

/// Enqueue a redelivery only if the generation that admitted the original
/// wake is still current. The scheduler job retains that expected epoch too,
/// so a generation transition racing after this admission still discards it
/// before adapter I/O.
pub(crate) fn enqueue_transport_delivery_at_epoch(
    home: &Path,
    agent: &str,
    notification: &str,
    expected_epoch: u64,
) -> Result<u64, TransportEnqueueError> {
    let epoch = transport_epoch_for_enqueue(home, agent)?;
    if epoch != expected_epoch {
        return Err(TransportEnqueueError::Fenced);
    }
    schedule_transport_delivery(TransportDelivery {
        home: home.to_path_buf(),
        agent: agent.to_string(),
        notification: notification.to_string(),
        epoch: expected_epoch,
    })?;
    Ok(expected_epoch)
}

fn schedule_transport_delivery(job: TransportDelivery) -> Result<(), TransportEnqueueError> {
    #[cfg(test)]
    if test_support::force_full() {
        return Err(TransportEnqueueError::QueueFull { epoch: job.epoch });
    }
    let scheduler = transport_scheduler();
    let mut queue = scheduler.state.queue.lock();
    if queue.pending.len() >= QUEUE_CAP {
        tracing::warn!(
            cap = QUEUE_CAP,
            "AUDIT2-006: transport scheduler queue full — dropping a delivery job"
        );
        return Err(TransportEnqueueError::QueueFull { epoch: job.epoch });
    }
    queue.pending.push_back(job);
    scheduler.state.wake.notify_one();
    Ok(())
}

/// Serialize a queue-full durable failure with the captured transport epoch.
/// The per-key state lock is deliberately held through the receipt write: if a
/// delete or generation transition wins first, this suppresses the stale
/// receipt; if the write wins first, teardown removes it afterward. Unrelated
/// transport keys retain independent state locks.
pub(crate) fn record_queue_full_drop_if_current(
    home: &Path,
    agent: &str,
    notification: &str,
    epoch: u64,
    reason: &str,
) -> anyhow::Result<bool> {
    let state = transport_epoch_state(home, agent);
    let state_guard = state.state.lock();
    if state_guard.cleanup_active || state_guard.epoch != epoch {
        return Ok(false);
    }
    crate::transport::record_delivery_drop(home, agent, notification, reason)?;
    Ok(true)
}

/// Arm actionable delivery verification only while the accepted enqueue's
/// epoch is still current. Filesystem/configuration reads happen before the
/// per-key state mutex; only the epoch check and memory-only commit are inside
/// the linearization section, so a delayed caller cannot arm a stale wake
/// after teardown completes.
pub(crate) fn arm_transport_verification_if_current(
    home: &Path,
    agent: &str,
    epoch: u64,
    notification: &str,
) -> bool {
    let mode = crate::transport::mode_for_instance(home, agent);
    let Some(prepared) =
        crate::daemon::inject_delivery::prepare_arm(home, agent, notification, mode, epoch)
    else {
        return false;
    };
    let committed = {
        let state = transport_epoch_state(home, agent);
        let state_guard = state.state.lock();
        if state_guard.cleanup_active || state_guard.epoch != epoch {
            return false;
        }
        crate::daemon::inject_delivery::commit_prepared_arm(prepared, epoch)
    };
    if committed {
        crate::daemon::inject_delivery::notify_arm_committed(agent);
    }
    committed
}

impl Drop for TransportGenerationGuard {
    fn drop(&mut self) {
        let mut state_guard = self.state.state.lock();
        state_guard.epoch = state_guard.epoch.saturating_add(1);
        self.state.publish_admission_snapshot(&state_guard);
        crate::daemon::inject_delivery::forget(&self._key.1);
        self._serial.release();
        if self.cleanup {
            #[cfg(test)]
            test_support::run_cleanup_release_tail_hook(&self._key.0, &self._key.1);
            state_guard.cleanup_active = false;
            self.state.publish_admission_snapshot(&state_guard);
        }
    }
}

/// Offload a Telegram send whose dedup claim was already recorded on the caller
/// thread. Returns `Err(())` when the queue is full — the caller MUST evict the
/// dedup claim so a later identical emit isn't suppressed for the whole TTL.
pub(crate) fn enqueue_telegram_send(job: TelegramSendJob) -> Result<(), ()> {
    try_enqueue(DeliveryJob::TelegramSend(job))
}

/// PR4: offload an out-of-band tick-stall page. The stall monitor calls this; it
/// `try_send`s and NEVER blocks. A full queue is the *observable drop path* —
/// `Err(())` plus a `tracing::error` carrying host / phase / generation — because
/// a wedged tick host is exactly when the operator must not silently lose the
/// page. The monitor treats `Err` as "page dropped" and simply moves on.
pub(crate) fn enqueue_tick_stall_alert(
    home: &std::path::Path,
    host: &str,
    phase: &str,
    handler: &str,
    generation: u64,
) -> Result<(), ()> {
    let result = try_enqueue(DeliveryJob::TickStallAlert {
        home: home.to_path_buf(),
        host: host.to_string(),
        phase: phase.to_string(),
        handler: handler.to_string(),
        generation,
    });
    if result.is_err() {
        tracing::error!(
            host = %host,
            phase = %phase,
            handler = %handler,
            generation,
            "tick_stall alert DROPPED: delivery queue full — the tick host is \
             wedged and its out-of-band page was lost"
        );
    }
    result
}

fn try_enqueue(job: DeliveryJob) -> Result<(), ()> {
    #[cfg(test)]
    if test_support::force_full() {
        return Err(());
    }
    match global().tx.try_send(job) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => {
            tracing::warn!(
                cap = QUEUE_CAP,
                "AUDIT2-006: delivery queue full — dropping a delivery job (caller records its own drop status)"
            );
            Err(())
        }
        // The worker is daemon-lifetime; disconnection only happens at process
        // teardown. Treat as a drop.
        Err(TrySendError::Disconnected(_)) => Err(()),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::atomic::{AtomicBool, Ordering};

    pub(crate) type CleanupReleaseTailHook =
        std::sync::Arc<dyn Fn(&std::path::Path, &str) + Send + Sync>;
    pub(crate) type DirectTransportAdmissionHook =
        std::sync::Arc<dyn Fn(&std::path::Path, &str) + Send + Sync>;
    pub(crate) type CleanupBeforeLaneAcquireHook =
        std::sync::Arc<dyn Fn(&std::path::Path, &str) + Send + Sync>;
    pub(crate) type QueueFullBeforeRecordHook =
        std::sync::Arc<dyn Fn(&std::path::Path, &str, u64) + Send + Sync>;
    pub(crate) type TransportAcceptedBeforeArmHook =
        std::sync::Arc<dyn Fn(&std::path::Path, &str, u64) + Send + Sync>;

    static FORCE_FULL: AtomicBool = AtomicBool::new(false);
    static LEGACY_WAKE_COUNT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static FF_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    static CLEANUP_TAIL_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    static CLEANUP_RELEASE_TAIL_HOOK: std::sync::OnceLock<
        parking_lot::Mutex<Option<CleanupReleaseTailHook>>,
    > = std::sync::OnceLock::new();
    static DIRECT_ADMISSION_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    static DIRECT_ADMISSION_HOOK: std::sync::OnceLock<
        parking_lot::Mutex<Option<DirectTransportAdmissionHook>>,
    > = std::sync::OnceLock::new();
    static CLEANUP_BEFORE_LANE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    static CLEANUP_BEFORE_LANE_HOOK: std::sync::OnceLock<
        parking_lot::Mutex<Option<CleanupBeforeLaneAcquireHook>>,
    > = std::sync::OnceLock::new();
    static QUEUE_FULL_BEFORE_RECORD_HOOK: std::sync::OnceLock<
        parking_lot::Mutex<Option<QueueFullBeforeRecordHook>>,
    > = std::sync::OnceLock::new();
    static QUEUE_FULL_BEFORE_RECORD_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    static TRANSPORT_ACCEPTED_BEFORE_ARM_HOOK: std::sync::OnceLock<
        parking_lot::Mutex<Option<TransportAcceptedBeforeArmHook>>,
    > = std::sync::OnceLock::new();
    static TRANSPORT_ACCEPTED_BEFORE_ARM_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    static TRANSPORT_COMPLETIONS: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::HashMap<(std::path::PathBuf, String), usize>>,
    > = std::sync::OnceLock::new();

    fn transport_completions(
    ) -> &'static parking_lot::Mutex<std::collections::HashMap<(std::path::PathBuf, String), usize>>
    {
        TRANSPORT_COMPLETIONS
            .get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()))
    }

    /// Force every `try_enqueue` to behave as if the bounded queue were full,
    /// WITHOUT actually filling 256 slots — lets callers unit-test the drop /
    /// dedup-rollback paths deterministically.
    pub(crate) fn set_force_full(on: bool) {
        FORCE_FULL.store(on, Ordering::Relaxed);
    }

    /// Serialize tests that toggle [`set_force_full`]: `FORCE_FULL` is process-
    /// global, so parallel test threads would otherwise corrupt each other's
    /// expected queue state. Hold the returned guard across the whole
    /// toggle→assert→reset window. Every force-full test MUST hold it.
    pub(crate) fn force_full_guard() -> parking_lot::MutexGuard<'static, ()> {
        FF_LOCK.lock()
    }

    pub(crate) struct CleanupReleaseTailHookGuard {
        _lock: parking_lot::MutexGuard<'static, ()>,
    }

    pub(crate) fn cleanup_release_tail_hook_guard() -> CleanupReleaseTailHookGuard {
        let lock = CLEANUP_TAIL_LOCK.lock();
        set_cleanup_release_tail_hook(None);
        CleanupReleaseTailHookGuard { _lock: lock }
    }

    pub(crate) fn set_cleanup_release_tail_hook(hook: Option<CleanupReleaseTailHook>) {
        *CLEANUP_RELEASE_TAIL_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock() = hook;
    }

    pub(super) fn run_cleanup_release_tail_hook(home: &std::path::Path, agent: &str) {
        let hook = CLEANUP_RELEASE_TAIL_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock()
            .as_ref()
            .cloned();
        if let Some(hook) = hook {
            hook(home, agent);
        }
    }

    impl Drop for CleanupReleaseTailHookGuard {
        fn drop(&mut self) {
            set_cleanup_release_tail_hook(None);
        }
    }

    pub(crate) struct DirectTransportAdmissionHookGuard {
        _lock: parking_lot::MutexGuard<'static, ()>,
    }

    pub(crate) fn direct_transport_admission_hook_guard() -> DirectTransportAdmissionHookGuard {
        let lock = DIRECT_ADMISSION_LOCK.lock();
        set_direct_transport_admission_hook(None);
        DirectTransportAdmissionHookGuard { _lock: lock }
    }

    pub(crate) fn set_direct_transport_admission_hook(hook: Option<DirectTransportAdmissionHook>) {
        *DIRECT_ADMISSION_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock() = hook;
    }

    pub(super) fn run_direct_transport_admission_hook(home: &std::path::Path, agent: &str) {
        let hook = DIRECT_ADMISSION_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock()
            .as_ref()
            .cloned();
        if let Some(hook) = hook {
            hook(home, agent);
        }
    }

    pub(crate) fn set_transport_accepted_before_arm_hook(
        hook: Option<TransportAcceptedBeforeArmHook>,
    ) {
        *TRANSPORT_ACCEPTED_BEFORE_ARM_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock() = hook;
    }

    pub(crate) struct TransportAcceptedBeforeArmHookGuard {
        _lock: parking_lot::MutexGuard<'static, ()>,
    }

    pub(crate) fn transport_accepted_before_arm_hook_guard() -> TransportAcceptedBeforeArmHookGuard
    {
        let lock = TRANSPORT_ACCEPTED_BEFORE_ARM_LOCK.lock();
        set_transport_accepted_before_arm_hook(None);
        TransportAcceptedBeforeArmHookGuard { _lock: lock }
    }

    impl Drop for TransportAcceptedBeforeArmHookGuard {
        fn drop(&mut self) {
            set_transport_accepted_before_arm_hook(None);
        }
    }

    pub(crate) fn run_transport_accepted_before_arm_hook(
        home: &std::path::Path,
        agent: &str,
        epoch: u64,
    ) {
        let hook = TRANSPORT_ACCEPTED_BEFORE_ARM_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock()
            .as_ref()
            .cloned();
        if let Some(hook) = hook {
            hook(home, agent, epoch);
        }
    }

    impl Drop for DirectTransportAdmissionHookGuard {
        fn drop(&mut self) {
            set_direct_transport_admission_hook(None);
        }
    }

    struct CleanupBeforeLaneAcquireObserver {
        home: std::path::PathBuf,
        agent: String,
        entered_tx: std::sync::mpsc::Sender<()>,
        continue_rx: std::sync::mpsc::Receiver<()>,
    }

    std::thread_local! {
        static CLEANUP_BEFORE_LANE_OBSERVER:
            std::cell::RefCell<Option<CleanupBeforeLaneAcquireObserver>> =
            const { std::cell::RefCell::new(None) };
    }

    pub(crate) struct CleanupBeforeLaneAcquireObserverGuard;

    pub(crate) fn cleanup_before_lane_acquire_observer(
        home: &std::path::Path,
        agent: &str,
        entered_tx: std::sync::mpsc::Sender<()>,
        continue_rx: std::sync::mpsc::Receiver<()>,
    ) -> CleanupBeforeLaneAcquireObserverGuard {
        CLEANUP_BEFORE_LANE_OBSERVER.with(|slot| {
            assert!(
                slot.borrow().is_none(),
                "cleanup-before-lane observer already installed on this thread"
            );
            *slot.borrow_mut() = Some(CleanupBeforeLaneAcquireObserver {
                home: home.to_path_buf(),
                agent: agent.to_string(),
                entered_tx,
                continue_rx,
            });
        });
        CleanupBeforeLaneAcquireObserverGuard
    }

    pub(crate) fn notify_cleanup_before_lane_acquire(home: &std::path::Path, agent: &str) {
        let observer = CLEANUP_BEFORE_LANE_OBSERVER.with(|slot| {
            let matches = slot
                .borrow()
                .as_ref()
                .is_some_and(|observer| observer.home == home && observer.agent == agent);
            matches.then(|| slot.borrow_mut().take()).flatten()
        });
        if let Some(observer) = observer {
            observer
                .entered_tx
                .send(())
                .expect("cleanup-before-lane observer receiver");
            observer
                .continue_rx
                .recv()
                .expect("cleanup-before-lane observer release");
        }
    }

    impl Drop for CleanupBeforeLaneAcquireObserverGuard {
        fn drop(&mut self) {
            CLEANUP_BEFORE_LANE_OBSERVER.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }

    pub(crate) struct CleanupBeforeLaneAcquireHookGuard {
        _lock: parking_lot::MutexGuard<'static, ()>,
    }

    pub(crate) fn cleanup_before_lane_acquire_hook_guard() -> CleanupBeforeLaneAcquireHookGuard {
        let lock = CLEANUP_BEFORE_LANE_LOCK.lock();
        set_cleanup_before_lane_acquire_hook(None);
        CleanupBeforeLaneAcquireHookGuard { _lock: lock }
    }

    pub(crate) fn set_cleanup_before_lane_acquire_hook(hook: Option<CleanupBeforeLaneAcquireHook>) {
        *CLEANUP_BEFORE_LANE_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock() = hook;
    }

    pub(super) fn run_cleanup_before_lane_acquire_hook(home: &std::path::Path, agent: &str) {
        let hook = CLEANUP_BEFORE_LANE_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock()
            .as_ref()
            .cloned();
        if let Some(hook) = hook {
            hook(home, agent);
        }
    }

    impl Drop for CleanupBeforeLaneAcquireHookGuard {
        fn drop(&mut self) {
            set_cleanup_before_lane_acquire_hook(None);
        }
    }

    pub(crate) fn set_queue_full_before_record_hook(hook: Option<QueueFullBeforeRecordHook>) {
        *QUEUE_FULL_BEFORE_RECORD_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock() = hook;
    }

    pub(crate) struct QueueFullBeforeRecordHookGuard {
        _lock: parking_lot::MutexGuard<'static, ()>,
    }

    pub(crate) fn queue_full_before_record_hook_guard() -> QueueFullBeforeRecordHookGuard {
        let lock = QUEUE_FULL_BEFORE_RECORD_LOCK.lock();
        set_queue_full_before_record_hook(None);
        QueueFullBeforeRecordHookGuard { _lock: lock }
    }

    impl Drop for QueueFullBeforeRecordHookGuard {
        fn drop(&mut self) {
            set_queue_full_before_record_hook(None);
        }
    }

    pub(crate) fn run_queue_full_before_record_hook(
        home: &std::path::Path,
        agent: &str,
        epoch: u64,
    ) {
        let hook = QUEUE_FULL_BEFORE_RECORD_HOOK
            .get_or_init(|| parking_lot::Mutex::new(None))
            .lock()
            .as_ref()
            .cloned();
        if let Some(hook) = hook {
            hook(home, agent, epoch);
        }
    }

    pub(crate) fn note_legacy_wake(agent: &str) {
        if agent == "legacy-agent" {
            LEGACY_WAKE_COUNT.fetch_add(1, Ordering::SeqCst);
        }
    }

    pub(crate) fn reset_legacy_wake_count() {
        LEGACY_WAKE_COUNT.store(0, Ordering::SeqCst);
    }

    pub(crate) fn legacy_wake_count() -> usize {
        LEGACY_WAKE_COUNT.load(Ordering::SeqCst)
    }

    pub(crate) fn note_transport_dispatch_complete(home: &std::path::Path, agent: &str) {
        let key = (home.to_path_buf(), agent.to_string());
        let mut completions = transport_completions().lock();
        *completions.entry(key).or_default() += 1;
    }

    pub(crate) fn transport_dispatch_count(home: &std::path::Path, agent: &str) -> usize {
        let key = (home.to_path_buf(), agent.to_string());
        transport_completions()
            .lock()
            .get(&key)
            .copied()
            .unwrap_or(0)
    }

    pub(super) fn force_full() -> bool {
        FORCE_FULL.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport_test_receipt(
        name: &str,
        body: &str,
    ) -> anyhow::Result<crate::transport::DeliveryReceipt> {
        let envelope = crate::transport::DeliveryEnvelope::new(
            name,
            crate::transport::SessionLocator::codex(
                std::path::PathBuf::from("/tmp/delivery-worker-test.sock"),
                Some("delivery-worker-test-thread".to_string()),
            ),
            crate::transport::DeliveryKind::Notification,
            body,
            None,
        );
        Ok(crate::transport::DeliveryReceipt::for_state(
            &envelope,
            crate::transport::DeliveryState::ProtocolAccepted,
        ))
    }

    /// The transport scheduler admission path must return immediately and never
    /// block the caller (tick) thread; a full queue must surface as a drop
    /// (`Err`) rather than a stall.
    #[test]
    fn enqueue_is_nonblocking_and_drops_when_full() {
        let _ff = test_support::force_full_guard();
        // Healthy queue: a transport delivery enqueues without blocking.
        test_support::set_force_full(false);
        assert!(
            enqueue_transport_delivery(std::path::Path::new("/tmp/aw"), "agentA", "ping").is_ok(),
            "a non-full delivery queue must accept the wake without blocking"
        );

        // Full queue: the enqueue is dropped (Err) — the caller, not the worker,
        // owns recording the drop. No block, no panic.
        test_support::set_force_full(true);
        assert!(
            enqueue_transport_delivery(std::path::Path::new("/tmp/aw"), "agentA", "ping").is_err(),
            "AUDIT2-006: a full delivery queue must drop (Err), never block the tick thread"
        );
        test_support::set_force_full(false);
    }

    #[test]
    fn ordinary_state_mutex_contention_is_not_fenced_admission() {
        let _ff = test_support::force_full_guard();
        test_support::set_force_full(true);
        let home = std::env::temp_dir().join(format!(
            "agend-transport-worker-state-contention-{}",
            uuid::Uuid::new_v4()
        ));
        let agent = "state-contention-agent";
        let state = transport_epoch_state(&home, agent);
        let _state_guard = state.state.lock();
        assert!(matches!(
            enqueue_transport_delivery(&home, agent, "contention probe"),
            Err(TransportEnqueueError::QueueFull { .. })
        ));
        test_support::set_force_full(false);
        let _ = std::fs::remove_dir_all(home);
    }

    /// PR4 (#8): a full delivery queue drops the tick-stall page (`Err`) — the
    /// observable drop path (the enqueue also logs a `tracing::error` with
    /// host/phase/generation) — instead of blocking the monitor thread.
    #[test]
    fn tick_stall_alert_drops_when_queue_full() {
        let _ff = test_support::force_full_guard();
        let home = std::env::temp_dir();

        test_support::set_force_full(false);
        assert!(
            enqueue_tick_stall_alert(&home, "daemon-tick", "handler", "slow_handler", 7).is_ok(),
            "a non-full delivery queue accepts the tick-stall page"
        );

        test_support::set_force_full(true);
        assert!(
            enqueue_tick_stall_alert(&home, "daemon-tick", "handler", "slow_handler", 7).is_err(),
            "a full delivery queue must drop the page (Err), never block the monitor"
        );
        test_support::set_force_full(false);
    }

    /// A missing Codex endpoint must not make the caller wait for the
    /// structured adapter's readiness timeout; only the worker may perform it.
    #[test]
    fn transport_delivery_enqueue_is_nonblocking_for_unavailable_codex() {
        let _ff = test_support::force_full_guard();
        let home = std::env::temp_dir().join(format!(
            "agend-transport-worker-codex-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  codex-agent:\n    backend: codex\n",
        )
        .expect("fleet");
        let started = std::time::Instant::now();
        assert!(enqueue_transport_delivery(&home, "codex-agent", "ping").is_ok());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "transport enqueue must not run Codex readiness on the caller thread"
        );
        // The daemon-lifetime worker may still be consuming this job; keep the
        // unique home available until its best-effort read has finished.
    }

    /// Ordinary queued notifications must not consume a fixed transport worker
    /// for the fresh-spawn ChannelBridge readiness window. The one-shot
    /// fresh-restart self-kick owns that wait; background notifications fail
    /// fast and leave capacity for unrelated keys.
    #[test]
    fn unavailable_channel_bridge_notification_releases_worker_without_readiness_wait() {
        let _ff = test_support::force_full_guard();
        test_support::set_force_full(false);
        let home = std::env::temp_dir().join(format!(
            "agend-transport-worker-channel-unready-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  claude-agent:\n    backend: claude\n",
        )
        .expect("fleet");
        assert_eq!(
            crate::transport::mode_for_instance(&home, "claude-agent"),
            crate::transport::TransportMode::ChannelBridge
        );
        assert!(enqueue_transport_delivery(&home, "claude-agent", "ordinary wake").is_ok());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while test_support::transport_dispatch_count(&home, "claude-agent") == 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            test_support::transport_dispatch_count(&home, "claude-agent"),
            1,
            "ordinary ChannelBridge notification must fail fast instead of occupying a worker"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn legacy_transport_delivery_executes_one_physical_wake_on_worker() {
        let _ff = test_support::force_full_guard();
        test_support::set_force_full(false);
        test_support::reset_legacy_wake_count();
        let home = std::env::temp_dir().join(format!(
            "agend-transport-worker-legacy-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  legacy-agent:\n    backend: claude\n    env:\n      AGEND_TRANSPORT_MODE: legacy_pty\n",
        )
        .expect("fleet");
        schedule_transport_delivery(TransportDelivery {
            home: home.clone(),
            agent: "legacy-agent".to_string(),
            notification: "one logical wake".to_string(),
            epoch: transport_epoch(&home, "legacy-agent"),
        })
        .expect("transport scheduler accepts test delivery");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while test_support::legacy_wake_count() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            test_support::legacy_wake_count(),
            1,
            "LegacyPty must make one physical wake on the existing worker lane"
        );
        test_support::set_force_full(false);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn transport_scheduler_unrelated_keys_progress_while_one_key_blocks() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let _ff = test_support::force_full_guard();
        test_support::set_force_full(false);
        let _hook_guard = crate::transport::test_support::delivery_hook_guard();
        let home_a = std::env::temp_dir().join(format!(
            "agend-transport-worker-unrelated-a-{}",
            uuid::Uuid::new_v4()
        ));
        let home_b = std::env::temp_dir().join(format!(
            "agend-transport-worker-unrelated-b-{}",
            uuid::Uuid::new_v4()
        ));
        for home in [&home_a, &home_b] {
            std::fs::create_dir_all(home).expect("home");
            std::fs::write(
                crate::fleet::fleet_yaml_path(home),
                "instances:\n  legacy-agent:\n    backend: claude\n    env:\n      AGEND_TRANSPORT_MODE: legacy_pty\n",
            )
            .expect("fleet");
        }

        let entered_a = Arc::new(AtomicBool::new(false));
        let release_a = Arc::new(AtomicBool::new(false));
        let completed_b = Arc::new(AtomicBool::new(false));
        let entered_a_hook = Arc::clone(&entered_a);
        let release_a_hook = Arc::clone(&release_a);
        let completed_b_hook = Arc::clone(&completed_b);
        let expected_a = home_a.clone();
        let expected_b = home_b.clone();
        crate::transport::test_support::set_delivery_hook(Some(Arc::new(
            move |home, name, body| {
                if home == expected_a.as_path() {
                    entered_a_hook.store(true, Ordering::SeqCst);
                    while !release_a_hook.load(Ordering::SeqCst) {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    return Some(transport_test_receipt(name, body));
                }
                if home == expected_b.as_path() {
                    completed_b_hook.store(true, Ordering::SeqCst);
                    return Some(transport_test_receipt(name, body));
                }
                None
            },
        )));
        assert!(enqueue_transport_delivery(&home_a, "legacy-agent", "blocked").is_ok());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !entered_a.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            entered_a.load(Ordering::SeqCst),
            "first key must enter its adapter"
        );

        assert!(enqueue_transport_delivery(&home_b, "legacy-agent", "unrelated").is_ok());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !completed_b.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let unrelated_completed_before_release = completed_b.load(Ordering::SeqCst);
        release_a.store(true, Ordering::SeqCst);
        crate::transport::test_support::set_delivery_hook(None);
        assert!(
            unrelated_completed_before_release,
            "a blocked transport key must not stop an unrelated key"
        );
        let _ = std::fs::remove_dir_all(home_a);
        let _ = std::fs::remove_dir_all(home_b);
    }

    #[test]
    fn transport_scheduler_same_key_jobs_are_excluded_and_fifo() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let _ff = test_support::force_full_guard();
        test_support::set_force_full(false);
        let _hook_guard = crate::transport::test_support::delivery_hook_guard();
        let home = std::env::temp_dir().join(format!(
            "agend-transport-worker-same-key-{}",
            uuid::Uuid::new_v4()
        ));
        let agent = "legacy-agent";
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  legacy-agent:\n    backend: claude\n    env:\n      AGEND_TRANSPORT_MODE: legacy_pty\n",
        )
        .expect("fleet");
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let entered_hook = Arc::clone(&entered);
        let release_hook = Arc::clone(&release);
        let calls_hook = Arc::clone(&calls);
        let bodies_hook = Arc::clone(&bodies);
        let active_hook = Arc::clone(&active);
        let max_active_hook = Arc::clone(&max_active);
        let expected_home = home.clone();
        crate::transport::test_support::set_delivery_hook(Some(Arc::new(
            move |called_home, name, body| {
                if called_home != expected_home.as_path() {
                    return None;
                }
                calls_hook.fetch_add(1, Ordering::SeqCst);
                bodies_hook.lock().push(body.to_string());
                let now = active_hook.fetch_add(1, Ordering::SeqCst) + 1;
                max_active_hook.fetch_max(now, Ordering::SeqCst);
                if now == 1 {
                    entered_hook.store(true, Ordering::SeqCst);
                    while !release_hook.load(Ordering::SeqCst) {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                }
                active_hook.fetch_sub(1, Ordering::SeqCst);
                Some(transport_test_receipt(name, body))
            },
        )));

        assert!(enqueue_transport_delivery(&home, agent, "first").is_ok());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !entered.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            entered.load(Ordering::SeqCst),
            "first same-key job must enter"
        );
        assert!(enqueue_transport_delivery(&home, agent, "second").is_ok());
        std::thread::sleep(std::time::Duration::from_millis(100));
        let calls_before_release = calls.load(Ordering::SeqCst);
        let max_before_release = max_active.load(Ordering::SeqCst);
        release.store(true, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while test_support::transport_dispatch_count(&home, agent) < 2
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let dispatch_count = test_support::transport_dispatch_count(&home, agent);
        crate::transport::test_support::set_delivery_hook(None);
        assert_eq!(calls_before_release, 1, "same-key jobs must remain FIFO");
        assert_eq!(max_before_release, 1, "same-key jobs must not overlap");
        assert_eq!(
            dispatch_count, 2,
            "both same-key jobs must complete exactly once"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(bodies.lock().as_slice(), ["first", "second"]);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn transport_scheduler_wakes_after_direct_serial_lane_release() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let _ff = test_support::force_full_guard();
        test_support::set_force_full(false);
        let _hook_guard = crate::transport::test_support::delivery_hook_guard();
        let home = std::env::temp_dir().join(format!(
            "agend-transport-worker-direct-release-{}",
            uuid::Uuid::new_v4()
        ));
        let agent = "legacy-agent";
        std::fs::create_dir_all(&home).expect("home");
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let entered_direct = Arc::clone(&entered);
        let release_direct = Arc::clone(&release);
        let direct_home = home.clone();
        let direct = std::thread::spawn(move || {
            with_transport_serial(&direct_home, agent, || {
                entered_direct.store(true, Ordering::SeqCst);
                while !release_direct.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            });
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !entered.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let calls_hook = Arc::clone(&calls);
        let expected_home = home.clone();
        crate::transport::test_support::set_delivery_hook(Some(Arc::new(
            move |called_home, name, body| {
                if called_home != expected_home.as_path() {
                    return None;
                }
                calls_hook.fetch_add(1, Ordering::SeqCst);
                Some(transport_test_receipt(name, body))
            },
        )));
        assert!(enqueue_transport_delivery(&home, agent, "direct release").is_ok());
        std::thread::sleep(std::time::Duration::from_millis(50));
        let blocked_before_release = calls.load(Ordering::SeqCst) == 0;
        release.store(true, Ordering::SeqCst);
        direct.join().expect("direct transport lane thread");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while calls.load(Ordering::SeqCst) < 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        crate::transport::test_support::set_delivery_hook(None);
        assert!(entered.load(Ordering::SeqCst));
        assert!(
            blocked_before_release,
            "direct serial lane must exclude queued work"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn stale_transport_delivery_cannot_recreate_receipts_after_cleanup() {
        let _ff = test_support::force_full_guard();
        let home = std::env::temp_dir().join(format!(
            "agend-transport-worker-teardown-{}",
            uuid::Uuid::new_v4()
        ));
        let agent = "legacy-agent";
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  legacy-agent:\n    backend: claude\n    env:\n      AGEND_TRANSPORT_MODE: legacy_pty\n",
        )
        .expect("fleet");

        let stale_epoch = transport_epoch(&home, agent);
        let cleanup = begin_transport_cleanup(&home, agent);
        assert!(schedule_transport_delivery(TransportDelivery {
            home: home.clone(),
            agent: agent.to_string(),
            notification: "stale teardown wake".to_string(),
            epoch: stale_epoch,
        })
        .is_ok());
        drop(cleanup);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while test_support::transport_dispatch_count(&home, agent) == 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            test_support::transport_dispatch_count(&home, agent),
            1,
            "the queued stale delivery must be processed by the worker before inspecting state"
        );

        let delivery_path = crate::transport::delivery_path_for_instance(&home, agent);
        assert!(
            !delivery_path.exists(),
            "teardown must prevent stale worker delivery from recreating the receipt body"
        );
        assert!(
            !delivery_path.with_extension("jsonl.lock").exists(),
            "teardown must prevent stale worker delivery from recreating the receipt lock"
        );
        assert!(
            !delivery_path.parent().is_some_and(std::path::Path::exists),
            "teardown must leave no delivery directory after a stale worker delivery"
        );

        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn delivery_enqueued_during_teardown_cannot_resurrect_receipts() {
        let _ff = test_support::force_full_guard();
        let home = std::env::temp_dir().join(format!(
            "agend-transport-worker-teardown-window-{}",
            uuid::Uuid::new_v4()
        ));
        let agent = "legacy-agent";
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  legacy-agent:\n    backend: claude\n    env:\n      AGEND_TRANSPORT_MODE: legacy_pty\n",
        )
        .expect("fleet");

        let cleanup = begin_transport_cleanup(&home, agent);
        assert!(enqueue_transport_delivery(&home, agent, "wake during teardown").is_err());
        crate::transport::remove_instance_delivery_state(&home, agent).expect("cleanup");
        drop(cleanup);

        let delivery_path = crate::transport::delivery_path_for_instance(&home, agent);
        assert!(
            !delivery_path.exists(),
            "a delivery enqueued during teardown must not resurrect the receipt body"
        );
        assert!(
            !delivery_path.with_extension("jsonl.lock").exists(),
            "a delivery enqueued during teardown must not resurrect the receipt lock"
        );
        assert!(
            !delivery_path.parent().is_some_and(std::path::Path::exists),
            "a delivery enqueued during teardown must leave no delivery directory"
        );

        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn cleanup_rejects_enqueue_during_final_release_tail() {
        let _ff = test_support::force_full_guard();
        test_support::set_force_full(false);
        let _tail_guard = test_support::cleanup_release_tail_hook_guard();
        let home = std::env::temp_dir().join(format!(
            "agend-transport-worker-cleanup-tail-{}",
            uuid::Uuid::new_v4()
        ));
        let agent = "legacy-agent";
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  legacy-agent:\n    backend: claude\n    env:\n      AGEND_TRANSPORT_MODE: legacy_pty\n",
        )
        .expect("fleet");

        let stale_epoch = transport_epoch(&home, agent);
        assert!(schedule_transport_delivery(TransportDelivery {
            home: home.clone(),
            agent: agent.to_string(),
            notification: "stale cleanup tail wake".to_string(),
            epoch: stale_epoch,
        })
        .is_ok());
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        test_support::set_cleanup_release_tail_hook(Some(Arc::new(
            move |hook_home, hook_agent| {
                result_tx
                    .send(enqueue_transport_delivery(
                        hook_home,
                        hook_agent,
                        "late cleanup tail wake",
                    ))
                    .expect("cleanup tail result receiver");
            },
        )));

        let cleanup = begin_transport_cleanup(&home, agent);
        crate::transport::remove_instance_delivery_state(&home, agent).expect("cleanup");
        drop(cleanup);

        assert_eq!(
            result_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("cleanup tail hook must run"),
            Err(TransportEnqueueError::Fenced),
            "an enqueue in the final bump/release tail must be rejected"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while test_support::transport_dispatch_count(&home, agent) == 0
            && std::time::Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        assert_eq!(
            test_support::transport_dispatch_count(&home, agent),
            1,
            "the pre-cleanup stale delivery must still be observed and discarded"
        );
        let delivery_path = crate::transport::delivery_path_for_instance(&home, agent);
        assert!(!delivery_path.exists());
        assert!(!delivery_path.with_extension("jsonl.lock").exists());
        assert!(!delivery_path.parent().is_some_and(std::path::Path::exists));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn cleanup_tail_is_keyed_and_unrelated_enqueue_completes() {
        let _ff = test_support::force_full_guard();
        test_support::set_force_full(false);
        let _hook_guard = crate::transport::test_support::delivery_hook_guard();
        let _tail_guard = test_support::cleanup_release_tail_hook_guard();
        let home_a = std::env::temp_dir().join(format!(
            "agend-transport-worker-cleanup-tail-a-{}",
            uuid::Uuid::new_v4()
        ));
        let home_b = std::env::temp_dir().join(format!(
            "agend-transport-worker-cleanup-tail-b-{}",
            uuid::Uuid::new_v4()
        ));
        let agent_a = "agent-a";
        let agent_b = "agent-b";
        for (home, agent) in [(&home_a, agent_a), (&home_b, agent_b)] {
            std::fs::create_dir_all(home).expect("home");
            std::fs::write(
                crate::fleet::fleet_yaml_path(home),
                format!(
                    "instances:\n  {agent}:\n    backend: claude\n    env:\n      AGEND_TRANSPORT_MODE: legacy_pty\n"
                ),
            )
            .expect("fleet");
        }
        let (tail_tx, tail_rx) = std::sync::mpsc::channel();
        let (b_done_tx, b_done_rx) = std::sync::mpsc::channel();
        let b_done_rx = Arc::new(parking_lot::Mutex::new(b_done_rx));
        let home_b_for_tail = home_b.clone();
        let expected_b = home_b.clone();
        crate::transport::test_support::set_delivery_hook(Some(Arc::new(
            move |called_home, _name, _body| {
                if called_home != expected_b.as_path() {
                    return None;
                }
                b_done_tx.send(()).expect("B adapter result receiver");
                Some(Err(anyhow::anyhow!("keyed tail probe")))
            },
        )));
        let expected_a = home_a.clone();
        let b_done_rx_for_tail = Arc::clone(&b_done_rx);
        crate::daemon::delivery_worker::test_support::set_cleanup_release_tail_hook(Some(
            Arc::new(move |hook_home, hook_agent| {
                if hook_home != expected_a.as_path() || hook_agent != agent_a {
                    return;
                }
                let a_result =
                    enqueue_transport_delivery(hook_home, hook_agent, "late A cleanup tail wake");
                let b_result =
                    enqueue_transport_delivery(&home_b_for_tail, agent_b, "B cleanup tail wake");
                tail_tx
                    .send((a_result, b_result))
                    .expect("tail result receiver");
                b_done_rx_for_tail
                    .lock()
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .expect("unrelated B delivery must complete while A finalizes");
            }),
        ));

        let cleanup = begin_transport_cleanup(&home_a, agent_a);
        crate::transport::remove_instance_delivery_state(&home_a, agent_a).expect("A cleanup");
        drop(cleanup);
        let (a_result, b_result) = tail_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("A cleanup tail hook must run");
        assert_eq!(
            a_result,
            Err(TransportEnqueueError::Fenced),
            "A remains stale through finalization"
        );
        assert_eq!(b_result, Ok(()), "B enqueue must not see A's state lock");
        assert!(
            !crate::transport::delivery_path_for_instance(&home_a, agent_a).exists(),
            "A cleanup must not resurrect receipts"
        );
        let _ = std::fs::remove_dir_all(home_a);
        let _ = std::fs::remove_dir_all(home_b);
    }

    #[test]
    fn generation_transition_invalidates_queued_epochs_before_and_during_spawn() {
        let _ff = test_support::force_full_guard();
        let home = std::env::temp_dir().join(format!(
            "agend-transport-worker-generation-transition-{}",
            uuid::Uuid::new_v4()
        ));
        let agent = "legacy-agent";
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  legacy-agent:\n    backend: claude\n    env:\n      AGEND_TRANSPORT_MODE: legacy_pty\n",
        )
        .expect("fleet");

        let before_epoch = transport_epoch(&home, agent);
        let transition = begin_transport_generation_transition(&home, agent);
        let during_epoch = transport_epoch(&home, agent);
        assert!(schedule_transport_delivery(TransportDelivery {
            home: home.clone(),
            agent: agent.to_string(),
            notification: "queued before spawn transition".to_string(),
            epoch: before_epoch,
        })
        .is_ok());
        assert!(schedule_transport_delivery(TransportDelivery {
            home: home.clone(),
            agent: agent.to_string(),
            notification: "queued during spawn transition".to_string(),
            epoch: during_epoch,
        })
        .is_ok());
        drop(transition);
        let after_epoch = transport_epoch(&home, agent);
        assert_ne!(before_epoch, during_epoch);
        assert_ne!(during_epoch, after_epoch);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while test_support::transport_dispatch_count(&home, agent) < 2
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            test_support::transport_dispatch_count(&home, agent),
            2,
            "both stale queued deliveries must be observed and discarded"
        );
        let delivery_path = crate::transport::delivery_path_for_instance(&home, agent);
        assert!(
            !delivery_path.exists(),
            "stale epochs must not recreate receipts"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn generation_transition_final_bump_forgets_pending_verification() {
        let home = std::env::temp_dir().join(format!(
            "agend-transport-worker-generation-verification-{}",
            uuid::Uuid::new_v4()
        ));
        let agent = "generation-verification-agent";
        std::fs::create_dir_all(&home).expect("home");
        crate::daemon::hook_shadow::record_event(agent, "UserPromptSubmit", None);
        crate::daemon::inject_delivery::arm(agent, "stale generation wake");
        assert!(crate::daemon::inject_delivery::is_armed_for_test(agent));

        let transition = begin_transport_generation_transition(&home, agent);
        drop(transition);
        assert!(
            !crate::daemon::inject_delivery::is_armed_for_test(agent),
            "generation finalization must clear stale verification"
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn generation_transition_lane_rejects_reentrant_transport_entry() {
        let home = std::env::temp_dir().join(format!(
            "agend-transport-worker-generation-reentry-{}",
            uuid::Uuid::new_v4()
        ));
        let transition = begin_transport_generation_transition(&home, "agent");
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            with_transport_serial(&home, "agent", || {
                tx.send(()).expect("reentry probe receiver")
            });
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(20))
                .is_err(),
            "spawn preparation must not re-enter the held transport lane"
        );
        drop(transition);
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(1)).is_ok(),
            "the lane must become available after generation transition"
        );
        worker.join().expect("reentry probe thread");
    }
}
