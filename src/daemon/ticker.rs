//! Sprint 23 P0 — `DaemonTicker` shutdown-aware periodic loop primitive.
//!
//! Cross-track infra shipped with Sprint 23 P0 (F6 lock-around-pair) and
//! consumed by Sprint 24 P0 PR2 (task sweep daemon). Forward-compat with
//! Sprint 25+ graceful-join refactor (shutdown-channel-plumbing track).
//!
//! ## Design
//!
//! Wraps `std::thread::Builder::spawn` with two contracts existing daemon
//! periodic loops (supervisor.rs / instance_monitor.rs / cron_tick.rs)
//! independently re-derived but never centralised:
//!
//! 1. **Shutdown-aware sleep**: 100ms sleep granularity bounds shutdown
//!    latency regardless of tick duration. A 5-minute task sweep tick still
//!    exits within 100ms of shutdown signal — without DaemonTicker, the
//!    naive `thread::sleep(tick_dur)` blocks for the whole 5 minutes.
//!
//! 2. **Stored JoinHandle**: forward-compat with Sprint 25+ graceful-join
//!    refactor. Today the handle is dropped (fire-and-forget per existing
//!    daemon convention); when graceful-join lands, callers opt in via
//!    `join_on_shutdown()` without touching the spawn site.
//!
//! ## Why not `tokio::time::interval` + `CancellationToken`
//!
//! Most daemon code is sync (PTY I/O, std::thread). Adding a tokio runtime
//! purely for cancellation token would force every consumer to manage a
//! runtime handle. A plain `Arc<AtomicBool>` shutdown flag composes with
//! the existing daemon shutdown signal (`bootstrap::signals::install`)
//! without changing concurrency model.

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Bounded shutdown-check interval for the sleep-with-cancel loop. 100ms
/// = imperceptible to operator (Ctrl+C feels instant) and cheap CPU-wise
/// (10 polls/s while idle is negligible).
const SHUTDOWN_POLL_GRANULARITY: Duration = Duration::from_millis(100);

/// Shutdown-aware periodic loop primitive.
///
/// Spawn via [`DaemonTicker::spawn`]; drop the returned value to relinquish
/// the JoinHandle (fire-and-forget — current daemon convention), or call
/// [`Self::join_on_shutdown`] after raising the shutdown flag for graceful
/// exit.
pub struct DaemonTicker {
    // Stored for the opt-in graceful-join path; current daemon callers retain
    // the established fire-and-forget drop behavior.
    #[allow(dead_code)]
    handle: Option<JoinHandle<()>>,
}

impl DaemonTicker {
    /// Spawn a named thread that runs `body` every `tick_dur` until
    /// `shutdown` is set to `true`. The closure is invoked once at thread
    /// start (no initial sleep — first tick is immediate) so callers don't
    /// need to wait `tick_dur` for the first iteration.
    ///
    /// `name` flows into the thread name so backtraces / `ps` / Activity
    /// Monitor surface a meaningful identifier; matches the existing
    /// daemon spawn-site naming convention (e.g. `"supervisor"`,
    /// `"daemon_tick"`).
    ///
    /// Returns a `DaemonTicker` whose `Drop` is a no-op — the spawned
    /// thread exits via the shutdown flag, not via the handle. Callers
    /// who want graceful join (Sprint 25+) call [`Self::join_on_shutdown`]
    /// after setting the flag.
    pub fn spawn<F>(
        name: &'static str,
        tick_dur: Duration,
        shutdown: Arc<AtomicBool>,
        mut body: F,
    ) -> Self
    where
        F: FnMut() + Send + 'static,
    {
        // fire-and-forget: shutdown flag is the exit signal; the thread
        // observes it inside `sleep_with_cancel`. JoinHandle is stored so
        // forward-compat with Sprint 25+ graceful-join lands without
        // touching the spawn site (per daemon ticker pattern, Sprint 23 P0).
        let handle = thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                while !shutdown.load(Ordering::Relaxed) {
                    body();
                    if !sleep_with_cancel(tick_dur, &shutdown) {
                        return;
                    }
                }
            })
            .ok();
        Self { handle }
    }

    /// Wait for the spawned thread to exit. Caller is responsible for
    /// having set the shutdown flag before calling — otherwise this blocks
    /// indefinitely.
    ///
    /// Idempotent: returns `Ok(())` if the handle was already taken (drop
    /// path) or never stored (spawn failure). Mirrors the
    /// "no-op-if-already-clean" semantics of `delete_transaction` from
    /// Phase 1 (PR #217 lifecycle helper) for predictable shutdown ordering.
    #[allow(dead_code)]
    pub fn join_on_shutdown(mut self) -> thread::Result<()> {
        match self.handle.take() {
            Some(h) => h.join(),
            None => Ok(()),
        }
    }
}

impl Drop for DaemonTicker {
    /// Drop is a no-op: the spawned thread exits via the shutdown flag, not
    /// via JoinHandle drop. This matches the current daemon fire-and-forget
    /// convention and lets callers ignore the returned ticker (the common
    /// case today). Sprint 25+ graceful-join consumers can switch to
    /// `join_on_shutdown()` without changing call shape.
    fn drop(&mut self) {
        // Intentionally empty.
    }
}

