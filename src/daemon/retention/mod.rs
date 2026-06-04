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
        // Pending-dispatch retention now defaults ON (opt-out via
        // AGEND_RETENTION_CUTOVER=0). It was a staged-rollout gate, not a
        // bug-gate; the sweep only deletes sidecars with `issued_at < now-14d`
        // (a real dispatch resolves in minutes, never survives 14 days), so it
        // is safe to enable by default — closing the `exceeded`/abandoned leak
        // that has no resolve event and can only be time-GC'd.
        let cutover = std::env::var("AGEND_RETENTION_CUTOVER").as_deref() != Ok("0");
        let dispatches_swept = pending_dispatches::sweep(home, cutover);
        let worktrees_swept = worktrees::sweep(home);
        // Drop terminal dispatch_tracking rows (completed/orphaned) each cycle so
        // they don't accumulate until the 30-day gc_old_entries backstop.
        let tracking_swept = crate::dispatch_tracking::sweep_terminal_entries(home);

        tracing::info!(
            decisions = decisions_swept,
            dispatches = dispatches_swept,
            tracking = tracking_swept,
            worktrees = worktrees_swept,
            "retention sweep: cycle complete"
        );
        crate::event_log::log(
            home,
            "retention_sweep",
            "supervisor",
            &format!(
                "decisions={decisions_swept} dispatches={dispatches_swept} tracking={tracking_swept} worktrees={worktrees_swept}"
            ),
        );
    }
}
