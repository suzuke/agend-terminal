//! Discord adapter — behind the `discord` feature gate.
//!
//! PR1 scope: gateway scaffold + auth + `ChannelEvent::Connected`.
//! Other trait methods stub `Err(NotSupported)` until PR2-4.

use crate::agent::AgentRegistry;
use crate::channel::{
    BindingRef, ChannelCapabilities, ChannelError, ChannelEvent, MarkdownDialect, MentionStyle,
    MsgRef, OutMsg, RateBudget,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::mpsc;

// ---------------------------------------------------------------------------
// Binding payload
// ---------------------------------------------------------------------------

/// Discord-specific binding payload stored inside [`BindingRef`].
/// Holds the channel/thread snowflake that messages are sent to.
#[derive(Debug, Clone, Copy)]
pub struct DiscordBindingPayload {
    pub channel_id: u64,
}

/// Construct a [`BindingRef`] for the contract test harness.
/// Deterministic channel_id derived from the instance name.
#[cfg(test)]
pub(crate) fn discord_make_binding(name: &str) -> BindingRef {
    let id = 1_000_000 + name.bytes().map(|b| b as u64).sum::<u64>();
    BindingRef::new(
        "discord",
        Some(format!("DC#{id}")),
        DiscordBindingPayload { channel_id: id },
    )
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Mutable state for the Discord adapter.
pub struct DiscordState {
    /// Instance → channel_id binding registry.
    pub instance_to_channel: HashMap<String, u64>,
    /// Reverse: channel_id → instance name.
    pub channel_to_instance: HashMap<u64, String>,
    /// Submit key per instance (PTY metadata, unused by Discord but
    /// stored to satisfy the `record_binding` contract).
    pub submit_keys: HashMap<String, String>,
    /// Agent registry wired post-bootstrap.
    pub registry: Option<AgentRegistry>,
    /// User allowlist (Discord user snowflakes). `None` = fail-closed.
    pub user_allowlist: Option<Vec<i64>>,
}

// ---------------------------------------------------------------------------
// Channel
// ---------------------------------------------------------------------------

/// Discord adapter implementing the `Channel` trait.
pub struct DiscordChannel {
    state: Mutex<DiscordState>,
    caps: ChannelCapabilities,
    /// Receiver end of the bounded event channel. The gateway reader
    /// task pushes `ChannelEvent`s here; `poll_event` drains them.
    event_rx: Mutex<mpsc::Receiver<ChannelEvent>>,
}

impl DiscordChannel {
    /// Production constructor. `event_rx` is the receiving end of the
    /// mpsc channel fed by the gateway reader task.
    pub fn new(event_rx: mpsc::Receiver<ChannelEvent>, user_allowlist: Option<Vec<i64>>) -> Self {
        Self {
            state: Mutex::new(DiscordState {
                instance_to_channel: HashMap::new(),
                channel_to_instance: HashMap::new(),
                submit_keys: HashMap::new(),
                registry: None,
                user_allowlist,
            }),
            caps: discord_caps(),
            event_rx: Mutex::new(event_rx),
        }
    }

    /// Test-only constructor that returns both the channel and the
    /// sender end so tests can inject events.
    #[cfg(test)]
    pub(crate) fn new_for_test() -> (Self, mpsc::Sender<ChannelEvent>) {
        let (tx, rx) = mpsc::channel();
        let ch = Self {
            state: Mutex::new(DiscordState {
                instance_to_channel: HashMap::new(),
                channel_to_instance: HashMap::new(),
                submit_keys: HashMap::new(),
                registry: None,
                user_allowlist: None,
            }),
            caps: discord_caps(),
            event_rx: Mutex::new(rx),
        };
        (ch, tx)
    }
}

/// Build the Discord capability matrix (pinned by S5 analysis).
fn discord_caps() -> ChannelCapabilities {
    ChannelCapabilities {
        // Transport
        emits_deletion_events: true,
        threads: true,
        buttons: false, // components deferred
        attachments: true,
        markdown: MarkdownDialect::DiscordMd,
        max_msg_bytes: 2000,
        rate_budget: RateBudget {
            per_second: 5,
            per_minute: 50,
        },
        // UX
        react: true,
        edit: true,
        typing_indicator: true,
        receives_edit_events: true,
        mention_parsing_hint: MentionStyle::AtSnowflake,
        bot_sees_read_receipts: false,
        has_native_multi_thread_view: None,
        ephemeral: false,
    }
}

// ---------------------------------------------------------------------------
// Gateway frame parsing — maps raw JSON to typed payloads / events
// ---------------------------------------------------------------------------

/// Opcode extracted from a raw gateway JSON frame.
/// Used by the gateway reader to dispatch on frame type before
/// deserializing the inner `d` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GatewayFrame {
    pub op: u8,
}

/// Parse the opcode from a raw gateway JSON frame.
/// Returns `None` if the frame is not valid JSON or lacks an `op` field.
pub fn parse_gateway_opcode(raw: &str) -> Option<GatewayFrame> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let op = v.get("op")?.as_u64()? as u8;
    Some(GatewayFrame { op })
}

