//! Claude Code ChannelBridge transport.
//!
//! Claude Channels are an MCP server-side notification capability.  The
//! bridge owns a loopback HTTP listener, forwards authenticated webhook
//! envelopes to Claude over MCP stdio, and records structured `reply` tool
//! receipts in a small durable event log.  It never falls back to PTY.

use super::{
    safe_component, AgentDeliveryTransport, BackendEvent, DeliveryEnvelope, DeliveryReceipt,
    DeliveryState, ReceiptStore, SessionLocator, TransportCapability, TransportMode,
};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use uuid::Uuid;

mod replay;

pub(crate) const CHANNEL_SERVER_NAME: &str = "agend-claude-channel";
const MIN_CLAUDE_VERSION: (u64, u64, u64) = (2, 1, 80);
const BRIDGE_VERSION: &str = "0.1.0";
const MAX_HEADERS: usize = 64 * 1024;
const MAX_BODY: usize = 1024 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const CHANNEL_READY_POLL_INTERVAL: Duration = Duration::from_millis(100);
const SSE_READ_TIMEOUT: Duration = Duration::from_secs(20);
/// A fresh-restart self-kick must prove consumer admission promptly. This is
/// deliberately a bounded ambiguity window, not a retry timer: accepted
/// without an exact consumer ack is never resent automatically.
const SELF_KICK_ACK_WINDOW: Duration = Duration::from_secs(30);
const HTTP_PATH: &str = "/webhook";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint {
    port: u16,
}

impl Endpoint {
    fn parse(locator: &SessionLocator) -> anyhow::Result<Self> {
        let raw = locator
            .endpoint_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Claude ChannelBridge endpoint is missing"))?;
        let rest = raw.strip_prefix("http://").ok_or_else(|| {
            anyhow::anyhow!("Claude ChannelBridge endpoint must use http:// loopback")
        })?;
        let (host, port) = rest
            .rsplit_once(':')
            .ok_or_else(|| anyhow::anyhow!("Claude ChannelBridge endpoint must include a port"))?;
        if host != "127.0.0.1" || port.contains('/') || port.contains('?') || port.contains('#') {
            anyhow::bail!("Claude ChannelBridge endpoint is not loopback");
        }
        let port = port.parse::<u16>()?;
        if port == 0 {
            anyhow::bail!("Claude ChannelBridge endpoint port must be non-zero");
        }
        Ok(Self { port })
    }
}

fn endpoint_address(locator: &SessionLocator) -> anyhow::Result<String> {
    Ok(format!("127.0.0.1:{}", Endpoint::parse(locator)?.port))
}

fn token(locator: &SessionLocator) -> anyhow::Result<&str> {
    locator
        .password
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Claude ChannelBridge bearer token is missing"))
}

fn channel_state_dir(home: &Path, instance: &str) -> PathBuf {
    home.join("transport")
        .join("claude-channel")
        .join(safe_component(instance))
}

fn channel_events_path(home: &Path, instance: &str) -> PathBuf {
    channel_state_dir(home, instance).join("events.jsonl")
}

fn channel_lock_path(home: &Path, instance: &str) -> PathBuf {
    channel_events_path(home, instance).with_extension("jsonl.lock")
}

