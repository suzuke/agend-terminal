//! #2549 W1 — collapses the four 360-tick GC/sweep handlers into ONE
//! registered [`PerTickHandler`] slot (`GcTickHandler`,
//! `WorkspaceBoundarySweepHandler`, `TmpReviewGcHandler`,
//! `ReconcileBackupsGcHandler` → `HourlyGcHandler`, 43 → 40 handlers in
//! `build_default_handlers`).
//!
//! This is a pure COMPOSITION wrapper, not a rewrite: each inner handler
//! keeps its own cadence gate and extra state completely unchanged. They are
//! NOT identical — only `WorkspaceBoundarySweepHandler` builds its gate via
//! `CadenceGate::new_with_boot_grace` (the other three use plain
//! `CadenceGate::new`), and `GcTickHandler` carries its own
//! `target_sweep_armed` first-firing suppression — none of that is touched
//! here (P2-2549-SPIKE.md §3a).
//!
//! Panic isolation is PER-SWEEP (this handler wraps each of its 4 inner sweep
//! calls in its own `catch_unwind`), so the pre-merge invariant — one GC
//! sub-task panicking never blocks the other three in the same tick — survives
//! the collapse into a single registered handler.
//!
//! ## #P1-2607 offload
//!
//! The four sweeps do potentially-slow work (git subprocess per worktree /
//! candidate, fs walks) that used to run INLINE on the daemon's main tick loop
//! — the same freeze class as `worktree_registry_sweep` (#2614). Here the
//! cheap CADENCE checks (`sub.due()`) stay on the tick loop so gate advancement
//! never drifts, but when any sweep is due the actual WORK (`sub.work()`) runs
//! in a background thread — `run()` never blocks the loop. `in_flight` skips a
//! new round's work while a previous one is still running (its gates already
//! advanced, so cadence is unaffected). Mirrors #2614's single offload shape.

use super::gc_tick::GcTickHandler;
use super::reconcile_backups_gc::ReconcileBackupsGcHandler;
use super::tmp_review_gc::TmpReviewGcHandler;
use super::workspace_boundary_sweep::WorkspaceBoundarySweepHandler;
use super::{PerTickHandler, TickContext};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub(crate) struct HourlyGcHandler {
    gc_tick: Arc<GcTickHandler>,
    workspace_boundary: Arc<WorkspaceBoundarySweepHandler>,
    tmp_review: Arc<TmpReviewGcHandler>,
    reconcile_backups: Arc<ReconcileBackupsGcHandler>,
    /// #P1-2607 re-entrancy guard: true while a round's work is running in its
    /// background thread. A later tick whose gate fires skips spawning a second
    /// overlapping round (cleared by `ClearOnDrop`, even on a panicking round).
    in_flight: Arc<AtomicBool>,
}

impl HourlyGcHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gc_tick: Arc::new(GcTickHandler::new(every_n_ticks)),
            workspace_boundary: Arc::new(WorkspaceBoundarySweepHandler::new(every_n_ticks)),
            tmp_review: Arc::new(TmpReviewGcHandler::new(every_n_ticks)),
            reconcile_backups: Arc::new(ReconcileBackupsGcHandler::new(every_n_ticks)),
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }

    #[cfg(test)]
    fn is_in_flight(&self) -> bool {
        self.in_flight.load(Ordering::Acquire)
    }
}

impl PerTickHandler for HourlyGcHandler {
    fn name(&self) -> &'static str {
        "hourly_gc"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        // Advance every sub-gate ON THE TICK LOOP, in the pinned order (cheap;
        // this is what keeps cadence from ever drifting), recording which are
        // due this tick.
        let due_gc = self.gc_tick.due();
        let due_wb = self.workspace_boundary.due();
        let due_tmp = self.tmp_review.due();
        let due_rec = self.reconcile_backups.due();
        if !(due_gc || due_wb || due_tmp || due_rec) {
            return;
        }
        // #P1-2607: a previous round's work is still running in its background
        // thread — skip THIS round's work (the gates already advanced above, so
        // the cadence is unaffected), retry next cadence.
        if self.in_flight.swap(true, Ordering::AcqRel) {
            tracing::warn!(
                "hourly_gc: previous round still in flight, skipping this round's \
                 work (gates advanced; retries next cadence)"
            );
            return;
        }
        let home = ctx.home.to_path_buf();
        let gc_tick = Arc::clone(&self.gc_tick);
        let workspace_boundary = Arc::clone(&self.workspace_boundary);
        let tmp_review = Arc::clone(&self.tmp_review);
        let reconcile_backups = Arc::clone(&self.reconcile_backups);
        let in_flight = Arc::clone(&self.in_flight);
        // fire-and-forget: #P1-2607 — the four sub-sweeps' potentially-slow work
        // (git subprocess per worktree/candidate) runs off the daemon's main
        // tick loop. `ClearOnDrop` releases `in_flight` even if a sweep panics
        // past its per-sweep isolation. Per-sweep results stay observable via
        // tracing/event_log, exactly as before this offload.
        std::thread::spawn(move || {
            let _guard = super::ClearOnDrop::new(in_flight);
            #[cfg(test)]
            test_hooks::maybe_delay();
            run_due_sweeps_isolated(
                &home,
                (due_gc, &gc_tick),
                (due_wb, &workspace_boundary),
                (due_tmp, &tmp_review),
                (due_rec, &reconcile_backups),
            );
        });
    }
}

