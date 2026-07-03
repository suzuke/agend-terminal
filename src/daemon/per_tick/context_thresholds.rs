//! #2549 W5 — collapses the two context-percent threshold watchdogs
//! (`ContextAlertHandler`, `ContextHandoffHandler`) into ONE registered
//! [`PerTickHandler`] slot (`ContextThresholdsHandler`).
//!
//! ## Premise check (P2-2549-SPIKE.md discipline — verify before merging)
//!
//! The #2549 issue describes both handlers as reading "同一份
//! transcript-estimate 檔" (the same transcript-estimate FILE) every 6
//! ticks. That is stale: per the #1945-disable decision (2026-06-10), the
//! transcript-estimate fallback is DISABLED in both — `context_alert.rs`'s
//! and `context_handoff.rs`'s own module docs, and `StateTracker::resolved_context`'s
//! doc comment, all say so explicitly. What both ACTUALLY read is
//! `handle.core.lock().state.resolved_context()` — the agent's own
//! in-memory statusline-pattern reading, not a file at all (the stale
//! "transcript-estimate file IO" framing also appears in this crate's own
//! `build_default_handlers` registration comment, now corrected below). The
//! underlying redundancy the issue is really pointing at is real (both
//! handlers separately lock the registry, iterate every live agent, and
//! call the SAME cheap in-memory accessor once each, every 6th tick) — just
//! smaller than "shared file I/O" implies: an in-memory Mutex lock + a
//! statusline-cache read, not a filesystem call.
//!
//! Given that, and given this is the SPIKE's own highest-risk merge group
//! (`ContextAlertHandler`'s re-alertable latch and `ContextHandoffHandler`'s
//! one-shot-per-episode latch are genuinely different state machines, see
//! §3c), this follows the W1–W4 pure-COMPOSITION precedent rather than
//! fusing the two registry scans into one shared snapshot: `context_alert.rs`
//! and `context_handoff.rs` are UNTOUCHED beyond two tiny `#[cfg(test)]`-only
//! accessor methods (`ContextAlertHandler::is_armed`, `ContextHandoffHandler::phase_of`)
//! used by this file's cross-independence pin below — zero production
//! behavior change. Each inner handler keeps its own `PerTickHandler` impl,
//! `CadenceGate` (both currently 6 ticks, exposed as separate constructor
//! params rather than hoisted onto one shared gate — same shape as W3's
//! genuinely-different-but-currently-equal cadences), and completely
//! separate per-agent latch state (`AlertState` vs `EpisodeState`/`Phase`).
//!
//! Panic isolation moves from PER-HANDLER to PER-CHECK (mirrors
//! `hourly_gc::run_sweep_isolated` / `notification_watchdogs::run_check_isolated`):
//! this handler wraps each of its 2 inner `.run()` calls in its own
//! `catch_unwind`, so the pre-merge invariant — one threshold watchdog
//! panicking never blocks the other in the same tick — survives the
//! collapse into a single registered handler.

use super::context_alert::ContextAlertHandler;
use super::context_handoff::ContextHandoffHandler;
use super::{PerTickHandler, TickContext};

pub(crate) struct ContextThresholdsHandler {
    context_alert: ContextAlertHandler,
    context_handoff: ContextHandoffHandler,
}

impl ContextThresholdsHandler {
    pub(crate) fn new(alert_ticks: u64, handoff_ticks: u64) -> Self {
        Self {
            context_alert: ContextAlertHandler::new(alert_ticks),
            context_handoff: ContextHandoffHandler::new(handoff_ticks),
        }
    }
}

impl PerTickHandler for ContextThresholdsHandler {
    fn name(&self) -> &'static str {
        "context_thresholds"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        run_check_isolated("context_alert", || self.context_alert.run(ctx));
        run_check_isolated("context_handoff", || self.context_handoff.run(ctx));
    }
}

/// Run one sub-check isolated from its sibling: a panic inside `f` is
/// caught and logged, never propagated — the per-check equivalent of the
/// outer per-tick loop's per-HANDLER `catch_unwind`. Preserves "one context
/// threshold watchdog panicking doesn't block the other" now that both run
/// inside a single registered handler's `run()` call.
fn run_check_isolated(name: &'static str, f: impl FnOnce()) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        #[cfg(test)]
        test_hooks::record_and_maybe_force_panic(name);
        f()
    }));
    if let Err(payload) = outcome {
        tracing::error!(
            check = name,
            error = %super::panic_payload_str(&payload),
            "context_thresholds: sub-check panicked — isolated, the other check in this tick still ran"
        );
    }
}

/// Test-only fault-injection seam: proves the per-check isolation property
/// against the REAL merged handler (not a mock). Mirrors `hourly_gc`'s and
/// `notification_watchdogs`'s identically-shaped `test_hooks`.
#[cfg(test)]
mod test_hooks {
    use std::cell::{Cell, RefCell};

