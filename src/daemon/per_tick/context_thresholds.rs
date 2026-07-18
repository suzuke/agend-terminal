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
use parking_lot::Mutex;
use std::collections::HashSet;
use std::sync::Arc;

pub(crate) struct ContextThresholdsHandler {
    context_alert: ContextAlertHandler,
    context_handoff: ContextHandoffHandler,
}

impl ContextThresholdsHandler {
    pub(crate) fn new(alert_ticks: u64, handoff_ticks: u64) -> Self {
        let invalid_override_warnings = Arc::new(Mutex::new(HashSet::new()));
        Self {
            context_alert: ContextAlertHandler::new_with_warnings(
                alert_ticks,
                Arc::clone(&invalid_override_warnings),
            ),
            context_handoff: ContextHandoffHandler::new_with_warnings(
                handoff_ticks,
                invalid_override_warnings,
            ),
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
    use serial_test::serial;
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::super::context_alert::ContextAlertHandler;
    use super::super::context_handoff::{ContextHandoffHandler, Phase};

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

    fn reset_runtime_defaults(home: &std::path::Path) {
        std::fs::write(home.join("runtime-config.json"), r#"{"schema_version": 1}"#).unwrap();
        crate::runtime_config::reload(home);
        for key in [
            "AGEND_CONTEXT_ALERT_PCT",
            "AGEND_CONTEXT_HANDOFF_PCT",
            "AGEND_CONTEXT_HANDOFF_ESCALATE_PCT",
        ] {
            std::env::remove_var(key);
        }
    }

    fn add_context_agent(registry: &crate::agent::AgentRegistry, name: &str, pct: f32) {
        let (handle, _reader) = crate::daemon::per_tick::mock_live_agent_with_context(name, pct);
        registry.lock().insert(handle.id, handle);
    }

    fn context_ctx<'a>(
        home: &'a std::path::Path,
        registry: &'a crate::agent::AgentRegistry,
        externals: &'a crate::agent::ExternalRegistry,
        configs: &'a Arc<PLMutex<HashMap<String, crate::daemon::AgentConfig>>>,
    ) -> TickContext<'a> {
        TickContext {
            home,
            registry,
            externals,
            configs,
        }
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

    #[test]
    #[serial(runtime_config)]
    fn per_instance_alert_thresholds_are_isolated() {
        let home = tmp_home("per-instance-alert-isolation");
        reset_runtime_defaults(&home);
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  low:\n    context_alert_pct: 70.0\n  high:\n    context_alert_pct: 84.0\n",
        )
        .unwrap();
        let (registry, externals, configs) = empty_ctx_parts();
        add_context_agent(&registry, "low", 82.0);
        add_context_agent(&registry, "high", 82.0);
        let handler = ContextAlertHandler::new(1);
        handler.run(&context_ctx(&home, &registry, &externals, &configs));

        assert_eq!(
            handler.is_armed("low"),
            Some(false),
            "the low per-instance alert threshold must fire at 82%"
        );
        assert_eq!(
            handler.is_armed("high"),
            Some(true),
            "the high per-instance alert threshold must remain armed at 82%"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[serial(runtime_config)]
    fn partial_instance_overlay_preserves_global_handoff_and_escalate() {
        let home = tmp_home("partial-handoff-overlay");
        reset_runtime_defaults(&home);
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  partial:\n    context_handoff_pct: 88.0\n",
        )
        .unwrap();
        let (registry, externals, configs) = empty_ctx_parts();
        add_context_agent(&registry, "partial", 86.0);
        let handler = ContextHandoffHandler::new(1);
        handler.run(&context_ctx(&home, &registry, &externals, &configs));

        assert_eq!(
            handler.phase_of("partial"),
            Some(Phase::Armed),
            "partial overlay must use the per-instance handoff threshold while retaining the global escalate threshold"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[serial(runtime_config)]
    fn invalid_instance_triplet_falls_back_atomically_without_poisoning_other_agents() {
        let home = tmp_home("invalid-instance-triplet");
        reset_runtime_defaults(&home);
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  invalid:\n    context_alert_pct: 95.0\n    context_handoff_pct: 90.0\n    context_handoff_escalate_pct: 92.0\n  valid:\n    context_alert_pct: 84.0\n",
        )
        .unwrap();
        let (registry, externals, configs) = empty_ctx_parts();
        add_context_agent(&registry, "invalid", 82.0);
        add_context_agent(&registry, "valid", 82.0);
        let handler = ContextAlertHandler::new(1);
        handler.run(&context_ctx(&home, &registry, &externals, &configs));

        assert_eq!(
            handler.is_armed("invalid"),
            Some(false),
            "invalid per-instance composition must fall back to the complete global triplet"
        );
        assert_eq!(
            handler.is_armed("valid"),
            Some(true),
            "one agent's invalid override must not poison another agent's valid override"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[serial(runtime_config)]
    fn missing_instance_falls_back_to_global_and_deleted_latch_is_pruned() {
        let home = tmp_home("missing-instance-fallback");
        reset_runtime_defaults(&home);
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  watched:\n    context_alert_pct: 84.0\n",
        )
        .unwrap();
        let (registry, externals, configs) = empty_ctx_parts();
        add_context_agent(&registry, "watched", 82.0);
        let handler = ContextAlertHandler::new(1);
        let ctx = context_ctx(&home, &registry, &externals, &configs);
        handler.run(&ctx);
        assert_eq!(
            handler.is_armed("watched"),
            Some(true),
            "the instance override must keep 82% below its 84% threshold"
        );

        std::fs::write(crate::fleet::fleet_yaml_path(&home), "instances: {}\n").unwrap();
        handler.run(&ctx);
        assert_eq!(
            handler.is_armed("watched"),
            Some(false),
            "a missing instance entry must use the complete global threshold triplet"
        );

        registry.lock().clear();
        handler.run(&ctx);
        assert_eq!(
            handler.is_armed("watched"),
            None,
            "deleted agents must be pruned"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[serial(runtime_config)]
    fn fleet_reload_changes_per_instance_threshold_on_next_fired_tick() {
        let home = tmp_home("fleet-reload-threshold");
        reset_runtime_defaults(&home);
        let fleet_path = crate::fleet::fleet_yaml_path(&home);
        std::fs::write(
            &fleet_path,
            "instances:\n  watched:\n    context_alert_pct: 84.0\n",
        )
        .unwrap();
        let (registry, externals, configs) = empty_ctx_parts();
        add_context_agent(&registry, "watched", 82.0);
        let handler = ContextAlertHandler::new(1);
        let ctx = context_ctx(&home, &registry, &externals, &configs);
        handler.run(&ctx);
        assert_eq!(handler.is_armed("watched"), Some(true));

        std::fs::write(
            &fleet_path,
            "instances:\n  watched:\n    context_alert_pct: 70\n",
        )
        .unwrap();
        handler.run(&ctx);
        assert_eq!(
            handler.is_armed("watched"),
            Some(false),
            "the next fired tick must observe a changed fleet override"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[serial(runtime_config)]
    #[tracing_test::traced_test]
    fn invalid_override_warning_is_shared_deduped_and_rearmed_after_valid() {
        let home = tmp_home("invalid-warning-dedup");
        reset_runtime_defaults(&home);
        let fleet_path = crate::fleet::fleet_yaml_path(&home);
        std::fs::write(
            &fleet_path,
            "instances:\n  watched:\n    context_alert_pct: 95.0\n    context_handoff_pct: 90.0\n    context_handoff_escalate_pct: 92.0\n",
        )
        .unwrap();
        let (registry, externals, configs) = empty_ctx_parts();
        add_context_agent(&registry, "watched", 82.0);
        let handler = ContextThresholdsHandler::new(1, 1);
        let ctx = context_ctx(&home, &registry, &externals, &configs);

        handler.run(&ctx);
        assert_eq!(
            handler.context_alert.invalid_warning_count(),
            1,
            "alert and handoff must share one invalid-warning entry per agent"
        );

        std::fs::write(
            &fleet_path,
            "instances:\n  watched:\n    context_alert_pct: 84.0\n    context_handoff_pct: 86.0\n    context_handoff_escalate_pct: 92.0\n",
        )
        .unwrap();
        handler.run(&ctx);
        assert!(
            handler.context_alert.invalid_warning_count() == 0,
            "a valid composition must reset the invalid warning episode"
        );

        std::fs::write(
            &fleet_path,
            "instances:\n  watched:\n    context_alert_pct: 95.0\n    context_handoff_pct: 90.0\n    context_handoff_escalate_pct: 92.0\n",
        )
        .unwrap();
        handler.run(&ctx);
        logs_assert(|lines: &[&str]| {
            let count = lines
                .iter()
                .filter(|line| line.contains("context thresholds invalid for instance"))
                .count();
            if count == 2 {
                Ok(())
            } else {
                Err(format!(
                    "invalid warning must emit once per invalid episode across both handlers; got {count}"
                ))
            }
        });
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[serial(runtime_config)]
    fn global_runtime_reload_updates_only_agents_without_overrides() {
        let home = tmp_home("runtime-reload-with-instance-override");
        reset_runtime_defaults(&home);
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  overridden:\n    context_alert_pct: 84.0\n",
        )
        .unwrap();
        let (registry, externals, configs) = empty_ctx_parts();
        add_context_agent(&registry, "plain", 82.0);
        add_context_agent(&registry, "overridden", 82.0);
        let ctx = context_ctx(&home, &registry, &externals, &configs);

        let first = ContextAlertHandler::new(1);
        first.run(&ctx);
        assert_eq!(first.is_armed("plain"), Some(false));
        assert_eq!(first.is_armed("overridden"), Some(true));

        std::fs::write(
            home.join("runtime-config.json"),
            r#"{"schema_version": 1, "context_alert_pct": 83.0, "context_handoff_pct": 85.0, "context_handoff_escalate_pct": 92.0}"#,
        )
        .unwrap();
        crate::runtime_config::reload(&home);
        let second = ContextAlertHandler::new(1);
        second.run(&ctx);
        assert_eq!(
            second.is_armed("plain"),
            Some(true),
            "a global runtime reload must affect an agent without an override"
        );
        assert_eq!(
            second.is_armed("overridden"),
            Some(true),
            "an instance override remains authoritative across global reload"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