/// Cancellable, coalescing producer for a host's maintenance ticks.
///
/// The receiver stays host-owned so each host preserves its own select
/// topology. This driver owns the producer's stop signal and thread handle:
/// dropping it wakes the producer out of `recv_timeout` and joins it before
/// returning. The bounded tick channel is never written with a blocking send;
/// a tick already queued is enough to represent the next maintenance wake.
pub(crate) struct MaintenanceTickDriver {
    stop_tx: Sender<()>,
    handle: Option<JoinHandle<()>>,
}

impl MaintenanceTickDriver {
    /// Start a producer and return its bounded tick receiver.
    ///
    /// The first tick is emitted after `interval`, matching the legacy host
    /// cadence. The producer exits when stopped, when the tick receiver is
    /// disconnected, or when its stop channel is disconnected.
    pub(crate) fn spawn(name: &'static str, interval: Duration) -> (Self, Receiver<()>) {
        let (tick_tx, tick_rx) = crossbeam_channel::bounded(1);
        let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        // fire-and-forget: false — `Self` retains the handle, and Drop signals
        // the worker before joining it, so this thread cannot outlive the driver.
        let handle = thread::Builder::new()
            .name(name.into())
            .spawn(move || run_maintenance_tick_driver(tick_tx, stop_rx, interval))
            .expect("spawn maintenance tick driver");
        (
            Self {
                stop_tx,
                handle: Some(handle),
            },
            tick_rx,
        )
    }

    fn stop_and_join(&mut self) {
        // `try_send` cannot block: this driver is the only stop sender and the
        // worker consumes at most one stop request before exiting.
        let _ = self.stop_tx.try_send(());
        if let Some(handle) = self.handle.take() {
            if handle.join().is_err() {
                tracing::error!("maintenance tick driver panicked while stopping");
            }
        }
    }
}

impl Drop for MaintenanceTickDriver {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

fn run_maintenance_tick_driver(tick_tx: Sender<()>, stop_rx: Receiver<()>, interval: Duration) {
    loop {
        match stop_rx.recv_timeout(interval) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => match tick_tx.try_send(()) {
                Ok(()) | Err(TrySendError::Full(())) => {}
                Err(TrySendError::Disconnected(())) => return,
            },
        }
    }
}

