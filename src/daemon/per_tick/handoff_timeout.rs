//! #1491(B) cadence wrapper for the handoff-timeout watchdog. Fires
//! [`crate::daemon::handoff_timeout_watchdog::scan_and_emit`] every
//! `every_n_ticks` ticks. The dedup map lives on the handler so escalation
//! state survives across ticks.

use super::{PerTickHandler, TickContext};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

pub(crate) struct HandoffTimeoutHandler {
    every_n_ticks: u64,
    counter: AtomicU64,
    last_escalated: Mutex<HashMap<(String, String), chrono::DateTime<chrono::Utc>>>,
    /// #1859: re-nudge dedup, owned alongside `last_escalated` so the interval
    /// gate survives across ticks.
    last_renudged: Mutex<HashMap<(String, String), chrono::DateTime<chrono::Utc>>>,
    /// ≈ daemon boot — drives boot-grace suppression (see [`super::in_boot_grace`]).
    created_at: Instant,
}

impl HandoffTimeoutHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            every_n_ticks,
            counter: AtomicU64::new(0),
            last_escalated: Mutex::new(HashMap::new()),
            last_renudged: Mutex::new(HashMap::new()),
            created_at: Instant::now(),
        }
    }

    #[cfg(test)]
    fn new_at(every_n_ticks: u64, created_at: Instant) -> Self {
        Self {
            every_n_ticks,
            counter: AtomicU64::new(0),
            last_escalated: Mutex::new(HashMap::new()),
            last_renudged: Mutex::new(HashMap::new()),
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

impl PerTickHandler for HandoffTimeoutHandler {
    fn name(&self) -> &'static str {
        "handoff_timeout_watchdog"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.should_run() {
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
    use std::time::Duration;

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

    /// #t-watchdog-boot-suppress: suppressed during boot-grace (no stale-handoff
    /// re-escalation on restart), fires on the first tick after grace. The
    /// underlying `handoff_timeout_watchdog::scan_and_emit` tests prove a genuine
    /// unclaimed handoff DOES escalate.
    #[test]
    fn boot_grace_suppresses_then_fires() {
        let fresh = HandoffTimeoutHandler::new(30);
        assert!(!fresh.should_run(), "in boot-grace → suppressed");
        assert!(
            !fresh.should_run(),
            "still suppressed; counter not consumed"
        );

        let past = Instant::now() - super::super::NOTIFICATION_BOOT_GRACE - Duration::from_secs(1);
        let aged = HandoffTimeoutHandler::new_at(30, past);
        assert!(aged.should_run(), "after grace, first tick fires");
    }
}
