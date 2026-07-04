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
        // #2550 W5: single driver now owns the whole worktree-GC lifecycle,
        // including `.trash` purging — previously a separate ~1h-delayed
        // independent sweep's job (`retention::worktrees::sweep`), now folded
        // in here on the SAME fire-on-first cadence (no phase lag, decision Q2).
        //
        // NOTE (behavior change, deliberate, NOT covered by decision Q3's
        // "gate coverage unchanged"): this call is UNCONDITIONAL, unlike the
        // old `sweep()` it replaces, which only ran (and so only purged
        // `.trash`) when `AGEND_WORKTREE_GC=1`. Q3 preserved the gate on the
        // ARCHIVE-DECISION path (CleanRelease's fallthrough); it says nothing
        // about cleaning up entries already IN `.trash`, which can land there
        // via the ForceReclaim path (never gated, before or after this PR) —
        // in a gate-off install, those entries used to accumulate forever
        // (nothing ever called `purge_trash`). `AGEND_WORKTREE_GC_TRASH_DAYS`
        // is the correct, already-independent lever for "how long to keep
        // `.trash`" (including "never purge": set it very large); coupling
        // the purge to the unrelated archive-decision gate was itself the gap.
        crate::daemon::retention::worktrees::purge_trash(ctx.home);

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
    use crate::agent::{AgentRegistry, ExternalRegistry};
    use parking_lot::Mutex as PLMutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn should_fire_respects_every_n_ticks() {
        let handler = GcTickHandler::new(3);
        assert!(handler.gate.fire()); // tick 0
        assert!(!handler.gate.fire()); // tick 1
        assert!(!handler.gate.fire()); // tick 2
        assert!(handler.gate.fire()); // tick 3
        assert!(!handler.gate.fire()); // tick 4
    }

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-gc-tick-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// #2550 W5 (seat-2 finding on PR #2599): `purge_trash` is now called
    /// UNCONDITIONALLY from `gc_tick`, unlike the old standalone `sweep()` it
    /// replaces, which only purged `.trash` when `AGEND_WORKTREE_GC=1` — a
    /// gate-off install accumulated `.trash` entries forever (nothing ever
    /// purged them). Deliberately NOT covered by decision Q3 ("gate coverage
    /// unchanged" applies to the archive-DECISION path only); this is a
    /// separate, intentional fix. Proves an aged `.trash` entry is purged by
    /// a `gc_tick` pass with `AGEND_WORKTREE_GC` left UNSET.
    #[test]
    fn purge_trash_runs_regardless_of_worktree_gc_gate_2550_w5() {
        std::env::remove_var("AGEND_WORKTREE_GC");
        let home = tmp_home("purge-gate-independent");
        let trash_dir = home
            .join(".trash")
            .join("worktrees")
            .join("agent-100-000000000"); // secs=100 → ancient, past any retention window
        std::fs::create_dir_all(&trash_dir).unwrap();

        let registry: AgentRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let configs: Arc<PLMutex<HashMap<String, crate::daemon::AgentConfig>>> =
            Arc::new(PLMutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        GcTickHandler::new(1).run(&ctx);

        assert!(
            !trash_dir.exists(),
            "an aged .trash entry must be purged by gc_tick even with \
             AGEND_WORKTREE_GC unset — purge_trash is no longer gated"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