fn restrict_private(path: &Path, mode: u32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(mode);
        std::fs::set_permissions(path, permissions)?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
enum ChannelLogRecord {
    Inbound {
        delivery_id: Uuid,
        chat_id: String,
        sender_id: Option<String>,
        content: String,
        recorded_at: String,
    },
    InboundPrepared {
        delivery_id: Uuid,
        chat_id: String,
        sender_id: Option<String>,
        content: String,
        recorded_at: String,
    },
    InboundAccepted {
        delivery_id: Uuid,
        chat_id: String,
        sender_id: Option<String>,
        content: String,
        recorded_at: String,
    },
    InboundRejected {
        delivery_id: Uuid,
        recorded_at: String,
    },
    Reply {
        delivery_id: Uuid,
        chat_id: String,
        text: String,
        reply_id: String,
        recorded_at: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ReplyEvent {
    delivery_id: Uuid,
    chat_id: String,
    text: String,
    reply_id: String,
    recorded_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InboundIdentity {
    chat_id: String,
    sender_id: String,
    content: String,
}

#[derive(Default)]
struct InboundIndex {
    by_chat: HashMap<String, Uuid>,
    by_delivery: HashMap<Uuid, InboundIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotificationAdmission {
    Accepted,
    Duplicate,
    Conflict,
    Unavailable,
}

fn append_log(home: &Path, instance: &str, record: &ChannelLogRecord) -> anyhow::Result<()> {
    let dir = channel_state_dir(home, instance);
    std::fs::create_dir_all(&dir)?;
    restrict_private(&dir, 0o700)?;
    let _lock = crate::store::acquire_file_lock(&channel_lock_path(home, instance))?;
    let path = channel_events_path(home, instance);
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    restrict_private(&path, 0o600)?;
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    file.write_all(&line)?;
    file.sync_data()?;
    Ok(())
}

fn load_log(home: &Path, instance: &str) -> anyhow::Result<Vec<ChannelLogRecord>> {
    let path = channel_events_path(home, instance);
    if !path.exists() {
        return Ok(Vec::new());
    }
    restrict_private(&path, 0o600)?;
    let file = File::open(path)?;
    BufReader::new(file)
        .lines()
        .map(|line| Ok(serde_json::from_str(&line?)?))
        .collect()
}

struct ChannelRuntime {
    home: PathBuf,
    instance: String,
    session_id: String,
    token: String,
    ready: AtomicBool,
    client_version: Mutex<Option<String>>,
    inbound: Mutex<InboundIndex>,
    replies: Mutex<HashMap<Uuid, ReplyEvent>>,
    subscribers: Mutex<Vec<Sender<ReplyEvent>>>,
    mcp_sender: Mutex<Option<SyncSender<Value>>>,
}

impl ChannelRuntime {
    fn new(home: &Path, instance: &str, locator: &SessionLocator) -> anyhow::Result<Self> {
        let mut inbound = InboundIndex::default();
        let mut prepared = HashMap::new();
        let mut replies = HashMap::new();
        for record in load_log(home, instance)? {
            match record {
                ChannelLogRecord::Inbound {
                    delivery_id,
                    chat_id,
                    sender_id,
                    content,
                    ..
                } => {
                    // Legacy `Inbound` was persisted before the notification
                    // entered the bridge queue, so it has the same ambiguous
                    // replay contract as an unresolved `InboundPrepared`.
                    prepared.insert(
                        delivery_id,
                        InboundIdentity {
                            chat_id,
                            sender_id: sender_id.unwrap_or_else(|| "agend-terminal".to_string()),
                            content,
                        },
                    );
                }
                ChannelLogRecord::InboundAccepted {
                    delivery_id,
                    chat_id,
                    sender_id,
                    content,
                    ..
                } => {
                    prepared.remove(&delivery_id);
                    inbound.by_chat.insert(chat_id.clone(), delivery_id);
                    inbound.by_delivery.insert(
                        delivery_id,
                        InboundIdentity {
                            chat_id,
                            sender_id: sender_id.unwrap_or_else(|| "agend-terminal".to_string()),
                            content,
                        },
                    );
                }
                ChannelLogRecord::InboundPrepared {
                    delivery_id,
                    chat_id,
                    sender_id,
                    content,
                    ..
                } => {
                    prepared.insert(
                        delivery_id,
                        InboundIdentity {
                            chat_id,
                            sender_id: sender_id.unwrap_or_else(|| "agend-terminal".to_string()),
                            content,
                        },
                    );
                }
                ChannelLogRecord::InboundRejected { delivery_id, .. } => {
                    prepared.remove(&delivery_id);
                }
                ChannelLogRecord::Reply {
                    delivery_id,
                    chat_id,
                    text,
                    reply_id,
                    recorded_at,
                } => {
                    replies.insert(
                        delivery_id,
                        ReplyEvent {
                            delivery_id,
                            chat_id,
                            text,
                            reply_id,
                            recorded_at,
                        },
                    );
                }
            }
        }
        replay::restore_prepared(home, instance, &mut inbound, prepared)?;
        Ok(Self {
            home: home.to_path_buf(),
            instance: instance.to_string(),
            session_id: locator
                .session_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Claude ChannelBridge session ID is missing"))?,
            token: token(locator)?.to_string(),
            ready: AtomicBool::new(false),
            client_version: Mutex::new(None),
            inbound: Mutex::new(inbound),
            replies: Mutex::new(replies),
            subscribers: Mutex::new(Vec::new()),
            mcp_sender: Mutex::new(None),
        })
    }

    fn set_sender(&self, sender: SyncSender<Value>) {
        *self.mcp_sender.lock() = Some(sender);
    }

    fn clear_sender(&self) {
        let mut client_version = self.client_version.lock();
        *self.mcp_sender.lock() = None;
        *client_version = None;
        self.ready.store(false, Ordering::Release);
    }

    fn set_client_version(&self, version: String) {
        *self.client_version.lock() = Some(version);
        self.ready.store(false, Ordering::Release);
    }

    fn mark_initialized(&self) {
        let client_version = self.client_version.lock();
        if client_version.is_some() && self.mcp_sender.lock().is_some() {
            self.ready.store(true, Ordering::Release);
        }
    }

    fn mark_unready(&self) {
        let mut client_version = self.client_version.lock();
        *client_version = None;
        self.ready.store(false, Ordering::Release);
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    fn client_version(&self) -> Option<String> {
        self.client_version.lock().clone()
    }

    fn health_json(&self) -> Value {
        json!({
            "ready": self.is_ready(),
            "backend": "claude",
            "mode": "channel_bridge",
            "backend_version": self.client_version(),
            "session_id": self.session_id,
            "capabilities": {
                "claude/channel": self.is_ready(),
                "tools": self.is_ready(),
            }
        })
    }

    #[cfg(test)]
    fn remember_inbound(
        &self,
        delivery_id: Uuid,
        chat_id: &str,
        sender_id: Option<&str>,
        content: &str,
    ) -> anyhow::Result<()> {
        append_log(
            &self.home,
            &self.instance,
            &ChannelLogRecord::Inbound {
                delivery_id,
                chat_id: chat_id.to_string(),
                sender_id: sender_id.map(str::to_string),
                content: content.to_string(),
                recorded_at: Utc::now().to_rfc3339(),
            },
        )?;
        let identity = InboundIdentity {
            chat_id: chat_id.to_string(),
            sender_id: sender_id.unwrap_or("agend-terminal").to_string(),
            content: content.to_string(),
        };
        let mut inbound = self.inbound.lock();
        inbound.by_chat.insert(chat_id.to_string(), delivery_id);
        inbound.by_delivery.insert(delivery_id, identity);
        Ok(())
    }

    fn delivery_for_chat(&self, chat_id: &str) -> Option<Uuid> {
        self.inbound.lock().by_chat.get(chat_id).copied()
    }

    fn inbound_contains_delivery(&self, delivery_id: Uuid) -> bool {
        self.inbound.lock().by_delivery.contains_key(&delivery_id)
    }

    fn self_kick_receipt(
        &self,
        delivery_id: Uuid,
    ) -> anyhow::Result<(ReceiptStore, DeliveryEnvelope, DeliveryReceipt)> {
        if !self.inbound_contains_delivery(delivery_id) {
            anyhow::bail!("self-kick delivery_id is unknown or stale")
        }
        let store = ReceiptStore::for_instance(&self.home, &self.instance)?;
        let Some((envelope, receipt)) = store.delivery(delivery_id)? else {
            anyhow::bail!("self-kick delivery_id has no durable receipt")
        };
        if !envelope.self_kick {
            anyhow::bail!("delivery_id is not a fresh-restart self-kick")
        }
        if envelope.session.session_id.as_deref() != Some(self.session_id.as_str()) {
            anyhow::bail!("self-kick delivery belongs to a different Claude session")
        }
        Ok((store, envelope, receipt))
    }

    /// Record the consumer's exact-id TurnStarted acknowledgement. The
    /// compare-and-append receipt transition is intentionally separate from
    /// `remember_reply`: it emits no reply event and no SSE notification.
    fn acknowledge_self_kick(&self, delivery_id: Uuid) -> anyhow::Result<DeliveryReceipt> {
        let (store, envelope, mut current) = self.self_kick_receipt(delivery_id)?;
        if current.state == DeliveryState::TurnStarted {
            return Ok(current);
        }
        if current.state.is_terminal() {
            anyhow::bail!("self-kick delivery is no longer awaiting start acknowledgement")
        }
        if !matches!(
            current.state,
            DeliveryState::Queued | DeliveryState::ProtocolAccepted | DeliveryState::AckOverdue
        ) {
            anyhow::bail!("self-kick delivery is not awaiting start acknowledgement")
        }

        // The bridge can write the notification and receive the consumer's
        // ack before the daemon-side post-202 receipt append. Preserve the
        // truthful ProtocolAccepted state first when the durable row is still
        // Queued, then advance it to TurnStarted.
        if current.state == DeliveryState::Queued {
            let mut accepted = current.clone();
            accepted.state = DeliveryState::ProtocolAccepted;
            accepted.protocol_request_id = Some(delivery_id.to_string());
            accepted.backend_session_id = Some(self.session_id.clone());
            accepted.backend_event = Some("webhook_accepted".to_string());
            if store.record_if_latest_state(delivery_id, DeliveryState::Queued, accepted)? {
                current = store
                    .latest(delivery_id)?
                    .ok_or_else(|| anyhow::anyhow!("self-kick receipt disappeared after accept"))?;
            } else {
                current = store.latest(delivery_id)?.ok_or_else(|| {
                    anyhow::anyhow!("self-kick receipt disappeared during accept")
                })?;
            }
        }

        if current.state == DeliveryState::TurnStarted {
            return Ok(current);
        }
        if !matches!(
            current.state,
            DeliveryState::ProtocolAccepted | DeliveryState::AckOverdue
        ) {
            anyhow::bail!("self-kick delivery is not protocol-accepted")
        }
        let mut started = DeliveryReceipt::for_state(&envelope, DeliveryState::TurnStarted);
        started.protocol_request_id = current
            .protocol_request_id
            .or_else(|| Some(delivery_id.to_string()));
        started.backend_session_id = Some(self.session_id.clone());
        started.backend_event = Some("claude_channel_turn_started".to_string());
        started.detail = Some("consumer acknowledged exact self-kick delivery_id".to_string());
        let mut expected = current.state;
        loop {
            if store.record_if_latest_state(delivery_id, expected, started.clone())? {
                return Ok(started);
            }
            let latest = store
                .latest(delivery_id)?
                .ok_or_else(|| anyhow::anyhow!("self-kick receipt disappeared during start ack"))?;
            if latest.state == DeliveryState::TurnStarted {
                return Ok(latest);
            }
            if !matches!(
                latest.state,
                DeliveryState::ProtocolAccepted | DeliveryState::AckOverdue
            ) {
                anyhow::bail!("self-kick start acknowledgement lost a receipt race")
            }
            expected = latest.state;
        }
    }

    /// Complete a self-kick only after the successor has finished its bounded
    /// recovery sequence. This is a local receipt transition: unlike `reply`,
    /// it has no outward channel or SSE effect.
    fn complete_self_kick(&self, delivery_id: Uuid) -> anyhow::Result<DeliveryReceipt> {
        let (store, envelope, current) = self.self_kick_receipt(delivery_id)?;
        if current.state == DeliveryState::Completed {
            return Ok(current);
        }
        if current.state != DeliveryState::TurnStarted {
            anyhow::bail!("self-kick completion requires a prior exact start acknowledgement")
        }
        let mut completed = DeliveryReceipt::for_state(&envelope, DeliveryState::Completed);
        completed.protocol_request_id = current.protocol_request_id;
        completed.backend_session_id = Some(self.session_id.clone());
        completed.backend_event = Some("claude_channel_self_kick_completed".to_string());
        completed.detail = Some("successor completed bounded restart recovery".to_string());
        if store.record_if_latest_state(
            delivery_id,
            DeliveryState::TurnStarted,
            completed.clone(),
        )? {
            Ok(completed)
        } else {
            let latest = store.latest(delivery_id)?.ok_or_else(|| {
                anyhow::anyhow!("self-kick receipt disappeared during completion")
            })?;
            if latest.state == DeliveryState::Completed {
                Ok(latest)
            } else {
                anyhow::bail!("self-kick completion lost a receipt race")
            }
        }
    }

    fn reject_self_kick_reply(&self, delivery_id: Uuid) -> anyhow::Result<()> {
        let store = ReceiptStore::for_instance(&self.home, &self.instance)?;
        if let Some((envelope, _)) = store.delivery(delivery_id)? {
            if envelope.self_kick {
                anyhow::bail!(
                    "fresh-restart self-kick has no outward reply; use ack_complete after recovery"
                )
            }
        }
        Ok(())
    }

    fn remember_reply(
        &self,
        delivery_id: Uuid,
        chat_id: &str,
        text: &str,
    ) -> anyhow::Result<ReplyEvent> {
        let event = ReplyEvent {
            delivery_id,
            chat_id: chat_id.to_string(),
            text: text.to_string(),
            reply_id: Uuid::new_v4().to_string(),
            recorded_at: Utc::now().to_rfc3339(),
        };
        append_log(
            &self.home,
            &self.instance,
            &ChannelLogRecord::Reply {
                delivery_id: event.delivery_id,
                chat_id: event.chat_id.clone(),
                text: event.text.clone(),
                reply_id: event.reply_id.clone(),
                recorded_at: event.recorded_at.clone(),
            },
        )?;
        self.replies.lock().insert(delivery_id, event.clone());
        self.subscribers
            .lock()
            .retain(|subscriber| subscriber.send(event.clone()).is_ok());
        Ok(event)
    }

    fn reply_for(&self, delivery_id: Uuid) -> Option<ReplyEvent> {
        self.replies.lock().get(&delivery_id).cloned()
    }

    fn subscribe(&self, after_reply_id: Option<&str>) -> (Receiver<ReplyEvent>, Vec<ReplyEvent>) {
        let (sender, receiver) = mpsc::channel();
        let replies = self.replies.lock();
        let mut replay: Vec<_> = replies.values().cloned().collect();
        replay.sort_by(|left, right| {
            left.recorded_at
                .cmp(&right.recorded_at)
                .then_with(|| left.reply_id.cmp(&right.reply_id))
        });
        if let Some(after_reply_id) = after_reply_id {
            if let Some(position) = replay
                .iter()
                .position(|event| event.reply_id == after_reply_id)
            {
                replay.drain(..=position);
            }
        }
        // Keep the reply and subscriber locks in the same order as
        // remember_reply so a reply cannot land between replay and subscribe.
        self.subscribers.lock().push(sender);
        (receiver, replay)
    }

    fn admit_channel_notification(
        &self,
        delivery_id: Uuid,
        chat_id: &str,
        sender_id: Option<&str>,
        content: &str,
    ) -> anyhow::Result<NotificationAdmission> {
        let identity = InboundIdentity {
            chat_id: chat_id.to_string(),
            sender_id: sender_id.unwrap_or("agend-terminal").to_string(),
            content: content.to_string(),
        };
        let mut inbound = self.inbound.lock();
        if let Some(existing) = inbound.by_delivery.get(&delivery_id) {
            return Ok(if existing == &identity {
                NotificationAdmission::Duplicate
            } else {
                NotificationAdmission::Conflict
            });
        }

        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/claude/channel",
            "params": {
                "content": content,
                "meta": {
                    "delivery_id": delivery_id.to_string(),
                    "chat_id": chat_id,
                    "sender_id": sender_id.unwrap_or("agend-terminal")
                }
            }
        });
        let sender = self.mcp_sender.lock().clone();
        let Some(sender) = sender else {
            return Ok(NotificationAdmission::Unavailable);
        };

        append_log(
            &self.home,
            &self.instance,
            &ChannelLogRecord::InboundPrepared {
                delivery_id,
                chat_id: identity.chat_id.clone(),
                sender_id: sender_id.map(str::to_string),
                content: identity.content.clone(),
                recorded_at: Utc::now().to_rfc3339(),
            },
        )?;
        inbound
            .by_chat
            .insert(identity.chat_id.clone(), delivery_id);
        inbound.by_delivery.insert(delivery_id, identity.clone());

        if matches!(
            sender.try_send(notification),
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_))
        ) {
            append_log(
                &self.home,
                &self.instance,
                &ChannelLogRecord::InboundRejected {
                    delivery_id,
                    recorded_at: Utc::now().to_rfc3339(),
                },
            )?;
            inbound.by_delivery.remove(&delivery_id);
            if inbound.by_chat.get(chat_id) == Some(&delivery_id) {
                inbound.by_chat.remove(chat_id);
            }
            return Ok(NotificationAdmission::Unavailable);
        }

        append_log(
            &self.home,
            &self.instance,
            &ChannelLogRecord::InboundAccepted {
                delivery_id,
                chat_id: identity.chat_id,
                sender_id: sender_id.map(str::to_string),
                content: identity.content,
                recorded_at: Utc::now().to_rfc3339(),
            },
        )?;
        Ok(NotificationAdmission::Accepted)
    }
}

fn parse_version(value: &str) -> Option<(u64, u64, u64)> {
    let mut numbers = value
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .take(3)
        .map(|part| part.parse::<u64>().ok());
    Some((
        numbers.next()??,
        numbers.next()??,
        numbers.next().unwrap_or(Some(0))?,
    ))
}

fn supported_claude_version(value: &str) -> bool {
    parse_version(value).is_some_and(|version| version >= MIN_CLAUDE_VERSION)
}

fn json_rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

fn mcp_initialize(message: &Value, runtime: &ChannelRuntime) -> Value {
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let version = message
        .pointer("/params/clientInfo/version")
        .and_then(Value::as_str);
    let Some(version) = version else {
        runtime.mark_unready();
        return json_rpc_error(id, -32001, "Claude Code client version is required");
    };
    if !supported_claude_version(version) {
        runtime.mark_unready();
        return json_rpc_error(
            id,
            -32001,
            "Claude Code Channels require client version 2.1.80 or newer",
        );
    }
    runtime.set_client_version(version.to_string());
    json!({
        "jsonrpc":"2.0",
        "id": id,
        "result": {
            "protocolVersion": message.pointer("/params/protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18"),
            "capabilities": {
                "experimental": {"claude/channel": {}},
                "tools": {}
            },
            "serverInfo": {"name": CHANNEL_SERVER_NAME, "version": BRIDGE_VERSION},
            "instructions": "Messages arrive as <channel source=\"agend-terminal\" chat_id=\"...\" delivery_id=\"...\">. Each delivery has a unique chat_id; reply with the reply tool using the same chat_id and delivery_id. For [AGEND-RESUME], immediately call ack_start with the exact delivery_id from channel metadata before recovery, then call ack_complete only after the bounded recovery sequence; self-kick acknowledgements never send an outward reply."
        }
    })
}

fn reply_tool_definition() -> Value {
    json!({
        "name": "reply",
        "description": "Send a structured reply back over the AgEnD Claude channel",
        "inputSchema": {
            "type": "object",
            "properties": {
                "chat_id": {"type":"string", "description":"The conversation to reply in"},
                "delivery_id": {"type":"string", "description":"The inbound delivery UUID"},
                "text": {"type":"string", "description":"The reply text"}
            },
            "required": ["chat_id", "text"]
        }
    })
}

fn self_kick_ack_tool_definition(name: &str, description: &str) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": {
                "delivery_id": {"type":"string", "description":"Exact UUID from the [AGEND-RESUME] channel metadata"}
            },
            "required": ["delivery_id"]
        }
    })
}

