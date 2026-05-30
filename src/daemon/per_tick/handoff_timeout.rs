//! #1491(B) cadence wrapper for the handoff-timeout watchdog. Fires
//! [`crate::daemon::handoff_timeout_watchdog::scan_and_emit`] every
//! `every_n_ticks` ticks. The dedup map lives on the handler so escalation
//! state survives across ticks.

use super::{PerTickHandler, TickContext};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct HandoffTimeoutHandler {
    every_n_ticks: u64,
    counter: AtomicU64,
    last_escalated: Mutex<HashMap<(String, String), chrono::DateTime<chrono::Utc>>>,
}

impl HandoffTimeoutHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            every_n_ticks,
            counter: AtomicU64::new(0),
            last_escalated: Mutex::new(HashMap::new()),
        }
    }

    /// Fires at tick indices 0, N, 2N, … (matches `PollReminderHandler`).
    fn should_fire(&self) -> bool {
        self.counter
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(self.every_n_ticks)
    }
}

impl PerTickHandler for HandoffTimeoutHandler {
    fn name(&self) -> &'static str {
        "handoff_timeout_watchdog"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.should_fire() {
            return;
        }
        let now = chrono::Utc::now();
        let mut last = self.last_escalated.lock();
        crate::daemon::handoff_timeout_watchdog::scan_and_emit(ctx.home, &now, &mut last);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_at_expected_cadence() {
        let h = HandoffTimeoutHandler::new(3);
        let fires: Vec<bool> = (0..7).map(|_| h.should_fire()).collect();
        assert_eq!(fires, vec![true, false, false, true, false, false, true]);
    }

    #[test]
    fn name_matches_module() {
        assert_eq!(
            HandoffTimeoutHandler::new(30).name(),
            "handoff_timeout_watchdog"
        );
    }
}
