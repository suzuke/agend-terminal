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

pub(crate) struct WorktreeRegistrySweepHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
}

impl WorktreeRegistrySweepHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
        }
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
        worktree_auto_cleanup(ctx.home, ctx.configs);
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

        std::fs::remove_dir_all(&home).ok();
    }
}