fn self_kick_ack_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id": id,
        "result": {
            "content": [{"type":"text","text": result["text"]}],
            "structuredContent": result
        }
    })
}

fn mcp_message(message: Value, runtime: &ChannelRuntime) -> Option<Value> {
    let method = message.get("method").and_then(Value::as_str)?;
    match method {
        "initialize" => Some(mcp_initialize(&message, runtime)),
        "notifications/initialized" => {
            runtime.mark_initialized();
            None
        }
        "ping" => Some(json!({
            "jsonrpc":"2.0",
            "id": message.get("id").cloned().unwrap_or(Value::Null),
            "result": {}
        })),
        "tools/list" => Some(json!({
            "jsonrpc":"2.0",
            "id": message.get("id").cloned().unwrap_or(Value::Null),
            "result": {"tools": [
                reply_tool_definition(),
                self_kick_ack_tool_definition(
                    "ack_start",
                    "Acknowledge exact [AGEND-RESUME] consumer admission before any recovery action. This has no outward reply or SSE side effect."
                ),
                self_kick_ack_tool_definition(
                    "ack_complete",
                    "Mark exact [AGEND-RESUME] recovery complete after the bounded task-board/inbox/list-instances recovery sequence. This has no outward reply or SSE side effect."
                )
            ]}
        })),
        "tools/call" => {
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            let name = message.pointer("/params/name").and_then(Value::as_str);
            let arguments = message
                .pointer("/params/arguments")
                .and_then(Value::as_object);
            let Some(arguments) = arguments else {
                return Some(json_rpc_error(
                    id,
                    -32602,
                    "reply arguments must be an object",
                ));
            };
            if matches!(name, Some("ack_start" | "ack_complete")) {
                let Some(delivery_id) = arguments.get("delivery_id").and_then(Value::as_str) else {
                    return Some(json_rpc_error(id, -32602, "delivery_id is required"));
                };
                let Ok(delivery_id) = Uuid::parse_str(delivery_id) else {
                    return Some(json_rpc_error(id, -32602, "delivery_id is invalid"));
                };
                let result = if name == Some("ack_start") {
                    runtime.acknowledge_self_kick(delivery_id)
                } else {
                    runtime.complete_self_kick(delivery_id)
                };
                return Some(match result {
                    Ok(receipt) => self_kick_ack_response(
                        id,
                        json!({
                            "text": if name == Some("ack_start") { "turn_started" } else { "completed" },
                            "delivery_id": receipt.delivery_id,
                            "state": receipt.state,
                        }),
                    ),
                    Err(error) => json_rpc_error(id, -32602, &error.to_string()),
                });
            }
            if name != Some("reply") {
                return Some(json_rpc_error(
                    id,
                    -32601,
                    "unknown Claude ChannelBridge tool",
                ));
            }
            let Some(chat_id) = arguments.get("chat_id").and_then(Value::as_str) else {
                return Some(json_rpc_error(id, -32602, "reply.chat_id is required"));
            };
            let Some(text) = arguments.get("text").and_then(Value::as_str) else {
                return Some(json_rpc_error(id, -32602, "reply.text is required"));
            };
            let mapped = runtime.delivery_for_chat(chat_id);
            let delivery_id = match arguments.get("delivery_id").and_then(Value::as_str) {
                Some(value) => match Uuid::parse_str(value) {
                    Ok(value) if mapped == Some(value) => value,
                    Ok(_) => {
                        return Some(json_rpc_error(
                            id,
                            -32602,
                            "reply.delivery_id does not match chat_id",
                        ));
                    }
                    Err(_) => {
                        return Some(json_rpc_error(id, -32602, "reply.delivery_id is invalid"))
                    }
                },
                None => match mapped {
                    Some(value) => value,
                    None => return Some(json_rpc_error(id, -32602, "reply.chat_id is unknown")),
                },
            };
            if let Err(error) = runtime.reject_self_kick_reply(delivery_id) {
                return Some(json_rpc_error(id, -32602, &error.to_string()));
            }
            match runtime.remember_reply(delivery_id, chat_id, text) {
                Ok(event) => Some(json!({
                    "jsonrpc":"2.0",
                    "id": id,
                    "result": {
                        "content": [{"type":"text","text":"sent"}],
                        "structuredContent": {"delivery_id": event.delivery_id, "reply_id": event.reply_id}
                    }
                })),
                Err(error) => Some(json_rpc_error(
                    id,
                    -32002,
                    &format!("reply receipt failed: {error}"),
                )),
            }
        }
        _ if message.get("id").is_some() => Some(json_rpc_error(
            message.get("id").cloned().unwrap_or(Value::Null),
            -32601,
            "unknown Claude ChannelBridge method",
        )),
        _ => None,
    }
}

