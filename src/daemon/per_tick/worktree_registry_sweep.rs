//! #2550 W5 PR-3: worktree-registry auto-cleanup — drops local branches
//! whose PRs have merged into main, via the runtime config registry (not the
//! GC-candidate marker walk `worktree_pool::gc.rs` uses). Extracted verbatim
//! from `inbox_maintenance.rs` (was one of its sub-ops, gated on the SAME
//! every-60-tick cadence) — this logic has nothing to do with inbox
//! maintenance and is semantically GC, but per decision Q4
//! (d-20260704035059093740-0) it keeps its OWN independent 60-tick
//! `PerTickHandler` registration rather than folding into `HourlyGcHandler`'s
//! 360-tick cadence: doing so would regress this cleanup's latency from
//! ~10min to ~1h, a real-world-visible regression the operator did not
//! approve.

use super::{PerTickHandler, TickContext};
use crate::api::ConfigRegistry;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// #P1-2607 incident: `worktree_auto_cleanup` (fetch --prune + a squash-merge
/// check per candidate branch) ran INLINE on the daemon's main tick loop —
/// its first production run against the canonical repo took 83s wall time
/// (172 accumulated candidates, five months of #2605's dead repo-discovery
/// finally fixed) and froze the ENTIRE daemon (TUI, inbox, every other
/// handler) for that whole window. Because dry-run mode never consumes
/// candidates, this repeated every ~10 minutes indefinitely.
///
/// Fix: the real work now runs in its own background thread; `run()` itself
/// only checks the cadence gate, checks/sets the re-entrancy guard, and
/// spawns — it never blocks the tick loop. `in_flight` prevents a second
/// round from stacking on top of one still running (which would compound the
/// cost, not fix it) — if the previous round hasn't finished by the next
/// scheduled fire, that fire is skipped and logged; the round after tries
/// again. See `worktree_cleanup::is_squash_gc_eligible`'s tip-SHA cache for
/// the complementary fix (bounds each round's cost to just the NEW/moved
/// candidates instead of re-deriving the whole accumulated set every time).
pub(crate) struct WorktreeRegistrySweepHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
    in_flight: Arc<AtomicBool>,
}

impl WorktreeRegistrySweepHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }

    #[cfg(test)]
    fn is_in_flight(&self) -> bool {
        self.in_flight.load(Ordering::Acquire)
    }
}

impl PerTickHandler for WorktreeRegistrySweepHandler {
    fn name(&self) -> &'static str {
        "worktree_registry_sweep"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        // #P1-2607: a previous round is still running in its background
        // thread — skip this tick's fire instead of stacking a second sweep
        // on top of it.
        if self.in_flight.swap(true, Ordering::AcqRel) {
            tracing::warn!(
                "worktree_registry_sweep: previous round still in flight, \
                 skipping this tick's fire (will retry next cadence)"
            );
            return;
        }
        let home = ctx.home.to_path_buf();
        let configs = Arc::clone(ctx.configs);
        let in_flight = Arc::clone(&self.in_flight);
        // fire-and-forget: #P1-2607 — moves the potentially-slow sweep
        // (git subprocess per candidate branch) off the daemon's main tick
        // loop. No JoinHandle is kept; completion is signaled via
        // `in_flight` (cleared once the sweep returns) and results remain
        // fully observable via tracing + event_log per candidate, same as
        // before this offload.
        std::thread::spawn(move || {
            #[cfg(test)]
            test_hooks::maybe_delay();
            worktree_auto_cleanup(&home, &configs);
            in_flight.store(false, Ordering::Release);
        });
    }
}

/// #P1-2607 regression-test seam: lets a test simulate a slow sweep (the
/// pathological case that froze the daemon) without needing a real repo with
/// hundreds of candidate branches. No-op in production — `maybe_delay` is
/// only ever called from a `#[cfg(test)]` call site.
#[cfg(test)]
mod test_hooks {
    use std::sync::atomic::{AtomicU64, Ordering};

    static DELAY_MS: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn set_delay_ms(ms: u64) {
        DELAY_MS.store(ms, Ordering::Release);
    }

    pub(crate) fn clear_delay() {
        DELAY_MS.store(0, Ordering::Release);
    }

    pub(crate) fn maybe_delay() {
        let ms = DELAY_MS.load(Ordering::Acquire);
        if ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }
}

