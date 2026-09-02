//! #3230 + t-20260902165405106195-82348-105: the Claude ChannelBridge
//! self-kick acknowledgement watchdog, and the operator-facing half of it.
//!
//! The bridge persists the accepted delivery before it can receive the
//! consumer's exact-ID `ack_start`. This per-tick scan replays that durable
//! state after daemon/bridge restarts and converts only an old, still-
//! ProtocolAccepted self-kick into one Ambiguous operator alert. It never
//! retries, reads screen state, or treats a hook/time correlation as a turn.
//!
//! #3230 made the transition truthful but silent to the fleet: `AckOverdue`
//! wrote an event-log line and pinged the escalation channels, so a fresh
//! restart whose `[AGEND-RESUME]` never became a turn simply sat idle
//! (8 events / 4 instances / 3 weeks). This handler adds the missing
//! observability, and nothing else:
//!
//! 1. an inbox notice to the agent's team orchestrator (item 5 below is the
//!    reason it lives here rather than in the transport);
//! 2. a CLASSIFICATION built from evidence the daemon already holds — whether
//!    a hook-authority turn was observed after the bridge accepted the kick.
//!    "Accepted" is HTTP 202 at the bridge, i.e. QUEUED, not consumed; the two
//!    failures need different operator action, so the notice names which one;
//!    the shadow evidence, not the clock, is what separates them;
//! 3. an evidence slot on [`crate::health::HealthTracker`] mirrored into
//!    `list_instances`, so the state is visible without reading the event log;
//! 4. a reconciliation notice when a late `ack_start` resolves the alarm — the
//!    operator who was told to check the agent is told when not to;
//! 5. and, UNLIKE every other watchdog here, it does NOT skip
//!    `recipient == agent`: a self-restarted ORCHESTRATOR's only inbox is its
//!    own, and its successor drains it when nudged.
//!
//! Still not done, deliberately: no retry, no re-kick, no PTY fallback, no
//! change to `SELF_KICK_ACK_WINDOW` or the delivery path. Detection and
//! notification only.

use super::{PerTickHandler, TickContext};
use crate::daemon::shadow::evidence::Authority;
use crate::daemon::shadow::reducer::{ObservedState, ObservedStatus};
use crate::health::SelfKickEvidence;
use crate::transport::claude_channel::{
    SelfKickOutcome, SelfKickOutcomeKind, TurnObservation, SELF_KICK_ACK_WINDOW,
};
use chrono::{DateTime, Utc};

const NOTICE_SOURCE: &str = "system:claude_self_kick";

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
        // Loaded once per pass (the context_alert convention), not per agent.
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
            // One brief registry lock per agent, taken BEFORE the watchdog's
            // file IO so no core lock is held across it.
            let evidence = agent_evidence(ctx.registry, name);
            let outcomes = match crate::transport::claude_channel::self_kick_watchdog_pass(
                ctx.home,
                name,
                &|accepted_at| turn_since(evidence.as_ref(), accepted_at),
            ) {
                Ok(outcomes) => outcomes,
                Err(error) => {
                    tracing::warn!(agent = %name, error = %error, "Claude self-kick watchdog scan failed");
                    continue;
                }
            };
            if outcomes.is_empty() {
                continue;
            }
            let recipient = crate::daemon::inbox_stuck_watchdog::orchestrator_for(&fleet, name)
                .unwrap_or_else(|| {
                    crate::daemon::inbox_stuck_watchdog::FALLBACK_RECIPIENT.to_string()
                });
            for outcome in outcomes {
                announce(ctx, name, &recipient, evidence.as_ref(), &outcome);
            }
        }
    }
}

/// The agent-side evidence the notices quote: the fused [`ObservedStatus`]
/// that `ShadowObserveHandler` hangs beside `agent_state`, and the handle's
/// spawn stamp. `None` when the agent is absent from the registry (a fleet
/// entry that is not running) — the notice then simply omits those clauses.
struct AgentEvidence {
    observed: Option<ObservedStatus>,
    spawned_at_epoch_ms: u64,
}

fn agent_evidence(registry: &crate::agent::AgentRegistry, name: &str) -> Option<AgentEvidence> {
    let reg = crate::agent::lock_registry(registry);
    let handle = reg.values().find(|h| h.name.as_str() == name)?;
    let observed = handle.core.lock().observed_status.clone();
    Some(AgentEvidence {
        observed,
        spawned_at_epoch_ms: handle.spawned_at_epoch_ms,
    })
}

