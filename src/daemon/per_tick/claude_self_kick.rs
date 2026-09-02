//! #3230: durable Claude ChannelBridge self-kick acknowledgement watchdog.
//!
//! The bridge persists the accepted delivery before it can receive the
//! consumer's exact-ID `ack_start`. This per-tick scan replays that durable
//! state after daemon/bridge restarts and converts only an old, still-
//! ProtocolAccepted self-kick into one Ambiguous operator alert. It never
//! retries, reads screen state, or treats a hook/time correlation as a turn.

use super::{PerTickHandler, TickContext};

pub(crate) struct ClaudeSelfKickHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
}

impl ClaudeSelfKickHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
        }
    }
}

impl PerTickHandler for ClaudeSelfKickHandler {
    fn name(&self) -> &'static str {
        "claude_self_kick"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        let Ok(fleet) =
            crate::fleet::FleetConfig::load_arc(&crate::fleet::fleet_yaml_path(ctx.home))
        else {
            return;
        };
        for name in fleet.instances.keys() {
            if crate::transport::mode_for_instance(ctx.home, name)
                != crate::transport::TransportMode::ChannelBridge
            {
                continue;
            }
            if let Err(error) =
                crate::transport::claude_channel::self_kick_watchdog_pass(ctx.home, name, &|_| None)
            {
                tracing::warn!(agent = %name, error = %error, "Claude self-kick watchdog scan failed");
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::daemon::shadow::evidence::{Authority, Confidence};
    use crate::daemon::shadow::reducer::{ObservedState, ObservedStatus};
    use crate::transport::{
        DeliveryEnvelope, DeliveryReceipt, DeliveryState, ReceiptStore, SessionLocator,
    };
    use chrono::{DateTime, Duration as ChronoDuration, Utc};
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Arc;
    use uuid::Uuid;

    const AGENT: &str = "worker";
    const LEAD: &str = "lead";

    /// A temp home whose fleet maps `worker` to orchestrator `orchestrator`.
    /// Both instances are `backend: claude`, so `mode_for_instance` reports
    /// `ChannelBridge` and the handler actually scans them.
    fn tmp_home(tag: &str, orchestrator: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let id = C.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-selfkick-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            crate::fleet::fleet_yaml_path(&dir),
            format!(
                "instances:\n  {LEAD}:\n    backend: claude\n  {AGENT}:\n    backend: claude\n\
                 teams:\n  t:\n    members: [{AGENT}, {LEAD}]\n    orchestrator: {orchestrator}\n"
            ),
        )
        .unwrap();
        dir
    }

    /// Seed a durable self-kick that the bridge accepted at `accepted_at`.
    fn seed_accepted_kick(home: &Path, accepted_at: DateTime<Utc>) -> Uuid {
        let locator = SessionLocator::claude(
            "http://127.0.0.1:1".to_string(),
            "self-kick-session".to_string(),
            "token".to_string(),
        );
        let envelope = DeliveryEnvelope::self_kick(AGENT, locator, "[AGEND-RESUME] id=test");
        let store = ReceiptStore::for_instance(home, AGENT).expect("store");
        store.record_queued(&envelope).expect("queued");
        let mut receipt = DeliveryReceipt::for_state(&envelope, DeliveryState::ProtocolAccepted);
        receipt.protocol_request_id = Some(envelope.delivery_id.to_string());
        receipt.backend_event = Some("webhook_accepted".to_string());
        receipt.recorded_at = accepted_at.to_rfc3339();
        store.record(receipt).expect("accepted");
        envelope.delivery_id
    }

    /// Write the `TurnStarted` receipt the transport writes for a late
    /// `ack_start` (`late_ack_secs` past the window). The transport-side half
    /// of this contract is pinned by `self_kick_tests.rs`.
    fn seed_late_ack(home: &Path, delivery_id: Uuid, late_by_secs: i64) {
        let store = ReceiptStore::for_instance(home, AGENT).expect("store");
        let (envelope, current) = store
            .delivery(delivery_id)
            .expect("delivery")
            .expect("receipt");
        let mut started = DeliveryReceipt::for_state(&envelope, DeliveryState::TurnStarted);
        started.protocol_request_id = Some(delivery_id.to_string());
        started.backend_event = Some("claude_channel_turn_started_late".to_string());
        started.late_ack_secs = Some(late_by_secs);
        assert!(store
            .record_if_latest_state(delivery_id, current.state, started)
            .expect("late ack CAS"));
    }

    struct Harness {
        home: std::path::PathBuf,
        registry: crate::agent::AgentRegistry,
        externals: crate::agent::ExternalRegistry,
        configs: Arc<Mutex<HashMap<String, crate::daemon::AgentConfig>>>,
        core: Arc<crate::sync_audit::CoreMutex<crate::agent::AgentCore>>,
        spawned_at_epoch_ms: u64,
        _reader: Box<dyn std::io::Read + Send>,
    }

    impl Harness {
        fn new(tag: &str, orchestrator: &str) -> Self {
            let home = tmp_home(tag, orchestrator);
            let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
            let (handle, reader) = crate::daemon::per_tick::mock_live_agent_no_context(AGENT);
            let core = Arc::clone(&handle.core);
            let spawned_at_epoch_ms = handle.spawned_at_epoch_ms;
            registry.lock().insert(handle.id, handle);
            Self {
                home,
                registry,
                externals: Arc::new(Mutex::new(HashMap::new())),
                configs: Arc::new(Mutex::new(HashMap::new())),
                core,
                spawned_at_epoch_ms,
                _reader: reader,
            }
        }

        fn tick(&self, handler: &ClaudeSelfKickHandler) {
            let ctx = TickContext {
                home: &self.home,
                registry: &self.registry,
                externals: &self.externals,
                configs: &self.configs,
            };
            handler.run(&ctx);
        }

        /// Drain `who`'s inbox and keep only the self-kick notices. `drain`
        /// is destructive, so each call reports what THIS pass produced.
        fn notices(&self, who: &str) -> Vec<String> {
            crate::inbox::drain(&self.home, who)
                .into_iter()
                .map(|m| m.text)
                .filter(|t| t.contains("[claude_self_kick]"))
                .collect()
        }

        fn evidence(&self) -> Option<crate::health::SelfKickEvidence> {
            self.core.lock().health.last_self_kick.clone()
        }

        /// Stamp a hook-authority turn observed `after_accept` seconds after
        /// `accepted_at` — the same slot `ShadowObserveHandler` writes.
        fn observe_turn(&self, accepted_at: DateTime<Utc>, after_accept_secs: i64) {
            self.core.lock().observed_status = Some(ObservedStatus {
                state: ObservedState::Active,
                confidence: Confidence::Confirmed,
                authority: Authority::Hook,
                evidence: Vec::new(),
                since_ms: (accepted_at.timestamp_millis() + after_accept_secs * 1_000) as u64,
            });
        }

        fn list_self_kick(&self) -> serde_json::Value {
            let snapshot =
                crate::agent_ops::list_snapshot(&self.home, &self.registry, &self.externals);
            snapshot["result"]["agents"]
                .as_array()
                .expect("agents")
                .iter()
                .find(|a| a["name"] == AGENT)
                .expect("worker in LIST")["self_kick"]
                .clone()
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.home).ok();
        }
    }