/// Run the DUE sub-sweeps in their pinned order (gc_tick → workspace_boundary →
/// tmp_review → reconcile_backups), each isolated from the others by its own
/// `catch_unwind` — a panic in one is logged, never blocks the rest (the
/// per-sweep guarantee from #2549 W1, preserved through the #P1-2607 offload).
/// Split out of the spawned closure so tests exercise the isolation
/// synchronously against the REAL sub-handlers, without a thread. The order
/// (gc_tick cleans worktrees before workspace_boundary sweeps for them) is
/// pinned by the tests below.
fn run_due_sweeps_isolated(
    home: &Path,
    gc: (bool, &GcTickHandler),
    wb: (bool, &WorkspaceBoundarySweepHandler),
    tmp: (bool, &TmpReviewGcHandler),
    rec: (bool, &ReconcileBackupsGcHandler),
) {
    if gc.0 {
        run_sweep_isolated("gc_tick", || gc.1.work(home));
    }
    if wb.0 {
        run_sweep_isolated("workspace_boundary_sweep", || wb.1.work(home));
    }
    if tmp.0 {
        run_sweep_isolated("tmp_review_gc", || tmp.1.work(home));
    }
    if rec.0 {
        run_sweep_isolated("reconcile_backups_gc", || rec.1.work(home));
    }
}

/// Run one sub-sweep isolated from its siblings: a panic inside `f` is caught
/// and logged, never propagated — the per-sweep equivalent of the outer
/// per-tick loop's per-HANDLER `catch_unwind` (P2-2549-SPIKE.md §3a).
fn run_sweep_isolated(name: &'static str, f: impl FnOnce()) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        #[cfg(test)]
        test_hooks::record_and_maybe_force_panic(name);
        f()
    }));
    if let Err(payload) = outcome {
        tracing::error!(
            sweep = name,
            error = %super::panic_payload_str(&payload),
            "hourly_gc: sweep panicked — isolated, the other sweeps in this round still ran"
        );
    }
}

/// Test-only seams: fault injection for the per-sweep isolation property, and a
/// delay seam for the #P1-2607 freeze regression. `FORCE_PANIC`/`INVOKED` are
/// thread-local because the isolation tests drive `run_due_sweeps_isolated`
/// SYNCHRONOUSLY on the test thread; `DELAY_MS` is a global because the freeze
/// test observes it from the spawned background thread (mirrors #2614's seam).
#[cfg(test)]
mod test_hooks {
    use std::cell::{Cell, RefCell};
    use std::sync::atomic::{AtomicU64, Ordering};

    thread_local! {
        static FORCE_PANIC: Cell<Option<&'static str>> = const { Cell::new(None) };
        static INVOKED: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    }

    static DELAY_MS: AtomicU64 = AtomicU64::new(0);

    /// Records that `name`'s sweep was reached (so a test can assert every DUE
    /// sweep was attempted, in order, even when one panics), then panics if
    /// `force_panic(name)` armed this name.
    pub(super) fn record_and_maybe_force_panic(name: &'static str) {
        INVOKED.with(|v| v.borrow_mut().push(name));
        if FORCE_PANIC.with(|p| p.get()) == Some(name) {
            panic!("fault-injection: forced panic in sweep '{name}'");
        }
    }

    pub(super) fn force_panic(name: &'static str) {
        FORCE_PANIC.with(|p| p.set(Some(name)));
    }

    pub(super) fn clear_force_panic() {
        FORCE_PANIC.with(|p| p.set(None));
    }

