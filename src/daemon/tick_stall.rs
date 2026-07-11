//! PR4 — opt-in out-of-band tick-stall diagnostics.
//!
//! A dedicated, joined per-host monitor watches the tick host's
//! [`per_tick`](super::per_tick) runner make progress and pages the operator
//! out-of-band when a handler (or a preflight / post-handler step) wedges the
//! tick thread — WITHOUT ever running on that thread, blocking it, or reusing
//! the blocking bounded tick producer. Decision d-20260711043612580372-4.
//!
//! Opt-in and default-OFF: enabled only when `AGEND_TICK_STALL_SECS` is set to a
//! value `>= 10` (see [`threshold_from_env`]). The daemon `run_core` host and an
//! *owned* app host each start their own monitor; an *attached* app never ticks,
//! so it never starts one.
//!
//! ## Consistent snapshot (odd/even seqlock)
//!
//! The tick thread is the sole writer of [`TickProgress`]; the monitor thread is
//! a lock-free reader. `state` packs the current [`Phase`] (+ handler index);
//! `generation` is an odd/even seqlock — odd while a write is in flight, even
//! when settled — so the monitor reads a torn-free (phase, generation) pair and
//! uses the *settled* generation as the host's progress signal. Both writer and
//! reader touch only atomics, so the monitor never blocks on a lock the tick
//! thread holds and can never itself be stalled by the stall it is watching.
//!
//! ## Alert path
//!
//! The monitor sampler performs NO network or shared-state-lock I/O. On a
//! confirmed stall it `try_send`s a bounded
//! [`DeliveryJob::TickStallAlert`](super::delivery_worker) through the existing
//! `delivery_worker`; that worker (off the tick thread) owns the `event_log`
//! write and the escalation-channel fan-out. A full delivery queue is a
//! `tracing::error` observable drop, never a block.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Env var gating the diagnostics. Unset / `0` = disabled.
const ENV_VAR: &str = "AGEND_TICK_STALL_SECS";
/// Minimum enabled threshold (seconds). Below this the diagnostics stay OFF — a
/// sub-10s window would false-alarm on ordinary slow ticks.
const MIN_THRESHOLD_SECS: u64 = 10;
/// Extra slack added to the base threshold for the `Waiting` phase. A full tick
/// period (10s) of `Waiting` between ticks is normal cadence, not a stall, so the
/// waiting threshold must clear it. A host stuck in `Waiting` beyond
/// `threshold + TICK_PERIOD_SLACK` (a dead tick producer) still pages as
/// `<waiting>`.
const TICK_PERIOD_SLACK: Duration = Duration::from_secs(10);

/// The tick host's coarse lifecycle position within one tick. `Handler` carries
/// the 0-based index into the host's immutable handler-name table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Phase {
    /// Idle, blocked awaiting the next tick signal. Ordinary between-tick cadence
    /// stays silent (the generation advances every tick); a host frozen here past
    /// the larger waiting-threshold — a dead tick producer — pages as `<waiting>`.
    Waiting,
    /// Per-tick config reloads before the handler sweep.
    Preflight,
    /// Running the handler at this index.
    Handler(u32),
    /// After the handler sweep — exit-event / crash-dispatch handling.
    PostHandlers,
}

impl Phase {
    // Packed u64 layout: [ tag : high 32 | handler_index : low 32 ].
    fn pack(self) -> u64 {
        match self {
            Phase::Waiting => 0,
            Phase::Preflight => 1 << 32,
            Phase::Handler(i) => (2u64 << 32) | u64::from(i),
            Phase::PostHandlers => 3u64 << 32,
        }
    }

    fn unpack(v: u64) -> Phase {
        match v >> 32 {
            0 => Phase::Waiting,
            1 => Phase::Preflight,
            2 => Phase::Handler((v & 0xFFFF_FFFF) as u32),
            _ => Phase::PostHandlers,
        }
    }

    /// Stable phase label for the alert payload / event log.
    fn label(self) -> &'static str {
        match self {
            Phase::Waiting => "waiting",
            Phase::Preflight => "preflight",
            Phase::Handler(_) => "handler",
            Phase::PostHandlers => "post-handlers",
        }
    }
}

/// A consistent (settled-generation, phase) reading taken by the monitor.
#[derive(Clone, Copy)]
struct Snapshot {
    generation: u64,
    phase: Phase,
}

/// Per-host tick-progress tracker. One `Arc` is written by the tick thread; a
/// clone is read by the monitor thread. See the module docs for the seqlock.
pub(crate) struct TickProgress {
    /// Stable payload label for this host (`daemon-tick` / `app-owned-tick`).
    host: &'static str,
    /// Immutable handler-name table, indexed by `Phase::Handler(i)`. Built once
    /// at construction and never mutated, so the monitor reads it unsynchronized.
    handler_names: Arc<[&'static str]>,
    /// Packed [`Phase`] (+ handler index).
    state: AtomicU64,
    /// Odd/even seqlock: odd = write in flight, even = settled. The settled value
    /// is the host's monotonic progress signal.
    generation: AtomicU64,
}

impl TickProgress {
    pub(crate) fn new(host: &'static str, handler_names: Arc<[&'static str]>) -> Arc<Self> {
        Arc::new(Self {
            host,
            handler_names,
            state: AtomicU64::new(Phase::Waiting.pack()),
            generation: AtomicU64::new(0), // even → no write in flight
        })
    }

