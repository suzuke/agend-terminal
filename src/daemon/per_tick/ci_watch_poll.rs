//! CI-watch poll per tick — thin wrapper around
//! [`crate::daemon::ci_watch::check_ci_watches`]. Extracted from
//! `src/daemon/mod.rs:614` (pre-T-B5), which was already a single
//! function call: the ci_watch module (split into its own
//! submodule tree by #701) owns all polling state, eager-GC pass,
//! and lazy expiry. Nothing to migrate to a handler field.
//!
//! Kept as a distinct handler (vs inlining the call) so the per-tick
//! dispatch shape stays uniform — every periodic concern flows through
//! a `PerTickHandler`, reachable by name from the future Vec aggregator.

use super::{PerTickHandler, TickContext};

pub(crate) struct CiWatchPollHandler;

impl CiWatchPollHandler {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl PerTickHandler for CiWatchPollHandler {
    fn name(&self) -> &'static str {
        "ci_watch_poll"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        crate::daemon::ci_watch::check_ci_watches(ctx.home, ctx.registry);
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

    /// Smoke test: empty registry + temp home with no watches → handler
    /// completes without panic. Integration paths (provider invocation,
    /// branch SHA polling, expiry sweep) are covered by existing
    /// `daemon::ci_watch::poller` tests.
    #[test]
    fn run_is_noop_with_no_watches() {
        let home = std::env::temp_dir().join(format!(
            "agend-ci-watch-handler-{}-{}",
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

        CiWatchPollHandler::new().run(&ctx);

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn name_matches_module() {
        assert_eq!(CiWatchPollHandler::new().name(), "ci_watch_poll");
    }
}
