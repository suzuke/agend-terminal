use super::*;
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
// Channel
// ---------------------------------------------------------------------------

/// Discord adapter implementing the `Channel` trait.
pub struct DiscordChannel {
    pub(super) state: Mutex<DiscordState>,
    caps: ChannelCapabilities,
    /// Receiver end of the unbounded event channel (`std::sync::mpsc::
    /// channel`, not `sync_channel`). The gateway reader task pushes
    /// `ChannelEvent`s here; `poll_event` drains them.
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

    /// Resolve which fleet instance owns `channel_id`, via the same
    /// `channel_to_instance` reverse lookup Telegram's `resolve_topic` uses
    /// for `topic_id` (#2562 PR-1). Miss (channel_id not bound to any
    /// instance) falls back to `"general"` + a warn log — mirrors
    /// `telegram/inbound.rs`'s topic-miss fallback semantics.
    pub(crate) fn resolve_instance_for_channel(&self, channel_id: u64) -> String {
        let instance = self
            .state
            .lock()
            .channel_to_instance
            .get(&channel_id)
            .cloned();
        instance.unwrap_or_else(|| {
            tracing::warn!(
                channel_id,
                "discord inbound: no instance bound to this channel, falling back to 'general'"
            );
            "general".to_string()
        })
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
        react: false, // M3: not implemented yet (returns NotSupported)
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
// Channel trait impl
// ---------------------------------------------------------------------------

/// Shared tokio runtime for Discord sync→async calls.
pub(super) fn discord_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| crate::shared_async::build_current_thread_runtime("discord tokio runtime"))
}

/// #1476: run a Discord async call to completion, safe even when already inside
/// a tokio runtime.
///
/// #1642: delegates to the shared [`crate::channel::shared_async::block_on_value`]
/// helper (deduped from the byte-identical telegram/discord copies — discord had
/// inherited telegram's nested-runtime panic AND its fix). See that helper for
/// the `current_thread` nested-runtime guard rationale.
pub(super) fn block_on_value<F>(fut: F) -> F::Output
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    crate::channel::shared_async::block_on_value(discord_runtime(), "discord", fut)
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
        block_on_value(async {
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
        block_on_value(async {
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
        block_on_value(async {
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
        block_on_value(async {
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
        block_on_value(async {
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
        // t-20260703164240502572-50899-11 (reviewer4 REJECTED finding): unlike
        // Telegram (whose create_topic persists to topics.json, a durable
        // self-healing source `resolve_topic` falls back to), Discord's
        // has_binding/take_binding read ONLY the in-memory `instance_to_channel`
        // map — populated ONLY by `record_binding`. Without this call the
        // channel this just created on the Discord API is unroutable (inbound
        // can't resolve it) and unreachable for cleanup (`take_binding` finds
        // nothing). Mirrors `app/discord_hooks.rs::maybe_create_discord_binding`'s
        // create_binding + record_binding pairing; submit_key fallback matches
        // that hook's own fallback (no registry/pane context at this layer to
        // look up the real one).
        self.record_binding(name, binding, "\r".to_string());
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
            crate::channel::AgentOutboundOp::Reply { text, .. } => {
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
