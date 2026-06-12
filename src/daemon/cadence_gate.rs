//! W2.4 (#2050, survey 01-R3) — one shared per-tick cadence gate.
//!
//! Two hand-rolled idioms had proliferated (~15 + 12 sites):
//!
//! 1. **Handlers** — `counter.fetch_add(1, Relaxed).is_multiple_of(n)`: an
//!    `AtomicU64` that fires on calls 1, N+1, 2N+1, … (tick 0 fires).
//! 2. **Supervisor trackers** — `tick_count += 1; if tick_count < N { return
//!    false } tick_count = 0; true`: a `&mut` counter that fires on calls N,
//!    2N, … (the first N-1 calls don't fire; the first scan is delayed N ticks).
//!
//! `CadenceGate` collapses both into ONE type, preserving each idiom's exact
//! fire phase via the counter's initial value (no behaviour change):
//!
//! - [`CadenceGate::new`] (counter starts 0) ≡ idiom 1 (fire-on-first).
//! - [`CadenceGate::new_interval`] (counter starts 1) ≡ idiom 2 (fire-on-Nth).
//!
//! It also STRUCTURALLY bundles the notification-watchdog boot-grace
//! (#t-watchdog-boot-suppress): [`CadenceGate::new_with_boot_grace`] suppresses
//! firing within the grace window of construction, so a watchdog cannot forget
//! the gate — it's inside `fire()`, not a separate hand-wired
//! `!in_boot_grace(self.created_at) && …` conjunction a new handler can drop.
//!
//! Interior atomic, so `fire(&self)` works from both `&self` handler `run` and
//! `&mut self` tracker `maybe_scan`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub(crate) struct CadenceGate {
    every_n: u64,
    counter: AtomicU64,
    /// `Some(boot, grace)` → `fire()` returns false (without advancing the
    /// counter) until `grace` has elapsed since `boot` (≈ construction).
    boot_grace: Option<(Instant, Duration)>,
}

impl CadenceGate {
    /// Fire-on-FIRST cadence (calls 1, N+1, 2N+1, …; tick 0 fires) — the
    /// handler `counter.fetch_add(1).is_multiple_of(n)` idiom.
    pub(crate) fn new(every_n: u64) -> Self {
        Self {
            every_n: every_n.max(1),
            counter: AtomicU64::new(0),
            boot_grace: None,
        }
    }

    /// Fire-on-Nth cadence (calls N, 2N, …; the first N-1 calls don't fire; the
    /// first fire is delayed N calls) — the supervisor-tracker `tick_count <
    /// N { reset }` idiom. Equivalent because the counter starts at 1, so the
    /// N-th call observes the value `N` (≡ `0 % N` after the tracker's reset).
    pub(crate) fn new_interval(every_n: u64) -> Self {
        Self {
            every_n: every_n.max(1),
            counter: AtomicU64::new(1),
            boot_grace: None,
        }
    }

    /// Fire-on-first cadence PLUS a boot-grace suppression window: never fires
    /// within `grace` of construction (≈ daemon boot), AND does not advance the
    /// counter during the grace window — so the cadence phase is anchored to
    /// grace-END (byte-identical to the old `!in_boot_grace(created_at) &&
    /// should_fire()`, whose `&&` short-circuit skipped the `fetch_add` during
    /// grace). #t-watchdog-boot-suppress.
    pub(crate) fn new_with_boot_grace(every_n: u64, grace: Duration) -> Self {
        Self {
            every_n: every_n.max(1),
            counter: AtomicU64::new(0),
            boot_grace: Some((Instant::now(), grace)),
        }
    }

    /// Test seam for the notification-watchdog `new_at` constructors: build a
    /// boot-grace gate anchored to an explicit `boot` Instant (counter 0), so a
    /// test can drive the grace window deterministically (a past `boot` → grace
    /// already expired). Mirrors `new_with_boot_grace` exactly, minus the
    /// `Instant::now()` anchor.
    #[cfg(test)]
    pub(crate) fn new_with_boot_grace_at(
        every_n: u64,
        boot: std::time::Instant,
        grace: Duration,
    ) -> Self {
        Self {
            every_n: every_n.max(1),
            counter: AtomicU64::new(0),
            boot_grace: Some((boot, grace)),
        }
    }

    /// Advance one tick and report whether this tick fires. During a boot-grace
    /// window: returns false WITHOUT advancing (preserves the old short-circuit
    /// phase). Otherwise: fires when the pre-increment counter is a multiple of
    /// `every_n`.
    pub(crate) fn fire(&self) -> bool {
        if let Some((boot, grace)) = self.boot_grace {
            if boot.elapsed() < grace {
                return false;
            }
        }
        self.counter
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(self.every_n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Idiom 1: fires on calls 1, N+1, 2N+1 (tick 0 fires) — matches
    /// `counter.fetch_add(1).is_multiple_of(N)`.
    #[test]
    fn new_fires_on_first_then_every_n() {
        let g = CadenceGate::new(3);
        let fires: Vec<bool> = (0..7).map(|_| g.fire()).collect();
        assert_eq!(
            fires,
            vec![true, false, false, true, false, false, true],
            "fire-on-first: calls 1,4,7 (0-indexed 0,3,6)"
        );
    }

    /// Idiom 2: the first N-1 calls don't fire; fires on the N-th, 2N-th — the
    /// tracker `tick_count += 1; if < N { false } else { reset; true }` schedule.
    #[test]
    fn new_interval_fires_on_nth_matching_tracker() {
        const N: u64 = 4;
        let g = CadenceGate::new_interval(N);
        // Reference: the literal tracker idiom.
        let mut tick_count: u64 = 0;
        let mut tracker_fire = || {
            tick_count += 1;
            if tick_count < N {
                false
            } else {
                tick_count = 0;
                true
            }
        };
        for call in 1..=(3 * N) {
            assert_eq!(
                g.fire(),
                tracker_fire(),
                "CadenceGate::new_interval must match the tracker idiom on call {call}"
            );
        }
    }

    /// Boot-grace: no fire (and no counter advance) during the window; the
    /// first post-grace call fires (counter still 0), matching the old
    /// `!in_boot_grace(created_at) && should_fire()` phase.
    #[test]
    fn boot_grace_suppresses_then_anchors_phase_to_grace_end() {
        // Already-expired grace (construct with a zero window): behaves like new().
        let g = CadenceGate::new_with_boot_grace(3, Duration::from_secs(0));
        assert!(g.fire(), "past grace, first call fires (counter 0)");
        assert!(!g.fire());
        assert!(!g.fire());
        assert!(g.fire());

        // Active grace: suppressed, counter not advanced.
        let g2 = CadenceGate::new_with_boot_grace(3, Duration::from_secs(3600));
        assert!(!g2.fire(), "within grace → suppressed");
        assert!(!g2.fire(), "still suppressed, no counter advance");
    }

    #[test]
    fn every_n_floored_to_one() {
        let g = CadenceGate::new(0);
        assert!(g.fire(), "n=0 floored to 1 → fires every call");
        assert!(g.fire());
    }
}