/// Sleep up to `dur` while polling `shutdown` every
/// [`SHUTDOWN_POLL_GRANULARITY`]. Returns `true` if the full duration
/// elapsed (continue ticking), `false` if shutdown was raised mid-sleep
/// (caller should exit the tick loop).
fn sleep_with_cancel(dur: Duration, shutdown: &Arc<AtomicBool>) -> bool {
    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        if shutdown.load(Ordering::Relaxed) {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(SHUTDOWN_POLL_GRANULARITY.min(remaining));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// DaemonTicker invokes `body` at least once when shutdown fires
    /// before the first tick — confirms the "no initial sleep" contract.
    #[test]
    fn ticker_invokes_body_at_least_once_then_exits_on_shutdown() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicU32::new(0));
        let count2 = Arc::clone(&count);
        let ticker = DaemonTicker::spawn(
            "test_immediate",
            Duration::from_secs(60),
            Arc::clone(&shutdown),
            move || {
                count2.fetch_add(1, Ordering::Relaxed);
            },
        );
        // Body runs immediately on entry, then would sleep 60s. Raise
        // shutdown to make the sleep-with-cancel return false.
        // Spin briefly to let the thread schedule.
        for _ in 0..50 {
            if count.load(Ordering::Relaxed) >= 1 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        shutdown.store(true, Ordering::Relaxed);
        // Wait for clean exit.
        let res = ticker.join_on_shutdown();
        assert!(res.is_ok(), "ticker thread joined cleanly");
        assert!(
            count.load(Ordering::Relaxed) >= 1,
            "body must run at least once before shutdown"
        );
    }

    /// Shutdown signal during sleep exits within bounded latency
    /// (`SHUTDOWN_POLL_GRANULARITY`-bounded). Pins the contract that a
    /// long tick_dur (e.g. 5 min) does NOT block daemon shutdown.
    #[test]
    fn ticker_shutdown_during_long_sleep_exits_within_poll_granularity() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicU32::new(0));
        let count2 = Arc::clone(&count);
        let ticker = DaemonTicker::spawn(
            "test_long_sleep",
            Duration::from_secs(60), // would block 60s without sleep_with_cancel
            Arc::clone(&shutdown),
            move || {
                count2.fetch_add(1, Ordering::Relaxed);
            },
        );
        // Wait for first body invocation.
        for _ in 0..50 {
            if count.load(Ordering::Relaxed) >= 1 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        // Now in sleep_with_cancel (60s sleep). Raise shutdown and time exit.
        let start = Instant::now();
        shutdown.store(true, Ordering::Relaxed);
        let res = ticker.join_on_shutdown();
        let elapsed = start.elapsed();
        assert!(res.is_ok(), "ticker thread joined cleanly");
        // Bound: poll granularity (100ms) + scheduling slack (500ms).
        assert!(
            elapsed < Duration::from_millis(600),
            "shutdown latency must be bounded by poll granularity, not tick_dur — \
             observed {elapsed:?}, expected < 600ms"
        );
    }

    /// `sleep_with_cancel` returns `false` immediately when shutdown is
    /// already true — no sleep at all. Pins the entry-condition shortcut.
    #[test]
    fn sleep_with_cancel_returns_false_when_shutdown_already_set() {
        let shutdown = Arc::new(AtomicBool::new(true));
        let start = Instant::now();
        let proceed = sleep_with_cancel(Duration::from_secs(60), &shutdown);
        let elapsed = start.elapsed();
        assert!(
            !proceed,
            "sleep_with_cancel must return false when shutdown already set"
        );
        assert!(
            elapsed < Duration::from_millis(150),
            "no sleep when shutdown already set — observed {elapsed:?}"
        );
    }

    #[test]
    fn maintenance_tick_driver_delays_first_tick() {
        let (driver, tick_rx) = MaintenanceTickDriver::spawn(
            "test_maintenance_tick_delayed",
            Duration::from_millis(40),
        );

        assert_eq!(
            tick_rx.recv_timeout(Duration::from_millis(5)),
            Err(crossbeam_channel::RecvTimeoutError::Timeout),
            "the first maintenance tick must wait for the configured interval"
        );
        assert_eq!(
            tick_rx.recv_timeout(Duration::from_secs(1)),
            Ok(()),
            "the delayed maintenance tick must eventually arrive"
        );
        drop(driver);
    }

    #[test]
    fn maintenance_tick_driver_coalesces_full_queue_without_blocking() {
        let (driver, tick_rx) = MaintenanceTickDriver::spawn(
            "test_maintenance_tick_coalesce",
            Duration::from_millis(2),
        );
        tick_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first maintenance tick");

        // Liveness. Model a slow handler by leaving the bounded receiver
        // untouched while several producer intervals elapse, so the producer
        // repeatedly hits `Full`. Draining and then receiving again proves it
        // coalesced rather than died: a counting assertion alone cannot tell
        // "coalesced" from "producer exited on the first Full".
        std::thread::sleep(Duration::from_millis(50));
        while tick_rx.try_recv().is_ok() {}
        tick_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("producer must continue after coalescing a full queue");

        // Coalescing, observed as a true snapshot. Saturate the slot again and
        // then stop the producer: `drop` joins the worker, so the queue can no
        // longer be refilled and what remains is exactly what it held. Counting
        // a live `try_iter()` instead is racy — the 2ms producer refills the
        // slot *during* iteration, so a slow enough consumer counts 2+ (the
        // macOS CI failure at run 33621682494).
        std::thread::sleep(Duration::from_millis(50));

        // If the producer were blocked in send, dropping it could not join
        // promptly while the receiver remains full.
        let started = Instant::now();
        drop(driver);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "a full tick queue must not prevent prompt stop/join"
        );
        assert_eq!(
            tick_rx.try_recv(),
            Ok(()),
            "a full bounded queue must hold exactly one coalesced tick"
        );
        assert_eq!(
            tick_rx.try_recv(),
            Err(crossbeam_channel::TryRecvError::Disconnected),
            "a full bounded queue must coalesce ticks instead of growing"
        );
    }

    #[test]
    fn maintenance_tick_driver_exits_when_tick_receiver_disconnects() {
        let (driver, tick_rx) = MaintenanceTickDriver::spawn(
            "test_maintenance_tick_disconnect",
            Duration::from_millis(2),
        );
        drop(tick_rx);

        for _ in 0..50 {
            if driver
                .handle
                .as_ref()
                .is_some_and(std::thread::JoinHandle::is_finished)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            driver
                .handle
                .as_ref()
                .is_some_and(std::thread::JoinHandle::is_finished),
            "a disconnected tick receiver must stop the producer"
        );
        drop(driver);
    }

    #[test]
    fn maintenance_tick_driver_drop_stops_and_joins_promptly() {
        let (driver, tick_rx) =
            MaintenanceTickDriver::spawn("test_maintenance_tick_stop", Duration::from_secs(30));
        let started = Instant::now();

        drop(driver);

        assert!(
            started.elapsed() < Duration::from_millis(500),
            "drop must wake recv_timeout and join promptly"
        );
        assert_eq!(
            tick_rx.try_recv(),
            Err(crossbeam_channel::TryRecvError::Disconnected),
            "the joined producer must close its tick sender"
        );
    }

    #[test]
    fn maintenance_tick_driver_emits_no_tick_after_drop() {
        let (driver, tick_rx) = MaintenanceTickDriver::spawn(
            "test_maintenance_tick_no_post_drop",
            Duration::from_millis(2),
        );
        tick_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first maintenance tick");
        while tick_rx.try_recv().is_ok() {}

        drop(driver);

        assert_eq!(
            tick_rx.recv_timeout(Duration::from_millis(50)),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected),
            "driver drop must not leave a producer that emits post-drop ticks"
        );
    }
}