    /// Publish a new phase via the odd/even seqlock write protocol: bump to odd
    /// (write begins), store the packed state, bump to even (write settled).
    fn publish(&self, phase: Phase) {
        self.generation.fetch_add(1, Ordering::AcqRel); // → odd
        self.state.store(phase.pack(), Ordering::Release);
        self.generation.fetch_add(1, Ordering::Release); // → even
    }

    pub(crate) fn enter_waiting(&self) {
        self.publish(Phase::Waiting);
    }
    pub(crate) fn enter_preflight(&self) {
        self.publish(Phase::Preflight);
    }
    pub(crate) fn enter_handler(&self, index: u32) {
        self.publish(Phase::Handler(index));
    }
    pub(crate) fn enter_post(&self) {
        self.publish(Phase::PostHandlers);
    }

    /// Lock-free consistent read. Returns `None` if a write is in flight or the
    /// read tore across a concurrent publish — the caller retries next sample.
    fn snapshot(&self) -> Option<Snapshot> {
        let g1 = self.generation.load(Ordering::Acquire);
        if g1 & 1 == 1 {
            return None; // writer mid-publish (odd)
        }
        let state = self.state.load(Ordering::Acquire);
        let g2 = self.generation.load(Ordering::Acquire);
        if g1 != g2 {
            return None; // torn across a concurrent publish
        }
        Some(Snapshot {
            generation: g1,
            phase: Phase::unpack(state),
        })
    }

    /// The alert `handler` label for a phase: the stalled handler's name for
    /// `Handler`, else a phase descriptor.
    fn handler_label(&self, phase: Phase) -> &str {
        match phase {
            Phase::Handler(i) => self
                .handler_names
                .get(i as usize)
                .copied()
                .unwrap_or("<unknown-handler>"),
            Phase::Preflight => "<preflight>",
            Phase::PostHandlers => "<post-handlers>",
            Phase::Waiting => "<waiting>",
        }
    }
}

/// A stall the [`StallDetector`] decided to raise.
struct Alert {
    generation: u64,
    phase: Phase,
}

/// Pure stall-decision state machine — no threads, no clock of its own. The
/// monitor thread feeds it `(now, snapshot)` each sample; unit tests feed
/// synthetic values for fully deterministic coverage.
struct StallDetector {
    /// Base threshold for the active phases (Preflight / Handler / PostHandlers).
    threshold: Duration,
    /// Threshold for the `Waiting` phase — `threshold + TICK_PERIOD_SLACK` — so
    /// ordinary between-tick cadence never alerts but a dead tick producer does.
    waiting_threshold: Duration,
    last_sample: Instant,
    /// Settled generation at the last observed progress (`None` before the first
    /// clean snapshot).
    last_progress_gen: Option<u64>,
    last_progress_at: Instant,
    /// Generation at which we last alerted — dedups one outage; cleared on
    /// progress so a later stall re-arms.
    alerted_gen: Option<u64>,
}

impl StallDetector {
    fn new(
        threshold: Duration,
        waiting_threshold: Duration,
        now: Instant,
        initial: Option<Snapshot>,
    ) -> Self {
        Self {
            threshold,
            waiting_threshold,
            last_sample: now,
            last_progress_gen: initial.map(|s| s.generation),
            last_progress_at: now,
            alerted_gen: None,
        }
    }

    /// The stall threshold for a phase: `Waiting` gets the extra tick-period
    /// slack, every other phase uses the base threshold.
    fn phase_threshold(&self, phase: Phase) -> Duration {
        if phase == Phase::Waiting {
            self.waiting_threshold
        } else {
            self.threshold
        }
    }

    /// Feed one sample. Returns `Some` exactly once per fresh stall outage.
    fn observe(&mut self, now: Instant, snap: Option<Snapshot>) -> Option<Alert> {
        let gap = now.duration_since(self.last_sample);
        self.last_sample = now;

        // Suspend / gross-gap reset: a SINGLE monitor sample gap wider than the
        // base threshold means the monitor thread itself was frozen for a whole
        // stall window (laptop sleep / severe CPU starvation) — the tick host was
        // frozen too, so any "no progress" reading is unreliable. Under normal or
        // even CI-loaded sampling the gap is ~`sample_interval` (≤ `threshold/4`),
        // so this never fires during a genuine tick stall. Re-baseline, no alert.
        if gap > self.threshold {
            self.last_progress_at = now;
            self.alerted_gen = None;
            self.last_progress_gen = snap.map(|s| s.generation);
            return None;
        }

        let snap = snap?; // torn / mid-publish → retry next sample

        if Some(snap.generation) != self.last_progress_gen {
            // The host advanced since we last saw it — record progress, re-arm.
            self.last_progress_gen = Some(snap.generation);
            self.last_progress_at = now;
            self.alerted_gen = None;
            return None;
        }

        // No progress since `last_progress_at`. Every phase can stall — even
        // `Waiting` (a dead tick producer) — but Waiting uses the larger
        // waiting_threshold so ordinary cadence stays silent.
        if now.duration_since(self.last_progress_at) >= self.phase_threshold(snap.phase)
            && self.alerted_gen != Some(snap.generation)
        {
            self.alerted_gen = Some(snap.generation);
            return Some(Alert {
                generation: snap.generation,
                phase: snap.phase,
            });
        }
        None
    }
}