/// A HOOK-authority turn observed at or after `accepted_at`.
///
/// Only the hook plane counts. `Authority::Screen` / `Inferred` is exactly the
/// time/appearance correlation #3230 refused to treat as a turn, and admitting
/// it here would put a wrong classification in front of the operator. The
/// turn-open states are the refined `Active` family: claude's
/// `UserPromptSubmit` reduces to `Active`, `PreToolUse` to `ToolUse`.
fn turn_since(
    evidence: Option<&AgentEvidence>,
    accepted_at: DateTime<Utc>,
) -> Option<TurnObservation> {
    let status = evidence?.observed.as_ref()?;
    if status.authority != Authority::Hook {
        return None;
    }
    if !matches!(
        status.state,
        ObservedState::Active
            | ObservedState::Thinking
            | ObservedState::ToolUse
            | ObservedState::Responding
    ) {
        return None;
    }
    let since_ms = i64::try_from(status.since_ms).ok()?;
    let accepted_ms = accepted_at.timestamp_millis();
    (since_ms >= accepted_ms).then(|| TurnObservation {
        after_accept_secs: (since_ms - accepted_ms) / 1_000,
    })
}

/// Seconds between the agent's spawn and the bridge's acceptance, or `None`
/// when it cannot be stated truthfully:
/// - the agent is not in the registry;
/// - the handle carries no spawn stamp (`0` is the codebase's sentinel for a
///   handle built without one — see `per_tick::mock_live_agent_no_context`);
/// - the LIVE handle POST-DATES the acceptance, i.e. a later restart replaced
///   the process the kick was aimed at, so "after spawn" would name the wrong
///   spawn.
fn spawn_delta_secs(evidence: Option<&AgentEvidence>, accepted_at: DateTime<Utc>) -> Option<i64> {
    let spawned_at_epoch_ms = i64::try_from(evidence?.spawned_at_epoch_ms).ok()?;
    if spawned_at_epoch_ms == 0 {
        return None;
    }
    let delta_ms = accepted_at.timestamp_millis() - spawned_at_epoch_ms;
    (delta_ms >= 0).then_some(delta_ms / 1_000)
}

/// The AckOverdue notice. Single paragraph: an orchestrator reads this in an
/// inbox drain, so the classification and the action are in the same block.
fn overdue_notice_text(
    agent: &str,
    delivery_id: &str,
    accepted_at: DateTime<Utc>,
    spawn_delta: Option<i64>,
    turn: Option<TurnObservation>,
) -> String {
    let spawn = spawn_delta.map_or_else(String::new, |d| format!(" (+{d}s after spawn)"));
    let classification = match turn {
        None => "No turn was observed since the kick — the resume most likely never became a \
                 session turn (delivery accepted ≠ consumed)"
            .to_string(),
        Some(turn) => format!(
            "A turn was observed at +{}s but no ack_start followed within the window — the agent \
             is late or did not follow the ack protocol; it may still be recovering",
            turn.after_accept_secs
        ),
    };
    format!(
        "[claude_self_kick] agent '{agent}' fresh-restart resume NOT confirmed: delivery \
         {delivery_id} was accepted by the channel bridge at {}{spawn} but no ack_start arrived \
         within {} s. {classification}. Action: check the agent — if it is idle at a fresh prompt \
         with no recovery in progress, send it a query or restart_instance(mode=fresh) again; its \
         board/inbox state is intact. Late acks are reconciled automatically (you will get a \
         follow-up notice).",
        accepted_at.to_rfc3339(),
        SELF_KICK_ACK_WINDOW.as_secs(),
    )
}

fn late_notice_text(agent: &str, delivery_id: &str, late_by_secs: i64) -> String {
    format!(
        "[claude_self_kick] agent '{agent}' resume ack arrived late (+{late_by_secs}s after the \
         window) for delivery {delivery_id} — the earlier AckOverdue is resolved; no action \
         needed."
    )
}

