//! PR4 — opt-in out-of-band tick-stall diagnostics.
//!
//! Decision d-20260711043612580372-4. A dedicated, joined per-host monitor
//! watches the tick host's [`per_tick`](super::per_tick) runner make progress
//! and pages the operator out-of-band when a handler wedges the tick thread —
//! WITHOUT ever running on that thread or reusing the blocking bounded tick
//! producer.
//!
//! ## RED anchor (this commit)
//!
//! The production tracker (`TickProgress`), the monitor thread
//! (`TickStallMonitorGuard`), the `AGEND_TICK_STALL_SECS` env gate and the
//! `delivery_worker` alert job are NOT wired yet — in a release build this
//! module compiles to **nothing** (every item below is `#[cfg(test)]`), so the
//! shipping binary is byte-identical to the pre-PR4 daemon.
//!
//! What lives here now is only the test scaffolding that pins the desired
//! behavior:
//!
//! * [`test_probe`] — a process-global observation seam. The GREEN alert path
//!   (the `delivery_worker` `TickStallAlert` dispatch arm) will emit the exact
//!   escalated `(host, handler)` here under `#[cfg(test)]`, so a test observes
//!   the real payload without a live Telegram / `event_log` side effect.
//! * the focused harness `drive_stalled_tick` + the frozen assertion
//!   `expect_stall_alert`. The harness drives the **real** per-tick runner with
//!   a `Condvar`-stalled handler (a genuine, deterministic tick stall). In RED
//!   there is no monitor, so nothing ever feeds the probe and the assertion
//!   FAILS by watchdog timeout (a real runtime panic — not a compile error and
//!   not an `is_err`/pass-on-old check). GREEN wires the tracker + monitor +
//!   delivery job; the harness body starts the monitor and drives the tracked
//!   runner, and the SAME assertion then receives `host + handler`.

/// Process-global alert-observation seam (test-only). The GREEN production
/// alert path emits the escalated identity here so tests can assert the exact
/// out-of-band payload without a real escalation side effect. Mirrors the
/// `canonical_heartbeat` `test_hooks` static-seam pattern.
#[cfg(test)]
pub(crate) mod test_probe {
    use parking_lot::Mutex;
    use std::sync::mpsc::{sync_channel, Receiver, SyncSender};

    /// Single install slot. `None` = no test is observing (emit is a no-op).
    static SINK: Mutex<Option<SyncSender<(String, String)>>> = Mutex::new(None);

    /// RAII install handle: emits are routed to [`ProbeGuard::rx`] until this
    /// guard drops, which clears the global slot so the next test starts clean.
    pub(crate) struct ProbeGuard {
        pub(crate) rx: Receiver<(String, String)>,
    }

    impl Drop for ProbeGuard {
        fn drop(&mut self) {
            *SINK.lock() = None;
        }
    }

    /// Begin observing stall-alert emissions. Emits go to the returned guard's
    /// receiver until it drops.
    pub(crate) fn install() -> ProbeGuard {
        let (tx, rx) = sync_channel(16);
        *SINK.lock() = Some(tx);
        ProbeGuard { rx }
    }

