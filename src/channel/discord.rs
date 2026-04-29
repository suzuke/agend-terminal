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
    /// twilight HTTP client for REST API calls. `None` only in test
    /// harness — production `new` always populates it.
    pub http_client: Option<std::sync::Arc<twilight_http::Client>>,
    /// Guild (server) snowflake for binding creation.
    pub guild_id: u64,
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
    pub fn new(
        event_rx: mpsc::Receiver<ChannelEvent>,
        user_allowlist: Option<Vec<i64>>,
        http_client: std::sync::Arc<twilight_http::Client>,
        guild_id: u64,
    ) -> Self {
        Self {
            state: Mutex::new(DiscordState {
                instance_to_channel: HashMap::new(),
                channel_to_instance: HashMap::new(),
                submit_keys: HashMap::new(),
                registry: None,
                user_allowlist,
                http_client: Some(http_client),
                guild_id,
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
                http_client: None,
                guild_id: 0,
            }),
            caps: discord_caps(),
            event_rx: Mutex::new(rx),
        };
        (ch, tx)
    }

    /// Test-only constructor with a configured allowlist so
    /// `outbound_authorized()` returns `true`.
    #[cfg(test)]
    pub(crate) fn new_for_test_authorized() -> (Self, mpsc::Sender<ChannelEvent>) {
        let (tx, rx) = mpsc::channel();
        let ch = Self {
            state: Mutex::new(DiscordState {
                instance_to_channel: HashMap::new(),
                channel_to_instance: HashMap::new(),
                submit_keys: HashMap::new(),
                registry: None,
                user_allowlist: Some(vec![1]),
                http_client: None,
                guild_id: 0,
            }),
            caps: discord_caps(),
            event_rx: Mutex::new(rx),
        };
        (ch, tx)
    }

    /// Test-only constructor with a custom twilight HTTP client (for
    /// mock-server tests that exercise the real send path).
    #[cfg(test)]
    pub(crate) fn new_for_test_with_http(
        http: std::sync::Arc<twilight_http::Client>,
    ) -> (Self, mpsc::Sender<ChannelEvent>) {
        let (tx, rx) = mpsc::channel();
        let ch = Self {
            state: Mutex::new(DiscordState {
                instance_to_channel: HashMap::new(),
                channel_to_instance: HashMap::new(),
                submit_keys: HashMap::new(),
                registry: None,
                user_allowlist: Some(vec![1]),
                http_client: Some(http),
                guild_id: 987654321098765432,
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
pub(crate) struct GatewayFrame {
    pub(crate) op: u8,
}

/// Parse the opcode from a raw gateway JSON frame.
/// Returns `None` if the frame is not valid JSON or lacks an `op` field.
pub(crate) fn parse_gateway_opcode(raw: &str) -> Option<GatewayFrame> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let op = v.get("op")?.as_u64()? as u8;
    Some(GatewayFrame { op })
}

/// Parse a HELLO frame (opcode 10) and return the heartbeat interval in ms.
pub(crate) fn parse_hello_interval(raw: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let d = v.get("d")?;
    let hello: twilight_model::gateway::payload::incoming::Hello =
        serde_json::from_value(d.clone()).ok()?;
    Some(hello.heartbeat_interval)
}

/// Build the IDENTIFY payload our adapter sends to the gateway.
/// Returns the full JSON frame (op=2 + d={token, intents, properties}).
pub(crate) fn build_identify_payload(
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
pub(crate) fn is_heartbeat_ack(raw: &str) -> bool {
    parse_gateway_opcode(raw).map_or(false, |f| f.op == 11)
}

/// Map a twilight `Ready` payload to `ChannelEvent::Connected`.
pub(crate) fn map_ready_to_connected(
    ready: &twilight_model::gateway::payload::incoming::Ready,
) -> ChannelEvent {
    ChannelEvent::Connected {
        kind: "discord".into(),
        who: ready.user.name.clone(),
    }
}

/// Map a twilight `Message` (from MESSAGE_CREATE dispatch) to
/// `ChannelEvent::MessageIn`.
pub(crate) fn map_message_create_to_message_in(
    msg: &twilight_model::channel::Message,
) -> ChannelEvent {
    use crate::channel::event::{MsgPayload, User};
    ChannelEvent::MessageIn {
        binding: BindingRef::new(
            "discord",
            Some(format!("DC#{}", msg.channel_id)),
            DiscordBindingPayload {
                channel_id: msg.channel_id.get(),
            },
        ),
        from: User {
            id: msg.author.id.to_string(),
            handle: Some(msg.author.name.clone()),
        },
        payload: MsgPayload {
            text: msg.content.clone(),
        },
        ts: chrono::DateTime::parse_from_rfc3339(&msg.timestamp.iso_8601().to_string())
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now()),
    }
}

/// Map a twilight `Message` (from REST response) to `MsgRef`.
pub(crate) fn map_message_to_msg_ref(
    msg: &twilight_model::channel::Message,
) -> crate::channel::MsgRef {
    crate::channel::MsgRef {
        binding: BindingRef::new(
            "discord",
            Some(format!("DC#{}", msg.channel_id)),
            DiscordBindingPayload {
                channel_id: msg.channel_id.get(),
            },
        ),
        id: msg.id.to_string(),
    }
}

/// Map a Discord CHANNEL_DELETE gateway event to `ChannelEvent::BindingRevoked`.
/// `channel_id` is the deleted channel's snowflake.
pub(crate) fn map_channel_delete_to_binding_revoked(channel_id: u64) -> ChannelEvent {
    ChannelEvent::BindingRevoked {
        binding: BindingRef::new(
            "discord",
            Some(format!("DC#{channel_id}")),
            DiscordBindingPayload { channel_id },
        ),
        reason: crate::channel::event::RevokeReason::Deleted,
    }
}

// ---------------------------------------------------------------------------
// Outbound request body construction
// ---------------------------------------------------------------------------

/// Build the JSON body for `POST /channels/{id}/messages` per Discord spec.
/// Ref: https://discord.com/developers/docs/resources/message#create-message-jsonform-params
///
/// This is the canonical shape our adapter transmits. The test suite
/// asserts this against the spec-quoted example (§3.5.10 outbound
/// request boundary).
pub(crate) fn build_create_message_body(text: &str) -> serde_json::Value {
    serde_json::json!({ "content": text })
}

// ---------------------------------------------------------------------------
// Auto-archive keepalive
// ---------------------------------------------------------------------------

/// Keepalive interval for Discord thread auto-archive prevention.
/// Discord's shortest auto-archive is 1 hour; 30 min refresh is safe.
pub(crate) const KEEPALIVE_INTERVAL_SECS: u64 = 30 * 60;

/// Start a background thread that periodically PATCHes all bound
/// Discord threads to prevent auto-archive.
pub(crate) fn start_keepalive(state: std::sync::Arc<Mutex<DiscordState>>) {
    // fire-and-forget: keepalive thread runs for the adapter's lifetime.
    // Stops when the daemon process exits. No JoinHandle needed — the
    // thread is purely side-effecting (PATCH calls) with no return value.
    if let Err(e) = std::thread::Builder::new()
        .name("discord-keepalive".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("keepalive tokio runtime");
            loop {
                std::thread::sleep(std::time::Duration::from_secs(KEEPALIVE_INTERVAL_SECS));
                let (http, channel_ids) = {
                    let s = state.lock();
                    let http = match s.http_client.clone() {
                        Some(h) => h,
                        None => continue,
                    };
                    let ids: Vec<u64> = s.instance_to_channel.values().copied().collect();
                    (http, ids)
                };
                for cid in channel_ids {
                    let id = twilight_model::id::Id::new(cid);
                    let http = http.clone();
                    rt.block_on(async {
                        if let Err(e) = http.update_thread(id).archived(false).await {
                            tracing::debug!(channel_id = cid, %e, "keepalive PATCH failed");
                        }
                    });
                }
            }
        })
    {
        tracing::error!(error = %e, "failed to spawn keepalive thread");
    }
}

/// Send a single keepalive PATCH for a specific channel. Extracted for
/// testability — the production `start_keepalive` loop calls this per
/// binding; tests call it directly against a mock server.
pub(crate) fn send_keepalive_patch(
    http: &twilight_http::Client,
    channel_id: u64,
) -> anyhow::Result<()> {
    let id = twilight_model::id::Id::new(channel_id);
    discord_runtime().block_on(async { http.update_thread(id).archived(false).await })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Channel trait impl
// ---------------------------------------------------------------------------

/// Shared tokio runtime for Discord sync→async calls.
fn discord_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("discord tokio runtime")
    })
}

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

    fn send(&self, binding: &BindingRef, msg: OutMsg) -> anyhow::Result<MsgRef> {
        let payload = binding
            .downcast::<DiscordBindingPayload>()
            .ok_or_else(|| anyhow::anyhow!("non-discord binding passed to send"))?;
        if msg.text.is_empty() {
            anyhow::bail!("OutMsg has no text (attachment-only sends deferred to PR3)");
        }
        let http = self
            .state
            .lock()
            .http_client
            .clone()
            .ok_or_else(|| anyhow::anyhow!("discord http client not initialized"))?;
        let channel_id = twilight_model::id::Id::new(payload.channel_id);
        let text = msg.text;
        let cp = *payload;
        discord_runtime().block_on(async {
            let response = http.create_message(channel_id).content(&text).await?;
            let sent = response.model().await?;
            Ok(MsgRef {
                binding: BindingRef::new("discord", Some(format!("DC#{}", cp.channel_id)), cp),
                id: sent.id.to_string(),
            })
        })
    }

    fn edit(&self, msg: &MsgRef, payload: OutMsg) -> anyhow::Result<()> {
        if payload.text.is_empty() {
            anyhow::bail!("OutMsg.text empty — Discord editMessage requires non-empty text");
        }
        let http = self
            .state
            .lock()
            .http_client
            .clone()
            .ok_or_else(|| anyhow::anyhow!("discord http client not initialized"))?;
        let channel_id: u64 = msg
            .binding
            .downcast::<DiscordBindingPayload>()
            .map(|p| p.channel_id)
            .ok_or_else(|| anyhow::anyhow!("non-discord binding in MsgRef"))?;
        let mid: u64 = msg
            .id
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid discord message_id: {}", msg.id))?;
        let text = payload.text;
        discord_runtime().block_on(async {
            http.update_message(
                twilight_model::id::Id::new(channel_id),
                twilight_model::id::Id::new(mid),
            )
            .content(Some(&text))
            .await?;
            Ok(())
        })
    }

    fn delete(&self, msg: &MsgRef) -> anyhow::Result<()> {
        let http = self
            .state
            .lock()
            .http_client
            .clone()
            .ok_or_else(|| anyhow::anyhow!("discord http client not initialized"))?;
        let channel_id: u64 = msg
            .binding
            .downcast::<DiscordBindingPayload>()
            .map(|p| p.channel_id)
            .ok_or_else(|| anyhow::anyhow!("non-discord binding in MsgRef"))?;
        let mid: u64 = msg
            .id
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid discord message_id: {}", msg.id))?;
        discord_runtime().block_on(async {
            http.delete_message(
                twilight_model::id::Id::new(channel_id),
                twilight_model::id::Id::new(mid),
            )
            .await?;
            Ok(())
        })
    }

    fn create_binding(
        &self,
        name: &str,
        opts: crate::channel::BindingOpts,
    ) -> anyhow::Result<BindingRef> {
        anyhow::bail!("stub");
        let (http, guild_id) = {
            let s = self.state.lock();
            let http = s
                .http_client
                .clone()
                .ok_or_else(|| anyhow::anyhow!("discord http client not initialized"))?;
            (http, s.guild_id)
        };
        let display_name = opts.display_name.as_deref().unwrap_or(name);
        let parent_id = opts
            .extra
            .get("category_id")
            .and_then(|v| v.parse::<u64>().ok());
        let gid = twilight_model::id::Id::new(guild_id);
        discord_runtime().block_on(async {
            let mut req = http.create_guild_channel(gid, display_name);
            if let Some(pid) = parent_id {
                req = req.parent_id(twilight_model::id::Id::new(pid));
            }
            let response = req.await?;
            let channel = response.model().await?;
            let cid = channel.id.get();
            Ok(BindingRef::new(
                "discord",
                Some(format!("DC#{cid}")),
                DiscordBindingPayload { channel_id: cid },
            ))
        })
    }

    fn remove_binding(&self, binding: &BindingRef) -> anyhow::Result<()> {
        let payload = binding
            .downcast::<DiscordBindingPayload>()
            .ok_or_else(|| anyhow::anyhow!("non-discord binding passed to remove_binding"))?;
        let http = self
            .state
            .lock()
            .http_client
            .clone()
            .ok_or_else(|| anyhow::anyhow!("discord http client not initialized"))?;
        let cid = twilight_model::id::Id::new(payload.channel_id);
        discord_runtime().block_on(async {
            http.delete_channel(cid).await?;
            Ok(())
        })
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

    fn create_topic(
        &self,
        name: &str,
    ) -> std::result::Result<crate::channel::TopicRef, ChannelError> {
        let binding = self
            .create_binding(name, crate::channel::BindingOpts::default())
            .map_err(ChannelError::Other)?;
        let cid = binding
            .downcast::<DiscordBindingPayload>()
            .map(|p| p.channel_id)
            .unwrap_or(0);
        Ok(crate::channel::TopicRef {
            id: cid.to_string(),
            channel_kind: crate::channel::ChannelKind::Discord,
        })
    }

    fn notify(
        &self,
        instance: &str,
        _severity: crate::channel::NotifySeverity,
        message: &str,
        _silent: bool, // Discord has no per-message notification suppression
    ) -> std::result::Result<(), ChannelError> {
        let cid = self.state.lock().instance_to_channel.get(instance).copied();
        let cid = cid.ok_or_else(|| {
            ChannelError::Other(anyhow::anyhow!("no discord binding for '{instance}'"))
        })?;
        let binding = BindingRef::new(
            "discord",
            Some(format!("DC#{cid}")),
            DiscordBindingPayload { channel_id: cid },
        );
        self.send(&binding, OutMsg::text(message))
            .map_err(ChannelError::Other)?;
        Ok(())
    }

    fn send_from_agent(
        &self,
        agent: &str,
        op: crate::channel::AgentOutboundOp,
    ) -> std::result::Result<MsgRef, ChannelError> {
        // Step 1: adapter-level allowlist gate (PR #216 contract).
        if !self.outbound_authorized() {
            return Err(ChannelError::Other(anyhow::anyhow!(
                "outbound disabled — channel.user_allowlist not configured"
            )));
        }

        // Step 2: dispatch.
        match op {
            crate::channel::AgentOutboundOp::Reply { text } => {
                let cid = self.state.lock().instance_to_channel.get(agent).copied();
                let cid = cid.ok_or_else(|| {
                    ChannelError::Other(anyhow::anyhow!("no discord binding for '{agent}'"))
                })?;
                let binding = BindingRef::new(
                    "discord",
                    Some(format!("DC#{cid}")),
                    DiscordBindingPayload { channel_id: cid },
                );
                self.send(&binding, OutMsg::text(text))
                    .map_err(ChannelError::Other)
            }
            crate::channel::AgentOutboundOp::Edit {
                message_id,
                new_text,
            } => {
                let cid = self.state.lock().instance_to_channel.get(agent).copied();
                let cid = cid.ok_or_else(|| {
                    ChannelError::Other(anyhow::anyhow!("no discord binding for '{agent}'"))
                })?;
                let msg_ref = MsgRef {
                    binding: BindingRef::new(
                        "discord",
                        Some(format!("DC#{cid}")),
                        DiscordBindingPayload { channel_id: cid },
                    ),
                    id: message_id.clone(),
                };
                self.edit(&msg_ref, OutMsg::text(new_text))
                    .map_err(ChannelError::Other)?;
                Ok(MsgRef {
                    binding: BindingRef::new(
                        "discord",
                        None,
                        DiscordBindingPayload { channel_id: cid },
                    ),
                    id: message_id,
                })
            }
            _ => Err(ChannelError::NotSupported(
                "React/InjectProvenance deferred to TIER-C".into(),
            )),
        }
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

    // ── PR2 tests: MessageIn + send + notify ─────────────────────────

    /// §3.5.10 wire-format fixture: MESSAGE_CREATE gateway event
    /// parsed into `ChannelEvent::MessageIn`.
    #[test]
    fn discord_message_create_emits_message_in() {
        let fixture = include_str!("../../tests/fixtures/discord-gateway-message-create.json");
        let frame: serde_json::Value = serde_json::from_str(fixture).expect("fixture must parse");
        let d = frame.get("d").expect("d field");
        let msg: twilight_model::channel::Message =
            serde_json::from_value(d.clone()).expect("Message");

        let event = super::map_message_create_to_message_in(&msg);

        match event {
            ChannelEvent::MessageIn {
                binding,
                from,
                payload,
                ts,
            } => {
                assert_eq!(binding.kind(), "discord");
                assert_eq!(from.id, "82198898841029460");
                assert_eq!(from.handle.as_deref(), Some("testoperator"));
                assert_eq!(payload.text, "hello from discord");
                // ts should be parseable (not epoch-zero)
                assert!(ts.timestamp() > 0);
            }
            other => panic!("expected MessageIn, got: {other:?}"),
        }
    }

    /// §3.5.10 wire-format fixture: outbound POST /channels/{id}/messages
    /// response parsed into `MsgRef`.
    #[test]
    fn discord_create_message_response_parses_to_msg_ref() {
        let fixture =
            include_str!("../../tests/fixtures/discord-rest-create-message-response.json");
        let msg: twilight_model::channel::Message =
            serde_json::from_str(fixture).expect("response must parse as Message");

        let msg_ref = super::map_message_to_msg_ref(&msg);

        assert_eq!(msg_ref.id, "444385199974967099");
        assert_eq!(msg_ref.binding.kind(), "discord");
    }

    /// send_from_agent(Reply) on an authorized channel with no binding
    /// for the agent should error with "no discord binding".
    #[test]
    fn send_from_agent_reply_errors_on_unbound_instance() {
        let (ch, _rx) = super::DiscordChannel::new_for_test_authorized();
        // Authorized but no binding → should error about binding.
        let result = crate::channel::Channel::send_from_agent(
            &ch,
            "unknown-agent",
            crate::channel::AgentOutboundOp::Reply { text: "hi".into() },
        );
        assert!(result.is_err(), "unbound instance must error");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("no discord binding"),
            "error must mention binding, got: {err_msg}"
        );
    }

    /// F2 fix: send_from_agent must check outbound_authorized() gate.
    /// When user_allowlist is None (unconfigured), the gate drops the call.
    #[test]
    fn send_from_agent_blocked_by_outbound_gate() {
        let (ch, _rx) = super::DiscordChannel::new_for_test(); // allowlist=None → unauthorized
        let result = crate::channel::Channel::send_from_agent(
            &ch,
            "any-agent",
            crate::channel::AgentOutboundOp::Reply { text: "hi".into() },
        );
        assert!(result.is_err(), "unauthorized channel must reject");
        let err_msg = format!("{}", result.unwrap_err());
        assert!(
            err_msg.contains("outbound disabled"),
            "error must mention outbound gate, got: {err_msg}"
        );
    }

    /// notify on an unbound instance should error gracefully.
    #[test]
    fn notify_errors_on_unbound_instance() {
        let (ch, _rx) = super::DiscordChannel::new_for_test();
        let result = crate::channel::Channel::notify(
            &ch,
            "unknown-agent",
            crate::channel::NotifySeverity::Info,
            "test notification",
            false,
        );
        assert!(result.is_err(), "notify on unbound instance must error");
    }

    // ── F3 fix: §3.5.10 outbound request body shape assertion ────────
    //
    // Production-path-coupled: exercises the real Channel::send() →
    // twilight_http::create_message() path against a mock HTTP server.
    // The mock captures the request body twilight actually transmits
    // and asserts it matches the Discord spec shape.

    /// §3.5.10 wire-format: outbound POST /channels/{id}/messages request
    /// body transmitted by twilight-http matches Discord spec shape.
    ///
    /// Uses a raw TCP listener as mock Discord API server. The twilight
    /// client is pointed at it via `proxy()`. Channel::send() exercises
    /// the real production code path.
    #[test]
    fn discord_send_outbound_body_matches_spec() {
        use crate::channel::Channel;
        use std::io::{Read, Write};
        use std::net::TcpListener;

        // Step 1: Start a mock HTTP server on an ephemeral port.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();

        // Step 2: Spawn a thread to handle one request.
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let captured_clone = captured.clone();
        // fire-and-forget: test mock server thread — lives only for this test
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).expect("read");
            let request = String::from_utf8_lossy(&buf[..n]).to_string();

            // Extract body after the \r\n\r\n header separator.
            if let Some(idx) = request.find("\r\n\r\n") {
                let body = &request[idx + 4..];
                *captured_clone.lock().expect("lock") = Some(body.to_string());
            }

            // Respond with a minimal valid Discord Message JSON.
            let response_body =
                include_str!("../../tests/fixtures/discord-rest-create-message-response.json");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream.write_all(response.as_bytes()).expect("write");
        });

        // Step 3: Create twilight client pointed at mock server.
        let client = twilight_http::Client::builder()
            .proxy(format!("127.0.0.1:{port}"), true)
            .build();
        let client = std::sync::Arc::new(client);

        // Step 4: Create DiscordChannel with this client + a recorded binding.
        let (ch, _tx) = super::DiscordChannel::new_for_test_with_http(client);
        let binding = crate::channel::BindingRef::new(
            "discord",
            Some("DC#290926798999357250".into()),
            super::DiscordBindingPayload {
                channel_id: 290926798999357250,
            },
        );
        ch.record_binding("test-agent", binding.clone(), "\r".into());

        // Step 5: Call the real production send() path.
        let result = crate::channel::Channel::send(
            &ch,
            &binding,
            crate::channel::OutMsg::text("Hello, World!"),
        );

        handle.join().expect("mock server thread");

        // Step 6: Assert the request body twilight transmitted.
        assert!(result.is_ok(), "send must succeed: {:?}", result.err());
        let body_str = captured
            .lock()
            .expect("lock")
            .take()
            .expect("body captured");
        let actual: serde_json::Value =
            serde_json::from_str(&body_str).expect("body must be valid JSON");
        let expected: serde_json::Value = serde_json::json!({"content": "Hello, World!"});
        assert_eq!(
            actual, expected,
            "outbound body must match Discord spec create-message shape"
        );
    }

    // ── PR3 tests: edit + delete production-path-coupled ─────────────

    /// Captured HTTP request from the mock server: method, path, body.
    struct CapturedRequest {
        method: String,
        path: String,
        body: String,
    }

    /// Reusable mock HTTP server that captures one request and responds
    /// with a canned response. Returns (port, join_handle, captured_arc).
    fn mock_http_server(
        response_status: u16,
        response_body: &str,
    ) -> (
        u16,
        std::thread::JoinHandle<()>,
        std::sync::Arc<std::sync::Mutex<Option<CapturedRequest>>>,
    ) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<CapturedRequest>));
        let captured_clone = captured.clone();
        let resp_body = response_body.to_string();
        let status = response_status;

        // fire-and-forget: test mock server thread — lives only for this test
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).expect("read");
            let request = String::from_utf8_lossy(&buf[..n]).to_string();

            // Parse method + path from first line.
            let first_line = request.lines().next().unwrap_or("");
            let parts: Vec<&str> = first_line.split_whitespace().collect();
            let method = parts.first().unwrap_or(&"").to_string();
            let path = parts.get(1).unwrap_or(&"").to_string();

            // Extract body after \r\n\r\n.
            let body = request
                .find("\r\n\r\n")
                .map(|idx| request[idx + 4..].to_string())
                .unwrap_or_default();

            *captured_clone.lock().expect("lock") = Some(CapturedRequest { method, path, body });

            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                resp_body.len(),
                resp_body
            );
            stream.write_all(response.as_bytes()).expect("write");
        });

        (port, handle, captured)
    }

    fn make_test_channel_with_mock(
        port: u16,
    ) -> (super::DiscordChannel, std::sync::mpsc::Sender<ChannelEvent>) {
        let client = twilight_http::Client::builder()
            .proxy(format!("127.0.0.1:{port}"), true)
            .build();
        super::DiscordChannel::new_for_test_with_http(std::sync::Arc::new(client))
    }

    fn test_binding(channel_id: u64) -> crate::channel::BindingRef {
        crate::channel::BindingRef::new(
            "discord",
            Some(format!("DC#{channel_id}")),
            super::DiscordBindingPayload { channel_id },
        )
    }

    fn test_msg_ref(channel_id: u64, msg_id: &str) -> crate::channel::MsgRef {
        crate::channel::MsgRef {
            binding: test_binding(channel_id),
            id: msg_id.to_string(),
        }
    }

    /// §3.5.10 wire-format: PATCH /channels/{cid}/messages/{mid} request
    /// body transmitted by twilight-http matches Discord edit-message spec.
    /// Ref: https://discord.com/developers/docs/resources/message#edit-message
    #[test]
    fn discord_edit_outbound_body_matches_spec() {
        use crate::channel::Channel;

        // Edit response is the updated message — reuse create-message fixture.
        let response_body =
            include_str!("../../tests/fixtures/discord-rest-create-message-response.json");
        let (port, handle, captured) = mock_http_server(200, response_body);
        let (ch, _tx) = make_test_channel_with_mock(port);

        let msg_ref = test_msg_ref(290926798999357250, "444385199974967099");
        let result = ch.edit(&msg_ref, crate::channel::OutMsg::text("edited text"));

        handle.join().expect("mock server");
        assert!(result.is_ok(), "edit must succeed: {:?}", result.err());

        let req = captured.lock().expect("lock").take().expect("captured");
        assert_eq!(req.method, "PATCH", "edit must use PATCH method");
        assert!(
            req.path.contains("/messages/444385199974967099"),
            "path must contain message id: {}",
            req.path
        );
        let body: serde_json::Value = serde_json::from_str(&req.body).expect("body must be JSON");
        assert_eq!(
            body["content"], "edited text",
            "edit body must contain updated content"
        );
    }

    /// §3.5.10 wire-format: DELETE /channels/{cid}/messages/{mid}
    /// Ref: https://discord.com/developers/docs/resources/message#delete-message
    #[test]
    fn discord_delete_outbound_method_matches_spec() {
        use crate::channel::Channel;

        // DELETE returns 204 No Content with empty body per spec.
        let (port, handle, captured) = mock_http_server(204, "");
        let (ch, _tx) = make_test_channel_with_mock(port);

        let msg_ref = test_msg_ref(290926798999357250, "444385199974967099");
        let result = ch.delete(&msg_ref);

        handle.join().expect("mock server");
        assert!(result.is_ok(), "delete must succeed: {:?}", result.err());

        let req = captured.lock().expect("lock").take().expect("captured");
        assert_eq!(req.method, "DELETE", "delete must use DELETE method");
        assert!(
            req.path.contains("/messages/444385199974967099"),
            "path must contain message id: {}",
            req.path
        );
        assert!(
            req.body.is_empty() || req.body.trim().is_empty(),
            "DELETE body must be empty per spec, got: '{}'",
            req.body
        );
    }

    /// send_from_agent(Edit) wires through edit() with gate check.
    #[test]
    fn send_from_agent_edit_wires_through_edit() {
        use crate::channel::Channel;

        let response_body =
            include_str!("../../tests/fixtures/discord-rest-create-message-response.json");
        let (port, handle, captured) = mock_http_server(200, response_body);
        let (ch, _tx) = make_test_channel_with_mock(port);

        // Record a binding so the agent lookup succeeds.
        ch.record_binding("test-agent", test_binding(290926798999357250), "\r".into());

        let result = ch.send_from_agent(
            "test-agent",
            crate::channel::AgentOutboundOp::Edit {
                message_id: "444385199974967099".into(),
                new_text: "updated".into(),
            },
        );

        handle.join().expect("mock server");
        assert!(
            result.is_ok(),
            "send_from_agent Edit must succeed: {:?}",
            result.err()
        );

        let req = captured.lock().expect("lock").take().expect("captured");
        assert_eq!(req.method, "PATCH");
    }

    // ── PR4 tests: binding lifecycle + CHANNEL_DELETE + persistence ───

    /// §3.5.10 wire-format: POST /guilds/{gid}/channels request via
    /// production Channel::create_binding() path.
    #[test]
    fn discord_create_binding_outbound_matches_spec() {
        use crate::channel::Channel;

        let response_body =
            include_str!("../../tests/fixtures/discord-rest-create-guild-channel-response.json");
        let (port, handle, captured) = mock_http_server(200, response_body);
        let (ch, _tx) = make_test_channel_with_mock(port);

        let result = ch.create_binding("test-agent", crate::channel::BindingOpts::default());

        handle.join().expect("mock server");
        assert!(
            result.is_ok(),
            "create_binding must succeed: {:?}",
            result.err()
        );

        let binding = result.expect("binding");
        assert_eq!(binding.kind(), "discord");
        // Channel ID from fixture response.
        assert_eq!(binding.display_tag(), Some("DC#555555555555555555"));

        let req = captured.lock().expect("lock").take().expect("captured");
        assert_eq!(req.method, "POST", "create_binding must use POST");
        assert!(
            req.path.contains("/guilds/"),
            "path must target guild: {}",
            req.path
        );
        let body: serde_json::Value = serde_json::from_str(&req.body).expect("body must be JSON");
        assert!(
            body["name"].is_string(),
            "request body must have 'name' field"
        );
    }

    /// §3.5.10 wire-format: DELETE /channels/{id} via production
    /// Channel::remove_binding() path.
    #[test]
    fn discord_remove_binding_outbound_matches_spec() {
        use crate::channel::Channel;

        // DELETE returns the deleted channel object per spec.
        let response_body =
            include_str!("../../tests/fixtures/discord-rest-create-guild-channel-response.json");
        let (port, handle, captured) = mock_http_server(200, response_body);
        let (ch, _tx) = make_test_channel_with_mock(port);

        let binding = test_binding(555555555555555555);
        let result = ch.remove_binding(&binding);

        handle.join().expect("mock server");
        assert!(
            result.is_ok(),
            "remove_binding must succeed: {:?}",
            result.err()
        );

        let req = captured.lock().expect("lock").take().expect("captured");
        assert_eq!(req.method, "DELETE", "remove_binding must use DELETE");
        assert!(
            req.path.contains("/channels/555555555555555555"),
            "path must contain channel id: {}",
            req.path
        );
    }

    /// §3.5.10 wire-format: CHANNEL_DELETE gateway event → BindingRevoked.
    #[test]
    fn discord_channel_delete_emits_binding_revoked() {
        let fixture = include_str!("../../tests/fixtures/discord-gateway-channel-delete.json");
        let frame: serde_json::Value = serde_json::from_str(fixture).expect("fixture must parse");

        // Extract channel_id from the event payload.
        let channel_id: u64 = frame["d"]["id"]
            .as_str()
            .expect("id")
            .parse()
            .expect("parse id");

        let event = super::map_channel_delete_to_binding_revoked(channel_id);

        match event {
            ChannelEvent::BindingRevoked { binding, reason } => {
                assert_eq!(binding.kind(), "discord");
                assert_eq!(reason, crate::channel::event::RevokeReason::Deleted);
            }
            other => panic!("expected BindingRevoked, got: {other:?}"),
        }
    }

    /// CHANNEL_DELETE delivered via poll_event: gateway pushes event,
    /// poll_event drains it as BindingRevoked.
    #[test]
    fn discord_channel_delete_via_poll_event() {
        use crate::channel::Channel;

        let (ch, tx) = super::DiscordChannel::new_for_test();
        let event = super::map_channel_delete_to_binding_revoked(290926798999357250);
        tx.send(event).expect("send");

        let polled = ch.poll_event().expect("should have event");
        assert!(
            matches!(polled, ChannelEvent::BindingRevoked { .. }),
            "expected BindingRevoked, got: {polled:?}"
        );
    }

    /// §3.5.10 persistence-replay: binding registry round-trip.
    /// Write state → serialize → deserialize → verify bindings intact.
    #[test]
    fn discord_binding_registry_persistence_round_trip() {
        use crate::channel::Channel;

        let (ch, _tx) = super::DiscordChannel::new_for_test();

        // Record two bindings.
        ch.record_binding("agent-a", test_binding(111), "\r".into());
        ch.record_binding("agent-b", test_binding(222), "\r".into());

        // Serialize the binding registry to JSON (simulating disk write).
        let snapshot: std::collections::HashMap<String, u64> = {
            let s = ch.state.lock();
            s.instance_to_channel.clone()
        };
        let json = serde_json::to_string(&snapshot).expect("serialize");

        // Simulate restart: deserialize and verify.
        let restored: std::collections::HashMap<String, u64> =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.len(), 2);
        assert_eq!(restored["agent-a"], 111);
        assert_eq!(restored["agent-b"], 222);

        // Verify the live channel still has correct bindings.
        assert!(ch.has_binding("agent-a"));
        assert!(ch.has_binding("agent-b"));
        assert!(!ch.has_binding("agent-c"));

        // Take and verify round-trip.
        let taken = ch.take_binding("agent-a").expect("take");
        assert_eq!(taken.kind(), "discord");
        assert!(!ch.has_binding("agent-a"));
    }

    // ── F1 fix: auto-archive keepalive test ──────────────────────────

    /// §3.5.10 production-path-coupled: keepalive PATCH via
    /// send_keepalive_patch() against mock server.
    #[test]
    fn discord_keepalive_patch_method_matches_spec() {
        let (port, handle, captured) = mock_http_server(200, "{}");
        let client = twilight_http::Client::builder()
            .proxy(format!("127.0.0.1:{port}"), true)
            .build();

        let result = super::send_keepalive_patch(&client, 290926798999357250);

        handle.join().expect("mock server");
        assert!(result.is_ok(), "keepalive must succeed: {:?}", result.err());

        let req = captured.lock().expect("lock").take().expect("captured");
        assert_eq!(req.method, "PATCH", "keepalive must use PATCH");
        assert!(
            req.path.contains("/channels/290926798999357250"),
            "path must target channel: {}",
            req.path
        );
        // Body must set archived=false per Discord thread update spec.
        let body: serde_json::Value = serde_json::from_str(&req.body).expect("body must be JSON");
        assert_eq!(body["archived"], false, "must set archived=false");
    }

    /// Keepalive interval constant is reasonable (≤ Discord's shortest
    /// auto-archive of 3600s).
    #[test]
    fn discord_keepalive_interval_within_auto_archive_window() {
        assert!(
            super::KEEPALIVE_INTERVAL_SECS < 3600,
            "keepalive must fire before shortest auto-archive (1h)"
        );
        assert!(
            super::KEEPALIVE_INTERVAL_SECS >= 60,
            "keepalive should not be too aggressive"
        );
    }
}
