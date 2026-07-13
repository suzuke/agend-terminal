//! #2755: periodic recovery of stuck `repo action=checkout` transaction
//! rollbacks. A worktree whose `git worktree remove --force` failed during a
//! provisioning rollback (Windows open-handle, transient FS) leaves a durably
//! `rollback_pending` journal; this handler retries it on backoff cadence and,
//! at the INTERVENTION ceiling, emits a deduped operator audit.
//!
//! It shares the SAME callable as boot-repair
//! (`checkout_txn::recover_pending_sweep_prod`) — there is NO dedicated worker
//! (§10.5); the per-tick host owns the cadence. Cadence-gated so the (cheap,
//! filesystem-only) sweep does not run every tick.

use super::{PerTickHandler, TickContext};

pub(crate) struct CheckoutTxnRecoverHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
}

impl CheckoutTxnRecoverHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
        }
    }
}

impl PerTickHandler for CheckoutTxnRecoverHandler {
    fn name(&self) -> &'static str {
        "checkout_txn_recover"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        // Same shared callable as boot-repair (no dedicated worker).
        let _ = crate::mcp::handlers::ci::checkout_txn::recover_pending_sweep_prod(ctx.home);
    }
}
