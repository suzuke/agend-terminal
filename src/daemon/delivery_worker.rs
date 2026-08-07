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
//! structured backend delivery, and Telegram send) onto a single bounded
//! background worker. Durable source-of-truth writes (inbox JSONL,
//! `notification_queue`, schedule `run_history`) stay SYNCHRONOUS on the caller
//! — a notification is a wakeup, not a commit barrier. `event_bus::emit`'s
//! handled-count is unaffected: it is decided by the synchronous kind-match
//! inside each subscriber, BEFORE any delivery is enqueued (see `event_bus.rs`).
//!
//! Backpressure: a bounded `sync_channel(QUEUE_CAP)`; transport and Telegram
//! enqueue functions use `try_send` and NEVER block. On a full queue the
//! job is dropped and the caller is told (`Err(())`) so it can record the drop
//! where it owns a durable status (cron → `drop_queue_full`; Telegram → evict the
//! dedup claim so a later identical emit isn't suppressed for the whole TTL).
//!
//! Shutdown is best-effort by design: the worker is a daemon-lifetime thread
//! reaped by the OS at process exit. There is no graceful join (Rust std threads
//! can't be safely cancelled mid-`block_on`); a queued-but-undrained job is
//! explained by its synchronous record (e.g. cron's `ok_queued`).
//!
//! Single worker (not a pool) on purpose: FIFO keeps Telegram retry / topic-
//! recreation / dedup and per-agent inject ordering trivially serial. If head-of-
//! line latency becomes real after the Telegram request timeout lands, split into
//! lanes (one Telegram, one PTY) later — not now.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Mutex, OnceLock};

/// Queue depth. Large enough to absorb a burst of watchdog / cron / crash
/// notifications, small enough that a wedged delivery path (post request-timeout)
/// surfaces as drops quickly rather than unbounded memory growth.
const QUEUE_CAP: usize = 256;

/// #3175: resume-attempt budget for a typed-inject readback-abort. Each attempt
/// is paced by the flush's busy-hold (fresh defer timestamp ⇒ released at the
/// MAX_DEFER cap, ~7s ambient while busy) and costs only a short readback poll
/// (500ms) — the payload is NEVER re-written. Covers multi-minute generations;
/// past the budget a persistently-unsettled pane gives up loudly (WARN +
/// event-log) and the agent is QUARANTINED (see [`QUARANTINED`]) instead of
/// retrying forever — held wakes stay blocked until a provable recovery.
const MAX_RESUME_ATTEMPTS: u32 = 30;

/// (home, agent) → held fresh wakes awaiting release (see [`PENDING_UNCONFIRMED`]).
type HeldWakeMap =
    HashMap<(PathBuf, String), VecDeque<crate::notification_queue::QueuedNotification>>;

/// #3176: per-agent serialization across the typed-inject unconfirmed state.
///
/// The failure contract of `inject_unconfirmed` is "the payload WAS written
/// (renders as a draft once the pane settles) but the submit key was withheld"
/// — so after an unconfirmed abort, the agent's input box holds an UNSENT draft
/// of that wake. If a later fresh wake for the SAME agent were physically typed
/// before the draft reaches a terminal resume outcome, the two texts would
/// combine in one input box and could be submitted together — corrupting both
/// notifications.
///
/// This map enforces the fix: PRESENCE of a `(home, agent)` key means an
/// unconfirmed wake for that agent is pending (its resume has not reached a
/// TERMINAL outcome). While present, fresh (`resume: false`) wakes for
/// that agent are HELD (never physically written) in the value's FIFO
/// `VecDeque`; the pending wake's own resume item (and nothing else) is allowed
/// to write submit-only.
///
/// Only a resume that returns `Ok` (the draft was provably submitted/cleared)
/// is a safe release: [`release_held`] then takes the FIFO and releases each
/// held wake IN ORDER — a released wake that itself aborts becomes the new
/// pending one. Because a resume `Ok` is the ONLY proof the input box is clean,
/// the terminal-but-unsettled outcomes (resume budget give-up, or an `Other`
/// write failure on a resume) do NOT release: the agent is recorded in
/// [`QUARANTINED`] and its held wakes stay blocked until an equivalent recovery
/// (agent/pane lifecycle invalidation, or an explicit operator recovery)
/// clears it — see [`lift_quarantine`]. Never write a fresh wake on top of a
/// stranded draft.
///
/// Held items live only in memory by design (AUDIT2-006 best-effort): the
/// pending window is bounded by the resume budget (~30 attempts, each paced by
/// the flush's MAX_DEFER caps), and a queued-but-undrained job is already
/// explained by its synchronous record. While quarantined, held items likewise
/// stay in-memory until lifecycle recovery; the queue itself remains bounded by
/// `QUEUE_CAP`.
static PENDING_UNCONFIRMED: OnceLock<Mutex<HeldWakeMap>> = OnceLock::new();

fn pending_unconfirmed_map() -> &'static Mutex<HeldWakeMap> {
    PENDING_UNCONFIRMED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// (home, agent) → agent is QUARANTINED: an unconfirmed draft may still sit in
/// the input box AND no resume will resolve it (the resume budget gave up, or a
/// resume-drain `Other` write failure occurred). Later same-agent wakes MUST NOT
/// be physically written until the draft is provably cleared (a later resume
/// `Ok`), the pane/agent lifecycle invalidates the draft (agent restart/removal),
/// or an explicit operator recovery resolves it.
static QUARANTINED: OnceLock<Mutex<HashSet<(PathBuf, String)>>> = OnceLock::new();

fn quarantined_set() -> &'static Mutex<HashSet<(PathBuf, String)>> {
    QUARANTINED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn pending_key(home: &Path, agent: &str) -> (PathBuf, String) {
    (home.to_path_buf(), agent.to_string())
}

/// #3176: is an unconfirmed wake for this agent still awaiting its terminal
/// resume outcome? If so, fresh wakes must be held, never physically written.
fn has_pending_unconfirmed(home: &Path, agent: &str) -> bool {
    pending_unconfirmed_map()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains_key(&pending_key(home, agent))
}