fn read_mcp_message<R: Read>(reader: &mut BufReader<R>) -> anyhow::Result<Option<Value>> {
    let mut first = String::new();
    loop {
        if reader.read_line(&mut first)? == 0 {
            return Ok(None);
        }
        if !first.trim().is_empty() {
            break;
        }
        first.clear();
    }
    if first.len() > MAX_BODY {
        anyhow::bail!("MCP message exceeds the size limit");
    }
    if !first.trim_start().starts_with('{') {
        anyhow::bail!("MCP stdio requires one JSON object per line");
    }
    Ok(Some(serde_json::from_str(first.trim())?))
}

fn write_mcp_message<W: Write>(writer: &mut W, message: &Value) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *writer, message)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_http_request(stream: &mut TcpStream) -> anyhow::Result<HttpRequest> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            anyhow::bail!("HTTP request closed before headers");
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > MAX_HEADERS {
            anyhow::bail!("HTTP headers exceed the size limit");
        }
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
    };
    let header_text = std::str::from_utf8(&bytes[..header_end])?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("HTTP request line is missing"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("HTTP method is missing"))?
        .to_string();
    let path = request_parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("HTTP path is missing"))?
        .to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length = headers
        .get("content-length")
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(0);
    if length > MAX_BODY {
        anyhow::bail!("HTTP body exceeds the size limit");
    }
    let body_start = header_end + 4;
    let mut body = bytes[body_start..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            anyhow::bail!("HTTP body ended early");
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn bearer_ok(request: &HttpRequest, expected: &str) -> bool {
    request
        .headers
        .get("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|value| value == expected)
}

fn json_response(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"error\":\"serialization failure\"}".to_vec())
}

fn handle_http(mut stream: TcpStream, runtime: Arc<ChannelRuntime>) -> anyhow::Result<()> {
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
    let request = read_http_request(&mut stream)?;
    if !bearer_ok(&request, &runtime.token) {
        write_http_response(
            &mut stream,
            401,
            "application/json",
            br#"{"error":"unauthorized"}"#,
        )?;
        return Ok(());
    }
    if request.method == "GET" && request.path == "/health" {
        write_http_response(
            &mut stream,
            200,
            "application/json",
            &json_response(runtime.health_json()),
        )?;
        return Ok(());
    }
    if request.method == "GET" && request.path == "/events" {
        let cursor = request.headers.get("last-event-id").map(String::as_str);
        let (receiver, replay) = runtime.subscribe(cursor);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n: connected\n\n"
        )?;
        for event in replay {
            write_sse_event(&mut stream, &event)?;
        }
        stream.flush()?;
        loop {
            match receiver.recv_timeout(Duration::from_secs(15)) {
                Ok(event) => {
                    write_sse_event(&mut stream, &event)?;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    stream.write_all(b": keepalive\n\n")?;
                    stream.flush()?;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        return Ok(());
    }
    if request.method == "GET" && request.path.starts_with("/receipts/") {
        let id = request
            .path
            .trim_start_matches("/receipts/")
            .parse::<Uuid>();
        let Ok(id) = id else {
            write_http_response(
                &mut stream,
                400,
                "application/json",
                br#"{"error":"invalid delivery_id"}"#,
            )?;
            return Ok(());
        };
        let reply = runtime.reply_for(id);
        let receipt = ReceiptStore::for_instance(&runtime.home, &runtime.instance)
            .ok()
            .and_then(|store| store.latest(id).ok().flatten());
        if reply.is_some() || receipt.is_some() {
            write_http_response(
                &mut stream,
                200,
                "application/json",
                &json_response(
                    json!({"reply":reply,"receipt":receipt,"state":receipt.as_ref().map(|r| r.state)}),
                ),
            )?;
        } else {
            write_http_response(&mut stream, 404, "application/json", br#"{"reply":null}"#)?;
        }
        return Ok(());
    }
    if request.method == "POST" && (request.path == HTTP_PATH || request.path == "/") {
        if !runtime.is_ready() {
            write_http_response(
                &mut stream,
                503,
                "application/json",
                br#"{"error":"channel_not_ready"}"#,
            )?;
            return Ok(());
        }
        let payload: Value = match serde_json::from_slice(&request.body) {
            Ok(payload) => payload,
            Err(_) => {
                write_http_response(
                    &mut stream,
                    400,
                    "application/json",
                    br#"{"error":"webhook body must be JSON"}"#,
                )?;
                return Ok(());
            }
        };
        let Some(delivery_id) = payload
            .get("delivery_id")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<Uuid>().ok())
        else {
            write_http_response(
                &mut stream,
                400,
                "application/json",
                br#"{"error":"webhook delivery_id is required and must be a UUID"}"#,
            )?;
            return Ok(());
        };
        let Some(chat_id) = payload
            .get("chat_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            write_http_response(
                &mut stream,
                400,
                "application/json",
                br#"{"error":"webhook chat_id is required"}"#,
            )?;
            return Ok(());
        };
        let Some(content) = payload
            .get("text")
            .or_else(|| payload.get("content"))
            .and_then(Value::as_str)
        else {
            write_http_response(
                &mut stream,
                400,
                "application/json",
                br#"{"error":"webhook text is required"}"#,
            )?;
            return Ok(());
        };
        let sender_id = payload.get("sender_id").and_then(Value::as_str);
        match runtime.admit_channel_notification(delivery_id, chat_id, sender_id, content)? {
            NotificationAdmission::Accepted | NotificationAdmission::Duplicate => {}
            NotificationAdmission::Conflict => {
                write_http_response(
                    &mut stream,
                    409,
                    "application/json",
                    br#"{"error":"delivery_id_payload_conflict"}"#,
                )?;
                return Ok(());
            }
            NotificationAdmission::Unavailable => {
                write_http_response(
                    &mut stream,
                    503,
                    "application/json",
                    br#"{"error":"channel_queue_unavailable"}"#,
                )?;
                return Ok(());
            }
        }
        write_http_response(
            &mut stream,
            202,
            "application/json",
            &json_response(json!({"accepted":true,"delivery_id":delivery_id,"chat_id":chat_id})),
        )?;
        return Ok(());
    }
    write_http_response(
        &mut stream,
        404,
        "application/json",
        br#"{"error":"not_found"}"#,
    )?;
    Ok(())
}

fn write_sse_event(stream: &mut TcpStream, event: &ReplyEvent) -> anyhow::Result<()> {
    write!(
        stream,
        "id: {}\nevent: reply\ndata: {}\n\n",
        event.reply_id,
        serde_json::to_string(event)?
    )?;
    stream.flush()?;
    Ok(())
}

fn run_http_listener(listener: TcpListener, runtime: Arc<ChannelRuntime>, stop: Arc<AtomicBool>) {
    let _ = listener.set_nonblocking(true);
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let runtime = Arc::clone(&runtime);
                // fire-and-forget: each HTTP/SSE connection owns its blocking socket until the client closes it; the listener remains available for webhooks.
                let _ = thread::Builder::new()
                    .name("claude-channel-http-client".to_string())
                    .spawn(move || {
                        let _ = handle_http(stream, runtime);
                    });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                tracing::error!(%error, "Claude ChannelBridge listener failed; terminating owner");
                // The PID/start-token is the credential gate. If the listener
                // cannot accept, terminate the owner so that gate cannot
                // bless a live process with no owned endpoint.
                std::process::exit(1);
            }
        }
    }
}

