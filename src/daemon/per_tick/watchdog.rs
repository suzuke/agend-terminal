//! Watchdog: classify PTY tail per agent and mutate `core.health` with
//! the resulting `BlockedReason`. Extracted verbatim from
//! `src/daemon/mod.rs:628-650` (pre-T-B4) — same iteration order, same
//! lock-acquisition chain, same vterm tail capture, same call to
//! `crate::daemon::watchdog::run_watchdog_pass`.
//!
//! **Cohort note** (T-B4): paired with [`super::hang_detection`] so the
//! same-tick `core.health` mutation sequence — hang → watchdog — stays
//! contained in one PR. Extracting either alone would route the
//! sequence across module boundaries with no compile-time signal that
//! the order matters.
//!
//! Env-flag note (per lead's T-B4 spot-check): `dry_run` is read ONCE
//! at construction (in `run_core`'s startup phase). `run()` reads
//! `self.dry_run` rather than re-querying the env on every tick — the
//! pre-extraction inline code only read `AGEND_WATCHDOG_DRY_RUN` at
//! daemon startup, so re-reading would be a subtle behavior change.

use super::{PerTickHandler, TickContext};
use crate::agent;

pub(crate) struct WatchdogHandler {
    dry_run: bool,
}

impl WatchdogHandler {
    pub(crate) fn new(dry_run: bool) -> Self {
        Self { dry_run }
    }
}

impl PerTickHandler for WatchdogHandler {
    fn name(&self) -> &'static str {
        "watchdog"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        // Lock-acquisition order per docs/DAEMON-LOCK-ORDERING.md:
        // registry (L0) → per-agent core (L1). `&mut core.health` is a
        // field borrow through the already-held L1 MutexGuard, not a
        // separate Mutex re-acquisition — same shape as HangDetection.
        // #941: holder-tracking wrapper — pairs with HangDetection's
        // migration so the ThreadDumpHandler can attribute either
        // handler as the H1 suspect.
        let reg = agent::lock_registry_tracked(ctx.registry, "watchdog");
        for handle in reg.values() {
            let backend = match crate::backend::Backend::from_command(&handle.backend_command) {
                Some(b) => b,
                None => continue,
            };
            let mut core = handle.core.lock();
            let rows = core.vterm.rows() as usize;
            let screen = core.vterm.tail_lines(rows);
            // bughunt2: pass the live AgentState so run_watchdog_pass can
            // auto-clear a stale rate-limit/quota latch on recovery.
            // KEEP-RAW (#2465): the watchdog is a health/recovery safety net — feeding it the
            // promoted/observed state could let a stale/false 'Active' hook MASK a genuinely
            // stuck agent. Do NOT migrate to operated_state.
            let current_state = core.state.current;
            crate::daemon::watchdog::run_watchdog_pass(
                ctx.home,
                handle.name.as_str(),
                &backend,
                &screen,
                &mut core.health,
                self.dry_run,
                current_state,
            );
        }
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

    /// Empty registry → no iteration, no panic. The interesting paths
    /// (Backend resolution, vterm tail, BlockedReason classification)
    /// are covered by existing `daemon::watchdog` tests; this PR is
    /// pure relocation.
    #[test]
    fn run_is_noop_on_empty_registry() {
        let home = std::env::temp_dir().join(format!(
            "agend-watchdog-handler-{}-{}",
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

        WatchdogHandler::new(false).run(&ctx);
        WatchdogHandler::new(true).run(&ctx);

        assert!(registry.lock().is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    /// Constructor stores `dry_run` for later reads — `run()` reads
    /// `self.dry_run` rather than re-querying the env on every tick.
    /// Pin this so a future refactor doesn't sneak a per-tick env read.
    #[test]
    fn dry_run_field_is_stored_at_construction() {
        let h_dry = WatchdogHandler::new(true);
        let h_live = WatchdogHandler::new(false);
        assert!(h_dry.dry_run);
        assert!(!h_live.dry_run);
    }

    #[test]
    fn name_matches_module() {
        assert_eq!(WatchdogHandler::new(false).name(), "watchdog");
    }
}