/// File one outcome: inbox notice to `recipient`, then the health evidence
/// slot `list_instances` mirrors.
fn announce(
    ctx: &TickContext<'_>,
    agent: &str,
    recipient: &str,
    evidence: Option<&AgentEvidence>,
    outcome: &SelfKickOutcome,
) {
    let delivery_id = outcome.delivery_id.to_string();
    let (text, kind, state, turn_observed) = match outcome.kind {
        SelfKickOutcomeKind::AckOverdue { turn } => (
            overdue_notice_text(
                agent,
                &delivery_id,
                outcome.at,
                spawn_delta_secs(evidence, outcome.at),
                turn,
            ),
            "claude_self_kick_ack_overdue",
            "ack_overdue",
            turn.is_some(),
        ),
        SelfKickOutcomeKind::AckLate { late_by_secs } => (
            late_notice_text(agent, &delivery_id, late_by_secs),
            "claude_self_kick_ack_late",
            "turn_started_late",
            // The ack itself is the turn proof — a session that called
            // `ack_start` demonstrably started a turn.
            true,
        ),
    };
    // NOTE: no `recipient == agent` skip. See the module doc, item 5.
    if let Err(error) = crate::inbox::notify_system(
        ctx.home,
        recipient,
        NOTICE_SOURCE,
        kind,
        text,
        Some(agent),
        None,
    ) {
        tracing::warn!(agent = %agent, %recipient, error = %error, "claude_self_kick: notify failed");
        return;
    }
    tracing::warn!(agent = %agent, %recipient, delivery_id = %delivery_id, state, "claude_self_kick: notified orchestrator");
    record_evidence(
        ctx.registry,
        agent,
        &delivery_id,
        state,
        turn_observed,
        outcome.at,
    );
}

/// Stamp the evidence on the agent's own `HealthTracker` — a second, brief
/// registry lock. Deliberately NOT a `BlockedReason` / `HealthState` change:
/// this is evidence `list_instances` mirrors, never a dispatch-gating signal.
///
/// `accepted_at` is carried over from the delivery's own earlier AckOverdue
/// record when this pass is reconciling it; otherwise (a late ack whose
/// overdue notice was raised before a daemon restart) the outcome's own
/// timestamp stands in.
fn record_evidence(
    registry: &crate::agent::AgentRegistry,
    agent: &str,
    delivery_id: &str,
    state: &'static str,
    turn_observed_since_kick: bool,
    at: DateTime<Utc>,
) {
    let reg = crate::agent::lock_registry(registry);
    let Some(handle) = reg.values().find(|h| h.name.as_str() == agent) else {
        return;
    };
    let mut core = handle.core.lock();
    let accepted_at = core
        .health
        .last_self_kick
        .as_ref()
        .filter(|prev| prev.delivery_id == delivery_id)
        .map_or(at, |prev| prev.accepted_at);
    core.health.record_self_kick(SelfKickEvidence {
        delivery_id: delivery_id.to_string(),
        accepted_at,
        state,
        turn_observed_since_kick,
        at: Utc::now(),
    });
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
        _reader: Box<dyn std::io::Read + Send>,
    }

    impl Harness {
        fn new(tag: &str, orchestrator: &str) -> Self {
            let home = tmp_home(tag, orchestrator);
            let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
            let (handle, reader) = crate::daemon::per_tick::mock_live_agent_no_context(AGENT);
            let core = Arc::clone(&handle.core);
            registry.lock().insert(handle.id, handle);
            Self {
                home,
                registry,
                externals: Arc::new(Mutex::new(HashMap::new())),
                configs: Arc::new(Mutex::new(HashMap::new())),
                core,
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

    /// The spawn-delta clause is a truth claim about WHICH spawn, so it is
    /// stated only when the live handle actually predates the acceptance. A
    /// handle with no stamp, or one that post-dates the kick (a later restart
    /// replaced the process the kick was aimed at), omits the clause rather
    /// than naming the wrong spawn.
    #[test]
    fn spawn_delta_is_stated_only_when_the_handle_predates_the_kick() {
        let accepted_at = Utc::now();
        let stamp = |offset_ms: i64| AgentEvidence {
            observed: None,
            spawned_at_epoch_ms: (accepted_at.timestamp_millis() + offset_ms) as u64,
        };
        assert_eq!(spawn_delta_secs(Some(&stamp(-4_200)), accepted_at), Some(4));
        assert_eq!(
            spawn_delta_secs(Some(&stamp(4_200)), accepted_at),
            None,
            "a handle spawned AFTER the kick was accepted is a different process"
        );
        assert_eq!(
            spawn_delta_secs(
                Some(&AgentEvidence {
                    observed: None,
                    spawned_at_epoch_ms: 0
                }),
                accepted_at
            ),
            None,
            "0 is the no-stamp sentinel, not 1970"
        );
        assert_eq!(spawn_delta_secs(None, accepted_at), None);

        assert!(
            overdue_notice_text("worker", "d-1", accepted_at, Some(4), None)
                .contains("(+4s after spawn)")
        );
        assert!(
            !overdue_notice_text("worker", "d-1", accepted_at, None, None).contains("after spawn")
        );
    }
}