/// #3176: is this agent QUARANTINED (unproven stranded draft + no resume in
/// flight)? Same write-blocking effect as [`has_pending_unconfirmed`].
fn is_quarantined(home: &Path, agent: &str) -> bool {
    quarantined_set()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains(&pending_key(home, agent))
}

/// #3176: record that an unconfirmed wake for this agent is awaiting resume.
/// Idempotent — the first unconfirmed of a cycle creates the entry, a re-
/// unconfirmed resume keeps it.
fn set_pending_unconfirmed(home: &Path, agent: &str) {
    pending_unconfirmed_map()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(pending_key(home, agent))
        .or_default();
}

/// #3176: quarantine this agent — its unproven stranded draft must block later
/// same-agent wakes until a provable recovery (resume `Ok`, pane/agent lifecycle
/// invalidation, or operator recovery) lifts it. Idempotent.
fn set_quarantined(home: &Path, agent: &str) {
    quarantined_set()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(pending_key(home, agent));
}

/// #3176: clear the quarantine marker once the stranded draft is provably
/// resolved (a resume reached `Ok`) or intentionally invalidated (pane/agent
/// lifecycle, operator recovery). Does NOT touch the held-wakes FIFO.
fn clear_quarantine(home: &Path, agent: &str) {
    quarantined_set()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&pending_key(home, agent));
}

/// #3176: permanently lift quarantine for an agent when the stranded draft has
/// been invalidated OUTSIDE the unconfirmed-resume cycle — the pane/agent
/// lifecycle tore it down (agent restarted or removed, so the old input box is
/// gone) or an explicit operator recovery replaced/cleared it. The held wakes
/// are re-released IN ORDER by re-enqueueing them as FRESH deliveries on the
/// worker thread (never a physical write from the caller's thread — the worker
/// is the single writer). Any wake the worker later flushes that aborts again
/// cycles through the normal pending/unconfirmed recovery.
pub(crate) fn lift_quarantine(home: &Path, agent: &str) {
    lift_quarantine_with(home, agent, &mut |h, a, n| {
        enqueue_transport_delivery(h, a, n.clone()).map_err(|()| {
            crate::inbox::notify::InjectDeliveryFailure::Other("delivery queue full".to_string())
        })
    });
}

/// #3176: core of [`lift_quarantine`], injectable for tests. Clears the
/// quarantine marker and re-hands each held wake to `inject` (in FIFO order);
/// a failed re-enqueue stops the lift (remaining wakes are dropped loud, the
/// quarantine is already lifted so fresh wakes may write again).
fn lift_quarantine_with<F>(home: &Path, agent: &str, inject: &mut F)
where
    F: FnMut(
        &Path,
        &str,
        &crate::notification_queue::QueuedNotification,
    ) -> Result<(), crate::inbox::notify::InjectDeliveryFailure>,
{
    clear_quarantine(home, agent);
    let Some(held) = take_held(home, agent) else {
        return;
    };
    tracing::info!(
        agent = %agent,
        tag = "#3176-lift-quarantine",
        held = held.len(),
        "quarantine lifted — held wakes re-enqueued"
    );
    for n in held {
        if inject(home, agent, &n).is_err() {
            tracing::warn!(
                agent = %agent,
                tag = "#3176-lift-quarantine",
                "held wake re-enqueue failed during quarantine lift — dropped"
            );
            break;
        }
    }
}

/// #3176: hold a fresh wake while the agent's pending unconfirmed resume has
/// not reached a terminal outcome. Order is preserved (FIFO) so held wakes
/// release in arrival order.
fn hold_item(
    home: &Path,
    agent: &str,
    notification: crate::notification_queue::QueuedNotification,
) {
    tracing::debug!(
        agent = %agent,
        tag = "#3176-hold",
        "fresh wake HELD — same-agent typed inject still unconfirmed (resume pending)"
    );
    pending_unconfirmed_map()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(pending_key(home, agent))
        .or_default()
        .push_back(notification);
}

/// #3176: take the held-wakes FIFO for an agent and clear its pending marker.
/// Returns `None` when no pending unconfirmed exists. Called only at a terminal
/// resume outcome — afterwards fresh wakes may write again.
fn take_held(
    home: &Path,
    agent: &str,
) -> Option<VecDeque<crate::notification_queue::QueuedNotification>> {
    pending_unconfirmed_map()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&pending_key(home, agent))
}

/// A unit of blocking delivery work offloaded off the tick / main-loop thread.
enum DeliveryJob {
    /// A selected transport delivery. Structured backends (Codex/OpenCode)
    /// deliver through their transport seam; LegacyPty performs its physical
    /// wake directly from this worker — it never enqueues a second physical
    /// wake job. #3175: carries the WHOLE `QueuedNotification` (resume flag +
    /// attempt budget) so a typed-inject readback-abort can be requeued as a
    /// RESUME item for the same payload.
    TransportDelivery {
        home: PathBuf,
        agent: String,
        notification: crate::notification_queue::QueuedNotification,
        epoch: u64,
    },
    /// A Telegram send whose dedup claim was already recorded on the caller
    /// thread (see `channel::telegram::notify`). On terminal send failure the
    /// worker evicts that claim.
    TelegramSend(TelegramSendJob),
    /// AUDIT2-006 C: a cron physical PTY inject. The prepare/gate phase (marker +
    /// #1513 defer) already ran synchronously on the tick thread; the worker does
    /// ONLY the physical write via the CAPTURED `InjectTarget`. It NEVER re-resolves
    /// `agent` (a same-name redeploy must not receive a stale fire — `agent` is for
    /// logging only).
    CronInject {
        target: crate::agent::InjectTarget,
        agent: String,
        text: Vec<u8>,
    },
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

struct TransportCoordinator {
    serial: parking_lot::Mutex<()>,
    epochs: parking_lot::Mutex<HashMap<(PathBuf, String), u64>>,
}

fn transport_coordinator() -> &'static TransportCoordinator {
    static COORDINATOR: OnceLock<TransportCoordinator> = OnceLock::new();
    COORDINATOR.get_or_init(|| TransportCoordinator {
        serial: parking_lot::Mutex::new(()),
        epochs: parking_lot::Mutex::new(HashMap::new()),
    })
}

