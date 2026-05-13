//! Cron + one-shot schedule firing per tick — thin wrapper around
//! [`crate::daemon::cron_tick::check_schedules`]. Extracted from
//! `src/daemon/mod.rs:613` (pre-T-B5), which was already a single
//! function call: all schedule state (the `.schedule_last_check`
//! file, the schedule store, fired-once tracking) lives inside
//! `cron_tick::check_schedules` and is persisted to disk under
//! `home`. Nothing to migrate to a handler field.
//!
//! Kept as a distinct handler (vs inlining `cron_tick::check_schedules`
//! directly at the call site) so the per-tick dispatch shape stays
//! uniform: every periodic concern flows through a `PerTickHandler`,
//! reachable by name from the future Vec aggregator.

use super::{PerTickHandler, TickContext};

pub(crate) struct CheckSchedulesHandler;

impl CheckSchedulesHandler {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl PerTickHandler for CheckSchedulesHandler {
    fn name(&self) -> &'static str {
        "check_schedules"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        crate::daemon::cron_tick::check_schedules(ctx.home, ctx.registry);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::agent::{AgentRegistry, ExternalRegistry};
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::Arc;

    /// Smoke test: empty registry + temp home with no `schedules.json`
    /// → `check_schedules` returns at the empty-store guard, no panic.
    /// The interesting integration paths (cron parsing, schedule firing,
    /// one-shot consumption) are covered by existing `cron_tick` and
    /// `schedules` tests; this PR is pure relocation.
    #[test]
    fn run_is_noop_with_no_schedules() {
        let home = std::env::temp_dir().join(format!(
            "agend-check-schedules-handler-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&home).ok();
        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        CheckSchedulesHandler::new().run(&ctx);

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn name_matches_module() {
        assert_eq!(CheckSchedulesHandler::new().name(), "check_schedules");
    }
}
