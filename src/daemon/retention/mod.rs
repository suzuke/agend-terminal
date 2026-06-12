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

pub(crate) struct RetentionSupervisor {
    /// Cadence gate — throttles sweeps to once per [`TICKS_PER_SCAN`]
    /// supervisor ticks (fire-on-Nth).
    gate: crate::daemon::cadence_gate::CadenceGate,
    last_run_at: Option<Instant>,
}

impl Default for RetentionSupervisor {
    fn default() -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_interval(TICKS_PER_SCAN),
            last_run_at: None,
        }
    }
}

impl RetentionSupervisor {
    pub(crate) fn maybe_sweep(&mut self, home: &Path) {
        if !self.gate.fire() {
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
        //
        // #env-cleanup decouple: `AGEND_RETENTION_CUTOVER` is now the
        // pending-dispatch kill-switch ONLY (opt-OUT). The decisions sweep used
        // to share this var with OPPOSITE polarity (opt-IN), which made
        // `pending-OFF + decisions-ON` unreachable; it now reads its own
        // `AGEND_RETENTION_DECISIONS_CUTOVER` (see `decisions::sweep`).
        // Pending-dispatch truth table: `=="0"` → OFF; unset / anything else → ON.
        let cutover = std::env::var("AGEND_RETENTION_CUTOVER").as_deref() != Ok("0");
        let dispatches_swept = pending_dispatches::sweep(home, cutover);
        let worktrees_swept = worktrees::sweep(home);
        // Drop terminal dispatch_tracking rows (completed/orphaned) each cycle so
        // they don't accumulate until the 30-day gc_old_entries backstop.
        let tracking_swept = crate::dispatch_tracking::sweep_terminal_entries(home);
        // #1969: passively GC the ci-handoff per-key `.lock` (never unlinked on
        // resolve — keys are reused → unlink-on-resolve is a flock race) and
        // crash-leftover `.tmp` sidecars, by mtime + orphan check.
        let ci_handoff_swept =
            crate::daemon::ci_handoff_track::gc_orphan_sidecars(home, std::time::SystemTime::now());
        // #2059 #2(c): drop verdict-buffer entries whose SHA never became a
        // branch head within 24h (abandoned PR / force-push past it) so a
        // never-resolving buffered verdict can't leak.
        let verdict_buffer_swept =
            crate::daemon::pr_state::verdict_buffer::sweep_expired(home, chrono::Utc::now());

        tracing::info!(
            decisions = decisions_swept,
            dispatches = dispatches_swept,
            tracking = tracking_swept,
            worktrees = worktrees_swept,
            ci_handoff = ci_handoff_swept,
            verdict_buffer = verdict_buffer_swept,
            "retention sweep: cycle complete"
        );
        crate::event_log::log(
            home,
            "retention_sweep",
            "supervisor",
            &format!(
                "decisions={decisions_swept} dispatches={dispatches_swept} tracking={tracking_swept} worktrees={worktrees_swept} ci_handoff={ci_handoff_swept}"
            ),
        );
    }
}