    pub(super) fn take_invoked() -> Vec<&'static str> {
        INVOKED.with(|v| std::mem::take(&mut *v.borrow_mut()))
    }

    pub(super) fn set_delay_ms(ms: u64) {
        DELAY_MS.store(ms, Ordering::Release);
    }

    pub(super) fn clear_delay() {
        DELAY_MS.store(0, Ordering::Release);
    }

    pub(super) fn maybe_delay() {
        let ms = DELAY_MS.load(Ordering::Acquire);
        if ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::agent::{AgentRegistry, ExternalRegistry};
    use parking_lot::Mutex as PLMutex;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-hourly-gc-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Runs all four sweeps as DUE, synchronously (no thread), so the per-sweep
    /// isolation is asserted via the thread-local INVOKED recorder. Explicit
    /// due-flags sidestep `workspace_boundary`'s boot-grace (which would
    /// otherwise leave it not-due in a fresh handler).
    fn run_all_due(home: &Path) {
        let gc = GcTickHandler::new(1);
        let wb = WorkspaceBoundarySweepHandler::new(1);
        let tmp = TmpReviewGcHandler::new(1);
        let rec = ReconcileBackupsGcHandler::new(1);
        run_due_sweeps_isolated(home, (true, &gc), (true, &wb), (true, &tmp), (true, &rec));
    }

    const ORDER: [&str; 4] = [
        "gc_tick",
        "workspace_boundary_sweep",
        "tmp_review_gc",
        "reconcile_backups_gc",
    ];

    #[test]
    fn name_is_hourly_gc() {
        assert_eq!(HourlyGcHandler::new(360).name(), "hourly_gc");
    }

    /// #2549 W1 pin (P2-2549-SPIKE.md §3a), preserved through the #P1-2607
    /// offload: force the FIRST sweep (`gc_tick`) to panic and assert
    /// (a) the round does not propagate the panic, and
    /// (b) all four sweeps still ran, in their original order — the three AFTER
    ///     the panicking one prove the isolation is real.
    #[test]
    fn one_sweep_panic_does_not_block_the_other_three() {
        let home = tmp_home("panic-isolation");
        test_hooks::force_panic("gc_tick");
        run_all_due(&home);
        test_hooks::clear_force_panic();
        assert_eq!(
            test_hooks::take_invoked(),
            ORDER,
            "all four sweeps must be attempted, in original order, even though \
             'gc_tick' (the first) panicked — per-sweep isolation (§3a)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Same property forcing a MIDDLE sweep — the neighbours on both sides ran.
    #[test]
    fn middle_sweep_panic_does_not_block_its_neighbors() {
        let home = tmp_home("panic-isolation-middle");
        test_hooks::force_panic("tmp_review_gc");
        run_all_due(&home);
        test_hooks::clear_force_panic();
        assert_eq!(
            test_hooks::take_invoked(),
            ORDER,
            "'tmp_review_gc' panicking must not stop 'reconcile_backups_gc' (after \
             it) nor un-run the two before it"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Baseline (no forced panic): all four run in order — the composition
    /// doesn't drop or reorder any sub-sweep.
    #[test]
    fn no_panic_all_four_run_in_order() {
        let home = tmp_home("baseline");
        run_all_due(&home);
        assert_eq!(test_hooks::take_invoked(), ORDER);
        std::fs::remove_dir_all(&home).ok();
    }

    /// #P1-2607 freeze-regression pin: `run()` must return near-instantly even
    /// while the round's work is slow (the pathological case that froze the
    /// daemon — heavy git subprocess work on the main tick loop, simulated via
    /// the `test_hooks` delay seam). Also pins the re-entrancy guard: a second
    /// fire while the first round is still running must skip, not spawn an
    /// overlapping round. Serial: the delay seam is a process-global.
    #[test]
    #[serial_test::serial(hourly_gc_delay)]
    fn run_does_not_block_tick_loop_during_slow_round_p1_2607() {
        let home = tmp_home("slow-round");
        let registry: AgentRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let configs: Arc<PLMutex<HashMap<String, crate::daemon::AgentConfig>>> =
            Arc::new(PLMutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        test_hooks::set_delay_ms(300);
        let h = HourlyGcHandler::new(1); // fires every call

        let start = Instant::now();
        h.run(&ctx);
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "run() must not block the tick loop on the (delayed) round, took {:?}",
            start.elapsed()
        );
        assert!(
            h.is_in_flight(),
            "the background round should still be running (300ms delay, checked immediately)"
        );

        // The very next tick arriving while the previous round is still in
        // flight: gates fire again, but the re-entrancy guard must skip spawning
        // a second overlapping round — and still return near-instantly.
        let start2 = Instant::now();
        h.run(&ctx);
        assert!(
            start2.elapsed() < Duration::from_millis(100),
            "the re-entrant skip path must also return near-instantly"
        );

        // Poll for the background round to finish (300ms delay; 2s is a generous
        // CI-jitter ceiling) and confirm the guard clears.
        let deadline = Instant::now() + Duration::from_secs(2);
        while h.is_in_flight() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !h.is_in_flight(),
            "in_flight must clear once the background round completes"
        );

        test_hooks::clear_delay();
        std::fs::remove_dir_all(&home).ok();
    }

    /// Smoke: `run()` against empty fixtures completes (the background round
    /// finishes and clears `in_flight`) without panic.
    #[test]
    #[serial_test::serial(hourly_gc_delay)]
    fn run_is_no_op_on_empty_fixtures() {
        let home = tmp_home("empty");
        let registry: AgentRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let configs: Arc<PLMutex<HashMap<String, crate::daemon::AgentConfig>>> =
            Arc::new(PLMutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        let h = HourlyGcHandler::new(1);
        h.run(&ctx);

        let deadline = Instant::now() + Duration::from_secs(2);
        while h.is_in_flight() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !h.is_in_flight(),
            "background round should finish on empty fixtures"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
