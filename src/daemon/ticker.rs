//! Sprint 23 P0 — `DaemonTicker` shutdown-aware periodic loop primitive.
//!
//! Cross-track infra shipped with Sprint 23 P0 (F6 lock-around-pair) and
//! consumed by Sprint 24 P0 PR2 (task sweep daemon). Forward-compat with
//! Sprint 25+ graceful-join refactor (shutdown-channel-plumbing track).
//!
//! NOTE: `#[allow(dead_code)]` at the module level is intentional — this
//! sprint ships the primitive ahead of its first consumer (Sprint 24 P0
//! PR2 task sweep daemon, dispatched concurrently per cross-track
//! sequencing). Lint-suppression dropped once the consumer lands.

#![allow(dead_code)]
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
}
