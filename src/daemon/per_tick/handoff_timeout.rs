//! #1491(B) cadence wrapper for the handoff-timeout watchdog. Fires
//! [`crate::daemon::handoff_timeout_watchdog::scan_and_emit`] every
//! `every_n_ticks` ticks. The dedup map lives on the handler so escalation
//! state survives across ticks.

use super::{PerTickHandler, TickContext};
use parking_lot::Mutex;
use std::collections::HashMap;

pub(crate) struct HandoffTimeoutHandler {
    /// Cadence + boot-grace, bundled (see [`super::NOTIFICATION_BOOT_GRACE`]):
    /// suppresses firing within the grace window of construction without
    /// advancing the counter, then fires on tick indices 0, N, 2N, ….
    gate: crate::daemon::cadence_gate::CadenceGate,
    last_escalated: Mutex<HashMap<(String, String), chrono::DateTime<chrono::Utc>>>,
    /// #1859: re-nudge dedup, owned alongside `last_escalated` so the interval
    /// gate survives across ticks.
    last_renudged: Mutex<HashMap<(String, String), chrono::DateTime<chrono::Utc>>>,
}

impl HandoffTimeoutHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_with_boot_grace(
                every_n_ticks,
                super::NOTIFICATION_BOOT_GRACE,
            ),
            last_escalated: Mutex::new(HashMap::new()),
            last_renudged: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    fn new_at(every_n_ticks: u64, created_at: std::time::Instant) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_with_boot_grace_at(
                every_n_ticks,
                created_at,
                super::NOTIFICATION_BOOT_GRACE,
            ),
            last_escalated: Mutex::new(HashMap::new()),
            last_renudged: Mutex::new(HashMap::new()),
        }
    }
}

impl PerTickHandler for HandoffTimeoutHandler {
    fn name(&self) -> &'static str {
        "handoff_timeout_watchdog"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        let now = chrono::Utc::now();
        let mut last = self.last_escalated.lock();
        let mut last_renudged = self.last_renudged.lock();
        crate::daemon::handoff_timeout_watchdog::scan_and_emit(
            ctx.home,
            &now,
            &mut last,
            &mut last_renudged,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn past_grace() -> Instant {
        Instant::now() - super::super::NOTIFICATION_BOOT_GRACE - Duration::from_secs(1)
    }

    #[test]
    fn fires_at_expected_cadence() {
        let h = HandoffTimeoutHandler::new_at(3, past_grace());
        let fires: Vec<bool> = (0..7).map(|_| h.gate.fire()).collect();
        assert_eq!(fires, vec![true, false, false, true, false, false, true]);
    }

    #[test]
    fn name_matches_module() {
        assert_eq!(
            HandoffTimeoutHandler::new(30).name(),
            "handoff_timeout_watchdog"
        );
    }

    /// #t-watchdog-boot-suppress: suppressed during boot-grace (no stale-handoff
    /// re-escalation on restart), fires on the first tick after grace. The
    /// underlying `handoff_timeout_watchdog::scan_and_emit` tests prove a genuine
    /// unclaimed handoff DOES escalate.
    #[test]
    fn boot_grace_suppresses_then_fires() {
        let fresh = HandoffTimeoutHandler::new(30);
        assert!(!fresh.gate.fire(), "in boot-grace → suppressed");
        assert!(!fresh.gate.fire(), "still suppressed; counter not consumed");

        let aged = HandoffTimeoutHandler::new_at(30, past_grace());
        assert!(aged.gate.fire(), "after grace, first tick fires");
    }
}
