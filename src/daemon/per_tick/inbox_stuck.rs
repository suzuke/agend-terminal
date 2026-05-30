//! #1491(A) cadence wrapper for the inbox-stuck watchdog. Fires
//! [`crate::daemon::inbox_stuck_watchdog::scan_and_emit`] every
//! `every_n_ticks` ticks (same cadence pattern as [`super::poll_reminder`]).
//! The dedup map lives on the handler so "already alerted" state survives
//! across ticks.

use super::{PerTickHandler, TickContext};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct InboxStuckHandler {
    every_n_ticks: u64,
    counter: AtomicU64,
    last_alerted: Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>,
}

impl InboxStuckHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            every_n_ticks,
            counter: AtomicU64::new(0),
            last_alerted: Mutex::new(HashMap::new()),
        }
    }

    /// Fires at tick indices 0, N, 2N, … (matches `PollReminderHandler`).
    fn should_fire(&self) -> bool {
        self.counter
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(self.every_n_ticks)
    }
}

impl PerTickHandler for InboxStuckHandler {
    fn name(&self) -> &'static str {
        "inbox_stuck_watchdog"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.should_fire() {
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

    #[test]
    fn fires_at_expected_cadence() {
        let h = InboxStuckHandler::new(3);
        let fires: Vec<bool> = (0..7).map(|_| h.should_fire()).collect();
        assert_eq!(fires, vec![true, false, false, true, false, false, true]);
    }

    #[test]
    fn name_matches_module() {
        assert_eq!(InboxStuckHandler::new(30).name(), "inbox_stuck_watchdog");
    }
}