/// Parse a HELLO frame (opcode 10) and return the heartbeat interval in ms.
pub fn parse_hello_interval(raw: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let d = v.get("d")?;
    let hello: twilight_model::gateway::payload::incoming::Hello =
        serde_json::from_value(d.clone()).ok()?;
    Some(hello.heartbeat_interval)
}

/// Build the IDENTIFY payload our adapter sends to the gateway.
/// Returns the full JSON frame (op=2 + d={token, intents, properties}).
pub fn build_identify_payload(
    token: &str,
    intents: twilight_model::gateway::Intents,
) -> serde_json::Value {
    serde_json::json!({
        "op": 2,
        "d": {
            "token": token,
            "intents": intents.bits(),
            "properties": {
                "os": std::env::consts::OS,
                "browser": "agend-terminal",
                "device": "agend-terminal"
            }
        }
    })
}

/// Returns `true` if the frame is a HEARTBEAT_ACK (opcode 11).
pub fn is_heartbeat_ack(raw: &str) -> bool {
    parse_gateway_opcode(raw).map_or(false, |f| f.op == 11)
}

/// Map a twilight `Ready` payload to `ChannelEvent::Connected`.
pub fn map_ready_to_connected(
    ready: &twilight_model::gateway::payload::incoming::Ready,
) -> ChannelEvent {
    ChannelEvent::Connected {
        kind: "discord".into(),
        who: ready.user.name.clone(),
    }
}

// ---------------------------------------------------------------------------
// Channel trait impl
// ---------------------------------------------------------------------------

