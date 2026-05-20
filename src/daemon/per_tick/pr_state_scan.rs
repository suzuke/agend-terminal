//! #972: per-tick scanner for the PR-state aggregator.
//!
//! Walks `<home>/pr-state/*.json` once per tick, fires
//! `[pr-ready-for-merge]` events for any newly-eligible PR (debounced
//! via `ready_emitted_for_sha`), and sweeps terminal-state files
//! (Merged / ClosedUnmerged) after emitting their close-loop events.
//!
//! Pure delegation to [`crate::daemon::pr_state::scan_and_emit`] —
//! kept as a distinct handler so the per-tick dispatch shape stays
//! uniform.

use super::{PerTickHandler, TickContext};

pub(crate) struct PrStateScanHandler;

impl PrStateScanHandler {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl PerTickHandler for PrStateScanHandler {
    fn name(&self) -> &'static str {
        "pr_state_scan"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        crate::daemon::pr_state::scan_and_emit(ctx.home, ctx.registry);
    }
}
