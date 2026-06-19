//! Inbox-maintenance composite: every-60-tick batch of 6 sub-ops gated
//! on a single cadence counter. Extracted verbatim from
//! `src/daemon/mod.rs:667-728` (pre-T-B3) — sub-ops preserved in the
//! same order, same call signatures, same gating. The pre-extraction
//! `static AtomicU64` counter moves onto the struct.

use super::{PerTickHandler, TickContext};
use crate::api::ConfigRegistry;
use std::path::Path;

pub(crate) struct InboxMaintenanceHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
}

impl InboxMaintenanceHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
        }
    }
}

impl PerTickHandler for InboxMaintenanceHandler {
    fn name(&self) -> &'static str {
        "inbox_maintenance"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        // Sub-op order matches the pre-extraction inline composite verbatim.
        // #1051: hotspot_scan sub-op removed — the hotspot collision
        // watchdog feature was Phase 1 design and is now handled by
        // fleet team-membership orchestration. The whole feature
        // (hotspot.rs + daemon::hotspot_scan wrapper) is deleted in
        // the same commit.
        crate::inbox::sweep_expired(ctx.home);
        // #2299: revert stale `delivering` rows (recipient turn died after the
        // drain, never confirmed) back to `unread` for re-delivery — the net
        // under explicit ack (C) + implicit next-drain ack (A).
        crate::inbox::reclaim_stale_delivering(ctx.home);
        crate::inbox::check_disk_space(ctx.home);
        crate::daemon::run_task_maintenance(ctx.home);
        worktree_auto_cleanup(ctx.home, ctx.configs);
    }
}

/// Worktree auto-cleanup (runtime registry based): drop branches whose
/// PRs have merged into main. Logged via `event_log` + tracing on every
/// removal so operators can audit. Verbatim from the pre-extraction
/// block at mod.rs:678-717.
fn worktree_auto_cleanup(home: &Path, configs: &ConfigRegistry) {
    let cfgs = configs.lock();
    let config_data: std::collections::HashMap<
        String,
        (Option<std::path::PathBuf>, Option<std::path::PathBuf>),
    > = cfgs
        .iter()
        .map(|(name, cfg)| {
            (
                name.clone(),
                (cfg.working_dir.clone(), cfg.worktree_source.clone()),
            )
        })
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
    let cleaned = crate::worktree_cleanup::sweep_from_registry(&config_data, &fleet_dirs);
    for (branch, path) in &cleaned {
        crate::event_log::log(
            home,
            "worktree_auto_removed",
            branch,
            &format!("path={path}, branch merged into main"),
        );
        tracing::info!(branch, path, "worktree auto-removed (branch merged)");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Cadence predicate test (same pattern as PollReminderHandler).
    /// N=60 is the production value; pin a small N for compact assertion.
    #[test]
    fn fires_on_first_tick_then_every_n() {
        let h = InboxMaintenanceHandler::new(4);
        let fires: Vec<bool> = (0..9).map(|_| h.gate.fire()).collect();
        assert_eq!(
            fires,
            vec![true, false, false, false, true, false, false, false, true]
        );
    }

    /// Smoke test: `run()` against an empty registry + temp home must
    /// complete without panic. Every sub-op tolerates missing state
    /// (empty inbox dir, no tasks.json, empty configs, no fleet.yaml),
    /// so this exercises the composite end-to-end.
    #[test]
    fn run_is_no_op_on_empty_fixtures() {
        use crate::agent::{AgentRegistry, ExternalRegistry};
        use parking_lot::Mutex;
        use std::collections::HashMap;
        use std::sync::Arc;

        let tag = std::process::id();
        let home = std::env::temp_dir().join(format!("agend-inbox-maint-handler-{tag}"));
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

        // N=1 forces every call to fire (every sub-op runs).
        let h = InboxMaintenanceHandler::new(1);
        h.run(&ctx);
        h.run(&ctx);

        // Cleanup
        std::fs::remove_dir_all(&home).ok();
    }
}
