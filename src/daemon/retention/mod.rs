//! Retention sweep supervisor — periodic cleanup of expired artifacts.
//!
//! Wired into supervisor.rs run_loop as a single `maybe_sweep()` call.
//! Internally dispatches to per-artifact sub-handlers: decisions,
//! pending-dispatches, worktrees. Inbox and schedules.run_history are
//! already self-sweeping (no new code needed).

pub(crate) mod decisions;
pub(crate) mod pending_dispatches;
pub(crate) mod worktrees;

use std::path::Path;
use std::time::Instant;

const TICKS_PER_SCAN: u64 = 360; // ~1 hour at 10s tick rate

#[derive(Default)]
pub(crate) struct RetentionSupervisor {
    tick_count: u64,
    last_run_at: Option<Instant>,
}

impl RetentionSupervisor {
    pub(crate) fn maybe_sweep(&mut self, home: &Path) {
        self.tick_count += 1;
        if !self.tick_count.is_multiple_of(TICKS_PER_SCAN) {
            return;
        }
        let now = Instant::now();
        self.last_run_at = Some(now);

        tracing::info!("retention sweep: starting cycle");

        let decisions_swept = decisions::sweep(home);
        let cutover = std::env::var("AGEND_RETENTION_CUTOVER").as_deref() == Ok("1");
        let dispatches_swept = pending_dispatches::sweep(home, cutover);
        let worktrees_swept = worktrees::sweep(home);

        tracing::info!(
            decisions = decisions_swept,
            dispatches = dispatches_swept,
            worktrees = worktrees_swept,
            "retention sweep: cycle complete"
        );
        crate::event_log::log(
            home,
            "retention_sweep",
            "supervisor",
            &format!(
                "decisions={decisions_swept} dispatches={dispatches_swept} worktrees={worktrees_swept}"
            ),
        );
    }
}