    /// (a) An accepted-but-unacknowledged self-kick must reach the team
    /// orchestrator's INBOX — not just an escalation channel and the event
    /// log — and must carry the evidence that says which failure it is.
    #[test]
    fn ack_overdue_notifies_the_orchestrator_inbox() {
        let h = Harness::new("overdue", LEAD);
        let accepted_at = Utc::now() - ChronoDuration::seconds(31);
        let delivery_id = seed_accepted_kick(&h.home, accepted_at);
        h.tick(&ClaudeSelfKickHandler::new(1));

        let notices = h.notices(LEAD);
        assert_eq!(
            notices.len(),
            1,
            "exactly one AckOverdue notice must reach the orchestrator: {notices:?}"
        );
        let text = &notices[0];
        assert!(
            text.contains(&format!("'{AGENT}'")),
            "names the agent: {text}"
        );
        assert!(
            text.contains(&delivery_id.to_string()),
            "carries the delivery_id: {text}"
        );
        assert!(
            text.contains("No turn was observed since the kick"),
            "classifies an unconsumed resume: {text}"
        );

        let ev = h.evidence().expect("health must carry self-kick evidence");
        assert_eq!(ev.state, "ack_overdue");
        assert!(!ev.turn_observed_since_kick);
        assert_eq!(ev.delivery_id, delivery_id.to_string());

        assert_eq!(
            h.list_self_kick()["state"],
            "ack_overdue",
            "list_instances must surface the overdue self-kick"
        );
    }

    /// (b) Control: an ack inside the window is not an incident. Nothing is
    /// notified and no evidence is stamped — a `None` slot means "nothing
    /// went wrong" (this handler records reconciliation outcomes only).
    #[test]
    fn ack_within_window_never_notifies() {
        let h = Harness::new("in-window", LEAD);
        let accepted_at = Utc::now() - ChronoDuration::seconds(10);
        seed_accepted_kick(&h.home, accepted_at);
        h.tick(&ClaudeSelfKickHandler::new(1));
        assert!(
            h.notices(LEAD).is_empty(),
            "an ack window that has not closed must be silent"
        );
        assert!(h.evidence().is_none(), "and must stamp no evidence");
        assert!(h.list_self_kick().is_null(), "and LIST stays null");
    }

