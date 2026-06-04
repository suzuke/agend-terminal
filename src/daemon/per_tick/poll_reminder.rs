//! Poll-reminder cadence wrapper: fires `crate::daemon::poll_reminder::poll_reminder_pass`
//! every `every_n_ticks` ticks. Extracted verbatim from `src/daemon/mod.rs:748-758`
//! (pre-#694 BLOCK 1) — the previous code used a function-local `static AtomicU64`
//! counter; this handler relocates that counter onto the struct.

use super::{PerTickHandler, TickContext};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

pub(crate) struct PollReminderHandler {
    every_n_ticks: u64,
    counter: AtomicU64,
    /// ≈ daemon boot (handlers are built once at startup). Drives the
    /// boot-grace suppression — see [`super::in_boot_grace`].
    created_at: Instant,
}

impl PollReminderHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            every_n_ticks,
            counter: AtomicU64::new(0),
            created_at: Instant::now(),
        }
    }

    #[cfg(test)]
    fn new_at(every_n_ticks: u64, created_at: Instant) -> Self {
        Self {
            every_n_ticks,
            counter: AtomicU64::new(0),
            created_at,
        }
    }

    /// `fetch_add` returns the PREVIOUS counter value, then atomically
    /// increments. So fires at tick indices 0, N, 2N, ... — matching the
    /// pre-extraction `static AtomicU64` cadence (first call returns 0,
    /// `0.is_multiple_of(N) == true`, so the very first tick fires).
    fn should_fire(&self) -> bool {
        self.counter
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(self.every_n_ticks)
    }

    /// Gate combining boot-grace + cadence. During boot-grace the `&&`
    /// short-circuits BEFORE `should_fire`, so the counter is NOT consumed —
    /// the first tick AFTER grace sees counter 0 and fires immediately.
    fn should_run(&self) -> bool {
        !super::in_boot_grace(self.created_at) && self.should_fire()
    }
}

impl PerTickHandler for PollReminderHandler {
    fn name(&self) -> &'static str {
        "poll_reminder"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if self.should_run() {
            crate::daemon::poll_reminder::poll_reminder_pass(ctx.home, ctx.registry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Pin the cadence: with N=3, fires on the 1st, 4th, 7th call
    /// (counter values 0, 3, 6 — each a multiple of 3).
    #[test]
    fn fires_at_expected_cadence() {
        let h = PollReminderHandler::new(3);
        let fires: Vec<bool> = (0..7).map(|_| h.should_fire()).collect();
        assert_eq!(fires, vec![true, false, false, true, false, false, true]);
    }

    /// Sanity-check the every-tick degenerate case (N=1 fires every call).
    #[test]
    fn n_equals_one_fires_every_tick() {
        let h = PollReminderHandler::new(1);
        let fires: Vec<bool> = (0..5).map(|_| h.should_fire()).collect();
        assert_eq!(fires, vec![true; 5]);
    }

    /// #t-watchdog-boot-suppress: a freshly-built handler (created_at ≈ now) is
    /// in boot-grace → `should_run` is false even though `should_fire` would be
    /// true on tick 0. The `&&` must NOT consume the counter during grace.
    #[test]
    fn boot_grace_suppresses_first_ticks() {
        let h = PollReminderHandler::new(1); // N=1 → should_fire always true
        assert!(!h.should_run(), "in boot-grace → suppressed");
        assert!(!h.should_run(), "still suppressed; counter not consumed");
    }

    /// Past the grace window, the first tick fires (counter still 0 because grace
    /// short-circuited it earlier).
    #[test]
    fn fires_on_first_tick_after_grace() {
        let past = Instant::now() - super::super::NOTIFICATION_BOOT_GRACE - Duration::from_secs(1);
        let h = PollReminderHandler::new_at(30, past);
        assert!(
            h.should_run(),
            "after grace, the first post-grace tick fires"
        );
    }
}
