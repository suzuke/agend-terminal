//! #1491(A) cadence wrapper for the inbox-stuck watchdog. Fires
//! [`crate::daemon::inbox_stuck_watchdog::scan_and_emit`] every
//! `every_n_ticks` ticks (same cadence pattern as [`super::poll_reminder`]).
//! The dedup map lives on the handler so "already alerted" state survives
//! across ticks.

use super::{PerTickHandler, TickContext};
use parking_lot::Mutex;
use std::collections::HashMap;

pub(crate) struct InboxStuckHandler {
    /// Cadence + boot-grace, bundled (see [`super::NOTIFICATION_BOOT_GRACE`]):
    /// suppresses firing within the grace window of construction without
    /// advancing the counter, then fires on tick indices 0, N, 2N, ….
    gate: crate::daemon::cadence_gate::CadenceGate,
    last_alerted: Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>,
}

impl InboxStuckHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_with_boot_grace(
                every_n_ticks,
                super::NOTIFICATION_BOOT_GRACE,
            ),
            last_alerted: Mutex::new(HashMap::new()),
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
            last_alerted: Mutex::new(HashMap::new()),
        }
    }
}

impl PerTickHandler for InboxStuckHandler {
    fn name(&self) -> &'static str {
        "inbox_stuck_watchdog"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        let now = chrono::Utc::now();
        let mut last = self.last_alerted.lock();
        crate::daemon::inbox_stuck_watchdog::scan_and_emit(ctx.home, &now, &mut last);
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
        let h = InboxStuckHandler::new_at(3, past_grace());
        let fires: Vec<bool> = (0..7).map(|_| h.gate.fire()).collect();
        assert_eq!(fires, vec![true, false, false, true, false, false, true]);
    }

    #[test]
    fn name_matches_module() {
        assert_eq!(InboxStuckHandler::new(30).name(), "inbox_stuck_watchdog");
    }

    /// #t-watchdog-boot-suppress: within boot-grace, `fire` is false (no alert
    /// for the stale backlog) and the counter is NOT consumed; past grace the
    /// first tick fires. Combined with `inbox_stuck_watchdog`'s scan_and_emit
    /// tests (which prove a real stuck pile DOES alert), this pins "suppressed
    /// during grace, fires for a genuine stuck agent after grace".
    #[test]
    fn boot_grace_suppresses_then_fires() {
        let fresh = InboxStuckHandler::new(30); // created_at ≈ now → in grace
        assert!(!fresh.gate.fire(), "in boot-grace → suppressed");
        assert!(!fresh.gate.fire(), "still suppressed; counter not consumed");

        let aged = InboxStuckHandler::new_at(30, past_grace());
        assert!(aged.gate.fire(), "after grace, first tick fires");
    }
}
