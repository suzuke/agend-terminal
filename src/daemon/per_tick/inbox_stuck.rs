//! #1491(A) cadence wrapper for the inbox-stuck watchdog. Fires
//! [`crate::daemon::inbox_stuck_watchdog::scan_and_emit`] every
//! `every_n_ticks` ticks (same cadence pattern as [`super::poll_reminder`]).
//! The dedup map lives on the handler so "already alerted" state survives
//! across ticks.

use super::{PerTickHandler, TickContext};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

/// #2127 Phase 1: the inbox-stuck dedup latch (agent → last-alert time), shared so
/// the reclaim handler can drop an agent's entry after reclaiming its board work
/// (resetting the repeat stuck-alert). `Arc<Mutex<…>>` because the two handlers
/// are independent `Box<dyn PerTickHandler>` instances and cannot reach each
/// other directly.
pub(crate) type AlertLatch = Arc<Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>>;

pub(crate) struct InboxStuckHandler {
    /// Cadence + boot-grace, bundled (see [`super::NOTIFICATION_BOOT_GRACE`]):
    /// suppresses firing within the grace window of construction without
    /// advancing the counter, then fires on tick indices 0, N, 2N, ….
    gate: crate::daemon::cadence_gate::CadenceGate,
    last_alerted: AlertLatch,
}

impl InboxStuckHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self::with_latch(every_n_ticks, Arc::new(Mutex::new(HashMap::new())))
    }

    /// Construct sharing an externally-owned [`AlertLatch`] so another handler
    /// (the #2127 reclaim handler) can clear an agent's entry. Production wiring
    /// in `build_default_handlers` uses this; `new` keeps a private latch.
    pub(crate) fn with_latch(every_n_ticks: u64, last_alerted: AlertLatch) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_with_boot_grace(
                every_n_ticks,
                super::NOTIFICATION_BOOT_GRACE,
            ),
            last_alerted,
        }
    }

    /// A clone of the shared dedup latch, for the reclaim handler to clear an
    /// agent's repeat-alert entry after reclaim.
    pub(crate) fn latch(&self) -> AlertLatch {
        self.last_alerted.clone()
    }

    #[cfg(test)]
    fn new_at(every_n_ticks: u64, created_at: std::time::Instant) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_with_boot_grace_at(
                every_n_ticks,
                created_at,
                super::NOTIFICATION_BOOT_GRACE,
            ),
            last_alerted: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl PerTickHandler for InboxStuckHandler {
    fn name(&self) -> &'static str {
        "inbox_stuck_watchdog"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        let now = chrono::Utc::now();
        // #latch-prune (cleanup-on-delete, #1923 G5 class): snapshot live agent
        // names (registry locked then dropped — BEFORE locking the latch, so no
        // nesting) so the `last_alerted` dedup latch can drop deleted agents
        // below; else a same-name redeploy inherits a stale re-alert timer.
        let live: std::collections::HashSet<String> = {
            let reg = crate::agent::lock_registry(ctx.registry);
            reg.values().map(|h| h.name.as_str().to_string()).collect()
        };
        let mut last = self.last_alerted.lock();
        crate::daemon::inbox_stuck_watchdog::scan_and_emit(ctx.home, &now, &mut last);
        last.retain(|name, _| live.contains(name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn past_grace() -> Instant {
        Instant::now() - super::super::NOTIFICATION_BOOT_GRACE - Duration::from_secs(1)
    }

    #[test]
    fn fires_at_expected_cadence() {
        let h = InboxStuckHandler::new_at(3, past_grace());
        let fires: Vec<bool> = (0..7).map(|_| h.gate.fire()).collect();
        assert_eq!(fires, vec![true, false, false, true, false, false, true]);
    }

    #[test]
    fn name_matches_module() {
        assert_eq!(InboxStuckHandler::new(30).name(), "inbox_stuck_watchdog");
    }

    /// #t-watchdog-boot-suppress: within boot-grace, `fire` is false (no alert
    /// for the stale backlog) and the counter is NOT consumed; past grace the
    /// first tick fires. Combined with `inbox_stuck_watchdog`'s scan_and_emit
    /// tests (which prove a real stuck pile DOES alert), this pins "suppressed
    /// during grace, fires for a genuine stuck agent after grace".
    #[test]
    fn boot_grace_suppresses_then_fires() {
        let fresh = InboxStuckHandler::new(30); // created_at ≈ now → in grace
        assert!(!fresh.gate.fire(), "in boot-grace → suppressed");
        assert!(!fresh.gate.fire(), "still suppressed; counter not consumed");

        let aged = InboxStuckHandler::new_at(30, past_grace());
        assert!(aged.gate.fire(), "after grace, first tick fires");
    }

    /// #latch-prune (cleanup-on-delete, #1923 G5 class): a `last_alerted` dedup
    /// entry for an agent no longer in the registry is dropped on the next
    /// `run` (real entry, empty registry = deleted; `new_at(.., past_grace())`
    /// so the boot-grace gate fires) — so a same-name redeploy doesn't inherit
    /// a stale re-alert timer that swallows its first stuck-inbox alert.
    #[test]
    fn deleted_agent_alert_timer_pruned_on_run() {
        use parking_lot::Mutex as PLMutex;
        use std::collections::HashMap;
        use std::sync::Arc;
        let home =
            std::env::temp_dir().join(format!("agend-inboxstuck-prune-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let registry: crate::agent::AgentRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let externals: crate::agent::ExternalRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let configs: Arc<PLMutex<HashMap<String, crate::daemon::AgentConfig>>> =
            Arc::new(PLMutex::new(HashMap::new()));
        let h = InboxStuckHandler::new_at(1, past_grace()); // past grace → gate fires
        h.last_alerted
            .lock()
            .insert("ghost".to_string(), chrono::Utc::now());
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        h.run(&ctx);
        assert!(
            !h.last_alerted.lock().contains_key("ghost"),
            "a deleted agent's re-alert timer must be pruned on run (cleanup-on-delete)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #latch-prune reverse-regression (reviewer-2 #2097): a LIVE agent keeps
    /// its re-alert timer. inbox_stuck's `live` is already unconditional
    /// (`reg.values().map(name)`, no `resolved_context()` gate), so there is no
    /// subset to regress into TODAY — this pins it that way (a future gating
    /// edit that dropped a live agent would re-fire its stuck alert).
    #[test]
    fn live_agent_keeps_alert_timer() {
        use parking_lot::Mutex as PLMutex;
        use std::collections::HashMap;
        use std::sync::Arc;
        let home =
            std::env::temp_dir().join(format!("agend-inboxstuck-keep-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let registry: crate::agent::AgentRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let (handle, _reader) = crate::daemon::per_tick::mock_live_agent_no_context("alive");
        registry.lock().insert(handle.id, handle);
        let externals: crate::agent::ExternalRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let configs: Arc<PLMutex<HashMap<String, crate::daemon::AgentConfig>>> =
            Arc::new(PLMutex::new(HashMap::new()));
        let h = InboxStuckHandler::new_at(1, past_grace());
        h.last_alerted
            .lock()
            .insert("alive".to_string(), chrono::Utc::now());
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        h.run(&ctx);
        assert!(
            h.last_alerted.lock().contains_key("alive"),
            "a LIVE agent must KEEP its re-alert timer (retain against ALL live agents)"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