fn transport_epoch(home: &Path, agent: &str) -> u64 {
    let key = (home.to_path_buf(), agent.to_string());
    let mut epochs = transport_coordinator().epochs.lock();
    *epochs.entry(key).or_insert(0)
}

fn bump_transport_epoch(home: &Path, agent: &str) {
    let key = (home.to_path_buf(), agent.to_string());
    let mut epochs = transport_coordinator().epochs.lock();
    let epoch = epochs.entry(key).or_insert(0);
    *epoch = epoch.saturating_add(1);
}

pub(crate) struct TransportCleanupGuard {
    _serial: parking_lot::MutexGuard<'static, ()>,
    key: (PathBuf, String),
}

/// Invalidate queued transport work and hold the transport lane through
/// teardown. The epoch bump at drop also invalidates jobs enqueued while the
/// deletion guard was held, so a late worker dequeue cannot recreate receipts.
pub(crate) fn begin_transport_cleanup(home: &Path, agent: &str) -> TransportCleanupGuard {
    let coordinator = transport_coordinator();
    let serial = coordinator.serial.lock();
    bump_transport_epoch(home, agent);
    TransportCleanupGuard {
        _serial: serial,
        key: (home.to_path_buf(), agent.to_string()),
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
        DeliveryJob::TransportDelivery {
            home,
            agent,
            notification,
            epoch,
        } => {
            let _serial = transport_coordinator().serial.lock();
            if crate::agent::deleting::is_deleting(&home, &agent)
                || transport_epoch(&home, &agent) != epoch
            {
                #[cfg(test)]
                test_support::note_transport_dispatch_complete(&home, &agent);
                tracing::debug!(
                    agent = %agent,
                    "delivery_worker: discarded stale transport delivery during teardown"
                );
                return;
            }
            // #3175+#3176: the legacy injector runs on the worker thread via
            // `dispatch_pty_wake`, which enforces per-agent serialization across
            // the unconfirmed state: a FRESH wake for an agent whose earlier wake
            // is still unconfirmed is HELD (never physically written) until the
            // pending resume reaches a terminal outcome. Structured backends
            // (Codex / OpenCode NativeShared) deliver through their own transport
            // seam instead — the closure here is only invoked for the LegacyPty
            // path.
            let body = notification.text.clone();
            let result = crate::transport::deliver_notification(
                &home,
                &agent,
                &body,
                move |home, agent, _notification| {
                    #[cfg(test)]
                    test_support::note_legacy_wake(agent);
                    dispatch_pty_wake(
                        home,
                        agent,
                        notification.clone(),
                        &mut crate::inbox::notify::inject_with_submit_direct,
                    );
                    Ok(())
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
        DeliveryJob::TelegramSend(job) => {
            crate::channel::telegram::notify::send_telegram_job(job);
        }
        DeliveryJob::CronInject {
            target,
            agent,
            text,
        } => {
            if let Err(e) = crate::agent::inject_target_physical(&target, &text) {
                tracing::debug!(agent = %agent, error = %e, "delivery_worker: cron inject failed");
            }
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

/// #3176: deliver ONE submit-aware PTY wake, enforcing per-agent serialization
/// across the typed-inject unconfirmed state (see [`PENDING_UNCONFIRMED`]).
///
/// Runs on the delivery worker thread. `inject` is the physical write
/// primitive; production passes [`crate::inbox::notify::inject_with_submit_direct`],
/// tests pass a scripted closure. NOTE: the caller MUST be the only thread
/// driving physical writes for an agent (the worker is single-threaded), or the
/// hold/release discipline is meaningless.
fn dispatch_pty_wake<F>(
    home: &Path,
    agent: &str,
    notification: crate::notification_queue::QueuedNotification,
    inject: &mut F,
) where
    F: FnMut(
        &Path,
        &str,
        &crate::notification_queue::QueuedNotification,
    ) -> Result<(), crate::inbox::notify::InjectDeliveryFailure>,
{
    use crate::inbox::notify::InjectDeliveryFailure;
    // #3176: while an earlier same-agent wake is unconfirmed (its payload is
    // already in the input area as an unsubmitted draft and its resume is
    // pending) OR the agent is QUARANTINED (a terminal-but-unsettled resume
    // outcome left an unPROVEN stranded draft in the box), a FRESH wake must
    // not physically write — typing it now would combine/corrupt the two
    // notifications and could submit them together. Hold it in memory; only a
    // provable recovery (resume `Ok`, pane/agent lifecycle invalidation, or an
    // explicit operator recovery via [`lift_quarantine`]) releases it.
    if !notification.resume && (has_pending_unconfirmed(home, agent) || is_quarantined(home, agent))
    {
        hold_item(home, agent, notification);
        return;
    }
    match inject(home, agent, &notification) {
        Ok(()) => {
            // Terminal success for the agent's pending resume (submit-only) —
            // the unsubmitted draft is provably submitted/cleared, so any
            // quarantine is lifted too and fresh wakes may write again.
            if notification.resume {
                clear_quarantine(home, agent);
                release_held(home, agent, inject);
            }
        }
        // #3175: typed-inject readback-abort — the payload WAS written
        // (renders as a draft once the pane settles) but the submit key
        // was withheld. Requeue as a RESUME item: the flush holds it
        // while busy (fresh defer timestamp) and re-attempts submit-only
        // — never a payload rewrite, so no buffer duplication, and it
        // survives arbitrarily long generations. Bounded by the attempt
        // budget so a dead pane gives up loudly instead of retrying
        // forever. #3176 wraps that recovery in per-agent serialization.
        Err(InjectDeliveryFailure::Unconfirmed(message)) => {
            if notification.attempts >= MAX_RESUME_ATTEMPTS {
                // Give-up is a LIVENESS terminal condition — the resume budget
                // is exhausted — but it is NOT proof that the stranded draft
                // was submitted or cleared. The pane never settled, so the
                // unsubmitted draft may still sit in the input box. QUARANTINE
                // the agent: held fresh wakes stay blocked (never physically
                // written) until a provable recovery — a later resume `Ok`,
                // pane/agent lifecycle invalidation, or an explicit operator
                // recovery via [`lift_quarantine`].
                set_quarantined(home, agent);
                tracing::warn!(
                    agent = %agent,
                    tag = "#3175-resume-give-up",
                    attempts = notification.attempts,
                    error = %message,
                    "typed inject unconfirmed after {MAX_RESUME_ATTEMPTS} resume attempts \
                     — agent QUARANTINED (pane never settled; stranded draft may remain; \
                     held wakes blocked pending recovery)"
                );
                crate::event_log::log(
                    home,
                    "inject_unconfirmed_gave_up",
                    agent,
                    "typed inject payload unconfirmed after resume budget — agent quarantined",
                );
            } else {
                tracing::debug!(
                    agent = %agent,
                    tag = "#3175-resume-requeue",
                    attempts = notification.attempts,
                    "typed inject unconfirmed — requeueing as resume item"
                );
                set_pending_unconfirmed(home, agent);
                crate::notification_queue::enqueue_resume(home, agent, &notification);
            }
        }
        // Any physical write failure other than the recoverable abort. On a
        // RESUME item the submit-only write did not complete — this does NOT
        // prove the pre-existing draft was cleared (e.g. self-IPC failed
        // before the submit landed), so held fresh wakes must NOT be released:
        // quarantine the agent instead, matching the give-up discipline above.
        // On a FRESH item `Other` is a plain failed write (no draft was
        // created); pre-#3176 drop semantics apply.
        Err(InjectDeliveryFailure::Other(e)) => {
            if notification.resume {
                set_quarantined(home, agent);
                tracing::warn!(
                    agent = %agent,
                    tag = "#3175-resume-other-quarantine",
                    error = %e,
                    "resume submit-only inject failed with Other — agent QUARANTINED \
                     (draft may remain; held wakes blocked pending recovery)"
                );
                crate::event_log::log(
                    home,
                    "inject_resume_other_quarantined",
                    agent,
                    "resume submit-only inject failed — agent quarantined; held wakes blocked pending recovery",
                );
            }
            tracing::debug!(agent = %agent, error = %e, "delivery_worker: PTY wake inject failed");
        }
    }
}

/// #3176: terminal resume outcome reached for an agent's pending unconfirmed
/// wake — take the held FIFO and release each held fresh wake IN ORDER. Each
/// release is a real physical write via `inject`; a released wake that aborts
/// unconfirmed becomes the agent's NEW pending wake and the remaining held
/// wakes stay held behind it (they never overtake an unconfirmed draft).
fn release_held<F>(home: &Path, agent: &str, inject: &mut F)
where
    F: FnMut(
        &Path,
        &str,
        &crate::notification_queue::QueuedNotification,
    ) -> Result<(), crate::inbox::notify::InjectDeliveryFailure>,
{
    use crate::inbox::notify::InjectDeliveryFailure;
    let Some(mut held) = take_held(home, agent) else {
        return;
    };
    while let Some(notification) = held.pop_front() {
        // A previous release aborted and re-marked this agent as pending —
        // the remainder must not write ahead of that fresh unconfirmed draft.
        if has_pending_unconfirmed(home, agent) {
            pending_unconfirmed_map()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .entry(pending_key(home, agent))
                .or_default()
                .extend(held);
            return;
        }
        match inject(home, agent, &notification) {
            Ok(()) => {}
            Err(InjectDeliveryFailure::Unconfirmed(message)) => {
                if notification.attempts >= MAX_RESUME_ATTEMPTS {
                    // Give-up here is the same LIVENESS terminal as in
                    // `dispatch_pty_wake`: the budget is exhausted but that is
                    // NOT proof the stranded draft was submitted/cleared. The
                    // remainder must NOT release over it — quarantine the agent
                    // and hold the rest behind (never writes on an unproven
                    // draft). `take_held` already removed this FIFO from the
                    // map, so re-insert the held remainder.
                    set_quarantined(home, agent);
                    pending_unconfirmed_map()
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .entry(pending_key(home, agent))
                        .or_default()
                        .extend(held);
                    tracing::warn!(
                        agent = %agent,
                        tag = "#3175-resume-give-up",
                        attempts = notification.attempts,
                        error = %message,
                        "typed inject unconfirmed after {MAX_RESUME_ATTEMPTS} resume attempts \
                         — agent QUARANTINED (pane never settled; stranded draft may remain; \
                         held wakes blocked pending recovery)"
                    );
                    crate::event_log::log(
                        home,
                        "inject_unconfirmed_gave_up",
                        agent,
                        "typed inject payload unconfirmed after resume budget — agent quarantined",
                    );
                    return;
                }
                set_pending_unconfirmed(home, agent);
                crate::notification_queue::enqueue_resume(home, agent, &notification);
                // This released wake is now the pending draft; the remainder
                // stays held behind it.
                pending_unconfirmed_map()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .entry(pending_key(home, agent))
                    .or_default()
                    .extend(held);
                return;
            }
            Err(InjectDeliveryFailure::Other(e)) => {
                tracing::debug!(agent = %agent, error = %e, "delivery_worker: PTY wake inject failed");
            }
        }
    }
}

/// Enqueue a complete backend-side delivery. The bounded queue is the
/// caller-facing non-blocking boundary; structured handshakes and protocol
/// waits happen only in [`dispatch`] on the worker thread.
/// #3175: takes the whole `QueuedNotification` (resume flag + attempt budget)
/// so the worker can requeue a typed-inject abort (`inject_unconfirmed`) as a
/// bounded RESUME item for the same payload.
pub(crate) fn enqueue_transport_delivery(
    home: &Path,
    agent: &str,
    notification: crate::notification_queue::QueuedNotification,
) -> Result<(), ()> {
    try_enqueue(DeliveryJob::TransportDelivery {
        home: home.to_path_buf(),
        agent: agent.to_string(),
        notification,
        epoch: transport_epoch(home, agent),
    })
}

impl Drop for TransportCleanupGuard {
    fn drop(&mut self) {
        bump_transport_epoch(&self.key.0, &self.key.1);
    }
}

/// Offload a Telegram send whose dedup claim was already recorded on the caller
/// thread. Returns `Err(())` when the queue is full — the caller MUST evict the
/// dedup claim so a later identical emit isn't suppressed for the whole TTL.
pub(crate) fn enqueue_telegram_send(job: TelegramSendJob) -> Result<(), ()> {
    try_enqueue(DeliveryJob::TelegramSend(job))
}

/// AUDIT2-006 C: offload a cron physical PTY inject. The caller (cron) has already
/// run the prepare/gate phase synchronously; `target` is the CAPTURED inject
/// snapshot — the worker never re-resolves `agent` (logging only). Returns `Err(())`
/// when the bounded queue is full, so the caller records `drop_queue_full`.
pub(crate) fn enqueue_cron_inject(
    target: crate::agent::InjectTarget,
    agent: &str,
    text: Vec<u8>,
) -> Result<(), ()> {
    try_enqueue(DeliveryJob::CronInject {
        target,
        agent: agent.to_string(),
        text,
    })
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

    static FORCE_FULL: AtomicBool = AtomicBool::new(false);
    static LEGACY_WAKE_COUNT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static FF_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
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

    /// #3176: clear the process-global unconfirmed-pending + quarantine
    /// registries. Tests key their state on unique tempdir homes (no cross-talk),
    /// but a deterministic reset also guards against a leaked entry surviving
    /// into another test. MUST be called while holding [`pending_guard`] — both
    /// registries are process-global and parallel tests would otherwise wipe
    /// each other's live state.
    pub(crate) fn reset_pending_unconfirmed() {
        super::pending_unconfirmed_map()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        super::quarantined_set()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    /// #3176: serialize tests that touch the process-global unconfirmed-pending
    /// / quarantine registries. Hold the returned guard across the whole
    /// reset→dispatch→assert→reset window. Every pending/quarantine-state test
    /// MUST hold it.
    pub(crate) fn pending_guard() -> parking_lot::MutexGuard<'static, ()> {
        static PENDING_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
        PENDING_LOCK.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hot path is `try_send`: a delivery enqueue must return immediately and
    /// never block the caller (tick) thread, and a full queue must surface as a
    /// drop (`Err`) rather than a stall.
    #[test]
    fn enqueue_is_nonblocking_and_drops_when_full() {
        let _ff = test_support::force_full_guard();
        // Healthy queue: a transport delivery enqueues without blocking.
        test_support::set_force_full(false);
        assert!(
            enqueue_transport_delivery(
                std::path::Path::new("/tmp/aw"),
                "agentA",
                crate::notification_queue::QueuedNotification::fresh("ping".to_string(), false)
            )
            .is_ok(),
            "a non-full delivery queue must accept the wake without blocking"
        );

        // Full queue: the enqueue is dropped (Err) — the caller, not the worker,
        // owns recording the drop. No block, no panic.
        test_support::set_force_full(true);
        assert!(
            enqueue_transport_delivery(
                std::path::Path::new("/tmp/aw"),
                "agentA",
                crate::notification_queue::QueuedNotification::fresh("ping".to_string(), false)
            )
            .is_err(),
            "AUDIT2-006: a full delivery queue must drop (Err), never block the tick thread"
        );
        test_support::set_force_full(false);
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
        assert!(enqueue_transport_delivery(
            &home,
            "codex-agent",
            crate::notification_queue::QueuedNotification::fresh("ping".to_string(), false)
        )
        .is_ok());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "transport enqueue must not run Codex readiness on the caller thread"
        );
        // The daemon-lifetime worker may still be consuming this job; keep the
        // unique home available until its best-effort read has finished.
    }

    #[test]
    fn legacy_transport_delivery_executes_one_physical_wake_on_worker() {
        let _ff = test_support::force_full_guard();
        test_support::set_force_full(true);
        test_support::reset_legacy_wake_count();
        let home = std::env::temp_dir().join(format!(
            "agend-transport-worker-legacy-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  legacy-agent:\n    backend: claude\n",
        )
        .expect("fleet");
        dispatch(DeliveryJob::TransportDelivery {
            home: home.clone(),
            agent: "legacy-agent".to_string(),
            notification: crate::notification_queue::QueuedNotification::fresh(
                "one logical wake".to_string(),
                false,
            ),
            epoch: transport_epoch(&home, "legacy-agent"),
        });
        assert_eq!(
            test_support::legacy_wake_count(),
            1,
            "LegacyPty must make one physical wake on the existing worker lane"
        );
        test_support::set_force_full(false);
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
            "instances:\n  legacy-agent:\n    backend: claude\n",
        )
        .expect("fleet");

        let stale_epoch = transport_epoch(&home, agent);
        let cleanup = begin_transport_cleanup(&home, agent);
        assert!(try_enqueue(DeliveryJob::TransportDelivery {
            home: home.clone(),
            agent: agent.to_string(),
            notification: crate::notification_queue::QueuedNotification::fresh(
                "stale teardown wake".to_string(),
                false,
            ),
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
            "instances:\n  legacy-agent:\n    backend: claude\n",
        )
        .expect("fleet");

        let cleanup = begin_transport_cleanup(&home, agent);
        assert!(enqueue_transport_delivery(
            &home,
            agent,
            crate::notification_queue::QueuedNotification::fresh(
                "wake during teardown".to_string(),
                false,
            ),
        )
        .is_ok());
        crate::transport::remove_instance_delivery_state(&home, agent).expect("cleanup");
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
            "the delivery enqueued during teardown must be processed before inspecting state"
        );

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

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn tmp_home(suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agend-delivery-worker-{suffix}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// #3176 regression test (reviewer: "two same-agent notifications where the
    /// first physical inject is forced unconfirmed and the second must not write
    /// before the first is resolved"):
    ///
    ///   1. WAKE_A is physically written and aborts unconfirmed — its payload is
    ///      now an unsubmitted draft in the pane; the worker records the pending
    ///      state and requeues WAKE_A as a resume item (submit-only, never
    ///      re-typed).
    ///   2. WAKE_B for the SAME agent arrives while WAKE_A is unresolved — it
    ///      MUST NOT physically write (typing it now would combine/corrupt the
    ///      two notifications and potentially submit them together).
    ///   3. Only once WAKE_A's resume reaches a terminal outcome (success) is
    ///      WAKE_B released and physically written.
    #[test]
    fn same_agent_second_wake_not_written_until_first_resolved_3176() {
        let _pending_guard = test_support::pending_guard();
        test_support::reset_pending_unconfirmed();
        let home = tmp_home("3176-serial");
        let agent = "agentA";
        let written: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let mut inject = |_: &std::path::Path,
                          _: &str,
                          n: &crate::notification_queue::QueuedNotification|
         -> Result<(), crate::inbox::notify::InjectDeliveryFailure> {
            let mut w = written.borrow_mut();
            w.push(n.text.clone());
            if w.len() == 1 {
                Err(crate::inbox::notify::InjectDeliveryFailure::Unconfirmed(
                    "forced abort".to_string(),
                ))
            } else {
                Ok(())
            }
        };

        let wake_a = crate::notification_queue::QueuedNotification::fresh("WAKE_A".into(), true);
        let wake_b = crate::notification_queue::QueuedNotification::fresh("WAKE_B".into(), false);

        // 1) First same-agent wake aborts unconfirmed → pending + resume requeued.
        dispatch_pty_wake(&home, agent, wake_a.clone(), &mut inject);
        assert_eq!(
            *written.borrow(),
            ["WAKE_A"],
            "first wake must be physically written"
        );
        assert!(
            has_pending_unconfirmed(&home, agent),
            "the unconfirmed wake must be recorded as pending for its agent"
        );
        let queue_file = home
            .join("notification-queue")
            .join(format!("{agent}.jsonl"));
        let queued = std::fs::read_to_string(&queue_file).expect("resume item must be on disk");
        assert!(
            queued.contains("WAKE_A") && queued.contains("\"resume\":true"),
            "the aborted wake must be requeued as a RESUME item (submit-only): {queued}"
        );

        // 2) Second same-agent wake while A unresolved → HELD, never written.
        dispatch_pty_wake(&home, agent, wake_b.clone(), &mut inject);
        assert_eq!(
            *written.borrow(),
            ["WAKE_A"],
            "second same-agent wake must NOT write before the first is resolved"
        );

        // 3) A's resume succeeds (terminal) → the held wake is released.
        let mut resume_a = wake_a.clone();
        resume_a.resume = true;
        resume_a.attempts = 1;
        dispatch_pty_wake(&home, agent, resume_a, &mut inject);
        assert_eq!(
            *written.borrow(),
            ["WAKE_A", "WAKE_A", "WAKE_B"],
            "held wake must be physically written only after the first wake is resolved"
        );
        assert!(
            !has_pending_unconfirmed(&home, agent),
            "pending unconfirmed state must clear at the terminal resume outcome"
        );
        test_support::reset_pending_unconfirmed();
    }

    /// #3176: the unconfirmed hold is PER-AGENT — a wake for a different agent
    /// must never be delayed by another agent's pending resume.
    #[test]
    fn unconfirmed_hold_is_per_agent_3176() {
        let _pending_guard = test_support::pending_guard();
        test_support::reset_pending_unconfirmed();
        let home = tmp_home("3176-peragent");
        let written: std::cell::RefCell<Vec<(String, String)>> =
            std::cell::RefCell::new(Vec::new());
        let mut inject = |_: &std::path::Path,
                          a: &str,
                          n: &crate::notification_queue::QueuedNotification|
         -> Result<(), crate::inbox::notify::InjectDeliveryFailure> {
            let mut w = written.borrow_mut();
            w.push((a.to_string(), n.text.clone()));
            if w.len() == 1 {
                Err(crate::inbox::notify::InjectDeliveryFailure::Unconfirmed(
                    "forced abort".to_string(),
                ))
            } else {
                Ok(())
            }
        };

        // agentA's wake aborts → pending ONLY for agentA.
        dispatch_pty_wake(
            &home,
            "agentA",
            crate::notification_queue::QueuedNotification::fresh("A1".into(), true),
            &mut inject,
        );
        assert!(has_pending_unconfirmed(&home, "agentA"));
        // agentB's wake writes immediately — serialization is per-agent.
        dispatch_pty_wake(
            &home,
            "agentB",
            crate::notification_queue::QueuedNotification::fresh("B1".into(), true),
            &mut inject,
        );
        assert_eq!(
            *written.borrow(),
            [
                ("agentA".to_string(), "A1".to_string()),
                ("agentB".to_string(), "B1".to_string())
            ],
            "a different agent's wake must not be held by agentA's pending resume"
        );
        test_support::reset_pending_unconfirmed();
    }

    /// #3176: held wakes release in FIFO arrival order once the pending resume
    /// succeeds. A released wake that itself aborts unconfirmed becomes the NEW
    /// pending wake; the remainder stays held behind it (never overtaking the
    /// fresh unconfirmed draft).
    #[test]
    fn held_wakes_release_in_fifo_order_and_reabort_holds_remainder_3176() {
        let _pending_guard = test_support::pending_guard();
        test_support::reset_pending_unconfirmed();
        let home = tmp_home("3176-release");
        let agent = "agentA";
        let calls: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let mut inject = |_: &std::path::Path,
                          _: &str,
                          n: &crate::notification_queue::QueuedNotification|
         -> Result<(), crate::inbox::notify::InjectDeliveryFailure> {
            let mut c = calls.borrow_mut();
            c.push(n.text.clone());
            // 1st physical write (A) and 3rd (B released) abort unconfirmed.
            match c.len() {
                1 | 3 => Err(crate::inbox::notify::InjectDeliveryFailure::Unconfirmed(
                    "forced abort".to_string(),
                )),
                _ => Ok(()),
            }
        };

        let wake_a = crate::notification_queue::QueuedNotification::fresh("WAKE_A".into(), true);
        let wake_b = crate::notification_queue::QueuedNotification::fresh("WAKE_B".into(), false);
        let wake_c = crate::notification_queue::QueuedNotification::fresh("WAKE_C".into(), false);

        // A aborts (pending); B and C are held.
        dispatch_pty_wake(&home, agent, wake_a.clone(), &mut inject);
        dispatch_pty_wake(&home, agent, wake_b.clone(), &mut inject);
        dispatch_pty_wake(&home, agent, wake_c.clone(), &mut inject);
        assert_eq!(
            *calls.borrow(),
            ["WAKE_A"],
            "only A has physically written so far"
        );

        // A's resume succeeds → release B (which aborts → becomes new pending),
        // C stays held behind it.
        let mut resume_a = wake_a.clone();
        resume_a.resume = true;
        resume_a.attempts = 1;
        dispatch_pty_wake(&home, agent, resume_a, &mut inject);
        assert_eq!(
            *calls.borrow(),
            ["WAKE_A", "WAKE_A", "WAKE_B"],
            "B released in order but aborted"
        );
        assert!(
            has_pending_unconfirmed(&home, agent),
            "the re-aborted released wake B must become the new pending wake; C stays held"
        );

        // B's resume succeeds → C is finally released.
        let mut resume_b = wake_b.clone();
        resume_b.resume = true;
        resume_b.attempts = 1;
        dispatch_pty_wake(&home, agent, resume_b, &mut inject);
        assert_eq!(
            *calls.borrow(),
            ["WAKE_A", "WAKE_A", "WAKE_B", "WAKE_B", "WAKE_C"],
            "held wakes release in FIFO order, each behind the previous resolution"
        );
        assert!(
            !has_pending_unconfirmed(&home, agent),
            "pending unconfirmed state must clear once the tail wake resolves"
        );
        test_support::reset_pending_unconfirmed();
    }

    /// #3176 (review R2): give-up (resume budget exhausted) is a LIVENESS
    /// terminal condition — but NOT proof the stranded draft was submitted or
    /// cleared. The pane never settled, so the unsubmitted draft may still sit
    /// in the input box: the agent is QUARANTINED and held wakes stay blocked
    /// (never physically written) until a provable recovery.
    #[test]
    fn give_up_quarantines_held_wakes_3176() {
        let _pending_guard = test_support::pending_guard();
        test_support::reset_pending_unconfirmed();
        let home = tmp_home("3176-giveup");
        let agent = "agentA";
        let written: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let mut inject = |_: &std::path::Path,
                          _: &str,
                          n: &crate::notification_queue::QueuedNotification|
         -> Result<(), crate::inbox::notify::InjectDeliveryFailure> {
            written.borrow_mut().push(n.text.clone());
            Err(crate::inbox::notify::InjectDeliveryFailure::Unconfirmed(
                "pane never settles".to_string(),
            ))
        };

        let wake_a = crate::notification_queue::QueuedNotification::fresh("WAKE_A".into(), true);
        let wake_b = crate::notification_queue::QueuedNotification::fresh("WAKE_B".into(), false);

        // A aborts → pending; B held.
        dispatch_pty_wake(&home, agent, wake_a.clone(), &mut inject);
        dispatch_pty_wake(&home, agent, wake_b.clone(), &mut inject);
        assert_eq!(
            *written.borrow(),
            ["WAKE_A"],
            "B must be held while A is unresolved"
        );

        // A's resume has exhausted the budget → give-up. A never settled, so the
        // draft may still be in the input box: QUARANTINE — B must NOT write.
        let mut resume_a = wake_a.clone();
        resume_a.resume = true;
        resume_a.attempts = MAX_RESUME_ATTEMPTS;
        dispatch_pty_wake(&home, agent, resume_a, &mut inject);
        assert_eq!(
            *written.borrow(),
            ["WAKE_A", "WAKE_A"],
            "give-up must NOT release held wake B while A's stranded draft remains"
        );
        assert!(
            is_quarantined(&home, agent),
            "give-up must quarantine the agent pending recovery"
        );

        // A later FRESH wake must stay blocked too while quarantined.
        let wake_c = crate::notification_queue::QueuedNotification::fresh("WAKE_C".into(), false);
        dispatch_pty_wake(&home, agent, wake_c.clone(), &mut inject);
        assert_eq!(
            *written.borrow(),
            ["WAKE_A", "WAKE_A"],
            "a fresh wake must NOT write while the agent is quarantined"
        );
        test_support::reset_pending_unconfirmed();
    }

    /// #3176 (review R2): a resume submit-only write failing with `Other` does
    /// NOT prove the pre-existing draft was cleared (e.g. self-IPC failed
    /// before the submit landed). The agent is QUARANTINED and held wakes stay
    /// blocked — B is never physically written while A's draft may remain.
    #[test]
    fn resume_other_quarantines_held_wakes_3176() {
        let _pending_guard = test_support::pending_guard();
        test_support::reset_pending_unconfirmed();
        let home = tmp_home("3176-other");
        let agent = "agentA";
        let written: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let mut inject = |_: &std::path::Path,
                          _: &str,
                          n: &crate::notification_queue::QueuedNotification|
         -> Result<(), crate::inbox::notify::InjectDeliveryFailure> {
            written.borrow_mut().push(n.text.clone());
            if n.resume {
                Err(crate::inbox::notify::InjectDeliveryFailure::Other(
                    "self-IPC failed before submit".to_string(),
                ))
            } else {
                Err(crate::inbox::notify::InjectDeliveryFailure::Unconfirmed(
                    "forced abort".to_string(),
                ))
            }
        };

        let wake_a = crate::notification_queue::QueuedNotification::fresh("WAKE_A".into(), true);
        let wake_b = crate::notification_queue::QueuedNotification::fresh("WAKE_B".into(), false);

        // A aborts → pending; B held.
        dispatch_pty_wake(&home, agent, wake_a.clone(), &mut inject);
        dispatch_pty_wake(&home, agent, wake_b.clone(), &mut inject);
        assert_eq!(
            *written.borrow(),
            ["WAKE_A"],
            "B must be held while A is unresolved"
        );

        // A's resume fails with `Other` → QUARANTINE; B must NOT write.
        let mut resume_a = wake_a.clone();
        resume_a.resume = true;
        resume_a.attempts = 1;
        dispatch_pty_wake(&home, agent, resume_a, &mut inject);
        assert_eq!(
            *written.borrow(),
            ["WAKE_A", "WAKE_A"],
            "resume Other must NOT release held wake B while A's draft may remain"
        );
        assert!(
            is_quarantined(&home, agent),
            "resume Other must quarantine the agent pending recovery"
        );

        // A later FRESH wake must stay blocked too while quarantined.
        let wake_c = crate::notification_queue::QueuedNotification::fresh("WAKE_C".into(), false);
        dispatch_pty_wake(&home, agent, wake_c.clone(), &mut inject);
        assert_eq!(
            *written.borrow(),
            ["WAKE_A", "WAKE_A"],
            "a fresh wake must NOT write while the agent is quarantined"
        );
        test_support::reset_pending_unconfirmed();
    }

    /// #3176 (review R2): quarantine is lifted by a PROVEN recovery — a later
    /// resume reaching `Ok` clears the marker and releases held wakes in order.
    #[test]
    fn resume_ok_lifts_quarantine_and_releases_3176() {
        let _pending_guard = test_support::pending_guard();
        test_support::reset_pending_unconfirmed();
        let home = tmp_home("3176-oklift");
        let agent = "agentA";
        let written: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let mut inject = |_: &std::path::Path,
                          _: &str,
                          n: &crate::notification_queue::QueuedNotification|
         -> Result<(), crate::inbox::notify::InjectDeliveryFailure> {
            written.borrow_mut().push(n.text.clone());
            if n.resume {
                Ok(())
            } else {
                Err(crate::inbox::notify::InjectDeliveryFailure::Unconfirmed(
                    "forced abort".to_string(),
                ))
            }
        };

        let wake_a = crate::notification_queue::QueuedNotification::fresh("WAKE_A".into(), true);
        let wake_b = crate::notification_queue::QueuedNotification::fresh("WAKE_B".into(), false);

        // A aborts → pending; B held. Quarantine A manually to model an earlier
        // unsettled terminal outcome whose quarantine is still in force.
        dispatch_pty_wake(&home, agent, wake_a.clone(), &mut inject);
        dispatch_pty_wake(&home, agent, wake_b.clone(), &mut inject);
        set_quarantined(&home, agent);
        assert!(is_quarantined(&home, agent));

        // A's resume reaches `Ok` (draft provably submitted/cleared) → the
        // quarantine is lifted and held B is released.
        let mut resume_a = wake_a.clone();
        resume_a.resume = true;
        resume_a.attempts = 1;
        dispatch_pty_wake(&home, agent, resume_a, &mut inject);
        assert_eq!(
            *written.borrow(),
            ["WAKE_A", "WAKE_A", "WAKE_B"],
            "resume Ok proves the draft cleared: quarantine lifted, held B released"
        );
        assert!(
            !is_quarantined(&home, agent),
            "resume Ok must lift the quarantine"
        );
        test_support::reset_pending_unconfirmed();
    }

    /// #3176 (review R2): explicit operator / lifecycle recovery lifts the
    /// quarantine and re-enqueues the held wakes as fresh deliveries.
    #[test]
    fn lift_quarantine_releases_held_wakes_3176() {
        let _pending_guard = test_support::pending_guard();
        test_support::reset_pending_unconfirmed();
        let home = tmp_home("3176-lift");
        let agent = "agentA";
        let written: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let mut inject = |_: &std::path::Path,
                          _: &str,
                          n: &crate::notification_queue::QueuedNotification|
         -> Result<(), crate::inbox::notify::InjectDeliveryFailure> {
            written.borrow_mut().push(n.text.clone());
            if written.borrow().len() == 1 {
                Err(crate::inbox::notify::InjectDeliveryFailure::Unconfirmed(
                    "forced abort".to_string(),
                ))
            } else {
                Ok(())
            }
        };

        let wake_a = crate::notification_queue::QueuedNotification::fresh("WAKE_A".into(), true);
        let wake_b = crate::notification_queue::QueuedNotification::fresh("WAKE_B".into(), false);
        let wake_c = crate::notification_queue::QueuedNotification::fresh("WAKE_C".into(), false);

        // A aborts → pending; B, C held; agent quarantined (unsettled terminal).
        dispatch_pty_wake(&home, agent, wake_a.clone(), &mut inject);
        dispatch_pty_wake(&home, agent, wake_b.clone(), &mut inject);
        dispatch_pty_wake(&home, agent, wake_c.clone(), &mut inject);
        set_quarantined(&home, agent);
        assert!(is_quarantined(&home, agent));

        // Lifecycle/operator recovery lifts the quarantine; held B, C are
        // re-released in order (the scripted inject records them).
        lift_quarantine_with(&home, agent, &mut inject);
        assert!(
            !is_quarantined(&home, agent),
            "lift must clear the quarantine marker"
        );
        assert!(
            !has_pending_unconfirmed(&home, agent),
            "lift must clear the held-wakes FIFO"
        );
        assert_eq!(
            *written.borrow(),
            ["WAKE_A", "WAKE_B", "WAKE_C"],
            "held wakes must be re-enqueued in FIFO order by the lift"
        );

        // A fresh wake writes again immediately (block removed).
        dispatch_pty_wake(
            &home,
            agent,
            crate::notification_queue::QueuedNotification::fresh("WAKE_D".into(), false),
            &mut inject,
        );
        assert_eq!(
            *written.borrow(),
            ["WAKE_A", "WAKE_B", "WAKE_C", "WAKE_D"],
            "fresh wakes may write again after the quarantine is lifted"
        );
        test_support::reset_pending_unconfirmed();
    }
}
