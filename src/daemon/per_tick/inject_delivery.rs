//! #2044: per-tick driver for the inject-delivery watchdog. The arm/verify
//! logic + state live in `crate::daemon::inject_delivery`; this is the thin
//! per-tick wrapper (mirrors `poll_reminder`'s shape). Runs every tick so a
//! delivered inject clears promptly and the 30s verify window is checked at
//! tick granularity (the pass is cheap — it iterates a usually-empty map).

use super::{PerTickHandler, TickContext};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct InjectDeliveryHandler {
    every_n_ticks: u64,
    counter: AtomicU64,
}

impl InjectDeliveryHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            every_n_ticks,
            counter: AtomicU64::new(0),
        }
    }

    fn should_fire(&self) -> bool {
        self.counter
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(self.every_n_ticks)
    }
}

impl PerTickHandler for InjectDeliveryHandler {
    fn name(&self) -> &'static str {
        "inject_delivery"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.should_fire() {
            return;
        }
        crate::daemon::inject_delivery::verify_pass(ctx.home);
    }
}