impl crate::channel::Channel for DiscordChannel {
    fn kind(&self) -> &'static str {
        "discord"
    }

    fn caps(&self) -> &ChannelCapabilities {
        &self.caps
    }

    fn poll_event(&self) -> Option<ChannelEvent> {
        self.event_rx.lock().try_recv().ok()
    }

    fn send(&self, _binding: &BindingRef, _msg: OutMsg) -> anyhow::Result<MsgRef> {
        anyhow::bail!("discord send not yet implemented (PR2)")
    }

    fn edit(&self, _msg: &MsgRef, _payload: OutMsg) -> anyhow::Result<()> {
        anyhow::bail!("discord edit not yet implemented (PR2)")
    }

    fn delete(&self, _msg: &MsgRef) -> anyhow::Result<()> {
        anyhow::bail!("discord delete not yet implemented (PR2)")
    }

    fn create_binding(
        &self,
        _name: &str,
        _opts: crate::channel::BindingOpts,
    ) -> anyhow::Result<BindingRef> {
        anyhow::bail!("discord create_binding not yet implemented (PR4)")
    }

    fn remove_binding(&self, _binding: &BindingRef) -> anyhow::Result<()> {
        anyhow::bail!("discord remove_binding not yet implemented (PR4)")
    }

    fn has_binding(&self, instance: &str) -> bool {
        self.state.lock().instance_to_channel.contains_key(instance)
    }

    fn record_binding(&self, instance: &str, binding: BindingRef, submit_key: String) {
        let Some(payload) = binding.downcast::<DiscordBindingPayload>() else {
            tracing::warn!(
                kind = binding.kind(),
                instance,
                "record_binding received non-discord binding — dropping"
            );
            return;
        };
        let cid = payload.channel_id;
        let mut s = self.state.lock();
        s.instance_to_channel.insert(instance.to_string(), cid);
        s.channel_to_instance.insert(cid, instance.to_string());
        s.submit_keys.insert(instance.to_string(), submit_key);
    }

    fn take_binding(&self, instance: &str) -> Option<BindingRef> {
        let mut s = self.state.lock();
        let cid = s.instance_to_channel.remove(instance)?;
        s.channel_to_instance.remove(&cid);
        s.submit_keys.remove(instance);
        drop(s);
        Some(BindingRef::new(
            "discord",
            Some(format!("DC#{cid}")),
            DiscordBindingPayload { channel_id: cid },
        ))
    }

    fn attach_registry(&self, registry: AgentRegistry) {
        self.state.lock().registry = Some(registry);
    }

    fn outbound_authorized(&self) -> bool {
        crate::channel::auth::is_outbound_authorized(&self.state.lock().user_allowlist)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::channel::ChannelEvent;

    /// §3.5.10 wire-format fixture: Discord Gateway READY payload
    /// (tests/fixtures/discord-gateway-ready.json) is deserialized via
    /// twilight-model and mapped to `ChannelEvent::Connected`.
    ///
    /// §3.5.11 test-first: this test was committed RED before the
    /// implementation existed. The GREEN commit adds `map_ready_to_connected`.
    #[test]
    fn discord_gateway_ready_emits_connected_event() {
        let fixture = include_str!("../../tests/fixtures/discord-gateway-ready.json");
        let frame: serde_json::Value =
            serde_json::from_str(fixture).expect("fixture must parse as JSON");
        let d = frame.get("d").expect("fixture must have 'd' field");
        let ready: twilight_model::gateway::payload::incoming::Ready =
            serde_json::from_value(d.clone()).expect("'d' must parse as Ready");

        let event = super::map_ready_to_connected(&ready);

        match event {
            ChannelEvent::Connected { kind, who } => {
                assert_eq!(kind, "discord");
                assert_eq!(who, "agend-bot");
            }
            other => panic!("expected Connected, got: {other:?}"),
        }
    }

    /// Contract test: DiscordChannel satisfies the registry-side
    /// contract from `src/channel/contract.rs`.
    #[test]
    fn discord_channel_satisfies_contract() {
        let (ch, _rx) = super::DiscordChannel::new_for_test();
        crate::channel::contract::run_registry_contract(ch, super::discord_make_binding);
    }

    /// Caps snapshot: pin the Discord capability matrix so reviewers
    /// can diff against the S5 analysis.
    #[test]
    fn discord_caps_match_s5_analysis() {
        let (ch, _rx) = super::DiscordChannel::new_for_test();
        let caps = crate::channel::Channel::caps(&ch);

        assert!(caps.emits_deletion_events);
        assert!(caps.threads);
        assert!(caps.attachments);
        assert!(caps.react);
        assert!(caps.edit);
        assert!(caps.typing_indicator);
        assert!(caps.receives_edit_events);
        assert_eq!(caps.max_msg_bytes, 2000);
        assert_eq!(caps.markdown, crate::channel::MarkdownDialect::DiscordMd);
        assert_eq!(
            caps.mention_parsing_hint,
            crate::channel::MentionStyle::AtSnowflake
        );
        assert!(!caps.bot_sees_read_receipts);
        assert!(caps.has_native_multi_thread_view.is_none());
        assert!(!caps.ephemeral);
    }

    /// poll_event drains the internal mpsc channel.
    #[test]
    fn poll_event_drains_mpsc() {
        let (ch, tx) = super::DiscordChannel::new_for_test();
        assert!(crate::channel::Channel::poll_event(&ch).is_none());

        tx.send(ChannelEvent::Connected {
            kind: "discord".into(),
            who: "test-bot".into(),
        })
        .expect("send");

        let event = crate::channel::Channel::poll_event(&ch).expect("should have event");
        match event {
            ChannelEvent::Connected { kind, who } => {
                assert_eq!(kind, "discord");
                assert_eq!(who, "test-bot");
            }
            other => panic!("expected Connected, got: {other:?}"),
        }

        assert!(crate::channel::Channel::poll_event(&ch).is_none());
    }

    // ── §3.5.10 expanded gateway handshake fixture tests ─────────────
    //
    // F1 fix: cover the full HELLO → IDENTIFY → HEARTBEAT → HEARTBEAT_ACK
    // → READY sequence using Discord API spec payloads.
    //
    // §3.5.11 r3 empirical-revert exemption: impl already exists from
    // GREEN commit; tests depend on impl-provided fns. Reviewer can
    // revert impl to verify test failure.

    /// HELLO (opcode 10): server sends heartbeat_interval after WS connect.
    /// Fixture: tests/fixtures/discord-gateway-hello.json (Discord API spec).
    #[test]
    fn discord_gateway_hello_parsed_correctly() {
        let fixture = include_str!("../../tests/fixtures/discord-gateway-hello.json");

        // Opcode must be 10 (Hello).
        let frame = super::parse_gateway_opcode(fixture).expect("must parse");
        assert_eq!(frame.op, 10, "HELLO opcode must be 10");

        // heartbeat_interval must be extractable.
        let interval = super::parse_hello_interval(fixture).expect("must parse interval");
        assert_eq!(interval, 41250, "fixture heartbeat_interval");
    }

    /// IDENTIFY (opcode 2): client sends token + intents after receiving HELLO.
    /// Asserts the frame our adapter builds matches Discord spec shape.
    #[test]
    fn discord_gateway_identify_shape_matches_spec() {
        let intents = twilight_model::gateway::Intents::GUILDS
            | twilight_model::gateway::Intents::GUILD_MESSAGES
            | twilight_model::gateway::Intents::MESSAGE_CONTENT;

        let frame = super::build_identify_payload("Bot test-token-redacted", intents);

        // op must be 2
        assert_eq!(frame["op"], 2, "IDENTIFY opcode must be 2");

        // d.token present
        assert_eq!(frame["d"]["token"], "Bot test-token-redacted");

        // d.intents is a numeric bitfield
        assert!(frame["d"]["intents"].is_u64(), "intents must be numeric");

        // d.properties has required fields per Discord spec
        let props = &frame["d"]["properties"];
        assert!(props["os"].is_string(), "properties.os required");
        assert_eq!(props["browser"], "agend-terminal");
        assert_eq!(props["device"], "agend-terminal");
    }

    /// HEARTBEAT_ACK (opcode 11): server acknowledges client heartbeat.
    /// Fixture: tests/fixtures/discord-gateway-heartbeat-ack.json.
    #[test]
    fn discord_gateway_heartbeat_ack_recognized() {
        let fixture = include_str!("../../tests/fixtures/discord-gateway-heartbeat-ack.json");

        let frame = super::parse_gateway_opcode(fixture).expect("must parse");
        assert_eq!(frame.op, 11, "HEARTBEAT_ACK opcode must be 11");
        assert!(super::is_heartbeat_ack(fixture), "is_heartbeat_ack");
    }

    /// HEARTBEAT (opcode 1): client sends sequence number periodically.
    /// Spec shape: `{"op": 1, "d": <last_sequence_or_null>}`.
    #[test]
    fn discord_gateway_heartbeat_shape() {
        // First heartbeat (no sequence yet) — d is null per spec.
        let first = r#"{"op": 1, "d": null}"#;
        let frame = super::parse_gateway_opcode(first).expect("must parse");
        assert_eq!(frame.op, 1, "HEARTBEAT opcode must be 1");
        assert!(!super::is_heartbeat_ack(first), "heartbeat is not ack");

        // Subsequent heartbeat with sequence number.
        let subsequent = r#"{"op": 1, "d": 42}"#;
        let frame = super::parse_gateway_opcode(subsequent).expect("must parse");
        assert_eq!(frame.op, 1);
    }

    /// Full handshake sequence: HELLO → IDENTIFY → HEARTBEAT → HEARTBEAT_ACK → READY.
    /// Asserts the correct opcode ordering and that each frame parses.
    #[test]
    fn discord_gateway_full_handshake_sequence() {
        let hello = include_str!("../../tests/fixtures/discord-gateway-hello.json");
        let heartbeat_ack = include_str!("../../tests/fixtures/discord-gateway-heartbeat-ack.json");
        let ready = include_str!("../../tests/fixtures/discord-gateway-ready.json");

        // Step 1: Server sends HELLO (op=10)
        let f1 = super::parse_gateway_opcode(hello).expect("hello");
        assert_eq!(f1.op, 10);

        // Step 2: Client sends IDENTIFY (op=2) — we build it
        let identify =
            super::build_identify_payload("Bot fake", twilight_model::gateway::Intents::GUILDS);
        assert_eq!(identify["op"], 2);

        // Step 3: Client sends HEARTBEAT (op=1)
        let hb = r#"{"op": 1, "d": null}"#;
        let f3 = super::parse_gateway_opcode(hb).expect("heartbeat");
        assert_eq!(f3.op, 1);

        // Step 4: Server sends HEARTBEAT_ACK (op=11)
        let f4 = super::parse_gateway_opcode(heartbeat_ack).expect("ack");
        assert_eq!(f4.op, 11);

        // Step 5: Server sends READY (op=0, t=READY)
        let f5 = super::parse_gateway_opcode(ready).expect("ready");
        assert_eq!(f5.op, 0);

        // Map READY to Connected event
        let frame: serde_json::Value = serde_json::from_str(ready).expect("json");
        let d = frame.get("d").expect("d");
        let ready_payload: twilight_model::gateway::payload::incoming::Ready =
            serde_json::from_value(d.clone()).expect("Ready");
        let event = super::map_ready_to_connected(&ready_payload);
        assert!(matches!(event, ChannelEvent::Connected { .. }));
    }
}
