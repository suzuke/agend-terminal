//! Poll-reminder cadence wrapper: fires `crate::daemon::poll_reminder::poll_reminder_pass`
//! every `every_n_ticks` ticks. Extracted verbatim from `src/daemon/mod.rs:748-758`
//! (pre-#694 BLOCK 1) — the previous code used a function-local `static AtomicU64`
//! counter; this handler relocates that counter onto the struct.

use super::{PerTickHandler, TickContext};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct PollReminderHandler {
    every_n_ticks: u64,
    counter: AtomicU64,
}

impl PollReminderHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            every_n_ticks,
            counter: AtomicU64::new(0),
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
}

impl PerTickHandler for PollReminderHandler {
    fn name(&self) -> &'static str {
        "poll_reminder"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if self.should_fire() {
            crate::daemon::poll_reminder::poll_reminder_pass(ctx.home, ctx.registry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
