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