fn self_kick_ack_elapsed(recorded_at: &str, now: DateTime<Utc>) -> Option<Duration> {
    let recorded = DateTime::parse_from_rfc3339(recorded_at)
        .ok()?
        .with_timezone(&Utc);
    now.signed_duration_since(recorded).to_std().ok()
}

/// Reconcile accepted self-kicks from the durable receipt log. This scan is
/// intentionally separate from Claude hook observations: elapsed time only
/// creates a truthful nonterminal AckOverdue alert, never a TurnStarted proof
/// or retry.
fn self_kick_watchdog_pass_at(
    home: &Path,
    instance: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<usize> {
    let store = ReceiptStore::for_instance(home, instance)?;
    let mut alerts = 0;
    for (envelope, current) in store.pending_deliveries()? {
        if !envelope.self_kick
            || current.state != DeliveryState::ProtocolAccepted
            || self_kick_ack_elapsed(&current.recorded_at, now)
                .is_none_or(|elapsed| elapsed < SELF_KICK_ACK_WINDOW)
        {
            continue;
        }
        let mut overdue = current.clone();
        overdue.state = DeliveryState::AckOverdue;
        overdue.backend_event = Some("claude_channel_self_kick_ack_timeout".to_string());
        overdue.detail = Some(format!(
            "ProtocolAccepted but no exact consumer ack within {:?}; no automatic retry; inspect the agent and reconcile delivery {}",
            SELF_KICK_ACK_WINDOW, envelope.delivery_id
        ));
        if !store.record_if_latest_state(
            envelope.delivery_id,
            DeliveryState::ProtocolAccepted,
            overdue,
        )? {
            continue;
        }
        let alert = format!(
            "[claude-self-kick] {} accepted delivery {} but received no exact consumer ack within {:?}; state=AckOverdue; no automatic retry — inspect the agent and reconcile manually",
            instance, envelope.delivery_id, SELF_KICK_ACK_WINDOW
        );
        let channels = crate::channel::notify_all_escalation_channels(
            instance,
            crate::channel::NotifySeverity::Error,
            &alert,
            false,
        );
        tracing::warn!(agent = %instance, delivery_id = %envelope.delivery_id, "{alert}");
        crate::event_log::log(
            home,
            "claude_self_kick_ack_overdue",
            instance,
            &format!("{alert} notified_channels={channels}"),
        );
        alerts += 1;
    }
    Ok(alerts)
}

pub(crate) fn self_kick_watchdog_pass(home: &Path, instance: &str) -> anyhow::Result<usize> {
    self_kick_watchdog_pass_at(home, instance, Utc::now())
}

pub(crate) fn run_channel_server(home: &Path, instance: &str) -> anyhow::Result<()> {
    let (locator, listener) = bind_and_publish_channel(home, instance)?;
    let runtime = Arc::new(ChannelRuntime::new(home, instance, &locator)?);
    let stop = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::sync_channel::<Value>(64);
    runtime.set_sender(sender.clone());
    let http_stop = Arc::clone(&stop);
    let http_runtime = Arc::clone(&runtime);
    // fire-and-forget: retained JoinHandle joins the HTTP listener during bridge shutdown.
    let http_join = thread::Builder::new()
        .name("claude-channel-http".to_string())
        .spawn(move || run_http_listener(listener, http_runtime, http_stop))?;

    // fire-and-forget: retained JoinHandle joins the MCP writer during bridge shutdown.
    let writer_join = thread::Builder::new()
        .name("claude-channel-mcp-writer".to_string())
        .spawn(move || {
            let stdout = io::stdout();
            let mut writer = io::BufWriter::new(stdout.lock());
            while let Ok(message) = receiver.recv() {
                if write_mcp_message(&mut writer, &message).is_err() {
                    break;
                }
            }
        })?;

    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    while let Some(message) = read_mcp_message(&mut reader)? {
        if let Some(response) = mcp_message(message, &runtime) {
            if sender.send(response).is_err() {
                break;
            }
        }
    }
    stop.store(true, Ordering::Release);
    runtime.clear_sender();
    drop(sender);
    let _ = http_join.join();
    let _ = writer_join.join();
    Ok(())
}

fn random_token() -> anyhow::Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

pub(crate) fn legacy_pty_opt_in(home: &Path, instance: &str) -> bool {
    crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
        .ok()
        .and_then(|fleet| fleet.resolve_instance(instance))
        .and_then(|resolved| resolved.env.get("AGEND_TRANSPORT_MODE").cloned())
        .is_some_and(|mode| mode.eq_ignore_ascii_case("legacy_pty"))
}

pub(crate) fn prepare_claude_channel(
    home: &Path,
    instance: &str,
) -> anyhow::Result<SessionLocator> {
    let locator = super::registry::claude_attach_locator(home, instance)?.ok_or_else(|| {
        anyhow::anyhow!("Claude ChannelBridge locator has not been published by a live bridge")
    })?;
    ensure_claude_bridge_owned(&locator)?;
    Ok(locator)
}

pub(crate) fn wait_for_ready_claude_channel(
    home: &Path,
    instance: &str,
    timeout: Duration,
) -> anyhow::Result<SessionLocator> {
    wait_for_ready_claude_channel_until(home, instance, timeout)
}

fn wait_for_ready_claude_channel_until(
    home: &Path,
    instance: &str,
    timeout: Duration,
) -> anyhow::Result<SessionLocator> {
    let started = Instant::now();
    loop {
        let last_error = match prepare_claude_channel(home, instance).and_then(|locator| {
            let capability = health_probe(&locator)?;
            if capability.ready {
                Ok(locator)
            } else {
                Err(anyhow::anyhow!(
                    "{}",
                    capability.degraded_reason.unwrap_or_else(|| {
                        "Claude ChannelBridge capability probe failed closed".to_string()
                    })
                ))
            }
        }) {
            Ok(locator) => return Ok(locator),
            Err(error) => error,
        };
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            anyhow::bail!(
                "Claude ChannelBridge did not become ready within {timeout:?}: {last_error}"
            );
        }
        thread::sleep(std::cmp::min(CHANNEL_READY_POLL_INTERVAL, remaining));
    }
}