    /// Emit an observed `(host, handler)` to the installed probe, if any.
    /// The GREEN `delivery_worker` `TickStallAlert` dispatch arm calls this
    /// (under `#[cfg(test)]`) with the exact escalated identity. No-op when no
    /// probe is installed.
    pub(crate) fn emit(host: &str, handler: &str) {
        // Clone the sender out so we never hold the lock across `try_send`.
        let sink = SINK.lock().clone();
        if let Some(tx) = sink {
            let _ = tx.try_send((host.to_string(), handler.to_string()));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::test_probe;
    use crate::agent::{AgentRegistry, ExternalRegistry};
    use crate::daemon::per_tick::{run_handlers_with_panic_guard, PerTickHandler, TickContext};
    use parking_lot::{Condvar, Mutex};
    use std::collections::HashMap;
    use std::sync::mpsc::Receiver;
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::Duration;

    /// The probe + delivery-worker force-full seams are process-global; serialize
    /// stall-diagnostics tests so they don't observe each other's emissions.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Failure ceiling for the frozen assertion. In RED nothing ever emits, so
    /// the RED test blocks the full watchdog then panics. In GREEN the monitor
    /// emits within a few injected sample intervals (≪ watchdog), so the pass is
    /// fast; the watchdog is only the timeout guard, never the happy path.
    const WATCHDOG: Duration = Duration::from_secs(2);
    /// Stable handler identity the alert must name.
    const STALL_HANDLER: &str = "slow_handler";
    /// Stable daemon host label (matches the GREEN payload label — Q5).
    const DAEMON_HOST: &str = "daemon-tick";

    /// A per-tick handler whose `run` blocks on a `Condvar` until released — a
    /// real, deterministic stall of the runner thread (no sleeps, no timing).
    struct BlockingHandler {
        gate: Arc<(Mutex<bool>, Condvar)>,
    }

    impl PerTickHandler for BlockingHandler {
        fn name(&self) -> &'static str {
            STALL_HANDLER
        }
        fn run(&self, _ctx: &TickContext<'_>) {
            let (lock, cv) = &*self.gate;
            let mut released = lock.lock();
            while !*released {
                cv.wait(&mut released);
            }
        }
    }

    /// The mutable RED↔GREEN SEAM (`drive_stalled_tick` + this struct).
    ///
    /// RED: install the probe and spawn the **real** untracked runner
    /// ([`run_handlers_with_panic_guard`]) with a `Condvar`-stalled handler.
    /// There is no monitor, so nothing ever feeds the probe.
    ///
    /// GREEN (follow-up commit): additionally build an `Arc<TickProgress>`,
    /// start the joined `TickStallMonitorGuard` on a short injected interval,
    /// and drive `run_handlers_with_progress` so the monitor samples the stall
    /// and the delivery path feeds the probe. Only this seam changes — the
    /// `#[test]` below and `expect_stall_alert` stay byte-identical.
    struct StalledTick {
        probe: test_probe::ProbeGuard,
        gate: Arc<(Mutex<bool>, Condvar)>,
        runner: Option<JoinHandle<()>>,
    }

    impl StalledTick {
        fn rx(&self) -> &Receiver<(String, String)> {
            &self.probe.rx
        }
    }

    impl Drop for StalledTick {
        fn drop(&mut self) {
            // Release the stalled handler so the runner thread can finish, then
            // join it — no leaked thread even when the assertion panicked.
            {
                let (lock, cv) = &*self.gate;
                *lock.lock() = true;
                cv.notify_all();
            }
            if let Some(handle) = self.runner.take() {
                let _ = handle.join();
            }
        }
    }

    fn drive_stalled_tick() -> StalledTick {
        let probe = test_probe::install();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let gate_for_runner = Arc::clone(&gate);
        // Owned fixture is moved into the runner thread; the borrowing
        // `TickContext` is constructed INSIDE the closure so the closure is
        // `'static` (a stalled runner outlives this function's frame).
        let runner = std::thread::spawn(move || {
            let home = std::env::temp_dir();
            let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
            let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
            let configs: Arc<Mutex<HashMap<String, crate::daemon::AgentConfig>>> =
                Arc::new(Mutex::new(HashMap::new()));
            let ctx = TickContext {
                home: &home,
                registry: &registry,
                externals: &externals,
                configs: &configs,
            };
            let handlers: Vec<Box<dyn PerTickHandler>> = vec![Box::new(BlockingHandler {
                gate: gate_for_runner,
            })];
            // RED: the real runner blocks inside `BlockingHandler::run`; no
            // monitor is watching, so no out-of-band stall alert is emitted.
            run_handlers_with_panic_guard(&handlers, &ctx);
        });
        StalledTick {
            probe,
            gate,
            runner: Some(runner),
        }
    }

    /// FROZEN assertion — byte-identical in RED and GREEN. While the real runner
    /// is stalled inside a handler, an out-of-band stall alert naming
    /// `(host, handler)` must reach the probe within the watchdog. RED has no
    /// monitor → `recv_timeout` expires → panic. GREEN's monitor + delivery job
    /// feed the probe → the same recv succeeds and the identities match.
    fn expect_stall_alert(rx: &Receiver<(String, String)>, want_host: &str, want_handler: &str) {
        match rx.recv_timeout(WATCHDOG) {
            Ok((host, handler)) => {
                assert_eq!(host, want_host, "stall alert host label");
                assert_eq!(handler, want_handler, "stall alert handler identity");
            }
            Err(_) => panic!(
                "RED: expected an out-of-band tick-stall alert host={want_host} \
                 handler={want_handler} while the real per-tick runner is \
                 Condvar-stalled, but none arrived within {WATCHDOG:?}"
            ),
        }
    }

    /// The observation seam itself must faithfully route an emitted identity to
    /// the installed receiver, so a GREEN stall-test failure can only mean "no
    /// monitor emit", never "the probe is broken". (Also exercises the emit path
    /// the GREEN `delivery_worker` dispatch arm will call.)
    #[test]
    fn probe_seam_roundtrips_host_and_handler() {
        let _serial = TEST_LOCK.lock();
        let probe = test_probe::install();
        test_probe::emit(DAEMON_HOST, STALL_HANDLER);
        let (host, handler) = probe
            .rx
            .recv_timeout(WATCHDOG)
            .expect("installed probe must receive the emitted alert");
        assert_eq!(host, DAEMON_HOST);
        assert_eq!(handler, STALL_HANDLER);
    }

    /// A wedged handler must raise an out-of-band alert that names the daemon
    /// host and the exact stalled handler. RED-anchor: FAILS by watchdog timeout
    /// (no monitor emits); GREEN wires the monitor + delivery job and the same
    /// assertion receives `daemon-tick` / `slow_handler`.
    #[test]
    fn stall_alert_names_blocked_handler_on_daemon_host() {
        let _serial = TEST_LOCK.lock();
        let st = drive_stalled_tick();
        expect_stall_alert(st.rx(), DAEMON_HOST, STALL_HANDLER);
        // `st` drops here → releases the stalled handler + joins the runner.
    }
}
