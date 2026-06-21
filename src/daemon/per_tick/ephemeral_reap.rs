//! #1967 Phase-1 (PR1 scaffold): periodic reap of ephemeral workers.
//!
//! Runs the [`crate::ephemeral_tracking::reap_sweep`] every ~1 min (6 ticks):
//! removes + terminates any worker that is terminal, past its max-wall-TTL (the
//! day-1 cost guard), or whose process is already gone. Runs in BOTH app and
//! `run_core` (the live daemon is app-mode; this handler is NOT in
//! `APP_TICK_ALLOWLIST`). Idle cost: one read of a usually-tiny JSON sidecar.

use super::{PerTickHandler, TickContext};

pub(crate) struct EphemeralReapHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
}

impl EphemeralReapHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
        }
    }
}

impl PerTickHandler for EphemeralReapHandler {
    fn name(&self) -> &'static str {
        "ephemeral_reap"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        let reaped = crate::ephemeral_tracking::reap_sweep(ctx.home);
        if !reaped.is_empty() {
            tracing::info!(
                target: "ephemeral",
                reaped = reaped.len(),
                "reaped due/expired/dead ephemeral workers"
            );
        }
    }
}
