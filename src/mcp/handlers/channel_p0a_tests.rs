//! Sprint 55 P0-A — `handle_reply` prefer-chain + structured-error tests.
//!
//! Located in this sibling file (loaded via `#[path]` from channel.rs) per
//! Sprint 54 PR #517 / Sprint 55 PR #522 cycle-10 file_size_invariant pattern.
//!
//! Covers 11 edge cases (7 from dev RCA + 4 reviewer-added per design doc
//! `docs/DESIGN-sprint55-p0a-channel-discipline-guard.md` §4). EC1/3/4/5/7
//! collapse onto the same "snapshot read at handler-time → fallback when
//! reply_to is None" path (Sprint 52 prior-art inheritance) and are
//! consolidated into a single test case to avoid 5× boilerplate.

use crate::channel::{
    AgentOutboundOp, BindingOpts, BindingRef, Channel, ChannelCapabilities, ChannelError,
    ChannelEvent, MsgRef, NotifySeverity, OutMsg, TopicRef,
};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::OnceLock;

// ── Mock channel + global guard ─────────────────────────────────────────

/// `register_active_channel` uses a process-wide `OnceLock`. These tests
/// share that slot; serialize through this guard.
fn registry_guard() -> parking_lot::MutexGuard<'static, ()> {
    static G: Mutex<()> = Mutex::new(());
    G.lock()
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ReplyOutcome {
    Ok,
    NotSupported,
}

struct MockChannel {
    kind: &'static str,
    caps: ChannelCapabilities,
    reply: ReplyOutcome,
}

impl MockChannel {
    fn arc(kind: &'static str, reply: ReplyOutcome) -> Arc<dyn Channel> {
        Arc::new(MockChannel {
            kind,
            caps: ChannelCapabilities::default(),
            reply,
        })
    }
}

impl Channel for MockChannel {
    fn kind(&self) -> &'static str {
        self.kind
    }
    fn caps(&self) -> &ChannelCapabilities {
        &self.caps
    }
    fn poll_event(&self) -> Option<ChannelEvent> {
        None
    }
    fn send(&self, _: &BindingRef, _: OutMsg) -> anyhow::Result<MsgRef> {
        anyhow::bail!("mock send unused")
    }
    fn edit(&self, _: &MsgRef, _: OutMsg) -> anyhow::Result<()> {
        Ok(())
    }
    fn delete(&self, _: &MsgRef) -> anyhow::Result<()> {
        Ok(())
    }
    fn create_binding(&self, _: &str, _: BindingOpts) -> anyhow::Result<BindingRef> {
        anyhow::bail!("mock binding unused")
    }
    fn remove_binding(&self, _: &BindingRef) -> anyhow::Result<()> {
        Ok(())
    }
    fn has_binding(&self, _: &str) -> bool {
        false
    }
    fn record_binding(&self, _: &str, _: BindingRef, _: String) {}
    fn take_binding(&self, _: &str) -> Option<BindingRef> {
        None
    }
    fn attach_registry(&self, _: crate::agent::AgentRegistry) {}
    fn create_topic(&self, _: &str) -> Result<TopicRef, ChannelError> {
        Err(ChannelError::NotSupported("create_topic".into()))
    }
    fn notify(&self, _: &str, _: NotifySeverity, _: &str, _: bool) -> Result<(), ChannelError> {
        Err(ChannelError::NotSupported("notify".into()))
    }
    fn send_from_agent(&self, _: &str, _: AgentOutboundOp) -> Result<MsgRef, ChannelError> {
        match self.reply {
            ReplyOutcome::Ok => Ok(MsgRef {
                binding: BindingRef::new(self.kind, None, ()),
                id: "mock-msg-1".into(),
            }),
            ReplyOutcome::NotSupported => {
                Err(ChannelError::NotSupported("send_from_agent.Reply".into()))
            }
        }
    }
}

// ── Fixtures ────────────────────────────────────────────────────────────

fn tmp_home(tag: &str) -> std::path::PathBuf {
    static COUNTER: OnceLock<Mutex<u64>> = OnceLock::new();
    let counter = COUNTER.get_or_init(|| Mutex::new(0));
    let id = {
        let mut g = counter.lock();
        *g += 1;
        *g
    };
    let dir = std::env::temp_dir().join(format!("agend-p0a-{}-{}-{}", std::process::id(), tag, id));
    std::fs::create_dir_all(&dir).ok();
    std::fs::write(
        dir.join("fleet.yaml"),
        "instances:\n  alpha:\n    backend: claude\n",
    )
    .ok();
    dir
}

