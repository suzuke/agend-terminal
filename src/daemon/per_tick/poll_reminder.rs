//! Poll-reminder cadence wrapper: fires `crate::daemon::poll_reminder::poll_reminder_pass`
//! every `every_n_ticks` ticks. Extracted verbatim from `src/daemon/mod.rs:748-758`
//! (pre-#694 BLOCK 1) — the previous code used a function-local `static AtomicU64`
//! counter; this handler relocates that counter onto the struct.

use super::{PerTickHandler, TickContext};

pub(crate) struct PollReminderHandler {
    /// Cadence + boot-grace, bundled (see [`super::NOTIFICATION_BOOT_GRACE`]):
    /// suppresses firing within the grace window of construction (≈ daemon boot)
    /// without advancing the counter, then fires on tick indices 0, N, 2N, …
    gate: crate::daemon::cadence_gate::CadenceGate,
}

impl PollReminderHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_with_boot_grace(
                every_n_ticks,
                super::NOTIFICATION_BOOT_GRACE,
            ),
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
        }
    }
}

impl PerTickHandler for PollReminderHandler {
    fn name(&self) -> &'static str {
        "poll_reminder"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if self.gate.fire() {
            crate::daemon::poll_reminder::poll_reminder_pass(ctx.home, ctx.registry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// An Instant comfortably past the boot-grace window — the gate's grace has
    /// expired, so `fire()` exercises pure cadence.
    fn past_grace() -> Instant {
        Instant::now() - super::super::NOTIFICATION_BOOT_GRACE - Duration::from_secs(1)
    }

    /// Pin the cadence: with N=3, fires on the 1st, 4th, 7th call
    /// (counter values 0, 3, 6 — each a multiple of 3).
    #[test]
    fn fires_at_expected_cadence() {
        let h = PollReminderHandler::new_at(3, past_grace());
        let fires: Vec<bool> = (0..7).map(|_| h.gate.fire()).collect();
        assert_eq!(fires, vec![true, false, false, true, false, false, true]);
    }

    /// Sanity-check the every-tick degenerate case (N=1 fires every call).
    #[test]
    fn n_equals_one_fires_every_tick() {
        let h = PollReminderHandler::new_at(1, past_grace());
        let fires: Vec<bool> = (0..5).map(|_| h.gate.fire()).collect();
        assert_eq!(fires, vec![true; 5]);
    }

    /// #t-watchdog-boot-suppress: a freshly-built handler (created_at ≈ now) is
    /// in boot-grace → `fire` is false even though the cadence would fire on tick
    /// 0. Boot-grace must NOT consume the counter during grace.
    #[test]
    fn boot_grace_suppresses_first_ticks() {
        let h = PollReminderHandler::new(1); // N=1 → cadence always true
        assert!(!h.gate.fire(), "in boot-grace → suppressed");
        assert!(!h.gate.fire(), "still suppressed; counter not consumed");
    }

    /// Past the grace window, the first tick fires (counter still 0 because grace
    /// short-circuited it earlier).
    #[test]
    fn fires_on_first_tick_after_grace() {
        let h = PollReminderHandler::new_at(30, past_grace());
        assert!(
            h.gate.fire(),
            "after grace, the first post-grace tick fires"
        );
    }
}
