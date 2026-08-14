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

    /// #2549 W3: `pub(crate)` (not just `#[cfg(test)] fn`) so
    /// `notification_watchdogs`'s merged-handler tests can construct this
    /// sub-handler past its boot-grace window too. Zero behavior change —
    /// visibility only.
    #[cfg(test)]
    pub(crate) fn new_at(every_n_ticks: u64, created_at: std::time::Instant) -> Self {
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
        // names and the live usage/quota predicate (registry L0 → core L1,
        // then both locks dropped BEFORE scanner file IO). A same-name
        // redeploy must not inherit a stale re-alert timer or blocked episode.
        let (live, usage_blocked): (
            std::collections::HashSet<String>,
            HashMap<String, Option<String>>,
        ) = {
            let reg = crate::agent::lock_registry(ctx.registry);
            let mut live = std::collections::HashSet::new();
            let mut usage_blocked = HashMap::new();
            for handle in reg.values() {
                let name = handle.name.as_str().to_string();
                live.insert(name.clone());
                let core = handle.core.lock();
                let state = crate::daemon::shadow::operated_state(
                    core.state.current,
                    core.observed_status.as_ref(),
                );
                let quota = matches!(
                    core.health.current_reason,
                    Some(crate::health::BlockedReason::QuotaExceeded)
                );
                if crate::daemon::per_tick::reclaim::is_usage_blocked(state, quota) {
                    let tail = core.vterm.tail_lines(10);
                    usage_blocked.insert(name, crate::daemon::supervisor::parse_unlock_at(&tail));
                }
            }
            (live, usage_blocked)
        };
        // The snapshot is the only current-state authority. Persist a recovery
        // boundary for every live agent absent from the blocked map, after the
        // registry/core locks are dropped, so a later null-unlock episode gets
        // a fresh readable notice without deleting prior ledger history.
        for name in &live {
            if !usage_blocked.contains_key(name) {
                crate::daemon::supervisor::mark_usage_limit_recovered(ctx.home, name);
            }
        }
        let mut last = self.last_alerted.lock();
        crate::daemon::inbox_stuck_watchdog::scan_and_emit_with_blocked(
            ctx.home,
            &now,
            &mut last,
            &usage_blocked,
        );
        last.retain(|name, _| live.contains(name));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-inboxstuck-3225-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_fleet(home: &std::path::Path, fleet: &str) {
        std::fs::write(crate::fleet::fleet_yaml_path(home), fleet).unwrap();
    }

    fn seed_unread(home: &std::path::Path, agent: &str) {
        std::fs::create_dir_all(home.join("inbox")).unwrap();
        for i in 0..4 {
            let mut msg =
                crate::inbox::InboxMessage::new_system("system:test", "task", format!("m{i}"));
            msg.timestamp = (chrono::Utc::now() - chrono::Duration::minutes(45 - i)).to_rfc3339();
            crate::inbox::enqueue(home, agent, msg).unwrap();
        }
    }

    fn run_with_agent(
        home: &std::path::Path,
        handler: &InboxStuckHandler,
        name: &str,
        state: crate::state::AgentState,
        reason: Option<crate::health::BlockedReason>,
    ) -> crate::agent::AgentRegistry {
        let registry: crate::agent::AgentRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let externals: crate::agent::ExternalRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let configs: std::sync::Arc<
            parking_lot::Mutex<std::collections::HashMap<String, crate::daemon::AgentConfig>>,
        > = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let (handle, _reader) = super::super::mock_live_agent_no_context(name);
        {
            let mut core = handle.core.lock();
            core.state.current = state;
            if let Some(reason) = reason {
                core.health.set_blocked_reason(reason);
            }
        }
        registry.lock().insert(handle.id, handle);
        let ctx = TickContext {
            home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        handler.run(&ctx);
        registry
    }

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

    #[test]
    fn usage_limit_with_proven_notice_suppresses_inbox_alert_at_real_entry() {
        let home = tmp_home("usage-suppressed");
        write_fleet(
            &home,
            "instances:\n  worker:\n    backend: claude\n  lead:\n    backend: claude\n\
             teams:\n  t:\n    members: [worker, lead]\n    orchestrator: lead\n",
        );
        seed_unread(&home, "worker");
        crate::daemon::supervisor::record_usage_limit_notified(
            &home,
            "worker",
            None,
            chrono::Utc::now(),
        );
        let handler = InboxStuckHandler::new_at(1, past_grace());
        let _registry = run_with_agent(
            &home,
            &handler,
            "worker",
            crate::state::AgentState::UsageLimit,
            Some(crate::health::BlockedReason::QuotaExceeded),
        );
        let lead = crate::inbox::drain(&home, "lead");
        assert!(
            lead.iter()
                .all(|m| m.kind.as_deref() != Some("inbox_stuck_watchdog")),
            "a proven readable UsageLimit notice suppresses the redundant inbox alert: {lead:?}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn no_team_usage_limit_gets_one_restart_stable_readable_fallback() {
        let home = tmp_home("usage-fallback");
        write_fleet(
            &home,
            "instances:\n  worker:\n    backend: claude\n  general:\n    backend: claude\n",
        );
        seed_unread(&home, "worker");
        let handler = InboxStuckHandler::new_at(1, past_grace());
        let registry = run_with_agent(
            &home,
            &handler,
            "worker",
            crate::state::AgentState::UsageLimit,
            Some(crate::health::BlockedReason::QuotaExceeded),
        );
        let first = crate::inbox::drain(&home, "general");
        assert_eq!(
            first.len(),
            1,
            "fallback must be readable exactly once: {first:?}"
        );
        assert!(
            first[0].text.contains("usage_limit"),
            "fallback identifies UsageLimit: {first:?}"
        );

        // Simulate a daemon restart: both the handler latch and live registry
        // are reconstructed; only usage_limit_notify.json survives.
        drop(registry);
        drop(handler);
        let restarted_handler = InboxStuckHandler::new_at(1, past_grace());
        let _restarted_registry = run_with_agent(
            &home,
            &restarted_handler,
            "worker",
            crate::state::AgentState::UsageLimit,
            Some(crate::health::BlockedReason::QuotaExceeded),
        );
        assert!(
            crate::inbox::drain(&home, "general").is_empty(),
            "fresh restart must not repeat within the same episode"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn recovered_null_unlock_gets_fresh_notice_after_restart() {
        let home = tmp_home("usage-null-recovery");
        write_fleet(
            &home,
            "instances:\n  worker:\n    backend: claude\n  general:\n    backend: claude\n",
        );
        seed_unread(&home, "worker");

        // Episode one: a fresh daemon entry emits exactly one readable fallback.
        let first_handler = InboxStuckHandler::new_at(1, past_grace());
        let first_registry = run_with_agent(
            &home,
            &first_handler,
            "worker",
            crate::state::AgentState::UsageLimit,
            Some(crate::health::BlockedReason::QuotaExceeded),
        );
        let first = crate::inbox::drain(&home, "general");
        assert_eq!(first.len(), 1, "first episode must notify once: {first:?}");
        assert!(first[0].text.contains("usage_limit"));
        drop(first_registry);
        drop(first_handler);

        // Recovery: clear the source pile so the recovery entry itself cannot
        // emit an ordinary inbox-stuck alert; it must still persist the reset.
        assert!(!crate::inbox::drain(&home, "worker").is_empty());
        let recovery_handler = InboxStuckHandler::new_at(1, past_grace());
        let recovery_registry = run_with_agent(
            &home,
            &recovery_handler,
            "worker",
            crate::state::AgentState::Idle,
            None,
        );
        assert!(crate::inbox::drain(&home, "general").is_empty());
        let recovered_record: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(crate::daemon::supervisor::usage_limit_notify_path(&home))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(recovered_record["worker"]["active"], false);
        assert_eq!(recovered_record["worker"]["episode_nonce"], 1);
        drop(recovery_registry);
        drop(recovery_handler);

        // Episode two starts inside the old 24-hour null-unlock window. A
        // fresh handler and fresh live registry must still emit its new notice.
        seed_unread(&home, "worker");
        let second_handler = InboxStuckHandler::new_at(1, past_grace());
        let _second_registry = run_with_agent(
            &home,
            &second_handler,
            "worker",
            crate::state::AgentState::UsageLimit,
            Some(crate::health::BlockedReason::QuotaExceeded),
        );
        let second = crate::inbox::drain(&home, "general");
        assert_eq!(
            second.len(),
            1,
            "a recovered null-unlock episode must notify again: {second:?}"
        );
        assert!(second[0].text.contains("usage_limit"));
        let reactivated_record: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(crate::daemon::supervisor::usage_limit_notify_path(&home))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(reactivated_record["worker"]["active"], true);
        assert_eq!(reactivated_record["worker"]["episode_nonce"], 1);
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn self_orchestrator_usage_limit_falls_back_to_general_once() {
        let home = tmp_home("usage-self-orch");
        write_fleet(
            &home,
            "instances:\n  worker:\n    backend: claude\n  general:\n    backend: claude\n\
             teams:\n  t:\n    members: [worker]\n    orchestrator: worker\n",
        );
        seed_unread(&home, "worker");
        let handler = InboxStuckHandler::new_at(1, past_grace());
        let _registry = run_with_agent(
            &home,
            &handler,
            "worker",
            crate::state::AgentState::UsageLimit,
            Some(crate::health::BlockedReason::QuotaExceeded),
        );
        let general = crate::inbox::drain(&home, "general");
        assert_eq!(
            general.len(),
            1,
            "self-orchestrator needs a readable peer fallback"
        );
        assert!(general[0].text.contains("usage_limit"));
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn clearing_usage_state_reenables_inbox_alert() {
        let home = tmp_home("usage-recovery");
        write_fleet(
            &home,
            "instances:\n  worker:\n    backend: claude\n  lead:\n    backend: claude\n\
             teams:\n  t:\n    members: [worker, lead]\n    orchestrator: lead\n",
        );
        seed_unread(&home, "worker");
        crate::daemon::supervisor::record_usage_limit_notified(
            &home,
            "worker",
            None,
            chrono::Utc::now(),
        );
        let handler = InboxStuckHandler::new_at(1, past_grace());
        let registry = run_with_agent(
            &home,
            &handler,
            "worker",
            crate::state::AgentState::UsageLimit,
            Some(crate::health::BlockedReason::QuotaExceeded),
        );
        assert!(crate::inbox::drain(&home, "lead").is_empty());
        {
            let registry_guard = registry.lock();
            let handle = registry_guard.values().next().unwrap();
            let mut core = handle.core.lock();
            core.state.current = crate::state::AgentState::Idle;
            core.health.clear_blocked_reason();
        }
        let externals: crate::agent::ExternalRegistry =
            std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let configs: std::sync::Arc<
            parking_lot::Mutex<std::collections::HashMap<String, crate::daemon::AgentConfig>>,
        > = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        handler.run(&ctx);
        let lead = crate::inbox::drain(&home, "lead");
        assert!(
            lead.iter()
                .any(|m| m.kind.as_deref() == Some("inbox_stuck_watchdog")),
            "clearing live usage signals re-enables ordinary inbox alerts: {lead:?}"
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn stale_quota_reason_on_rate_limit_keeps_inbox_alert() {
        for (tag, state) in [
            ("rate-limit", crate::state::AgentState::RateLimit),
            (
                "server-rate-limit",
                crate::state::AgentState::ServerRateLimit,
            ),
            ("api-error", crate::state::AgentState::ApiError),
        ] {
            let home = tmp_home(tag);
            write_fleet(
                &home,
                "instances:\n  worker:\n    backend: claude\n  lead:\n    backend: claude\n\
                 teams:\n  t:\n    members: [worker, lead]\n    orchestrator: lead\n",
            );
            seed_unread(&home, "worker");
            let handler = InboxStuckHandler::new_at(1, past_grace());
            let _registry = run_with_agent(
                &home,
                &handler,
                "worker",
                state,
                Some(crate::health::BlockedReason::QuotaExceeded),
            );
            let lead = crate::inbox::drain(&home, "lead");
            assert!(
                lead.iter()
                    .any(|m| m.kind.as_deref() == Some("inbox_stuck_watchdog")),
                "{state:?} plus stale QuotaExceeded must preserve the ordinary alert: {lead:?}"
            );
            std::fs::remove_dir_all(home).ok();
        }
    }
}
