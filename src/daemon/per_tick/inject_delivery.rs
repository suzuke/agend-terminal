//! #2044: per-tick driver for the inject-delivery watchdog. The arm/verify
//! logic + state live in `crate::daemon::inject_delivery`; this is the thin
//! per-tick wrapper (mirrors `poll_reminder`'s shape). Runs every tick so a
//! delivered inject clears promptly and the 30s verify window is checked at
//! tick granularity (the pass is cheap — it iterates a usually-empty map).

use super::{PerTickHandler, TickContext};

pub(crate) struct InjectDeliveryHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
}

impl InjectDeliveryHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
        }
    }
}

impl PerTickHandler for InjectDeliveryHandler {
    fn name(&self) -> &'static str {
        "inject_delivery"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        crate::daemon::inject_delivery::verify_pass(ctx.home);
    }
}
