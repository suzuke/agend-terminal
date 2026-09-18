//! #3666: per-tick driver for the busy-park redrive scan. The park/redrive
//! state machine lives in `crate::mcp::handlers::comms_gates::busy_park`;
//! this is the thin per-tick wrapper (mirrors `inject_delivery`'s shape).
//! Scans every 6 ticks (~60s, the dispatch-idle L1 cadence) so a target that
//! goes idle picks up its parked dispatch promptly; an empty park store costs
//! a single `read_dir`.

use super::{PerTickHandler, TickContext};

pub(crate) struct BusyParkRedriveHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
}

impl BusyParkRedriveHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
        }
    }
}

impl PerTickHandler for BusyParkRedriveHandler {
    fn name(&self) -> &'static str {
        "busy_park_redrive"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        let stats = crate::mcp::handlers::comms_gates::busy_park::scan_and_redrive(ctx.home);
        if stats.scanned > 0 {
            tracing::debug!(
                scanned = stats.scanned,
                delivered = stats.delivered,
                reparked = stats.reparked,
                dropped = stats.dropped,
                "busy-park redrive scan"
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The handler must be part of the default daemon pipeline — the redrive
    /// only converges if the scan actually runs headless.
    #[test]
    fn registered_in_default_daemon_pipeline() {
        let stale: crate::daemon::mcp_registry_watcher::DaemonBinaryStale =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handlers = crate::daemon::build_default_handlers(stale);
        assert!(
            handlers.iter().any(|h| h.name() == "busy_park_redrive"),
            "BusyParkRedriveHandler must run in headless run_core"
        );
    }

    #[test]
    fn fires_at_expected_cadence() {
        let h = BusyParkRedriveHandler::new(3);
        let fires: Vec<bool> = (0..7).map(|_| h.gate.fire()).collect();
        assert_eq!(fires, vec![true, false, false, true, false, false, true]);
    }

    #[test]
    fn name_matches_module() {
        assert_eq!(BusyParkRedriveHandler::new(1).name(), "busy_park_redrive");
    }
}