/// Worktree auto-cleanup (runtime registry based): drop branches whose
/// PRs have merged into main. Logged via `event_log` + tracing on every
/// removal so operators can audit. Verbatim from `inbox_maintenance.rs`
/// (was verbatim from the pre-extraction block at mod.rs:678-717).
///
/// #2605: repo discovery moved to live `binding.json` state
/// (`sweep_from_registry` reads it via `home`) instead of the removed
/// `AgentConfig.worktree_source` cache. Real deletion is additionally gated by
/// `worktree_cleanup::prune_live_enabled` (default off) — while off, the same
/// candidates are identified but not deleted, and are logged under a distinct
/// dry-run event kind so an operator can diff them against a fresh audit
/// before opting in.
fn worktree_auto_cleanup(home: &Path, configs: &ConfigRegistry) {
    let cfgs = configs.lock();
    let config_data: std::collections::HashMap<String, Option<std::path::PathBuf>> = cfgs
        .iter()
        .map(|(name, cfg)| (name.clone(), cfg.working_dir.clone()))
        .collect();
    drop(cfgs);
    let fleet_dirs: Vec<std::path::PathBuf> =
        crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
            .ok()
            .map(|c| {
                c.instance_names()
                    .iter()
                    .filter_map(|n| c.resolve_instance(n).and_then(|r| r.working_directory))
                    .collect()
            })
            .unwrap_or_default();
    let cleaned = crate::worktree_cleanup::sweep_from_registry(home, &config_data, &fleet_dirs);
    let dry_run = !crate::worktree_cleanup::prune_live_enabled();
    let event_kind = if dry_run {
        "worktree_prune_dry_run_candidate"
    } else {
        "worktree_auto_removed"
    };
    for (branch, path, reason) in &cleaned {
        let detail = if dry_run {
            format!("path={path}, reason={reason} (dry-run candidate, not deleted)")
        } else {
            format!("path={path}, reason={reason}")
        };
        crate::event_log::log(home, event_kind, branch, &detail);
        if dry_run {
            tracing::info!(
                branch,
                path,
                reason,
                "worktree prune candidate (dry-run, not deleted)"
            );
        } else {
            tracing::info!(branch, path, reason, "worktree auto-removed");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Cadence predicate test (same pattern as InboxMaintenanceHandler's,
    /// which this handler's cadence was extracted out of unchanged).
    #[test]
    fn fires_on_first_tick_then_every_n() {
        let h = WorktreeRegistrySweepHandler::new(4);
        let fires: Vec<bool> = (0..9).map(|_| h.gate.fire()).collect();
        assert_eq!(
            fires,
            vec![true, false, false, false, true, false, false, false, true]
        );
    }

    /// Smoke test: `run()` against an empty registry + temp home must
    /// complete without panic (empty configs, no fleet.yaml).
    #[test]
    fn run_is_no_op_on_empty_fixtures() {
        use crate::agent::{AgentRegistry, ExternalRegistry};
        use parking_lot::Mutex;
        use std::collections::HashMap;
        use std::sync::Arc;

        let tag = std::process::id();
        let home = std::env::temp_dir().join(format!("agend-worktree-reg-sweep-handler-{tag}"));
        std::fs::create_dir_all(&home).unwrap();

        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        // N=1 forces every call to fire.
        let h = WorktreeRegistrySweepHandler::new(1);
        h.run(&ctx);
        h.run(&ctx);

        // #P1-2607: the real work now runs in a background thread; give it a
        // moment to finish before tearing down `home` out from under it.
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::remove_dir_all(&home).ok();
    }

    /// #P1-2607 freeze-regression pin: `run()` must return near-instantly
    /// even while the sweep itself is slow (the pathological case that froze
    /// the daemon — 83s of git subprocess work on the main tick loop,
    /// simulated here via the `test_hooks` delay seam instead of a real repo
    /// with hundreds of candidates). Also pins the re-entrancy guard: a
    /// second fire while the first round is still "running" must skip, not
    /// spawn an overlapping second round.
    #[test]
    fn run_does_not_block_tick_loop_during_slow_sweep_p1_2607() {
        use crate::agent::{AgentRegistry, ExternalRegistry};
        use parking_lot::Mutex;
        use std::collections::HashMap;
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let tag = std::process::id();
        let home = std::env::temp_dir().join(format!("agend-worktree-reg-sweep-slow-{tag}"));
        std::fs::create_dir_all(&home).unwrap();

        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        test_hooks::set_delay_ms(300);
        let h = WorktreeRegistrySweepHandler::new(1); // fires on every call

        let start = Instant::now();
        h.run(&ctx);
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "run() must not block the tick loop on the (delayed) sweep, took {:?}",
            start.elapsed()
        );
        assert!(
            h.is_in_flight(),
            "the background round should still be running (300ms delay, checked immediately)"
        );

        // Simulates the very next tick arriving while the previous round is
        // still in flight: the gate fires again, but the re-entrancy guard
        // must skip spawning a second overlapping round.
        let start2 = Instant::now();
        h.run(&ctx);
        assert!(
            start2.elapsed() < Duration::from_millis(100),
            "the re-entrant skip path must also return near-instantly"
        );

        // Poll for the background round to finish (bounded — the delay is
        // 300ms, so 2s is a generous ceiling for CI jitter) and confirm the
        // guard clears once it does.
        let deadline = Instant::now() + Duration::from_secs(2);
        while h.is_in_flight() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !h.is_in_flight(),
            "in_flight must clear once the background sweep completes"
        );

        test_hooks::clear_delay();
        std::fs::remove_dir_all(&home).ok();
    }
}
