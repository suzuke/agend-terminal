use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use uuid::Uuid;

/// The session identity required to reconnect a backend without guessing from
/// a pane or a process.  `thread_id` is mandatory for Codex NativeShared.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionLocator {
    pub backend: String,
    pub endpoint: Option<PathBuf>,
    pub thread_id: Option<String>,
    pub session_id: Option<String>,
    /// HTTP endpoint used by OpenCode NativeShared.  This is separate from
    /// `endpoint` because the latter is a filesystem Unix-socket path for
    /// Codex and must remain backwards-compatible in durable locators.
    #[serde(default)]
    pub endpoint_url: Option<String>,
    /// OpenCode's HTTP Basic-auth username.  Never included in normal logs.
    #[serde(default)]
    pub username: Option<String>,
    /// OpenCode's loopback server password.  The locator is stored in the
    /// private transport state directory and is never logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Optional provider/model selected for the shared OpenCode session.
    #[serde(default)]
    pub model: Option<String>,
    /// Monotonic local SSE position used to prove how far the daemon has
    /// consumed an event stream after reconnect. OpenCode does not provide a
    /// durable event cursor in the HTTP API.
    #[serde(default)]
    pub event_cursor: Option<u64>,
    /// Whether AgEnD owns the OpenCode server lifecycle. Explicit external
    /// endpoints can opt out while still using the same adapter.
    #[serde(default)]
    pub managed: bool,
    /// PID of the managed `opencode serve` process, when one was launched by
    /// AgEnD.  The start token below makes this safe across PID reuse and
    /// daemon restarts.
    #[serde(default)]
    pub server_pid: Option<u32>,
    /// OS process-start identity paired with `server_pid`.
    #[serde(default)]
    pub server_start_token: Option<u64>,
}

impl SessionLocator {
    pub(crate) fn codex(endpoint: PathBuf, thread_id: Option<String>) -> Self {
        Self {
            backend: "codex".to_string(),
            endpoint: Some(endpoint),
            thread_id,
            session_id: None,
            endpoint_url: None,
            username: None,
            password: None,
            model: None,
            event_cursor: None,
            managed: false,
            server_pid: None,
            server_start_token: None,
        }
    }

    pub(crate) fn opencode(
        endpoint_url: String,
        session_id: Option<String>,
        username: String,
        password: String,
    ) -> Self {
        Self {
            backend: "opencode".to_string(),
            endpoint: None,
            thread_id: None,
            session_id,
            endpoint_url: Some(endpoint_url),
            username: Some(username),
            password: Some(password),
            model: None,
            event_cursor: Some(0),
            managed: true,
            server_pid: None,
            server_start_token: None,
        }
    }

    pub(crate) fn claude(endpoint_url: String, session_id: String, token: String) -> Self {
        Self {
            backend: "claude".to_string(),
            endpoint: None,
            thread_id: None,
            session_id: Some(session_id),
            endpoint_url: Some(endpoint_url),
            username: Some("bearer".to_string()),
            password: Some(token),
            model: None,
            event_cursor: Some(0),
            managed: false,
            server_pid: None,
            server_start_token: None,
        }
    }

    pub(crate) fn remote_attach_arg(&self) -> String {
        self.endpoint
            .as_ref()
            .map(|path| format!("unix://{}", path.display()))
            .unwrap_or_else(|| "unix://".to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DeliveryKind {
    Prompt,
    Notification,
    Task,
    Query,
    Handoff,
    Steer,
    Interrupt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DeliveryEnvelope {
    pub delivery_id: Uuid,
    pub instance: String,
    /// Delivery persistence/export must retain the locator needed to identify
    /// a session without copying OpenCode's Basic password into the durable
    /// receipt. The private session-locator artifact still serializes the
    /// complete locator for backend restart/reconnect.
    #[serde(with = "durable_session_locator")]
    pub session: SessionLocator,
    pub kind: DeliveryKind,
    pub body: String,
    pub correlation_id: Option<String>,
    /// Fresh-restart self-kicks have a consumer acknowledgement contract.
    /// This is persisted with the envelope so a bridge restart can replay the
    /// watchdog without treating ordinary notifications as missing acks.
    #[serde(default)]
    pub self_kick: bool,
    pub created_at: String,
    /// Digest of the exact structured payload.  This is persisted with the
    /// receipt so reconnect/reconcile never has to compare prompt text.
    pub payload_digest: String,
    /// Actual registry-selected route. This is persisted with the envelope so
    /// every receipt can explain which adapter was attempted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_mode: Option<String>,
    /// Explicit durable inbox row identity used to number retries. Absent for
    /// independent internal deliveries, which must each remain attempt 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_delivery_id: Option<String>,
    /// #3324: the EXTERNAL channel this delivery originated on, when it did.
    ///
    /// The ChannelBridge is an inbound transport to the agent, not an outbound
    /// sender: a bridge `reply` records a transport acknowledgement and never
    /// reaches Telegram. Two tools are named `reply` in that environment, so
    /// without a typed origin the wrong one succeeds silently. `None` means the
    /// delivery originated inside AgEnD and the bridge owns its reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_origin: Option<crate::channel::ChannelKind>,
}

mod durable_session_locator {
    use super::SessionLocator;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(super) fn serialize<S>(locator: &SessionLocator, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut redacted = locator.clone();
        if redacted.backend == "opencode" {
            redacted.password = None;
        }
        redacted.serialize(serializer)
    }

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<SessionLocator, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Accept legacy receipt rows that carried the password. New rows are
        // emitted through `serialize`, which omits it for OpenCode.
        SessionLocator::deserialize(deserializer)
    }
}

impl DeliveryEnvelope {
    pub(crate) fn new(
        instance: impl Into<String>,
        session: SessionLocator,
        kind: DeliveryKind,
        body: impl Into<String>,
        correlation_id: Option<String>,
    ) -> Self {
        let body = body.into();
        let payload_digest = digest(&body);
        Self {
            delivery_id: Uuid::new_v4(),
            instance: instance.into(),
            session,
            kind,
            body,
            correlation_id,
            self_kick: false,
            created_at: Utc::now().to_rfc3339(),
            payload_digest,
            transport_mode: None,
            logical_delivery_id: None,
            channel_origin: None,
        }
    }

    pub(crate) fn self_kick(
        instance: impl Into<String>,
        session: SessionLocator,
        body: impl Into<String>,
    ) -> Self {
        let mut envelope = Self::new(instance, session, DeliveryKind::Notification, body, None);
        envelope.self_kick = true;
        envelope
    }
}

pub(crate) fn digest(body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_digest_is_stable_for_exact_payload() {
        let envelope = DeliveryEnvelope::new(
            "agent",
            SessionLocator::codex(PathBuf::from("/tmp/sock"), Some("t".to_string())),
            DeliveryKind::Prompt,
            "hello",
            Some("corr".to_string()),
        );
        assert_eq!(envelope.payload_digest, digest("hello"));
        assert_ne!(envelope.payload_digest, digest("hello "));
    }
}
