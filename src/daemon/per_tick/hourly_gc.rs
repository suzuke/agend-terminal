//! #2549 W1 — collapses the four 360-tick GC/sweep handlers into ONE
//! registered [`PerTickHandler`] slot (`GcTickHandler`,
//! `WorkspaceBoundarySweepHandler`, `TmpReviewGcHandler`,
//! `ReconcileBackupsGcHandler` → `HourlyGcHandler`, 43 → 40 handlers in
//! `build_default_handlers`).
//!
//! This is a pure COMPOSITION wrapper, not a rewrite: each inner handler
//! keeps its own `PerTickHandler` impl, cadence gate, and extra state
//! completely unchanged. They are NOT identical — only
//! `WorkspaceBoundarySweepHandler` builds its gate via
//! `CadenceGate::new_with_boot_grace` (the other three use plain
//! `CadenceGate::new`), and `GcTickHandler` carries its own
//! `target_sweep_armed` first-firing suppression for the `target/` sweep
//! sub-part — none of that is touched here (P2-2549-SPIKE.md §3a).
//!
//! Panic isolation moves from PER-HANDLER (the outer per-tick loop wrapped
//! each of the 4 previously-separately-registered handlers in its own
//! `catch_unwind`, see `run_handlers_with_panic_guard`) to PER-SWEEP (this
//! handler wraps each of its 4 inner `.run()` calls in its own
//! `catch_unwind`), so the pre-merge invariant — one GC sub-task panicking
//! never blocks the other three in the same tick — survives the collapse
//! into a single registered handler.

use super::gc_tick::GcTickHandler;
use super::reconcile_backups_gc::ReconcileBackupsGcHandler;
use super::tmp_review_gc::TmpReviewGcHandler;
use super::workspace_boundary_sweep::WorkspaceBoundarySweepHandler;
use super::{PerTickHandler, TickContext};

pub(crate) struct HourlyGcHandler {
    gc_tick: GcTickHandler,
    workspace_boundary: WorkspaceBoundarySweepHandler,
    tmp_review: TmpReviewGcHandler,
    reconcile_backups: ReconcileBackupsGcHandler,
}

impl HourlyGcHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gc_tick: GcTickHandler::new(every_n_ticks),
            workspace_boundary: WorkspaceBoundarySweepHandler::new(every_n_ticks),
            tmp_review: TmpReviewGcHandler::new(every_n_ticks),
            reconcile_backups: ReconcileBackupsGcHandler::new(every_n_ticks),
        }
    }
}

impl PerTickHandler for HourlyGcHandler {
    fn name(&self) -> &'static str {
        "hourly_gc"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        run_sweep_isolated("gc_tick", || self.gc_tick.run(ctx));
        run_sweep_isolated("workspace_boundary_sweep", || {
            self.workspace_boundary.run(ctx)
        });
        run_sweep_isolated("tmp_review_gc", || self.tmp_review.run(ctx));
        run_sweep_isolated("reconcile_backups_gc", || self.reconcile_backups.run(ctx));
    }
}

/// Run one sub-sweep isolated from its siblings: a panic inside `f` is
/// caught and logged, never propagated — the per-sweep equivalent of the
/// outer per-tick loop's per-HANDLER `catch_unwind`. Preserves "one GC
/// sub-task panicking doesn't block the other three" now that all four run
/// inside a single registered handler's `run()` call (P2-2549-SPIKE.md
/// §3a).
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
            "hourly_gc: sweep panicked — isolated, the other sweeps in this tick still ran"
        );
    }
}

/// Test-only fault-injection seam: proves the per-sweep isolation property
/// against the REAL merged handler (not a mock), without needing to trigger
/// a genuine panic from inside any of the four sweeps' real filesystem
/// logic. Mirrors the `AGEND_FORCE_SUCCESSOR_FAIL`-style injection seams
/// already established elsewhere in the daemon for hard-to-trigger paths.
#[cfg(test)]
mod test_hooks {
    use std::cell::{Cell, RefCell};

    thread_local! {
        static FORCE_PANIC: Cell<Option<&'static str>> = const { Cell::new(None) };
        static INVOKED: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    }

    /// Records that `name`'s sweep was reached (so a test can assert ALL
    /// FOUR were attempted, in order, even when one panics), then panics if
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

    #[test]
    fn name_is_hourly_gc() {
        assert_eq!(HourlyGcHandler::new(360).name(), "hourly_gc");
    }

    /// #2549 W1 pin (P2-2549-SPIKE.md §3a): the outer per-tick loop used to
    /// isolate panics PER-HANDLER — 4 separately-registered handlers meant a
    /// panic in one never touched the other 3's invocation this tick. After
    /// collapsing all 4 into `HourlyGcHandler`, that guarantee must be
    /// reproduced INSIDE `run()` at per-sweep granularity. Force the FIRST
    /// sweep (`gc_tick`) to panic and assert:
    /// (a) `run()` itself does not propagate the panic, and
    /// (b) all four sweeps were still reached, in their original order —
    ///     the three AFTER the panicking one are proof the isolation is
    ///     real, not just "nothing after the panic point crashed by luck".
    #[test]
    fn one_sweep_panic_does_not_block_the_other_three() {
        let home = tmp_home("panic-isolation");
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

        let handler = HourlyGcHandler::new(1);
        test_hooks::force_panic("gc_tick");

        // Must NOT propagate — HourlyGcHandler::run itself stays panic-free
        // from the caller's perspective, exactly like the outer per-tick
        // loop's existing per-handler guarantee.
        handler.run(&ctx);

        test_hooks::clear_force_panic();
        assert_eq!(
            test_hooks::take_invoked(),
            vec![
                "gc_tick",
                "workspace_boundary_sweep",
                "tmp_review_gc",
                "reconcile_backups_gc"
            ],
            "all four sweeps must be attempted, in original order, even though \
             'gc_tick' (the first) panicked — per-sweep isolation (§3a)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Same property, forcing a MIDDLE sweep — the two on either side both
    /// still ran, closing the "only proved trailing sweeps survive" gap the
    /// first test alone would leave.
    #[test]
    fn middle_sweep_panic_does_not_block_its_neighbors() {
        let home = tmp_home("panic-isolation-middle");
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

        let handler = HourlyGcHandler::new(1);
        test_hooks::force_panic("tmp_review_gc");

        handler.run(&ctx);

        test_hooks::clear_force_panic();
        assert_eq!(
            test_hooks::take_invoked(),
            vec![
                "gc_tick",
                "workspace_boundary_sweep",
                "tmp_review_gc",
                "reconcile_backups_gc"
            ],
            "'tmp_review_gc' panicking must not stop 'reconcile_backups_gc' \
             (after it) from running, nor does it retroactively un-run the \
             two before it"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Baseline (no forced panic): all four still run in order, on a single
    /// `run()` call — the composition itself doesn't drop or reorder any of
    /// the four sub-sweeps.
    #[test]
    fn no_panic_all_four_run_in_order() {
        let home = tmp_home("baseline");
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

        let handler = HourlyGcHandler::new(1);
        handler.run(&ctx);

        assert_eq!(
            test_hooks::take_invoked(),
            vec![
                "gc_tick",
                "workspace_boundary_sweep",
                "tmp_review_gc",
                "reconcile_backups_gc"
            ]
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