    /// (c) The distinguishing evidence: a hook-authority turn WAS observed
    /// after the kick was accepted, so this is a late/absent ack, not a lost
    /// delivery. The two cases need different operator action.
    #[test]
    fn overdue_with_observed_turn_classifies_as_late_ack() {
        let h = Harness::new("observed", LEAD);
        let accepted_at = Utc::now() - ChronoDuration::seconds(31);
        seed_accepted_kick(&h.home, accepted_at);
        h.observe_turn(accepted_at, 5);
        h.tick(&ClaudeSelfKickHandler::new(1));

        let notices = h.notices(LEAD);
        assert_eq!(notices.len(), 1, "one notice: {notices:?}");
        let text = &notices[0];
        assert!(
            text.contains("A turn was observed at +5s"),
            "must report the observed turn and its offset: {text}"
        );
        let ev = h.evidence().expect("evidence");
        assert!(
            ev.turn_observed_since_kick,
            "turn_observed_since_kick must be true when a hook turn followed the kick"
        );
    }

    /// (d) A late ack reconciles the earlier AckOverdue exactly once: the
    /// operator who was told to check the agent must be told it recovered.
    #[test]
    fn late_ack_after_overdue_emits_resolved_notice_once() {
        let h = Harness::new("late-ack", LEAD);
        let handler = ClaudeSelfKickHandler::new(1);
        let accepted_at = Utc::now() - ChronoDuration::seconds(31);
        let delivery_id = seed_accepted_kick(&h.home, accepted_at);
        h.tick(&handler);
        assert_eq!(h.notices(LEAD).len(), 1, "the overdue notice");

        seed_late_ack(&h.home, delivery_id, 14);
        h.tick(&handler);
        let resolved = h.notices(LEAD);
        assert_eq!(
            resolved.len(),
            1,
            "exactly one late-ack reconciliation notice: {resolved:?}"
        );
        assert!(
            resolved[0].contains("arrived late (+14s after the window)")
                && resolved[0].contains("resolved"),
            "must say the earlier AckOverdue is resolved: {}",
            resolved[0]
        );
        assert_eq!(
            h.evidence().expect("evidence").state,
            "turn_started_late",
            "the evidence slot must reconcile too"
        );

        h.tick(&handler);
        assert!(
            h.notices(LEAD).is_empty(),
            "a second pass must not re-announce the late ack"
        );
    }

    /// (e) A self-restarted ORCHESTRATOR's only inbox is its own, and its
    /// successor drains it when nudged. Unlike every other watchdog, this one
    /// must NOT skip `recipient == agent`.
    #[test]
    fn self_restarted_orchestrator_is_notified_in_its_own_inbox() {
        let h = Harness::new("self-orch", AGENT);
        seed_accepted_kick(&h.home, Utc::now() - ChronoDuration::seconds(31));
        h.tick(&ClaudeSelfKickHandler::new(1));
        assert_eq!(
            h.notices(AGENT).len(),
            1,
            "an agent that is its own orchestrator must be notified in its own inbox"
        );
        assert!(
            h.notices(LEAD).is_empty(),
            "and nobody else is notified for it"
        );
    }

    /// (f) Exactly-once: the durable `ProtocolAccepted -> AckOverdue` CAS is
    /// the latch, so a second pass over the same delivery is silent.
    #[test]
    fn watchdog_pass_twice_notifies_once() {
        let h = Harness::new("twice", LEAD);
        let handler = ClaudeSelfKickHandler::new(1);
        seed_accepted_kick(&h.home, Utc::now() - ChronoDuration::seconds(31));
        h.tick(&handler);
        let first = h.notices(LEAD).len();
        h.tick(&handler);
        let second = h.notices(LEAD).len();
        assert_eq!(first, 1, "the first pass notifies");
        assert_eq!(
            second, 0,
            "the second pass over the same delivery is silent"
        );
    }

    /// The spawn-delta clause is present when the handle carries a spawn
    /// time, and is truthful about which direction the delta runs.
    #[test]
    fn notice_carries_the_spawn_delta_when_it_is_known() {
        let h = Harness::new("spawn-delta", LEAD);
        let accepted_at = Utc::now() - ChronoDuration::seconds(31);
        seed_accepted_kick(&h.home, accepted_at);
        h.tick(&ClaudeSelfKickHandler::new(1));
        let notices = h.notices(LEAD);
        let expected =
            (accepted_at.timestamp_millis() - h.spawned_at_epoch_ms as i64).div_euclid(1_000);
        assert!(
            notices[0].contains(&format!("(+{expected}s after spawn)")),
            "spawn delta clause missing/incorrect (expected +{expected}s): {}",
            notices[0]
        );
    }
}