fn ensure_claude_bridge_owned(locator: &SessionLocator) -> anyhow::Result<()> {
    if locator.backend != "claude" {
        anyhow::bail!(
            "Claude ChannelBridge locator belongs to backend {}",
            locator.backend
        );
    }
    let (Some(pid), Some(start_token)) = (locator.server_pid, locator.server_start_token) else {
        anyhow::bail!("Claude ChannelBridge locator has no live bridge identity");
    };
    if crate::process::process_start_token(pid) != Some(start_token) {
        anyhow::bail!("Claude ChannelBridge locator is not owned by a live bridge");
    }
    Ok(())
}

fn bind_and_publish_channel(
    home: &Path,
    instance: &str,
) -> anyhow::Result<(SessionLocator, TcpListener)> {
    // Inspect the old artifact only to reject a foreign backend. The new
    // listener is always the source of truth; a rotated token/session makes
    // reusing a now-free numeric port safe, and avoids allocator-dependent
    // retry loops.
    let _ = super::registry::claude_attach_locator(home, instance)?;
    let pid = std::process::id();
    let start_token = crate::process::process_start_token(pid)
        .ok_or_else(|| anyhow::anyhow!("Claude ChannelBridge process identity is unavailable"))?;

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let mut locator = SessionLocator::claude(
        format!("http://127.0.0.1:{port}"),
        format!("claude-{}", Uuid::new_v4()),
        random_token()?,
    );
    locator.managed = true;
    locator.server_pid = Some(pid);
    locator.server_start_token = Some(start_token);
    // The listener remains open while secure_atomic_write atomically
    // publishes this exact endpoint, so a port squatter cannot win the
    // bind/persist gap that existed in the old probe-then-drop flow.
    super::registry::save_session_locator(home, instance, &locator)?;
    Ok((locator, listener))
}

