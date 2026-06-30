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

    /// Force every `try_enqueue` to behave as if the bounded queue were full,
    /// WITHOUT actually filling 256 slots — lets callers unit-test the drop /
    /// dedup-rollback paths deterministically.
    pub(crate) fn set_force_full(on: bool) {
        FORCE_FULL.store(on, Ordering::Relaxed);
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
}
