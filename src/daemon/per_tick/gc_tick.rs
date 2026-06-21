//! Hourly worktree GC tick — removes daemon-managed worktrees that have
//! been released past the grace period and are not pinned or bound.
//! Also cleans up stale ci-watch lock files.

use super::{PerTickHandler, TickContext};

pub(crate) struct GcTickHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
    /// t-…50793-9 / FIX6: the target/ sweep is skipped on the FIRST gc_tick
    /// firing (which lands during daemon boot) so its fs walks never compete
    /// with the boot-critical path (the `api_port` budget). Flips true after the
    /// first firing; the sweep first runs on the SECOND firing (~1h uptime).
    target_sweep_armed: std::sync::atomic::AtomicBool,
}

impl GcTickHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
            target_sweep_armed: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl PerTickHandler for GcTickHandler {
    fn name(&self) -> &'static str {
        "gc_tick"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
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

        // t-…50793-9: reclaim stale managed-worktree `target/` build dirs (the
        // dominant fleet disk consumer) without deleting the worktrees. Gated on
        // marker + confinement + mtime-staleness; honors AGEND_TARGET_GC_DISABLE.
        // FIX6: skip the FIRST firing (boot window) — first real sweep is the
        // second firing (~1h uptime); hourly maintenance never needs to run at boot.
        let armed = self
            .target_sweep_armed
            .swap(true, std::sync::atomic::Ordering::Relaxed);
        if armed {
            if let Some((max_age, min_size)) = crate::worktree_pool::target_gc_config() {
                let swept = crate::worktree_pool::target_sweep_run(ctx.home, max_age, min_size);
                let reclaimed = swept.iter().filter(|r| r.removed).count();
                if reclaimed > 0 {
                    let freed: u64 = swept
                        .iter()
                        .filter(|r| r.removed)
                        .map(|r| r.freed_bytes)
                        .sum();
                    tracing::info!(
                        reclaimed,
                        freed_mb = freed / (1024 * 1024),
                        "gc_tick: stale target/ build dirs reclaimed"
                    );
                }
            }
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
        assert!(handler.gate.fire()); // tick 0
        assert!(!handler.gate.fire()); // tick 1
        assert!(!handler.gate.fire()); // tick 2
        assert!(handler.gate.fire()); // tick 3
        assert!(!handler.gate.fire()); // tick 4
    }
}