pub(crate) fn channel_server_entry(home: &Path, instance: &str) -> anyhow::Result<Value> {
    let executable = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "agend-terminal".to_string());
    Ok(json!({
        "command": executable,
        "args": ["channel-bridge", "--instance", instance],
        "env": {
            "AGEND_HOME": home.display().to_string(),
            "AGEND_INSTANCE_NAME": instance
        }
    }))
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn client_request(
    locator: &SessionLocator,
    method: &str,
    path: &str,
    body: &[u8],
    accept: &str,
) -> anyhow::Result<HttpResponse> {
    ensure_claude_bridge_owned(locator)?;
    let address = endpoint_address(locator)?;
    let mut stream = TcpStream::connect_timeout(&address.parse()?, HTTP_TIMEOUT)?;
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
    let token = token(locator)?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nAccept: {accept}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            anyhow::bail!("Claude ChannelBridge closed before HTTP headers");
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > MAX_HEADERS {
            anyhow::bail!("Claude ChannelBridge headers exceed the size limit");
        }
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
    };
    let header_text = std::str::from_utf8(&bytes[..header_end])?;
    let mut lines = header_text.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| anyhow::anyhow!("Claude ChannelBridge status is invalid"))?
        .parse::<u16>()?;
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length = headers
        .get("content-length")
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(0);
    if length > MAX_BODY {
        anyhow::bail!("Claude ChannelBridge response exceeds the size limit");
    }
    let mut response_body = bytes[header_end + 4..].to_vec();
    while response_body.len() < length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            anyhow::bail!("Claude ChannelBridge response ended early");
        }
        response_body.extend_from_slice(&chunk[..read]);
    }
    response_body.truncate(length);
    Ok(HttpResponse {
        status,
        body: response_body,
    })
}

fn health_probe(locator: &SessionLocator) -> anyhow::Result<TransportCapability> {
    let response = client_request(locator, "GET", "/health", &[], "application/json")?;
    if response.status != 200 {
        anyhow::bail!(
            "Claude ChannelBridge health returned HTTP {}",
            response.status
        );
    }
    let value: Value = serde_json::from_slice(&response.body)?;
    let expected_session = locator
        .session_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("Claude ChannelBridge session identity is missing"))?;
    let observed_session = value.get("session_id").and_then(Value::as_str);
    if observed_session != Some(expected_session) {
        anyhow::bail!("Claude ChannelBridge session identity mismatch");
    }
    let version = value
        .get("backend_version")
        .and_then(Value::as_str)
        .map(str::to_string);
    let ready = value.get("ready").and_then(Value::as_bool) == Some(true)
        && value
            .pointer("/capabilities/claude~1channel")
            .and_then(Value::as_bool)
            == Some(true)
        && value
            .pointer("/capabilities/tools")
            .and_then(Value::as_bool)
            == Some(true)
        && version.as_deref().is_some_and(supported_claude_version);
    Ok(TransportCapability {
        backend: "claude".to_string(),
        mode: TransportMode::ChannelBridge,
        ready,
        backend_version: version,
        degraded_reason: if ready {
            None
        } else {
            Some("Claude ChannelBridge capability/version probe failed".to_string())
        },
    })
}

fn response_detail(response: &HttpResponse) -> String {
    serde_json::from_slice::<Value>(&response.body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("HTTP {}", response.status))
}

fn complete_receipt(home: &Path, instance: &str, event: &ReplyEvent) -> anyhow::Result<()> {
    let store = ReceiptStore::for_instance(home, instance)?;
    let Some(previous) = store.latest(event.delivery_id)? else {
        return Ok(());
    };
    if previous.state.is_terminal() {
        return Ok(());
    }
    let envelope = DeliveryEnvelope::new(
        instance,
        SessionLocator::claude(
            "http://127.0.0.1:1".to_string(),
            "reconcile".to_string(),
            "reconcile".to_string(),
        ),
        super::DeliveryKind::Notification,
        "reconcile",
        Some(event.chat_id.clone()),
    );
    let mut receipt = DeliveryReceipt::for_state(&envelope, DeliveryState::Completed);
    receipt.delivery_id = event.delivery_id;
    receipt.payload_digest = previous.payload_digest;
    receipt.protocol_request_id = previous.protocol_request_id;
    receipt.backend_event = Some("claude_reply".to_string());
    store.record(receipt)
}

struct SseStream {
    reader: BufReader<TcpStream>,
}

impl SseStream {
    fn connect(locator: &SessionLocator, last_event_id: Option<&str>) -> anyhow::Result<Self> {
        ensure_claude_bridge_owned(locator)?;
        let address = endpoint_address(locator)?;
        let mut stream = TcpStream::connect_timeout(&address.parse()?, HTTP_TIMEOUT)?;
        stream.set_read_timeout(Some(SSE_READ_TIMEOUT))?;
        stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
        let token = token(locator)?;
        let cursor = last_event_id
            .map(|value| format!("Last-Event-ID: {value}\r\n"))
            .unwrap_or_default();
        write!(
            stream,
            "GET /events HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nAccept: text/event-stream\r\nConnection: keep-alive\r\n{cursor}\r\n"
        )?;
        stream.flush()?;
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status)?;
        if !status.contains(" 200 ") {
            anyhow::bail!("Claude ChannelBridge SSE returned {status:?}");
        }
        loop {
            let mut header = String::new();
            reader.read_line(&mut header)?;
            if header == "\r\n" || header == "\n" {
                break;
            }
        }
        Ok(Self { reader })
    }

    fn next(&mut self) -> anyhow::Result<Option<ReplyEvent>> {
        let mut data = String::new();
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                return Ok(None);
            }
            if line == "\n" || line == "\r\n" {
                if data.is_empty() {
                    continue;
                }
                return Ok(Some(serde_json::from_str(data.trim())?));
            }
            if let Some(value) = line.strip_prefix("data:") {
                data.push_str(value.trim_start());
            }
        }
    }
}

struct EventWorker {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    locator: SessionLocator,
}

fn event_workers() -> &'static Mutex<HashMap<String, EventWorker>> {
    static WORKERS: OnceLock<Mutex<HashMap<String, EventWorker>>> = OnceLock::new();
    WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn worker_key(home: &Path, instance: &str) -> String {
    format!("{}\0{}", home.display(), instance)
}

fn ensure_event_worker(home: &Path, instance: &str, locator: &SessionLocator) {
    let key = worker_key(home, instance);
    let mut workers = event_workers().lock();
    if let Some(existing) = workers.get(&key) {
        if existing.locator == *locator {
            return;
        }
    }
    if let Some(mut existing) = workers.remove(&key) {
        existing.stop.store(true, Ordering::Release);
        if let Some(join) = existing.join.take() {
            let _ = join.join();
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let home = home.to_path_buf();
    let instance = instance.to_string();
    let locator = locator.clone();
    let worker_locator = locator.clone();
    let mut last_event_id = None::<String>;
    let mut seen_reply_ids = HashSet::new();
    // fire-and-forget: this resident listener owns reconnect/reconciliation for the daemon lifetime; cleanup stops and joins it.
    let join = thread::Builder::new()
        .name("claude-channel-events".to_string())
        .spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match SseStream::connect(&worker_locator, last_event_id.as_deref()) {
                    Ok(mut stream) => loop {
                        if thread_stop.load(Ordering::Acquire) {
                            return;
                        }
                        match stream.next() {
                            Ok(Some(event)) => {
                                if seen_reply_ids.contains(&event.reply_id) {
                                    // This event was reconciled successfully on an
                                    // earlier connection; it is safe to advance
                                    // past the duplicate replay.
                                    last_event_id = Some(event.reply_id);
                                } else if complete_receipt(&home, &instance, &event).is_ok() {
                                    // Advance the cursor only after receipt
                                    // persistence succeeds. A failed write must
                                    // be replayed after reconnect.
                                    seen_reply_ids.insert(event.reply_id.clone());
                                    last_event_id = Some(event.reply_id);
                                } else {
                                    break;
                                }
                            }
                            Ok(None) | Err(_) => break,
                        }
                    },
                    Err(_) => thread::sleep(Duration::from_millis(250)),
                }
            }
        })
        .expect("Claude ChannelBridge event worker thread must start");
    workers.insert(
        key,
        EventWorker {
            stop,
            join: Some(join),
            locator: locator.clone(),
        },
    );
}

