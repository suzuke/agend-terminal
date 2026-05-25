//! Hourly worktree GC tick — removes daemon-managed worktrees that have
//! been released past the grace period and are not pinned or bound.
//! Also cleans up stale ci-watch lock files.

use super::{PerTickHandler, TickContext};
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct GcTickHandler {
    every_n_ticks: u64,
    counter: AtomicU64,
}

impl GcTickHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            every_n_ticks,
            counter: AtomicU64::new(0),
        }
    }

    fn should_fire(&self) -> bool {
        self.counter
            .fetch_add(1, Ordering::Relaxed)
            .is_multiple_of(self.every_n_ticks)
    }
}

impl PerTickHandler for GcTickHandler {
    fn name(&self) -> &'static str {
        "gc_tick"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.should_fire() {
            return;
        }
        let results = crate::worktree_pool::gc_run(ctx.home);
        let removed = results.iter().filter(|r| r.removed).count();
        let failed = results.iter().filter(|r| !r.removed).count();
        if removed > 0 || failed > 0 {
            tracing::info!(removed, failed, "gc_tick: worktree GC complete");
        }

        let stale_locks = crate::worktree_pool::gc_stale_ci_watch_locks(ctx.home);
        if stale_locks > 0 {
            tracing::info!(stale_locks, "gc_tick: stale ci-watch locks cleaned");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn should_fire_respects_every_n_ticks() {
        let handler = GcTickHandler::new(3);
        assert!(handler.should_fire()); // tick 0
        assert!(!handler.should_fire()); // tick 1
        assert!(!handler.should_fire()); // tick 2
        assert!(handler.should_fire()); // tick 3
        assert!(!handler.should_fire()); // tick 4
    }
}
