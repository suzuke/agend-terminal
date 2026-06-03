//! Hang detection + health decay: walks the agent registry every tick,
//! decays each agent's health, classifies hangs, logs warnings on the
//! transition. Extracted verbatim from `src/daemon/mod.rs:591-626`
//! (pre-T-B4) — same iteration order, same lock-acquisition chain,
//! same `tracing::warn!` field names.
//!
//! **Cohort note** (T-B4): this handler MUTATES `core.health` (via
//! `maybe_decay` + the implicit `check_hang` side-effects on transition
//! tracking), and is followed in the same tick by [`super::watchdog`]
//! which also mutates `core.health` (BlockedReason classification). The
//! two handlers are extracted together so the same-tick mutation
//! sequence stays contained in a single PR — splitting would route the
//! sequence across module boundaries with no compile-time signal that
//! the ordering matters.

use super::{PerTickHandler, TickContext};
use crate::agent;

pub(crate) struct HangDetectionHandler;

impl HangDetectionHandler {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl PerTickHandler for HangDetectionHandler {
    fn name(&self) -> &'static str {
        "hang_detection"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        // Lock-acquisition order per docs/DAEMON-LOCK-ORDERING.md:
        // registry (L0, root) → per-agent core (L1) → heartbeat_pair
        // (L3 leaf, acquired+released synchronously by `snapshot_for`).
        // Rule 3: the leaf lock is never held while acquiring another
        // lock — `snapshot_for` returns a copy and drops its pair guard
        // before `check_hang` runs.
        // #941: use the holder-tracking wrapper so the periodic
        // ThreadDumpHandler can surface "hang_detection wedged" if this
        // handler ever blocks the main loop (the H1 hypothesis from
        // #932 RCA).
        // Phase 1 (under the registry lock): decay + classify hangs, and collect
        // the names currently in Hung. NO teams-file read here — that runs
        // lock-free in phase 2 (#1530 / DAEMON-LOCK-ORDERING: no file IO under
        // the registry lock).
        let hung_now: Vec<String> = {
            let reg = agent::lock_registry_tracked(ctx.registry, "hang_detection");
            let mut hung = Vec::new();
            for handle in reg.values() {
                let name = handle.name.as_str();
                let mut core = handle.core.lock();
                core.health.maybe_decay();
                let agent_state = core.state.current;
                let silent = core.state.last_output.elapsed();
                // F9 (#685 sub-task 4): productive-silence reads the new
                // `last_productive_output` field which is bumped only when
                // `infer_productivity` returns a Productive signal. Default
                // shadow-mode in `check_hang` gates classification on
                // `AGEND_PRODUCTIVE_GATE=1`.
                let silent_productive = core.state.last_productive_output.elapsed();
                let pair = crate::daemon::heartbeat_pair::snapshot_for(name);
                if core.health.check_hang(
                    agent_state,
                    silent,
                    silent_productive,
                    pair.last_input_at_ms,
                    pair.heartbeat_at_ms,
                ) {
                    tracing::warn!(
                        agent = %name,
                        state = agent_state.display_name(),
                        silent = ?silent,
                        "hang detected"
                    );
                }
                if core.health.state == crate::health::HealthState::Hung {
                    hung.push(name.to_string());
                }
            }
            hung
        };

        // Phase 2/3 (#1701 Hung half): a self-orchestrator stuck Hung past the
        // confirm-window has no peer to relay — escalate operator P0, INDEPENDENT
        // of `hang_auto_recovery_enabled` (a leaderless team must page even in
        // recovery shadow mode). `is_self_orchestrator` is a teams-file read (done
        // lock-free); the confirm-window + cooldown gate (`hung_escalation_due`)
        // then runs under a brief per-candidate re-lock and stamps, so a
        // persisting Hung pages at most once per cooldown. Self-orch Hung is rare,
        // so the re-lock cost is negligible.
        for name in hung_now {
            if !crate::teams::is_self_orchestrator(ctx.home, &name) {
                continue;
            }
            let due = {
                let reg = agent::lock_registry(ctx.registry);
                reg.values()
                    .find(|h| h.name.as_str() == name)
                    .map(|h| {
                        h.core
                            .lock()
                            .health
                            .hung_escalation_due(HUNG_ESCALATE_AFTER)
                    })
                    .unwrap_or(false)
            };
            if due {
                notify_self_orch_hung(&name);
            }
        }
    }
}

/// #1701: how long a self-orchestrator must stay Hung before its hang escalates
/// to the operator — a confirm-window FP-filter on top of `check_hang`'s
/// Hung/IdleLong split (which already excludes the 04:00 idle false-alarm). 60s
/// is conservative: a real hang pages within a minute (orchestrator recovery is
/// not millisecond-critical), while transient residual FPs (F39 stale-Thinking
/// scrollback, F10 1-byte-output exit, E1 keystroke-draining) don't survive it.
/// TODO: revisit if #685 enables the F9 productive-path by default (it shifts
/// Hung sensitivity) — see docs/HUNG-STATE-TRANSITIONS.md.
const HUNG_ESCALATE_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// #1701: page the operator that a self-orchestrator is hung. Mirrors
/// `crash_respawn::notify_self_orch_crash` — same `gated_notify(Error)`
/// Sleep-penetrating path (#1595/#1717). Channel + name only (no registry).
fn notify_self_orch_hung(name: &str) {
    tracing::warn!(
        agent = %name,
        "#1701: self-orchestrator hung past confirm-window — escalating P0 to operator"
    );
    let msg = format!(
        "🛑 {name} (team orchestrator) has been HUNG ≥{}s — no peer can relay this and \
         the team is stalled until it recovers. Manual intervention likely (check the \
         pane / interrupt / re-prime).",
        HUNG_ESCALATE_AFTER.as_secs()
    );
    if let Some(ch) = crate::channel::active_channel() {
        let _ = crate::channel::gated_notify(
            ch.as_ref(),
            name,
            crate::channel::NotifySeverity::Error,
            &msg,
            false,
        );
    } else {
        tracing::debug!(agent = %name, "no active channel for self-orch hung P0");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::agent::{AgentRegistry, ExternalRegistry};
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    /// Smoke test: empty registry → no-op. The interesting integration
    /// paths (hang threshold tripping, heartbeat_pair freshness, health
    /// decay) are covered by the existing tests in `crate::health` and
    /// `daemon::supervisor`; this PR is pure relocation so we only need
    /// to prove `run()` doesn't panic on the empty case.
    #[test]
    fn run_is_noop_on_empty_registry() {
        let home = std::env::temp_dir().join(format!(
            "agend-hang-handler-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).ok();
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        HangDetectionHandler::new().run(&ctx);

        assert!(registry.lock().is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    /// Name pin — used by future Vec<Box<dyn PerTickHandler>> aggregator
    /// for tracing spans / diagnostic dumps.
    #[test]
    fn name_matches_module() {
        assert_eq!(HangDetectionHandler::new().name(), "hang_detection");
    }
}
