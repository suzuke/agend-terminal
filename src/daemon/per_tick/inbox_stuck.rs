//! #1491(A) cadence wrapper for the inbox-stuck watchdog. Fires
//! [`crate::daemon::inbox_stuck_watchdog::scan_and_emit`] every
//! `every_n_ticks` ticks (same cadence pattern as [`super::poll_reminder`]).
//! The dedup map lives on the handler so "already alerted" state survives
//! across ticks.

use super::{PerTickHandler, TickContext};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

pub(crate) struct InboxStuckHandler {
    every_n_ticks: u64,
    counter: AtomicU64,
    last_alerted: Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>,
    /// ≈ daemon boot — drives boot-grace suppression (see [`super::in_boot_grace`]).
    created_at: Instant,
}

impl InboxStuckHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            every_n_ticks,
            counter: AtomicU64::new(0),
            last_alerted: Mutex::new(HashMap::new()),
            created_at: Instant::now(),
        }
    }

    #[cfg(test)]
    fn new_at(every_n_ticks: u64, created_at: Instant) -> Self {
        Self {
            every_n_ticks,
            counter: AtomicU64::new(0),
            last_alerted: Mutex::new(HashMap::new()),
            created_at,
        }
    }

    /// Fires at tick indices 0, N, 2N, … (matches `PollReminderHandler`).
    fn should_fire(&self) -> bool {
        self.counter
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(self.every_n_ticks)
    }

    /// Boot-grace + cadence gate. `&&` short-circuits before `should_fire`
    /// during grace, so the counter is not consumed (see `PollReminderHandler`).
    fn should_run(&self) -> bool {
        !super::in_boot_grace(self.created_at) && self.should_fire()
    }
}

impl PerTickHandler for InboxStuckHandler {
    fn name(&self) -> &'static str {
        "inbox_stuck_watchdog"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.should_run() {
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
    use std::time::Duration;

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

    /// #t-watchdog-boot-suppress: within boot-grace, `should_run` is false (no
    /// alert for the stale backlog) and the counter is NOT consumed; past grace
    /// the first tick fires. Combined with `inbox_stuck_watchdog`'s scan_and_emit
    /// tests (which prove a real stuck pile DOES alert), this pins "suppressed
    /// during grace, fires for a genuine stuck agent after grace".
    #[test]
    fn boot_grace_suppresses_then_fires() {
        let fresh = InboxStuckHandler::new(30); // created_at ≈ now → in grace
        assert!(!fresh.should_run(), "in boot-grace → suppressed");
        assert!(
            !fresh.should_run(),
            "still suppressed; counter not consumed"
        );

        let past = Instant::now() - super::super::NOTIFICATION_BOOT_GRACE - Duration::from_secs(1);
        let aged = InboxStuckHandler::new_at(30, past);
        assert!(aged.should_run(), "after grace, first tick fires");
    }
}