pub(crate) fn stop_instance_state(home: &Path, instance: &str) {
    let key = worker_key(home, instance);
    if let Some(mut worker) = event_workers().lock().remove(&key) {
        worker.stop.store(true, Ordering::Release);
        if let Some(join) = worker.join.take() {
            let _ = join.join();
        }
    }
    let _ = std::fs::remove_dir_all(channel_state_dir(home, instance));
    if matches!(
        super::registry::claude_attach_locator(home, instance),
        Ok(Some(_))
    ) {
        let _ = super::registry::remove_session_locator(home, instance);
    }
}

pub(crate) fn deliver_resident(
    home: &Path,
    instance: &str,
    envelope: DeliveryEnvelope,
) -> anyhow::Result<DeliveryReceipt> {
    let locator = prepare_claude_channel(home, instance)?;
    ensure_event_worker(home, instance, &locator);
    let store = ReceiptStore::for_instance(home, instance)?;
    store.record_queued(&envelope)?;
    let capability = health_probe(&locator).map_err(|error| {
        let mut receipt = DeliveryReceipt::for_state(&envelope, DeliveryState::Failed);
        receipt.detail = Some(format!(
            "Claude ChannelBridge readiness failed closed: {error}"
        ));
        let _ = store.record(receipt);
        error
    })?;
    if !capability.ready {
        let mut receipt = DeliveryReceipt::for_state(&envelope, DeliveryState::Failed);
        receipt.detail = capability.degraded_reason;
        store.record(receipt)?;
        anyhow::bail!("Claude ChannelBridge capability probe failed closed");
    }
    let chat_id = chat_id_for_delivery(&envelope.instance, envelope.delivery_id);
    let payload = json!({
        "delivery_id": envelope.delivery_id,
        "chat_id": chat_id,
        "sender_id": "agend-terminal",
        "text": envelope.body,
    });
    let response = client_request(
        &locator,
        "POST",
        HTTP_PATH,
        &serde_json::to_vec(&payload)?,
        "application/json",
    )?;
    if response.status != 202 {
        let mut receipt = DeliveryReceipt::for_state(&envelope, DeliveryState::Failed);
        receipt.detail = Some(format!(
            "Claude ChannelBridge webhook rejected: {}",
            response_detail(&response)
        ));
        store.record(receipt)?;
        anyhow::bail!("Claude ChannelBridge webhook rejected: {}", response.status);
    }
    let mut receipt = DeliveryReceipt::for_state(&envelope, DeliveryState::ProtocolAccepted);
    receipt.protocol_request_id = Some(envelope.delivery_id.to_string());
    receipt.backend_event = Some("webhook_accepted".to_string());
    if let Some(previous) = store.latest(envelope.delivery_id)? {
        if previous.state.is_terminal()
            || matches!(
                previous.state,
                DeliveryState::TurnStarted | DeliveryState::AckOverdue
            )
        {
            // Claude may acknowledge (or complete) the self-kick while the
            // daemon-side delivery worker is still writing its post-202
            // receipt. Preserve every later state rather than appending a
            // ProtocolAccepted regression.
            return Ok(previous);
        }
    }
    store.record(receipt.clone())?;
    if let Ok(response) = client_request(
        &locator,
        "GET",
        &format!("/receipts/{}", envelope.delivery_id),
        &[],
        "application/json",
    ) {
        if response.status == 200 {
            if let Ok(value) = serde_json::from_slice::<Value>(&response.body) {
                if let Ok(event) = serde_json::from_value::<ReplyEvent>(value["reply"].clone()) {
                    let _ = complete_receipt(home, instance, &event);
                }
            }
        }
    }
    Ok(receipt)
}

fn chat_id_for_delivery(instance: &str, delivery_id: Uuid) -> String {
    format!("agend:{instance}:{delivery_id}")
}

#[async_trait::async_trait]
impl AgentDeliveryTransport for ClaudeChannelBridge {
    fn mode(&self) -> TransportMode {
        TransportMode::ChannelBridge
    }

    async fn start_or_attach(
        &mut self,
        locator: SessionLocator,
    ) -> anyhow::Result<TransportCapability> {
        self.locator = Some(locator);
        self.health_blocking()
    }

    async fn deliver(&mut self, envelope: DeliveryEnvelope) -> anyhow::Result<DeliveryReceipt> {
        deliver_resident(&self.home, &self.instance, envelope)
    }

    async fn next_event(&mut self) -> anyhow::Result<BackendEvent> {
        let locator = self.locator()?;
        let mut stream = SseStream::connect(&locator, None)?;
        match stream.next()? {
            Some(event) => Ok(BackendEvent::Completed {
                delivery_id: event.delivery_id,
                event: "claude_reply".to_string(),
            }),
            None => Err(anyhow::anyhow!("Claude ChannelBridge SSE closed")),
        }
    }

    async fn reconcile(&mut self, delivery_id: Uuid) -> anyhow::Result<DeliveryState> {
        let locator = self.locator()?;
        let store = ReceiptStore::for_instance(&self.home, &self.instance)?;
        let Some(previous) = store.latest(delivery_id)? else {
            return Ok(DeliveryState::Ambiguous);
        };
        if previous.state.is_terminal() {
            return Ok(previous.state);
        }
        let response = client_request(
            &locator,
            "GET",
            &format!("/receipts/{delivery_id}"),
            &[],
            "application/json",
        )?;
        if response.status == 200 {
            let value: Value = serde_json::from_slice(&response.body)?;
            if let Ok(event) = serde_json::from_value::<ReplyEvent>(value["reply"].clone()) {
                complete_receipt(&self.home, &self.instance, &event)?;
                return Ok(DeliveryState::Completed);
            }
        }
        Ok(previous.state)
    }

    async fn health(&self) -> TransportCapability {
        self.health_blocking()
            .unwrap_or_else(|error| TransportCapability {
                backend: "claude".to_string(),
                mode: TransportMode::ChannelBridge,
                ready: false,
                backend_version: None,
                degraded_reason: Some(error.to_string()),
            })
    }
}

pub(crate) struct ClaudeChannelBridge {
    home: PathBuf,
    instance: String,
    locator: Option<SessionLocator>,
}

impl ClaudeChannelBridge {
    fn locator(&self) -> anyhow::Result<SessionLocator> {
        self.locator
            .clone()
            .map(Ok)
            .unwrap_or_else(|| prepare_claude_channel(&self.home, &self.instance))
    }

    fn health_blocking(&self) -> anyhow::Result<TransportCapability> {
        health_probe(&self.locator()?)
    }
}

#[cfg(test)]
#[path = "claude_channel_tests.rs"]
mod tests;