/// Monitor timing + destination. Built from the environment for production hosts
/// ([`MonitorConfig::from_env`]); tests build it directly with tiny durations.
pub(crate) struct MonitorConfig {
    threshold: Duration,
    waiting_threshold: Duration,
    sample_interval: Duration,
    home: PathBuf,
}

impl MonitorConfig {
    /// Build from `AGEND_TICK_STALL_SECS`; `None` when disabled / invalid / so
    /// large the waiting threshold would overflow `Duration`.
    pub(crate) fn from_env(home: &Path) -> Option<Self> {
        let threshold = threshold_from_env()?;
        let Some(waiting_threshold) = waiting_threshold_for(threshold) else {
            tracing::warn!(
                threshold_secs = threshold.as_secs(),
                "{ENV_VAR} is so large the waiting threshold overflows Duration — \
                 tick-stall diagnostics DISABLED"
            );
            return None;
        };
        Some(Self {
            waiting_threshold,
            sample_interval: sample_interval_for(threshold),
            threshold,
            home: home.to_path_buf(),
        })
    }

    #[cfg(test)]
    fn for_test(
        threshold: Duration,
        waiting_threshold: Duration,
        sample_interval: Duration,
        home: PathBuf,
    ) -> Self {
        Self {
            threshold,
            waiting_threshold,
            sample_interval,
            home,
        }
    }
}

/// Production sample cadence: ~4 samples per threshold window (detection latency
/// ≤ ~1.25× threshold), floored at 1s so the monitor never busy-loops.
fn sample_interval_for(threshold: Duration) -> Duration {
    (threshold / 4).max(Duration::from_secs(1))
}

/// The `Waiting`-phase threshold for a base threshold (`threshold + slack`).
/// Returns `None` when the base is so large the add would overflow `Duration`
/// (e.g. `AGEND_TICK_STALL_SECS=u64::MAX`): the caller disables the diagnostics
/// with a warning rather than panicking at startup — `Duration + Duration` panics
/// on overflow, so the addition MUST stay checked.
fn waiting_threshold_for(threshold: Duration) -> Option<Duration> {
    threshold.checked_add(TICK_PERIOD_SLACK)
}

/// Parse `AGEND_TICK_STALL_SECS`. Unset / `0` → disabled (no warning). `1..=9` →
/// **invalid**: warn and disable (never silently clamp). `>= 10` → enabled.
/// Malformed → warn and disable.
pub(crate) fn threshold_from_env() -> Option<Duration> {
    parse_threshold(std::env::var(ENV_VAR).ok().as_deref())
}

fn parse_threshold(raw: Option<&str>) -> Option<Duration> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    match raw.parse::<u64>() {
        Ok(0) => None,
        Ok(n) if n < MIN_THRESHOLD_SECS => {
            tracing::warn!(
                value = n,
                min = MIN_THRESHOLD_SECS,
                "{ENV_VAR}={n} is below the {MIN_THRESHOLD_SECS}s minimum — tick-stall \
                 diagnostics DISABLED (set >= {MIN_THRESHOLD_SECS}, or 0/unset to disable)"
            );
            None
        }
        Ok(n) => Some(Duration::from_secs(n)),
        Err(_) => {
            tracing::warn!(
                value = %raw,
                "{ENV_VAR} is not a non-negative integer — tick-stall diagnostics DISABLED"
            );
            None
        }
    }
}

/// A running per-host stall monitor. Dropping the guard signals the thread to
/// stop (waking its `recv_timeout` at once) and joins it — the thread never
/// outlives the host.
pub(crate) struct TickStallMonitorGuard {
    stop_tx: Option<Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl TickStallMonitorGuard {
    /// Spawn the monitor for `progress`. Holds a clone of the `Arc` for reading;
    /// the caller keeps its own clone for writing. Returns `None` when the OS
    /// refuses the thread (thread exhaustion) — the caller then leaves the host
    /// UNTRACKED rather than pretending tracking is on with a dead handle.
    pub(crate) fn spawn(progress: Arc<TickProgress>, config: MonitorConfig) -> Option<Self> {
        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        // store JoinHandle: the guard joins on drop (no fire-and-forget — a stall
        // monitor must not outlive the host it watches).
        match std::thread::Builder::new()
            .name("agend-tick-stall".into())
            .spawn(move || monitor_loop(&progress, &config, &stop_rx))
        {
            Ok(handle) => Some(Self {
                stop_tx: Some(stop_tx),
                handle: Some(handle),
            }),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "tick_stall: failed to spawn monitor thread — diagnostics inactive for this host"
                );
                None
            }
        }
    }
}

