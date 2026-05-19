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
        let reg = agent::lock_registry_tracked(ctx.registry, "hang_detection");
        for (name, handle) in reg.iter() {
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
        }
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
