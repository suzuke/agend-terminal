//! Structured backend delivery transports.
//!
//! The transport layer is deliberately separate from the TUI.  A backend mode
//! is selected before delivery and failures in a structured adapter are
//! terminal for that adapter; they never silently become a PTY write.

mod codex_app_server;
mod envelope;
mod legacy_pty;
mod receipt;
mod registry;

pub(crate) use envelope::{DeliveryEnvelope, DeliveryKind, SessionLocator};
pub(crate) use receipt::{
    delivery_path_for_instance, remove_instance_delivery_state, DeliveryReceipt, DeliveryState,
    ReceiptStore,
};
#[cfg(test)]
pub(crate) use registry::mode_for_backend;

pub(crate) use registry::{deliver_notification, record_delivery_drop};

/// Explicit transport selected for a backend instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum TransportMode {
    NativeShared,
    ChannelBridge,
    ManagedHeadless,
    ManualRequired,
    LegacyPty,
}

/// Capability and readiness summary.  It intentionally contains no prompt,
/// token, tool output, or other sensitive payload.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct TransportCapability {
    pub backend: String,
    pub mode: TransportMode,
    pub ready: bool,
    pub backend_version: Option<String>,
    pub degraded_reason: Option<String>,
}

/// Normalized event emitted by a structured adapter.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum BackendEvent {
    Ready,
    ProtocolAccepted {
        delivery_id: uuid::Uuid,
        request_id: String,
    },
    ObservedInSession {
        delivery_id: uuid::Uuid,
        event: String,
    },
    TurnStarted {
        delivery_id: uuid::Uuid,
        turn_id: Option<String>,
    },
    Completed {
        delivery_id: uuid::Uuid,
        event: String,
    },
    Failed {
        delivery_id: Option<uuid::Uuid>,
        reason: String,
    },
    ReverseRequest {
        request_id: String,
        method: String,
        params: serde_json::Value,
    },
    Unknown {
        method: Option<String>,
    },
}

/// Transport lifecycle and receipt interface shared by structured adapters.
#[allow(dead_code)]
#[async_trait::async_trait]
pub(crate) trait AgentDeliveryTransport: Send {
    fn mode(&self) -> TransportMode;
    async fn start_or_attach(
        &mut self,
        locator: SessionLocator,
    ) -> anyhow::Result<TransportCapability>;
    async fn deliver(&mut self, envelope: DeliveryEnvelope) -> anyhow::Result<DeliveryReceipt>;
    async fn next_event(&mut self) -> anyhow::Result<BackendEvent>;
    async fn reconcile(&mut self, delivery_id: uuid::Uuid) -> anyhow::Result<DeliveryState>;
    async fn health(&self) -> TransportCapability;
}

#[cfg(test)]
mod tests {
    use super::codex_app_server::CodexNativeShared;
    use super::*;
    use crate::backend::Backend;
    use std::path::PathBuf;

    #[test]
    fn modes_are_explicit_and_non_codex_backends_remain_legacy() {
        assert_eq!(
            mode_for_backend(&Backend::Codex),
            TransportMode::NativeShared
        );
        assert_eq!(
            mode_for_backend(&Backend::OpenCode),
            TransportMode::NativeShared
        );
        for backend in [
            Backend::ClaudeCode,
            Backend::Grok,
            Backend::KiroCli,
            Backend::Agy,
        ] {
            assert_eq!(mode_for_backend(&backend), TransportMode::LegacyPty);
        }
    }

    #[test]
    fn remote_attach_args_use_the_same_socket_locator() {
        let locator = SessionLocator::codex(
            PathBuf::from("/tmp/agend-codex.sock"),
            Some("thread-1".to_string()),
        );
        assert_eq!(locator.remote_attach_arg(), "unix:///tmp/agend-codex.sock");
        assert_eq!(
            CodexNativeShared::remote_attach_args(&locator),
            vec!["--remote", "unix:///tmp/agend-codex.sock"]
        );
    }
}