impl Drop for TickStallMonitorGuard {
    fn drop(&mut self) {
        // Signal stop first (wakes the monitor's recv_timeout immediately), then
        // join so teardown is prompt.
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The sampling loop. Runs on its own thread; touches only atomics, the stop
/// channel, and `delivery_worker::enqueue_tick_stall_alert`. NO event_log /
/// channel calls here — the delivery worker owns those off this thread.
fn monitor_loop(progress: &TickProgress, config: &MonitorConfig, stop_rx: &Receiver<()>) {
    let mut detector = StallDetector::new(
        config.threshold,
        config.waiting_threshold,
        Instant::now(),
        progress.snapshot(),
    );
    loop {
        match stop_rx.recv_timeout(config.sample_interval) {
            // Stop signalled, or the guard (sole sender) dropped: exit promptly.
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if let Some(alert) = detector.observe(Instant::now(), progress.snapshot()) {
            let handler = progress.handler_label(alert.phase);
            // Log the detection on THIS thread first. The monitor thread is never
            // wedged, so a stall stays locally observable even if the single
            // delivery worker accepts the job but is stuck on prior channel I/O —
            // detection must not depend on the escalation path draining.
            tracing::error!(
                host = progress.host,
                phase = alert.phase.label(),
                handler,
                generation = alert.generation,
                "tick_stall DETECTED — tick host made no progress past its threshold"
            );
            // Then hand the escalation off the tick+monitor threads to the worker.
            let _ = super::delivery_worker::enqueue_tick_stall_alert(
                &config.home,
                progress.host,
                alert.phase.label(),
                handler,
                alert.generation,
            );
        }
    }
}

/// Start the tick-stall diagnostics for a host, IF `AGEND_TICK_STALL_SECS`
/// enables them. Returns `(Some(progress), Some(guard))` only when enabled AND
/// the monitor thread spawned; `(None, None)` when the diagnostics are disabled
/// (default OFF / invalid / overflowing value) OR the monitor thread failed to
/// spawn — either way the tick loop stays untracked (no orphan atomic writes with
/// nothing reading them). The progress `Arc` is the loop's writer handle; the
/// guard stops+joins the monitor on drop. One-line wiring for the daemon
/// `run_core` and owned-app tick loops.
pub(crate) fn start_for_host(
    host: &'static str,
    handlers: &[Box<dyn super::per_tick::PerTickHandler>],
    home: &Path,
) -> (Option<Arc<TickProgress>>, Option<TickStallMonitorGuard>) {
    let Some(config) = MonitorConfig::from_env(home) else {
        return (None, None); // disabled / invalid / overflowing → truly untracked
    };
    let names: Arc<[&'static str]> = handlers.iter().map(|h| h.name()).collect();
    let progress = TickProgress::new(host, names);
    match TickStallMonitorGuard::spawn(Arc::clone(&progress), config) {
        // Enabled: hand the writer `Arc` back to the tick loop + keep the guard.
        Some(guard) => (Some(progress), Some(guard)),
        // Monitor thread failed to spawn: leave the host UNTRACKED (no orphan
        // progress writes with nothing reading them), not enabled-with-dead-handle.
        None => (None, None),
    }
}

/// Process-global alert-observation seam (test-only). The production alert path
/// (the `delivery_worker` `TickStallAlert` dispatch arm) emits the escalated
/// identity here so tests can assert the exact out-of-band payload without a
/// real escalation side effect. Mirrors the `canonical_heartbeat` `test_hooks`
/// static-seam pattern.
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

    /// Emit an observed `(host, handler)` to the installed probe, if any. The
    /// `delivery_worker` `TickStallAlert` dispatch arm calls this (under
    /// `#[cfg(test)]`) with the exact escalated identity. No-op when no probe is
    /// installed.
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
    use super::{Alert, Phase, Snapshot, StallDetector, TickProgress};
    use crate::agent::{AgentRegistry, ExternalRegistry};
    use crate::daemon::per_tick::{run_handlers_with_progress, PerTickHandler, TickContext};
    use parking_lot::{Condvar, Mutex};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Receiver;
    use std::sync::Arc;
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    /// The probe seam is process-global; serialize stall-diagnostics tests so
    /// they don't observe each other's emissions.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Failure ceiling for the frozen assertion. In RED nothing ever emits, so
    /// the RED test blocks the full watchdog then panics. In GREEN the monitor
    /// emits within a few injected sample intervals (≪ watchdog); the watchdog is
    /// only the timeout guard, never the happy path.
    const WATCHDOG: Duration = Duration::from_secs(2);
    /// Stable handler identity the alert must name.
    const STALL_HANDLER: &str = "slow_handler";
    /// Stable daemon host label (matches the GREEN payload label — Q5).
    const DAEMON_HOST: &str = "daemon-tick";

    // ── Shared test doubles ────────────────────────────────────────────────

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

    /// A handler that panics on its (only) run — proves the runner advances the
    /// tracked identity after `catch_unwind`.
    struct PanicHandler;
    impl PerTickHandler for PanicHandler {
        fn name(&self) -> &'static str {
            "flaky_handler"
        }
        fn run(&self, _ctx: &TickContext<'_>) {
            panic!("PR4 test: handler panics");
        }
    }

    // ── e2e harness (the mutable RED↔GREEN seam) ───────────────────────────

    /// A live monitored tick: the real runner drives `TickProgress`, a real
    /// `TickStallMonitorGuard` samples it, and stall alerts flow through the real
    /// `delivery_worker` to the probe. Dropping it releases the stalled handler,
    /// joins the runner, and stops+joins the monitor.
    struct MonitoredTick {
        probe: test_probe::ProbeGuard,
        gate: Arc<(Mutex<bool>, Condvar)>,
        _monitor: super::TickStallMonitorGuard,
        runner: Option<JoinHandle<()>>,
    }

    impl MonitoredTick {
        fn rx(&self) -> &Receiver<(String, String)> {
            &self.probe.rx
        }
    }

    impl Drop for MonitoredTick {
        fn drop(&mut self) {
            {
                let (lock, cv) = &*self.gate;
                *lock.lock() = true;
                cv.notify_all();
            }
            if let Some(handle) = self.runner.take() {
                let _ = handle.join();
            }
            // `_monitor` (field) drops after this body: stop + join.
        }
    }

    /// Spawn the real runner over `handlers` under a short-threshold monitor for
    /// `host`. The runner drives the tracked companion `run_handlers_with_progress`
    /// so the monitor sees phase/handler progress and, on a stall, the delivery
    /// path feeds the probe.
    fn spawn_monitored(
        host: &'static str,
        handlers: Vec<Box<dyn PerTickHandler>>,
        gate: Arc<(Mutex<bool>, Condvar)>,
    ) -> MonitoredTick {
        let probe = test_probe::install();
        let names: Arc<[&'static str]> = handlers.iter().map(|h| h.name()).collect();
        let progress = TickProgress::new(host, names);
        // Generous margins so ordinary CI scheduling jitter can't trip the
        // suspend reset (gap > threshold) or blow the watchdog: detection is
        // ~threshold + a sample (~220ms) vs the 2s watchdog.
        let config = super::MonitorConfig::for_test(
            Duration::from_millis(200),
            Duration::from_secs(5),
            Duration::from_millis(20),
            std::env::temp_dir(),
        );
        let monitor = super::TickStallMonitorGuard::spawn(Arc::clone(&progress), config)
            .expect("monitor thread must spawn in tests");
        let progress_for_runner = Arc::clone(&progress);
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
            run_handlers_with_progress(&handlers, &ctx, Some(&*progress_for_runner));
        });
        MonitoredTick {
            probe,
            gate,
            _monitor: monitor,
            runner: Some(runner),
        }
    }

    fn drive_stalled_tick() -> MonitoredTick {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let handlers: Vec<Box<dyn PerTickHandler>> = vec![Box::new(BlockingHandler {
            gate: Arc::clone(&gate),
        })];
        spawn_monitored(DAEMON_HOST, handlers, gate)
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

    // ── #1: probe seam sanity + frozen e2e assertion ───────────────────────

    /// The observation seam itself must faithfully route an emitted identity to
    /// the installed receiver, so a stall-test failure can only mean "no monitor
    /// emit", never "the probe is broken".
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

    // ── #4/#9: panic advances identity; exact payload ──────────────────────

    /// A panicking handler must not leave stale identity: after `catch_unwind`
    /// the runner advances, so a LATER stalled handler is the one the alert
    /// names (never the panicked predecessor).
    #[test]
    fn panicking_handler_advances_identity_then_alert_names_later_stall() {
        let _serial = TEST_LOCK.lock();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let handlers: Vec<Box<dyn PerTickHandler>> = vec![
            Box::new(PanicHandler),
            Box::new(BlockingHandler {
                gate: Arc::clone(&gate),
            }),
        ];
        let st = spawn_monitored(DAEMON_HOST, handlers, gate);
        expect_stall_alert(st.rx(), DAEMON_HOST, STALL_HANDLER);
    }

    // ── #7: guard stop/join is prompt ──────────────────────────────────────

    /// Dropping the guard must wake the monitor's `recv_timeout` at once and
    /// join. Proven with a completion channel + watchdog (NOT a wall-clock speed
    /// assertion): a helper thread drops the guard and signals when the join
    /// returns. If Stop failed to wake `recv_timeout`, the join would block the
    /// full 30s sample interval and the watchdog `recv_timeout` would fire first.
    #[test]
    fn monitor_guard_stops_and_joins_promptly() {
        let _serial = TEST_LOCK.lock();
        let names: Arc<[&'static str]> = Arc::from(vec!["h0"]);
        let progress = TickProgress::new(DAEMON_HOST, names);
        let config = super::MonitorConfig::for_test(
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(30), // long: a naive join would block this long
            std::env::temp_dir(),
        );
        let guard = super::TickStallMonitorGuard::spawn(Arc::clone(&progress), config)
            .expect("monitor thread must spawn in tests");

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let dropper = std::thread::spawn(move || {
            drop(guard); // stop + join
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "Stop must wake recv_timeout and join promptly; a hang would exceed the watchdog"
        );
        // Reached only on success (the assert panics on timeout before this): the
        // dropper already signalled, so the join completes at once — no leaked thread.
        let _ = dropper.join();
    }

    // ── Detector unit tests (deterministic, no threads) ────────────────────

    fn snap(generation: u64, phase: Phase) -> Snapshot {
        Snapshot { generation, phase }
    }

    /// #2 + waiting-stall: ordinary Waiting cadence (a tick advances the
    /// generation each period) never alerts, but a host frozen in Waiting past
    /// the waiting-threshold (a dead tick producer) pages as `<waiting>`.
    #[test]
    fn detector_waiting_cadence_silent_but_dead_producer_alerts() {
        let thr = Duration::from_millis(40);
        let wait_thr = Duration::from_millis(140);
        let si = Duration::from_millis(5);
        let period_samples = 20u64; // 20 * 5ms = 100ms < wait_thr → ordinary cadence
        let t0 = Instant::now();
        let mut d = StallDetector::new(thr, wait_thr, t0, Some(snap(0, Phase::Waiting)));

        // Ordinary cadence: a tick advances the generation every ~100ms while the
        // host sits in Waiting between ticks. Must never alert.
        let mut gen = 0u64;
        let mut k = 0u64;
        for _cycle in 0..5 {
            for _ in 0..period_samples {
                k += 1;
                assert!(
                    d.observe(t0 + si * k as u32, Some(snap(gen, Phase::Waiting)))
                        .is_none(),
                    "ordinary Waiting cadence must not alert"
                );
            }
            // A tick fired → generation advances (progress), resetting the clock.
            gen += 2;
            k += 1;
            assert!(d
                .observe(t0 + si * k as u32, Some(snap(gen, Phase::Waiting)))
                .is_none());
        }

        // The tick producer dies: Waiting with NO further generation advance. Past
        // the waiting-threshold it must alert as <waiting>.
        let mut fired = None;
        for _ in 0..40 {
            k += 1;
            if let Some(a) = d.observe(t0 + si * k as u32, Some(snap(gen, Phase::Waiting))) {
                fired = Some(a);
                break;
            }
        }
        assert!(
            matches!(
                fired,
                Some(Alert {
                    phase: Phase::Waiting,
                    ..
                })
            ),
            "an unchanged Waiting past the waiting-threshold pages as <waiting>"
        );
    }

    /// #3: one alert per outage (dedup), then re-arm after progress and alert
    /// again on the next stall.
    #[test]
    fn detector_alerts_once_dedups_then_rearms() {
        let thr = Duration::from_millis(40);
        let wait_thr = Duration::from_millis(140);
        let si = Duration::from_millis(5);
        let t0 = Instant::now();
        let mut d = StallDetector::new(thr, wait_thr, t0, Some(snap(2, Phase::Handler(0))));

        // Step at the sampling cadence until the first alert.
        let mut first_at = None;
        for k in 1..=20u64 {
            let now = t0 + si * k as u32;
            if let Some(a) = d.observe(now, Some(snap(2, Phase::Handler(0)))) {
                assert!(matches!(
                    a,
                    Alert {
                        generation: 2,
                        phase: Phase::Handler(0)
                    }
                ));
                first_at = Some(k);
                break;
            }
        }
        let first = first_at.expect("a >= threshold stall must alert");

        // Dedup: further samples of the same stuck generation raise nothing.
        for k in (first + 1)..(first + 5) {
            let now = t0 + si * k as u32;
            assert!(
                d.observe(now, Some(snap(2, Phase::Handler(0)))).is_none(),
                "only one alert per outage"
            );
        }

        // Progress: generation advances → re-arm (no alert on the progress sample).
        let p = first + 6;
        assert!(d
            .observe(t0 + si * p as u32, Some(snap(4, Phase::Handler(1))))
            .is_none());

        // A fresh stall at the new generation alerts again.
        let mut re = None;
        for k in (p + 1)..(p + 20) {
            let now = t0 + si * k as u32;
            if let Some(a) = d.observe(now, Some(snap(4, Phase::Handler(1)))) {
                re = Some(a);
                break;
            }
        }
        assert!(matches!(
            re.expect("re-armed stall alerts again"),
            Alert {
                generation: 4,
                phase: Phase::Handler(1)
            }
        ));
    }

    /// #5: a monitor sample gap far larger than the cadence (laptop sleep / CPU
    /// starvation) re-baselines instead of false-alarming — and detection still
    /// works afterward.
    #[test]
    fn detector_suspend_gap_resets_and_suppresses_false_alert() {
        let thr = Duration::from_millis(40);
        let wait_thr = Duration::from_millis(140);
        let si = Duration::from_millis(5);
        let t0 = Instant::now();
        let mut d = StallDetector::new(thr, wait_thr, t0, Some(snap(2, Phase::Handler(0))));

        // One normal sample, still stuck, before threshold.
        assert!(d
            .observe(t0 + si, Some(snap(2, Phase::Handler(0))))
            .is_none());

        // A single huge gap (>> sample_interval): the monitor itself was frozen.
        let after_suspend = t0 + si + si * 8 + thr;
        assert!(
            d.observe(after_suspend, Some(snap(2, Phase::Handler(0))))
                .is_none(),
            "suspend gap must reset the baseline, not alert"
        );

        // Re-baselined from the wake instant: a genuine post-wake stall still fires.
        let mut fired = None;
        for k in 1..=20u64 {
            let now = after_suspend + si * k as u32;
            if let Some(a) = d.observe(now, Some(snap(2, Phase::Handler(0)))) {
                fired = Some(a);
                break;
            }
        }
        assert!(
            fired.is_some(),
            "detector still works after a suspend reset"
        );
    }

    /// #5b: a torn read (writer mid-publish → `None` snapshot) is skipped, not
    /// mistaken for progress or a stall.
    #[test]
    fn detector_skips_torn_snapshots() {
        let thr = Duration::from_millis(40);
        let wait_thr = Duration::from_millis(140);
        let si = Duration::from_millis(5);
        let t0 = Instant::now();
        let mut d = StallDetector::new(thr, wait_thr, t0, Some(snap(2, Phase::Handler(0))));
        // Interleave torn reads with real stuck reads; still alerts once elapsed.
        let mut fired = None;
        for k in 1..=20u64 {
            let now = t0 + si * k as u32;
            let s = if k % 2 == 0 {
                None
            } else {
                Some(snap(2, Phase::Handler(0)))
            };
            if let Some(a) = d.observe(now, s) {
                fired = Some(a);
                break;
            }
        }
        assert!(
            matches!(fired, Some(Alert { generation: 2, .. })),
            "torn reads are skipped but a persistent stall still alerts"
        );
    }

    /// #10: two hosts track independently — a stall on one does not alert for the
    /// other, and each keeps its own dedup/re-arm state.
    #[test]
    fn detectors_two_hosts_are_independent() {
        let thr = Duration::from_millis(40);
        let wait_thr = Duration::from_millis(140);
        let si = Duration::from_millis(5);
        let t0 = Instant::now();
        let mut daemon = StallDetector::new(thr, wait_thr, t0, Some(snap(2, Phase::Handler(0))));
        let mut app = StallDetector::new(thr, wait_thr, t0, Some(snap(2, Phase::Handler(0))));

        // The app host keeps progressing; the daemon host is stuck.
        let mut daemon_fired = false;
        for k in 1..=20u64 {
            let now = t0 + si * k as u32;
            // app advances its generation every sample (always progressing).
            assert!(
                app.observe(now, Some(snap(2 + 2 * k, Phase::Handler(k as u32 % 3))))
                    .is_none(),
                "a progressing host never alerts"
            );
            if daemon
                .observe(now, Some(snap(2, Phase::Handler(0))))
                .is_some()
            {
                daemon_fired = true;
            }
        }
        assert!(
            daemon_fired,
            "the stuck daemon host must alert independently"
        );
    }

    // ── Seqlock concurrency stress (#3.9 concurrent-state) ─────────────────

    /// Under a concurrent writer, every accepted snapshot is torn-free AND
    /// coherent: the handler index corresponds to the generation of the SAME
    /// publish (a torn read pairing phase-A with generation-B would mismatch).
    /// A writer-start handshake guarantees the reader observes real concurrent
    /// progress, not just the initial state.
    #[test]
    fn seqlock_reader_sees_coherent_phase_generation_pairs() {
        // Encode the write count in the handler index: publish #n sets
        // generation = 2n and index = n % LIMIT, so a coherent read has
        // Handler((generation / 2) % LIMIT).
        const LIMIT: u64 = 1_000_000;
        let progress = TickProgress::new("stress-tick", Arc::from(vec!["stress"]));
        let writer = Arc::clone(&progress);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_w = Arc::clone(&stop);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let mut count = 0u64;
            loop {
                count += 1;
                writer.enter_handler((count % LIMIT) as u32);
                if count == 1 {
                    let _ = started_tx.send(()); // writer-start handshake
                }
                if stop_w.load(Ordering::Relaxed) {
                    break;
                }
            }
        });
        // Progress handshake: don't read until the writer has published at least once.
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("writer must start publishing");

        let mut last_gen = 0u64;
        let mut first_gen: Option<u64> = None;
        let mut coherent_reads = 0u64;
        for _ in 0..500_000 {
            if let Some(s) = progress.snapshot() {
                assert_eq!(
                    s.generation % 2,
                    0,
                    "a settled snapshot has even generation"
                );
                assert!(
                    s.generation >= last_gen,
                    "generation is monotonic non-decreasing"
                );
                last_gen = s.generation;
                first_gen.get_or_insert(s.generation);
                match s.phase {
                    Phase::Handler(idx) => assert_eq!(
                        u64::from(idx),
                        (s.generation / 2) % LIMIT,
                        "phase index must correspond to the generation (coherent seqlock read)"
                    ),
                    other => panic!("writer only publishes Handler; torn variant {other:?}"),
                }
                coherent_reads += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();
        assert!(coherent_reads > 0, "reader must observe concurrent writes");
        assert!(
            last_gen > first_gen.unwrap_or(0),
            "the writer must make progress during the read window"
        );
    }

    // ── Env gating (Q4) ────────────────────────────────────────────────────

    /// Unset/0 disable silently; 1..=9 are invalid (disable, never clamp);
    /// >=10 enable at that value; malformed disables.
    #[test]
    fn env_threshold_parsing_matches_q4() {
        use super::parse_threshold;
        assert_eq!(parse_threshold(None), None, "unset → disabled");
        assert_eq!(parse_threshold(Some("")), None, "empty → disabled");
        assert_eq!(parse_threshold(Some("0")), None, "0 → disabled");
        assert_eq!(
            parse_threshold(Some("1")),
            None,
            "1 → invalid, disabled (no clamp)"
        );
        assert_eq!(
            parse_threshold(Some("9")),
            None,
            "9 → invalid, disabled (no clamp)"
        );
        assert_eq!(
            parse_threshold(Some("10")),
            Some(Duration::from_secs(10)),
            "10 → enabled at 10s"
        );
        assert_eq!(
            parse_threshold(Some("  45 ")),
            Some(Duration::from_secs(45)),
            "whitespace-tolerant; enabled at 45s"
        );
        assert_eq!(parse_threshold(Some("nope")), None, "malformed → disabled");
    }

    /// r2 regression: `AGEND_TICK_STALL_SECS=u64::MAX` parses fine (>= 10) but the
    /// waiting threshold (`base + 10s`) would overflow `Duration` and panic at
    /// startup. The checked add must instead disable (`None`), never crash.
    #[test]
    fn oversized_threshold_disables_instead_of_overflowing() {
        use super::{parse_threshold, waiting_threshold_for};
        // The parser accepts any u64 >= 10, u64::MAX included (r1 semantics).
        assert_eq!(
            parse_threshold(Some("18446744073709551615")),
            Some(Duration::from_secs(u64::MAX)),
            "parser accepts u64::MAX"
        );
        // But composing the waiting threshold must NOT overflow-panic — it
        // returns None so from_env disables the diagnostics.
        assert_eq!(
            waiting_threshold_for(Duration::from_secs(u64::MAX)),
            None,
            "u64::MAX base → overflow → disabled (no panic)"
        );
        // A normal value still composes to base + 10s.
        assert_eq!(
            waiting_threshold_for(Duration::from_secs(10)),
            Some(Duration::from_secs(20))
        );
    }

    // ── #6: attached app starts no monitor (structural source-pin) ─────────

    /// An attached app never ticks, so it must never start a stall monitor. This
    /// is a structural invariant of the app wiring (same idiom as
    /// `per_tick::tests::notification_handlers_wire_boot_grace`): a future edit
    /// that drops the `attached_mode` gate would silently start a monitor on a
    /// host that never ticks. Also pins the daemon `run_core` host wiring.
    #[test]
    fn hosts_wired_daemon_yes_attached_app_no() {
        let app = std::fs::read_to_string("src/app/mod.rs")
            .or_else(|_| std::fs::read_to_string("agend-terminal/src/app/mod.rs"))
            .expect("src/app/mod.rs must be readable");
        let anchor = app
            .find("let (app_tick_progress, _app_stall_monitor)")
            .expect("app must bind the owned-app stall monitor");
        let region = &app[anchor..(anchor + 600).min(app.len())];
        assert!(
            region.contains("if attached_mode"),
            "the app monitor must be gated on attached_mode"
        );
        assert!(
            region.contains("(None, None)"),
            "an attached app must resolve to (None, None) — no monitor"
        );
        assert!(
            region.contains("start_for_host(\"app-owned-tick\""),
            "an owned app starts the app-owned-tick monitor"
        );

        let daemon = std::fs::read_to_string("src/daemon/mod.rs")
            .or_else(|_| std::fs::read_to_string("agend-terminal/src/daemon/mod.rs"))
            .expect("src/daemon/mod.rs must be readable");
        assert!(
            daemon.contains("start_for_host(\"daemon-tick\""),
            "the daemon run_core host starts the daemon-tick monitor"
        );
    }
}
