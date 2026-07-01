//! AUDIT2-014 (B', daemon detective backstop, multi-board only): periodic
//! reconciliation for the cross-board claim TOCTOU. A claim's precondition
//! validates a cross-board `depends_on` via a lock-free replay of the FOREIGN
//! board (`tasks::DepResolver::status_of`), while the local `Claimed` commit
//! lands under only the LOCAL board's lock — a foreign-board write landing in
//! that window lets a claim commit against a dependency that is, by commit
//! time, no longer Done. List-time dep eval never revisits an already-Claimed
//! task, so without this backstop the bad claim is permanently stuck.
//!
//! Delegates entirely to [`crate::tasks::reconcile_stale_cross_board_claims`]
//! (pure relocation — the scan/remedy logic lives with the rest of the task
//! board machinery); this handler just owns the cadence and turns the result
//! into loud, greppable audit events.

use super::{PerTickHandler, TickContext};

pub(crate) struct CrossBoardDepDetectiveHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
}

impl CrossBoardDepDetectiveHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
        }
    }
}

impl PerTickHandler for CrossBoardDepDetectiveHandler {
    fn name(&self) -> &'static str {
        "cross_board_dep_detective"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        let report = crate::tasks::reconcile_stale_cross_board_claims(ctx.home);
        for tid in &report.released {
            crate::event_log::log(
                ctx.home,
                "audit2_014_cross_board_claim_released",
                tid,
                "cross-board dependency no longer Done — claim released back to open \
                 (self-healed a claim-time TOCTOU race, AUDIT2-014)",
            );
            tracing::warn!(
                task_id = %tid,
                "AUDIT2-014: released a Claimed task whose cross-board dependency is no longer Done"
            );
        }
        for tid in &report.flagged_in_progress {
            crate::event_log::log(
                ctx.home,
                "audit2_014_cross_board_dep_stale_in_progress",
                tid,
                "in-progress task's cross-board dependency is no longer Done — flagged only, \
                 not auto-released (AUDIT2-014)",
            );
            tracing::warn!(
                task_id = %tid,
                "AUDIT2-014: in-progress task's cross-board dependency is no longer Done"
            );
        }
    }
}
