//! AUDIT2-006: bounded delivery worker.
//!
//! The daemon's main tick / `run_core` loop emits events and, via the event bus
//! subscribers, delivers notifications by injecting into agent PTYs and sending
//! Telegram messages. Both are BLOCKING I/O: a Telegram network black-hole (no
//! local request timeout) or a slow PTY readback would otherwise park the tick
//! thread — stalling the hang-detection, recovery-dispatcher and crash handling
//! that share that one thread.
//!
//! This module offloads ONLY the blocking *wake* effect (the physical PTY poke
//! and the Telegram send) onto a single bounded background worker. Durable
//! source-of-truth writes (inbox JSONL, `notification_queue`, schedule
//! `run_history`) stay SYNCHRONOUS on the caller — a notification is a wakeup,
//! not a commit barrier. `event_bus::emit`'s handled-count is unaffected: it is
//! decided by the synchronous kind-match inside each subscriber, BEFORE any
//! delivery is enqueued (see `event_bus.rs`).
//!
//! Backpressure: a bounded `sync_channel(QUEUE_CAP)`; [`enqueue_pty_wake`] /
//! [`enqueue_telegram_send`] use `try_send` and NEVER block. On a full queue the
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

use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::OnceLock;

/// Queue depth. Large enough to absorb a burst of watchdog / cron / crash
/// notifications, small enough that a wedged delivery path (post request-timeout)
/// surfaces as drops quickly rather than unbounded memory growth.
const QUEUE_CAP: usize = 256;

/// A unit of blocking delivery work offloaded off the tick / main-loop thread.
enum DeliveryJob {
    /// Physical submit-aware PTY inject — the `inbox::notify::inject_with_submit`
    /// primitive (an `api::call(INJECT)` loopback). The worker calls the `_direct`
    /// primitive, NEVER the offload wrapper, so there is no recursive re-enqueue.
    PtyWake {
        home: std::path::PathBuf,
        agent: String,
        notification: String,
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
        DeliveryJob::PtyWake {
            home,
            agent,
            notification,
        } => {
            if let Err(e) =
                crate::inbox::notify::inject_with_submit_direct(&home, &agent, &notification)
            {
                tracing::debug!(agent = %agent, error = %e, "delivery_worker: PTY wake inject failed");
            }
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
            #[cfg(test)]
            crate::daemon::tick_stall::test_probe::emit(&host, &handler);
        }
    }
}

/// Offload a physical submit-aware PTY wake. Returns `Err(())` when the bounded
/// queue is full — the wake is dropped and the caller owns whether/how to record
/// that (most notify callers discard the result; a WARN is emitted internally).
pub(crate) fn enqueue_pty_wake(
    home: &std::path::Path,
    agent: &str,
    notification: &str,
) -> Result<(), ()> {
    try_enqueue(DeliveryJob::PtyWake {
        home: home.to_path_buf(),
        agent: agent.to_string(),
        notification: notification.to_string(),
    })
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
    static FF_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

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

    pub(super) fn force_full() -> bool {
        FORCE_FULL.load(Ordering::Relaxed)
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
        // Healthy queue: a PTY wake enqueues without blocking.
        test_support::set_force_full(false);
        assert!(
            enqueue_pty_wake(std::path::Path::new("/tmp/aw"), "agentA", "ping").is_ok(),
            "a non-full delivery queue must accept the wake without blocking"
        );

        // Full queue: the enqueue is dropped (Err) — the caller, not the worker,
        // owns recording the drop. No block, no panic.
        test_support::set_force_full(true);
        assert!(
            enqueue_pty_wake(std::path::Path::new("/tmp/aw"), "agentA", "ping").is_err(),
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
}