fn set_reply_to(instance: &str, channel: Option<&str>) {
    crate::daemon::heartbeat_pair::update_with(instance, |p| {
        p.reply_to_channel = channel.map(String::from);
    });
}

// ── Edge cases ──────────────────────────────────────────────────────────

#[test]
fn ec1_3_4_5_7_snapshot_at_handler_time_falls_back_to_active_when_reply_to_none() {
    // Consolidates EC1 (TUI clear) / EC3 (multi-turn) / EC4 (restart) /
    // EC5 (concurrent last-wins) / EC7 (turn def). All collapse onto
    // "reply_to_channel == None at handler-call-time → fallback to
    // active_channel singleton".
    let _g = registry_guard();
    crate::channel::reset_active_channel_for_test();
    let home = tmp_home("ec1357");
    set_reply_to("alpha", None);
    crate::channel::register_active_channel(MockChannel::arc("telegram", ReplyOutcome::Ok));

    let result = super::handle_reply(&home, &serde_json::json!({"message": "hi"}), "alpha");
    assert_eq!(
        result["message_id"], "mock-msg-1",
        "fallback to singleton must succeed: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ec2_6_10_unavailable_returns_structured_error_when_lookup_misses() {
    // EC2 mid-message drop / EC6 disconnect / EC10 TOCTOU all surface
    // as `reply_to_channel = "discord"` + lookup miss.
    let _g = registry_guard();
    crate::channel::reset_active_channel_for_test();
    let home = tmp_home("ec2610");
    set_reply_to("alpha", Some("discord"));
    crate::channel::register_active_channel(MockChannel::arc("telegram", ReplyOutcome::Ok));

    let result = super::handle_reply(&home, &serde_json::json!({"message": "hi"}), "alpha");
    assert_eq!(result["code"], "reply_channel_unavailable");
    assert!(
        result["error"]
            .as_str()
            .unwrap_or_default()
            .contains("'discord'"),
        "error must name the missing channel: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ec8_no_active_channel_when_no_external_channel_registered() {
    // Agent-peer-only mode: reply_to None + ZERO channels registered.
    // Deterministic via reset_active_channel_for_test() — no dual-accept.
    let _g = registry_guard();
    crate::channel::reset_active_channel_for_test();
    let home = tmp_home("ec8");
    set_reply_to("alpha", None);

    let result = super::handle_reply(&home, &serde_json::json!({"message": "hi"}), "alpha");
    assert_eq!(
        result["code"], "no_active_channel",
        "no channels registered must surface no_active_channel: {result}"
    );
    assert!(
        result["message_id"].is_null(),
        "no message_id when no channel: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ec12_multi_channel_lookup_routes_to_tagged_not_singleton() {
    // Variant-extend contract proof: register telegram + discord; tag
    // sender's reply_to as "discord" → reply MUST land on discord even
    // though telegram was registered first. Demonstrates real HashMap
    // by-name routing per reviewer Finding #1.
    let _g = registry_guard();
    crate::channel::reset_active_channel_for_test();
    crate::channel::register_active_channel(MockChannel::arc(
        "telegram",
        ReplyOutcome::NotSupported,
    ));
    crate::channel::register_active_channel(MockChannel::arc("discord", ReplyOutcome::Ok));
    let home = tmp_home("ec12");
    set_reply_to("alpha", Some("discord"));

    let result = super::handle_reply(&home, &serde_json::json!({"message": "hi"}), "alpha");
    // discord mock returns Ok → message_id present. If routing collapsed
    // to telegram (singleton-style fallback), it would have surfaced
    // channel_capability_unsupported instead.
    assert_eq!(
        result["message_id"], "mock-msg-1",
        "multi-channel routing must prefer tagged 'discord': {result}"
    );
    assert!(
        result.get("code").is_none(),
        "no error code on successful multi-channel routing: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ec9_channel_kind_mismatch_is_unavailable_not_silent_fallback() {
    // Aliasing/migration: caller tagged "telegram-main" but channel.kind()
    // is "telegram". lookup_channel_by_name returns None → must surface
    // unavailable, NOT silently fall back to the singleton.
    let _g = registry_guard();
    crate::channel::reset_active_channel_for_test();
    let home = tmp_home("ec9");
    set_reply_to("alpha", Some("telegram-main"));
    crate::channel::register_active_channel(MockChannel::arc("telegram", ReplyOutcome::Ok));

    let result = super::handle_reply(&home, &serde_json::json!({"message": "hi"}), "alpha");
    assert_eq!(
        result["code"], "reply_channel_unavailable",
        "kind mismatch must NOT silently fall back: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn ec11_capability_unsupported_returns_structured_error() {
    // Tagged channel exists + matches kind, but its send_from_agent
    // returns NotSupported. Must surface channel_capability_unsupported.
    let _g = registry_guard();
    crate::channel::reset_active_channel_for_test();
    let home = tmp_home("ec11");
    set_reply_to("alpha", Some("telegram"));
    crate::channel::register_active_channel(MockChannel::arc(
        "telegram",
        ReplyOutcome::NotSupported,
    ));

    let result = super::handle_reply(&home, &serde_json::json!({"message": "hi"}), "alpha");
    assert_eq!(result["code"], "channel_capability_unsupported");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn happy_path_tagged_channel_matches_singleton_returns_message_id() {
    // Common single-channel-fleet steady state: reply_to == singleton's
    // kind. Succeeds + sets Sprint 52 mirror_skip flag.
    let _g = registry_guard();
    crate::channel::reset_active_channel_for_test();
    let home = tmp_home("happy");
    set_reply_to("alpha", Some("telegram"));
    crate::channel::register_active_channel(MockChannel::arc("telegram", ReplyOutcome::Ok));

    let result = super::handle_reply(&home, &serde_json::json!({"message": "hi"}), "alpha");
    assert_eq!(result["message_id"], "mock-msg-1");
    let snap = crate::daemon::heartbeat_pair::snapshot_for("alpha");
    assert!(
        snap.mirror_skip_until_next_turn,
        "Sprint 52 mirror_skip flag must be set on successful reply"
    );
    std::fs::remove_dir_all(&home).ok();
}

// #1665 (codex catch): the "No fleet.yaml" early-return is a reply send-failure
// too — it MUST record SendFailed on the audited turn, not leave it Pending
// (which the reply-ledger would later mis-classify as a plain silent drop rather
// than a Gap D send-failure). Pre-fix this assertion FAILS (outcome stays
// Pending). `crate::reply_ledger::ReplyOutcome` is fully qualified to avoid the
// local mock `ReplyOutcome` (Ok/NotSupported) in this file.
#[test]
fn no_fleet_yaml_reply_exit_records_send_failed_gap_d_1665() {
    let _g = registry_guard();
    crate::channel::reset_active_channel_for_test();
    // A home WITHOUT fleet.yaml (the shared `tmp_home` writes one) so handle_reply
    // hits the no-fleet early-return.
    let home = std::env::temp_dir().join(format!("agend-1665-nofleet-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();
    std::fs::remove_file(home.join("fleet.yaml")).ok();
    let agent = "reply-ledger-nofleet-1665";
    crate::reply_ledger::arm(
        &home,
        agent,
        crate::channel::ChannelKind::Telegram,
        Some("m-1".into()),
        None,
        None,
        Some("user:op"),
        Some("hi"),
    );
    let result = super::handle_reply(&home, &serde_json::json!({"message": "hi"}), agent);
    assert!(
        result["error"]
            .as_str()
            .unwrap_or_default()
            .contains("No fleet.yaml"),
        "must hit the no-fleet.yaml exit: {result}"
    );
    let turn = crate::daemon::heartbeat_pair::snapshot_for(agent)
        .pending_user_turn
        .expect("turn stays armed through the failed reply");
    assert_eq!(
        turn.reply_outcome,
        crate::reply_ledger::ReplyOutcome::SendFailed,
        "no-fleet.yaml reply exit must record SendFailed (Gap D), not stay Pending"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #2622 PR-2c: inbox action=discharge (channel-reply obligation) ────────

/// Enqueue a channel (user) message into `agent`'s inbox and return its id.
fn enqueue_channel_msg(home: &std::path::Path, agent: &str, id: &str, from: &str, text: &str) {
    let msg = crate::inbox::InboxMessage {
        schema_version: 1,
        id: Some(id.into()),
        from: from.into(),
        text: text.into(),
        kind: None,
        timestamp: "2026-06-22T16:41:45Z".into(),
        channel: Some(crate::channel::ChannelKind::Telegram),
        ..Default::default()
    };
    crate::inbox::enqueue(home, agent, msg).expect("test setup: enqueue must succeed");
}

#[test]
fn discharge_requires_message_id_and_reason_2622() {
    let _g = registry_guard();
    let home = tmp_home("discharge-args");
    // Missing message_id.
    let r = super::handle_discharge(&home, &serde_json::json!({"reason": "x"}), "alpha");
    assert_eq!(
        r["code"], "missing_message_id",
        "no message_id → error: {r}"
    );
    // Missing / empty reason (the anti-backdoor gate).
    let r = super::handle_discharge(
        &home,
        &serde_json::json!({"message_id": "m-1", "reason": "  "}),
        "alpha",
    );
    assert_eq!(
        r["code"], "missing_reason",
        "self-discharge without a reason must be rejected (loud, never silent): {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn discharge_unknown_message_errors_2622() {
    let _g = registry_guard();
    let home = tmp_home("discharge-unknown");
    let r = super::handle_discharge(
        &home,
        &serde_json::json!({"message_id": "m-nope", "reason": "stale"}),
        "alpha",
    );
    assert_eq!(
        r["code"], "message_not_found",
        "discharging a message not in any inbox must error, not silently succeed: {r}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn discharge_records_ledger_stops_rearm_and_settles_row_2622() {
    let _g = registry_guard();
    let home = tmp_home("discharge-core");
    let agent = "discharge-core-agent-2622";
    let text = "please analyze this 13-day-old paper";
    enqueue_channel_msg(&home, agent, "m-125", "user:op", text);
    // Arm the obligation (as a drain would), then drain to clear the unread→
    // delivering bookkeeping so the row is in a realistic post-drain state.
    crate::reply_ledger::arm(
        &home,
        agent,
        crate::channel::ChannelKind::Telegram,
        Some("m-125".into()),
        None,
        None,
        Some("user:op"),
        Some(text),
    );
    assert!(
        crate::daemon::heartbeat_pair::snapshot_for(agent)
            .pending_user_turn
            .is_some(),
        "precondition: obligation armed"
    );

    let r = super::handle_discharge(
        &home,
        &serde_json::json!({
            "message_id": "m-125",
            "reason": "stale, operator no longer needs an answer"
        }),
        agent,
    );
    assert_eq!(r["discharged"], true, "discharge must succeed: {r}");
    assert_eq!(
        r["cleared_turn"], true,
        "the live matching turn must be cleared: {r}"
    );

    // (1) Ledger recorded — future arms are suppressed.
    let gk = crate::reply_ledger::group_key(Some("user:op"), Some(text));
    assert!(
        crate::daemon::channel_reply_discharge::is_discharged(&home, agent, gk.as_deref(), "m-125")
            .is_some(),
        "the discharge must be durably recorded"
    );
    // (2) The nudge ladder is stopped now.
    assert!(
        crate::daemon::heartbeat_pair::snapshot_for(agent)
            .pending_user_turn
            .is_none(),
        "the live obligation's turn must be cleared by the discharge"
    );
    // (3) A redelivery re-arm is blocked (the -125 loop is broken).
    crate::reply_ledger::arm(
        &home,
        agent,
        crate::channel::ChannelKind::Telegram,
        Some("m-125".into()),
        None,
        None,
        Some("user:op"),
        Some(text),
    );
    assert!(
        crate::daemon::heartbeat_pair::snapshot_for(agent)
            .pending_user_turn
            .is_none(),
        "a discharged obligation must never re-arm on redelivery"
    );
    // (4) The persistent row is settled (no longer unread → stops redelivering).
    assert!(
        crate::inbox::storage::find_message(&home, "m-125")
            .and_then(|m| m.read_at)
            .is_some(),
        "the discharged row must be marked read so it stops redelivering"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn discharge_does_not_clobber_a_different_live_obligation_2622() {
    let _g = registry_guard();
    let home = tmp_home("discharge-noclobber");
    let agent = "discharge-noclobber-agent-2622";
    // Live obligation A.
    enqueue_channel_msg(&home, agent, "m-A", "user:op", "question A");
    crate::reply_ledger::arm(
        &home,
        agent,
        crate::channel::ChannelKind::Telegram,
        Some("m-A".into()),
        None,
        None,
        Some("user:op"),
        Some("question A"),
    );
    // A DIFFERENT message B exists in the inbox; discharge B.
    enqueue_channel_msg(&home, agent, "m-B", "user:op", "unrelated question B");
    let r = super::handle_discharge(
        &home,
        &serde_json::json!({"message_id": "m-B", "reason": "handled B another way"}),
        agent,
    );
    assert_eq!(r["discharged"], true);
    assert_eq!(
        r["cleared_turn"], false,
        "discharging B must NOT clear A's live turn: {r}"
    );
    // A's obligation survives untouched.
    let turn = crate::daemon::heartbeat_pair::snapshot_for(agent)
        .pending_user_turn
        .expect("A's live obligation must survive discharging an unrelated message B");
    assert_eq!(
        turn.inbound_msg_id.as_deref(),
        Some("m-A"),
        "the surviving turn must still be A's"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2622 reviewer4 r0 REJECTED finding: the durable discharge ledger was keyed
/// globally by `group_key`/`message_id` with no recipient/agent dimension, so
/// one agent's self-discharge of a message could suppress a DIFFERENT agent's
/// independent channel-reply obligation sharing the same sender+text. End-to-end
/// through the real `handle_discharge` entry point (not a unit-level ledger call).
#[test]
fn discharge_for_one_agent_must_not_suppress_same_text_for_another_agent() {
    let _g = registry_guard();
    let home = tmp_home("discharge-cross-agent-2622");
    let agent_a = "discharge-cross-agent-a-2622";
    let agent_b = "discharge-cross-agent-b-2622";
    let text = "please analyze this 13-day-old paper";

    // Agent A receives and self-discharges its own obligation.
    enqueue_channel_msg(&home, agent_a, "m-a-1", "user:op", text);
    crate::reply_ledger::arm(
        &home,
        agent_a,
        crate::channel::ChannelKind::Telegram,
        Some("m-a-1".into()),
        None,
        None,
        Some("user:op"),
        Some(text),
    );
    let r = super::handle_discharge(
        &home,
        &serde_json::json!({
            "message_id": "m-a-1",
            "reason": "handled out of band"
        }),
        agent_a,
    );
    assert_eq!(
        r["discharged"], true,
        "agent A's discharge must succeed: {r}"
    );

    // Agent B receives an INDEPENDENT message — same sender+text (same
    // group_key), different message_id — its obligation must still arm.
    enqueue_channel_msg(&home, agent_b, "m-b-1", "user:op", text);
    crate::reply_ledger::arm(
        &home,
        agent_b,
        crate::channel::ChannelKind::Telegram,
        Some("m-b-1".into()),
        None,
        None,
        Some("user:op"),
        Some(text),
    );
    assert!(
        crate::daemon::heartbeat_pair::snapshot_for(agent_b)
            .pending_user_turn
            .is_some(),
        "a discharge by/for agent A must not suppress agent B's independent channel-reply obligation with the same sender+text"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #2622 PR-3: `reply` optional `message_id` — targeted-channel routing ──

/// Like [`enqueue_channel_msg`] but lets the test pick the row's channel kind
/// (the shared helper hardcodes Telegram — these tests need a row whose
/// channel DIFFERS from whatever the prefer-chain would otherwise pick, to
/// prove the `message_id` path overrides it).
fn enqueue_channel_msg_with_kind(
    home: &std::path::Path,
    agent: &str,
    id: &str,
    from: &str,
    text: &str,
    kind: crate::channel::ChannelKind,
) {
    let msg = crate::inbox::InboxMessage {
        schema_version: 1,
        id: Some(id.into()),
        from: from.into(),
        text: text.into(),
        kind: None,
        timestamp: "2026-06-22T16:41:45Z".into(),
        channel: Some(kind),
        ..Default::default()
    };
    crate::inbox::enqueue(home, agent, msg).expect("test setup: enqueue must succeed");
}

/// The core PR-3 fix: a `message_id` routes by THAT message's own channel
/// (from its inbox row), not the sender's process-global `reply_to_channel`
/// tag — so a late reply to an old/reclaimed message lands correctly even
/// when the agent's CURRENT tag points somewhere else (or somewhere broken).
#[test]
fn reply_with_message_id_routes_by_row_channel_not_prefer_chain_2622() {
    let _g = registry_guard();
    crate::channel::reset_active_channel_for_test();
    let agent = "reply-mid-routes-2622";
    let home = tmp_home("reply-mid-routes");
    // The sender's CURRENT tag points at telegram, which is registered but
    // deliberately incapable — proves the targeted path does NOT consult it.
    crate::channel::register_active_channel(MockChannel::arc(
        "telegram",
        ReplyOutcome::NotSupported,
    ));
    crate::channel::register_active_channel(MockChannel::arc("discord", ReplyOutcome::Ok));
    set_reply_to(agent, Some("telegram"));

    enqueue_channel_msg_with_kind(
        &home,
        agent,
        "m-old-1",
        "user:op",
        "old question",
        crate::channel::ChannelKind::Discord,
    );

    let result = super::handle_reply(
        &home,
        &serde_json::json!({"message": "answer", "message_id": "m-old-1"}),
        agent,
    );
    assert_eq!(
        result["message_id"], "mock-msg-1",
        "must route via the row's OWN channel (discord), not the stale telegram tag: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// No `message_id` → byte-identical to the pre-#2622 prefer-chain: the same
/// stale telegram tag now DOES get consulted and its incapability surfaces.
#[test]
fn reply_without_message_id_still_uses_prefer_chain_2622() {
    let _g = registry_guard();
    crate::channel::reset_active_channel_for_test();
    let agent = "reply-mid-backcompat-2622";
    let home = tmp_home("reply-mid-backcompat");
    crate::channel::register_active_channel(MockChannel::arc(
        "telegram",
        ReplyOutcome::NotSupported,
    ));
    crate::channel::register_active_channel(MockChannel::arc("discord", ReplyOutcome::Ok));
    set_reply_to(agent, Some("telegram"));

    let result = super::handle_reply(&home, &serde_json::json!({"message": "answer"}), agent);
    assert_eq!(
        result["code"], "channel_capability_unsupported",
        "omitting message_id must still hit the tagged (incapable) channel: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Fork C: on send success, a targeted reply also settles the persistent row
/// (unconditionally — an `unread` row from an old/reclaimed message is the
/// core use case) so it stops redelivering.
#[test]
fn reply_with_message_id_settles_the_row_on_success_2622() {
    let _g = registry_guard();
    crate::channel::reset_active_channel_for_test();
    let agent = "reply-mid-settles-2622";
    let home = tmp_home("reply-mid-settles");
    crate::channel::register_active_channel(MockChannel::arc("telegram", ReplyOutcome::Ok));

    enqueue_channel_msg(&home, agent, "m-settle-1", "user:op", "old question");
    assert!(
        crate::inbox::storage::find_message(&home, "m-settle-1")
            .and_then(|m| m.read_at)
            .is_none(),
        "precondition: the row starts unread"
    );

    let result = super::handle_reply(
        &home,
        &serde_json::json!({"message": "answer", "message_id": "m-settle-1"}),
        agent,
    );
    assert_eq!(
        result["message_id"], "mock-msg-1",
        "reply must succeed: {result}"
    );
    assert!(
        crate::inbox::storage::find_message(&home, "m-settle-1")
            .and_then(|m| m.read_at)
            .is_some(),
        "a successful targeted reply must settle the row so it stops redelivering"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// An unknown `message_id` must error, never silently fall back to the
/// prefer-chain (that would silently answer on the WRONG channel).
#[test]
fn reply_with_unknown_message_id_errors_2622() {
    let _g = registry_guard();
    crate::channel::reset_active_channel_for_test();
    let agent = "reply-mid-unknown-2622";
    let home = tmp_home("reply-mid-unknown");
    crate::channel::register_active_channel(MockChannel::arc("telegram", ReplyOutcome::Ok));

    let result = super::handle_reply(
        &home,
        &serde_json::json!({"message": "answer", "message_id": "m-nope"}),
        agent,
    );
    assert_eq!(
        result["code"], "message_not_found",
        "an unknown message_id must error, not silently fall back: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// A row with no recorded channel (e.g. a non-channel inbox message) can't be
/// targeted-routed — must error, never silently fall back to the prefer-chain.
#[test]
fn reply_with_message_id_missing_channel_on_row_errors_2622() {
    let _g = registry_guard();
    crate::channel::reset_active_channel_for_test();
    let agent = "reply-mid-nochannel-2622";
    let home = tmp_home("reply-mid-nochannel");
    crate::channel::register_active_channel(MockChannel::arc("telegram", ReplyOutcome::Ok));

    let msg = crate::inbox::InboxMessage {
        schema_version: 1,
        id: Some("m-nochannel-1".into()),
        from: "peer".into(),
        text: "not a channel message".into(),
        kind: Some("update".into()),
        timestamp: "2026-06-22T16:41:45Z".into(),
        channel: None,
        ..Default::default()
    };
    crate::inbox::enqueue(&home, agent, msg).expect("test setup: enqueue must succeed");

    let result = super::handle_reply(
        &home,
        &serde_json::json!({"message": "answer", "message_id": "m-nochannel-1"}),
        agent,
    );
    assert_eq!(
        result["code"], "message_has_no_channel",
        "a channel-less row must error, not silently fall back: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}
