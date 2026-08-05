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
}

impl SessionLocator {
    pub(crate) fn codex(endpoint: PathBuf, thread_id: Option<String>) -> Self {
        Self {
            backend: "codex".to_string(),
            endpoint: Some(endpoint),
            thread_id,
            session_id: None,
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
    pub session: SessionLocator,
    pub kind: DeliveryKind,
    pub body: String,
    pub correlation_id: Option<String>,
    pub created_at: String,
    /// Digest of the exact structured payload.  This is persisted with the
    /// receipt so reconnect/reconcile never has to compare prompt text.
    pub payload_digest: String,
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
            created_at: Utc::now().to_rfc3339(),
            payload_digest,
        }
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
