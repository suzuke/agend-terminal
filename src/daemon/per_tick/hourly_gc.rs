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
            // The guard is SCOPED so `in_flight` is cleared (its `Drop`) BEFORE
            // the test-only completion signal below — a test waking on that
            // signal then observes `!is_in_flight()` deterministically, with no
            // wall-clock poll. In non-test builds both `#[cfg(test)]` lines
            // vanish and the extra scope is a no-op (the guard still drops at
            // closure end), so production behaviour is byte-identical.
            {
                let _guard = super::ClearOnDrop::new(in_flight);
                #[cfg(test)]
                test_hooks::round_gate();
                run_due_sweeps_isolated(
                    &home,
                    (due_gc, &gc_tick),
                    (due_wb, &workspace_boundary),
                    (due_tmp, &tmp_review),
                    (due_rec, &reconcile_backups),
                );
            }
            #[cfg(test)]
            test_hooks::signal_round_complete();
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

/// Test-only seams. Two independent groups:
///
/// * Per-sweep panic isolation: `FORCE_PANIC`/`INVOKED` are thread-local
///   because the isolation tests drive `run_due_sweeps_isolated` SYNCHRONOUSLY
///   on the test thread.
/// * #P1-2607 offload determinism (t-20260706034654060827-81457-3): a `GATE`
///   the spawned round blocks on (so a test can hold a round provably
///   in-flight) and a monotone `COMPLETIONS` counter bumped AFTER the round's
///   `ClearOnDrop` clears `in_flight`. These replace the old wall-clock
///   `DELAY_MS` sleep, whose `elapsed() < 100ms` / 2s-poll assertions flaked
///   under full-parallel-nextest CPU contention. They are process-global
///   because the round observes them from its background thread; the two tests
///   that use them are `serial(hourly_gc_delay)`, so they never race each other.
#[cfg(test)]
mod test_hooks {
    use parking_lot::{Condvar, Mutex};
    use std::cell::{Cell, RefCell};

    thread_local! {
        static FORCE_PANIC: Cell<Option<&'static str>> = const { Cell::new(None) };
        static INVOKED: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    }

    /// `true` while the gate is armed — a spawned round blocks in `round_gate`.
    static GATE_ARMED: Mutex<bool> = Mutex::new(false);
    static GATE_CV: Condvar = Condvar::new();
    /// Monotone count of rounds that have finished (and cleared `in_flight`).
    static COMPLETIONS: Mutex<u64> = Mutex::new(0);
    static COMPLETIONS_CV: Condvar = Condvar::new();

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

    /// Reset both offload seams to idle. Called at the start of each test that
    /// uses them so a prior test — or an early return/panic before
    /// `release_gate` — can't leave the gate armed or the counter dirty.
    pub(super) fn reset() {
        *GATE_ARMED.lock() = false;
        GATE_CV.notify_all();
        *COMPLETIONS.lock() = 0;
    }

    /// Arm the gate so the NEXT spawned round blocks in `round_gate` until
    /// `release_gate`, letting a test hold a round provably in-flight.
    pub(super) fn arm_gate() {
        *GATE_ARMED.lock() = true;
    }

    /// Release a gated round (and any future ones) so it runs to completion.
    pub(super) fn release_gate() {
        *GATE_ARMED.lock() = false;
        GATE_CV.notify_all();
    }

    /// Called by a spawned round (replaces the old `maybe_delay`): blocks while
    /// the gate is armed, else falls straight through.
    pub(super) fn round_gate() {
        let mut armed = GATE_ARMED.lock();
        while *armed {
            GATE_CV.wait(&mut armed);
        }
    }

    /// Bumped by a spawned round AFTER its `ClearOnDrop` cleared `in_flight`.
    pub(super) fn signal_round_complete() {
        *COMPLETIONS.lock() += 1;
        COMPLETIONS_CV.notify_all();
    }

    /// Snapshot of the completed-round count (baseline before an action).
    pub(super) fn completions() -> u64 {
        *COMPLETIONS.lock()
    }

    /// Block until the completed-round count exceeds `prev`. Deterministic — no
    /// wall-clock ceiling; a genuine hang is caught by nextest's slow-timeout.
    pub(super) fn wait_for_completion(prev: u64) {
        let mut n = COMPLETIONS.lock();
        while *n <= prev {
            COMPLETIONS_CV.wait(&mut n);
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

    /// #P1-2607 freeze-regression pin, made DETERMINISTIC
    /// (t-20260706034654060827-81457-3): `run()` must offload the round's work
    /// to a background thread and return without waiting for it. The old version
    /// proved this with wall-clock `elapsed() < 100ms` assertions, which flaked
    /// under full-parallel-nextest CPU contention (3 samples). Here a test GATE
    /// holds the spawned round provably in-flight, so the property is proven
    /// STRUCTURALLY at any machine speed: had `run()` done the work inline it
    /// would block on the armed gate and never return (a hang nextest's
    /// slow-timeout attributes), so reaching the post-`run()` assertions — with
    /// the round still `in_flight` — is itself the proof of offload. Also pins
    /// the re-entrancy guard deterministically: exactly ONE round completes even
    /// though `run()` fired twice while the first was in flight. Serial: the
    /// gate/counter seams are process-global.
    #[test]
    #[serial_test::serial(hourly_gc_delay)]
    fn run_does_not_block_tick_loop_during_slow_round_p1_2607() {
        test_hooks::reset();
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

        // Arm the gate so the spawned round blocks mid-flight until we release.
        test_hooks::arm_gate();
        let h = HourlyGcHandler::new(1); // fires every call

        // If `run()` ran the round inline it would block on the armed gate and
        // never return — reaching the next line proves it offloaded.
        h.run(&ctx);
        assert!(
            h.is_in_flight(),
            "the offloaded round must still be in flight (blocked on the gate)"
        );

        // A second tick while the first round is still in flight: the
        // re-entrancy guard must skip spawning a second overlapping round — and
        // this call must also return (not block on the gate).
        h.run(&ctx);
        assert!(
            h.is_in_flight(),
            "re-entrant tick must neither block nor clear the in-flight round"
        );

        // Release the round and wait — deterministically — for it to finish.
        let before = test_hooks::completions();
        test_hooks::release_gate();
        test_hooks::wait_for_completion(before);

        assert_eq!(
            test_hooks::completions(),
            before + 1,
            "exactly ONE round may complete — the re-entrant second run() must \
             have skipped, not spawned an overlapping round"
        );
        assert!(
            !h.is_in_flight(),
            "in_flight must clear once the background round completes"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// Smoke, made deterministic: `run()` against empty fixtures spawns a round
    /// that finishes and clears `in_flight`. No wall-clock poll — the test
    /// blocks on the completion signal (bumped after the guard clears the flag).
    #[test]
    #[serial_test::serial(hourly_gc_delay)]
    fn run_is_no_op_on_empty_fixtures() {
        test_hooks::reset();
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
        let before = test_hooks::completions();
        h.run(&ctx);
        test_hooks::wait_for_completion(before);
        assert!(
            !h.is_in_flight(),
            "background round should finish on empty fixtures"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