    thread_local! {
        static FORCE_PANIC: Cell<Option<&'static str>> = const { Cell::new(None) };
        static INVOKED: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn record_and_maybe_force_panic(name: &'static str) {
        INVOKED.with(|v| v.borrow_mut().push(name));
        if FORCE_PANIC.with(|p| p.get()) == Some(name) {
            panic!("fault-injection: forced panic in check '{name}'");
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use parking_lot::Mutex as PLMutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-context-thresholds-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn empty_ctx_parts() -> (
        crate::agent::AgentRegistry,
        crate::agent::ExternalRegistry,
        Arc<PLMutex<HashMap<String, crate::daemon::AgentConfig>>>,
    ) {
        (
            Arc::new(PLMutex::new(HashMap::new())),
            Arc::new(PLMutex::new(HashMap::new())),
            Arc::new(PLMutex::new(HashMap::new())),
        )
    }

    #[test]
    fn name_is_context_thresholds() {
        assert_eq!(
            ContextThresholdsHandler::new(6, 6).name(),
            "context_thresholds"
        );
    }

    /// #2549 W5 pin (mirrors `hourly_gc`/`notification_watchdogs`): the outer
    /// per-tick loop used to isolate panics PER-HANDLER — 2 separately-
    /// registered handlers meant a panic in one never touched the other's
    /// invocation this tick. After collapsing both into
    /// `ContextThresholdsHandler`, that guarantee must be reproduced INSIDE
    /// `run()` at per-check granularity.
    #[test]
    fn alert_panic_does_not_block_handoff() {
        let home = tmp_home("panic-alert");
        let (registry, externals, configs) = empty_ctx_parts();
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        let handler = ContextThresholdsHandler::new(1, 1);
        test_hooks::force_panic("context_alert");
        handler.run(&ctx); // must not propagate

        test_hooks::clear_force_panic();
        assert_eq!(
            test_hooks::take_invoked(),
            vec!["context_alert", "context_handoff"],
            "'context_alert' panicking must not stop 'context_handoff' from running"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn handoff_panic_does_not_block_alert() {
        let home = tmp_home("panic-handoff");
        let (registry, externals, configs) = empty_ctx_parts();
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        let handler = ContextThresholdsHandler::new(1, 1);
        test_hooks::force_panic("context_handoff");
        handler.run(&ctx);

        test_hooks::clear_force_panic();
        assert_eq!(
            test_hooks::take_invoked(),
            vec!["context_alert", "context_handoff"],
            "'context_handoff' panicking must not retroactively un-run 'context_alert' (before it)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn no_panic_both_run_in_order() {
        let home = tmp_home("baseline");
        let (registry, externals, configs) = empty_ctx_parts();
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        let handler = ContextThresholdsHandler::new(1, 1);
        handler.run(&ctx);

        assert_eq!(
            test_hooks::take_invoked(),
            vec!["context_alert", "context_handoff"]
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2549 W5 cross-independence pin (P2-2549-SPIKE.md §3c, the exact
    /// property the task calls out): drive the REAL merged handler against a
    /// live agent at 82% — ABOVE ContextAlert's 80% threshold, BELOW
    /// ContextHandoff's 85% threshold (and its own 80% hysteresis floor, so
    /// handoff's `decide()` takes the "hold current phase, no action"
    /// branch). Assert:
    /// (a) ContextAlert's latch fired (armed → disarmed) for this agent —
    ///     proves the merged handler's alert leg still actually ran the real
    ///     decision, not just "was invoked".
    /// (b) ContextHandoff's episode phase for this SAME agent stayed at the
    ///     default `Armed` — proves alert firing never touches handoff's
    ///     independent latch.
    #[test]
    fn alert_firing_does_not_perturb_handoff_state() {
        let home = tmp_home("cross-independence-alert-only");
        let (registry, externals, configs) = empty_ctx_parts();
        let (handle, _reader) =
            crate::daemon::per_tick::mock_live_agent_with_context("watched", 82.0);
        registry.lock().insert(handle.id, handle);
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        let handler = ContextThresholdsHandler::new(1, 1);
        handler.run(&ctx);

        assert_eq!(
            handler.context_alert.is_armed("watched"),
            Some(false),
            "82% crosses the 80% alert threshold — alert must have fired \
             (armed → disarmed) for a real decision to have run"
        );
        assert_eq!(
            handler.context_handoff.phase_of("watched"),
            Some(super::super::context_handoff::Phase::Armed),
            "82% is below the 85% handoff threshold (and above its 80% \
             hysteresis floor) — handoff's episode phase must stay Armed, \
             completely unaffected by the alert leg firing on the SAME agent \
             in the SAME run() call"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Mirror direction: a live agent at 90% crosses BOTH thresholds (alert
    /// 80%, handoff 85%). `mock_live_agent_with_context` only feeds a
    /// statusline frame (no activity pattern), which the REAL state
    /// classifier honestly resolves as `Idle` — matching an actual idle
    /// Claude pane sitting at its prompt — so handoff's own `decide()` takes
    /// the idle branch (`IdleMarked`, not `Inject`; #2008's "idle
    /// context-full is not urgent" rule). What this test proves is
    /// independence, not which specific handoff phase results: alert firing
    /// (`armed → disarmed`) must not perturb handoff's own phase transition
    /// (`Armed → IdleMarked`), even though both ran off the SAME registry
    /// scan on the SAME agent in the SAME `run()` call.
    #[test]
    fn both_firing_on_same_agent_keep_independent_latches() {
        let home = tmp_home("cross-independence-both");
        let (registry, externals, configs) = empty_ctx_parts();
        let (handle, _reader) =
            crate::daemon::per_tick::mock_live_agent_with_context("watched", 90.0);
        registry.lock().insert(handle.id, handle);
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        let handler = ContextThresholdsHandler::new(1, 1);
        handler.run(&ctx);

        assert_eq!(
            handler.context_alert.is_armed("watched"),
            Some(false),
            "90% crosses the alert threshold — alert fired"
        );
        assert_eq!(
            handler.context_handoff.phase_of("watched"),
            Some(super::super::context_handoff::Phase::IdleMarked),
            "90% crosses the handoff threshold on an Idle mock agent — handoff \
             must have independently transitioned Armed → IdleMarked, got {:?}",
            handler.context_handoff.phase_of("watched")
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
