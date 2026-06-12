//! #1523 phase-2 prerequisite — per-tick wiring for the heuristic⨯hook
//! divergence telemetry (see `crate::daemon::divergence_telemetry`).
//!
//! Shadow-mode, instrument-only, ZERO behaviour: every tick it records, for
//! each hook-capable agent, the screen heuristic state vs the freshness-
//! resolved hook state (agree / disagree / no-hook-signal). Every
//! `flush_every` ticks it flushes one aggregated JSONL line + one INFO log.
//! Nothing reads the telemetry; it gates no decider.

use super::{PerTickHandler, TickContext};
use crate::state::AgentState;
use std::sync::atomic::{AtomicU64, Ordering};

/// Tick interval the daemon main loop runs at (seconds) — used only to label
/// the flushed window's wall-clock span.
const TICK_SECS: u64 = 10;

pub(crate) struct DivergenceTelemetryHandler {
    flush_every: u64,
    counter: AtomicU64,
}

impl DivergenceTelemetryHandler {
    pub(crate) fn new(flush_every: u64) -> Self {
        Self {
            flush_every: flush_every.max(1),
            counter: AtomicU64::new(0),
        }
    }
}

impl PerTickHandler for DivergenceTelemetryHandler {
    fn name(&self) -> &'static str {
        "divergence_telemetry"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        // Collect (name, heuristic) for hook-capable backends under the
        // registry→core lock order (mirrors snapshot.rs); resolve the hook
        // state AFTER dropping the locks (it reads the separate hook-shadow
        // store — never under registry/core, no inversion).
        let snapshot: Vec<(String, AgentState)> = {
            let reg = crate::agent::lock_registry(ctx.registry);
            reg.values()
                .filter(|h| {
                    crate::backend::Backend::parse_str(&h.backend_command).has_state_hooks()
                })
                .map(|h| {
                    let heuristic = h.core.lock().state.get_state();
                    (h.name.to_string(), heuristic)
                })
                .collect()
        };
        for (name, heuristic) in snapshot {
            let hook = crate::daemon::hook_shadow::resolved_state_for(&name);
            crate::daemon::divergence_telemetry::record(heuristic, &hook);
        }

        // Periodic flush (NOT per tick): fire at ticks flush_every, 2×, … (not
        // tick 0 — that would flush a 1-tick window). One JSONL line + INFO.
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        if n != 0 && n.is_multiple_of(self.flush_every) {
            crate::daemon::divergence_telemetry::flush(ctx.home, self.flush_every * TICK_SECS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_matches_module() {
        assert_eq!(
            DivergenceTelemetryHandler::new(360).name(),
            "divergence_telemetry"
        );
    }

    #[test]
    fn flush_every_floored_to_one() {
        // Guard against a div-by-zero / always-flush misconfig.
        assert_eq!(DivergenceTelemetryHandler::new(0).flush_every, 1);
    }
}
