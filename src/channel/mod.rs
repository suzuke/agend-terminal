//! Channel abstraction — platform-neutral surface for messaging backends.
//!
//! This module defines the trait + types that `src/telegram.rs` (and future
//! Discord / Slack / Matrix adapters) implement. The design follows
//! `docs/PLAN-channel-abstraction.md` §3.
//!
//! **Status (T1 prep scaffold):** this module is intentionally unused by any
//! call site. PR2 in the T1 series (the atomic type cut-over) is the one that
//! wires `Arc<Mutex<TelegramState>>` leaks through `Bootstrap` / `Daemon` /
//! `App` onto this trait. Everything here carries `#[allow(dead_code)]` until
//! then — the dead-code allow is consumed in PR2.
//!
//! ## Design decisions frozen in this PR
//!
//! - **`BindingRef` is opaque** — core code never reads the inner platform
//!   payload. It only holds a `kind: &'static str` discriminator and an
//!   optional human-readable `display_tag` (so the TUI / logs can render a
//!   binding without platform-specific conditionals).
//! - **Event / outbound naming** — we use the parent plan's §3.1 names
//!   (`ChannelEvent`, `OutMsg`, `MsgRef`) rather than the UX-layer plan's
//!   `InboundEvent` / `OutboundIntent` terminology. The transport trait lives
//!   in this module; UX layer sits on top (see `PLAN-channel-ux-layer.md`) and
//!   can rename as needed without touching the transport contract.
//! - **`ChannelCapabilities::Default`** — conservative "nothing supported".
//!   Concrete adapters must opt-in per capability. This makes a new adapter's
//!   feature matrix explicit at review time.

// Entire module is scaffold-only in this PR — consumed in PR2 (T1 main
// atomic cut-over). Silences dead-code on type defs and unused-imports
// on the `pub use` re-exports below.
#![allow(dead_code, unused_imports)]

pub mod binding;
pub mod caps;
pub mod contract;
#[cfg(feature = "discord")]
pub mod discord;
pub mod event;
pub mod sink_registry;
pub mod telegram;
pub mod ux_event;

pub use binding::BindingRef;
pub use caps::{ChannelCapabilities, MarkdownDialect, MentionStyle, NativeSeeAllHint, RateBudget};
pub use event::{
    Attachment, AttachmentKind, ChannelEvent, MsgPayload, MsgRef, OutMsg, RevokeReason, User,
};
pub use sink_registry::{registry, UxSinkRegistry};
pub use ux_event::{select_action, FleetEvent, NoopUxSink, UxAction, UxEvent, UxEventSink};

use crate::agent::AgentRegistry;
use anyhow::Result;
use std::sync::{Arc, OnceLock};

// ---------------------------------------------------------------------------
// Supporting types for trait methods added in PR-AE3
// ---------------------------------------------------------------------------

/// Error type for channel operations that may not be supported.
#[derive(Debug)]
pub enum ChannelError {
    NotSupported(String),
    Other(anyhow::Error),
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSupported(op) => write!(f, "operation not supported: {op}"),
            Self::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ChannelError {}

impl From<anyhow::Error> for ChannelError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(e)
    }
}

/// Reference to a created topic / thread / channel.
#[derive(Debug, Clone)]
pub struct TopicRef {
    pub id: String,
    pub channel_kind: ChannelKind,
}

/// Severity level for channel notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifySeverity {
    Info,
    Warn,
    Error,
}

// ---------------------------------------------------------------------------
// Process-wide active channel registry
// ---------------------------------------------------------------------------

static ACTIVE_CHANNEL: OnceLock<Arc<dyn Channel>> = OnceLock::new();

/// Register the process-wide active channel. Called once during bootstrap.
pub fn register_active_channel(channel: Arc<dyn Channel>) {
    let _ = ACTIVE_CHANNEL.set(channel);
}

/// Get the process-wide active channel, if one has been registered.
pub fn active_channel() -> Option<&'static Arc<dyn Channel>> {
    ACTIVE_CHANNEL.get()
}

/// Typed channel kind — replaces magic strings like `"telegram"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Telegram,
    Discord,
    // Future: Slack, Matrix, ...
}

/// Platform-neutral channel trait. Implementations live next to their
/// platform glue (e.g., `src/telegram.rs` → future `src/channel/telegram.rs`).
///
/// Signature mirrors `docs/PLAN-channel-abstraction.md` §3.1. Events are
/// delivered through a pull-style API (`poll_event`) rather than an async
/// stream, to keep the trait agnostic to the caller's runtime choice
/// (today's core loop is sync; teloxide runs on a private tokio runtime
/// inside the Telegram adapter). The dispatcher in PR2 wraps this in a
/// merged `Receiver<ChannelEvent>`.
pub trait Channel: Send + Sync {
    /// Short kind discriminator, e.g. `"telegram"`, `"discord"`.
    fn kind(&self) -> &'static str;

    /// Feature matrix — transport + UX-layer capabilities.
    fn caps(&self) -> &ChannelCapabilities;

    /// Non-blocking poll for the next inbound event, if any. Returns `None`
    /// when the channel has no pending events. The dispatcher (added in PR2)
    /// merges per-channel `poll_event` streams into a single event queue.
    fn poll_event(&self) -> Option<ChannelEvent>;

    /// Send an outbound message to a binding. Returns an opaque `MsgRef`
    /// that can be used later for `edit` / `delete`.
    fn send(&self, binding: &BindingRef, msg: OutMsg) -> Result<MsgRef>;

    /// Edit a previously sent message in place. Implementations may return
    /// an error when `caps().edit == false`.
    fn edit(&self, msg: &MsgRef, payload: OutMsg) -> Result<()>;

    /// Delete a previously sent message. Implementations may return
    /// an error when the channel disallows deletion.
    fn delete(&self, msg: &MsgRef) -> Result<()>;

    /// Create a new binding (topic / channel / thread) for an instance.
    fn create_binding(&self, name: &str, opts: BindingOpts) -> Result<BindingRef>;

    /// Tear down a binding. Core code should also drop any references.
    fn remove_binding(&self, binding: &BindingRef) -> Result<()>;

    // -----------------------------------------------------------------
    // Registry-side helpers
    //
    // These let core code (`app::telegram_hooks`, `daemon::supervisor`)
    // ask "is this instance bound?" / "remember this binding" without
    // poking the adapter's private state. The in-memory map of
    // instance → binding lives next to the concrete adapter so its
    // locking / lifetime rules are a single-adapter concern.
    // -----------------------------------------------------------------

    /// Does the adapter already have a recorded binding for `instance`?
    fn has_binding(&self, instance: &str) -> bool;

    /// Remember a binding for `instance`. `submit_key` is PTY metadata
    /// that later inbound events carry through to the registry (e.g.
    /// the keystroke used to submit a message to a running agent).
    fn record_binding(&self, instance: &str, binding: BindingRef, submit_key: String);

    /// Remove and return the recorded binding for `instance`, if any.
    /// Call sites typically follow up with [`Channel::remove_binding`]
    /// to also tear down the platform resource.
    fn take_binding(&self, instance: &str) -> Option<BindingRef>;

    /// Register the in-process agent registry with the adapter so
    /// inbound events can route to the right agent without a
    /// cross-thread round-trip. Two-phase because adapters initialize
    /// during bootstrap (before the registry exists).
    fn attach_registry(&self, registry: AgentRegistry);

    /// Create a per-instance discussion thread (Telegram forum topic /
    /// Discord thread / Slack channel).
    /// Default: `Err(ChannelError::NotSupported)` so channels without the
    /// concept opt out gracefully.
    fn create_topic(&self, _name: &str) -> std::result::Result<TopicRef, ChannelError> {
        Err(ChannelError::NotSupported("create_topic".into()))
    }

    /// Send a notification (stall warning, system event) to an instance's
    /// channel. `silent` suppresses push/vibrate when the platform supports it.
    /// Default: `Err(ChannelError::NotSupported)`.
    fn notify(
        &self,
        _instance: &str,
        _severity: NotifySeverity,
        _message: &str,
        _silent: bool,
    ) -> std::result::Result<(), ChannelError> {
        Err(ChannelError::NotSupported("notify".into()))
    }
}

/// Options passed to `Channel::create_binding`. Platform-specific hints live
/// under `extra` as free-form string pairs (e.g. Discord `category_name`).
#[derive(Debug, Default, Clone)]
pub struct BindingOpts {
    /// Human-visible display name for the binding (topic title, channel
    /// name, etc.). Optional — adapters fall back to `name`.
    pub display_name: Option<String>,
    /// Free-form platform hints. The adapter decides what keys it honors.
    pub extra: std::collections::HashMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock channel that hits the default `Err(NotSupported)` for both
    /// `create_topic` and `notify`. Exercises the opt-out path that
    /// channels without topic/notify support follow.
    struct MockChannel {
        caps: ChannelCapabilities,
    }

    impl MockChannel {
        fn new() -> Self {
            Self {
                caps: ChannelCapabilities::default(),
            }
        }
    }

    impl Channel for MockChannel {
        fn kind(&self) -> &'static str {
            "mock"
        }
        fn caps(&self) -> &ChannelCapabilities {
            &self.caps
        }
        fn poll_event(&self) -> Option<ChannelEvent> {
            None
        }
        fn send(&self, _: &BindingRef, _: OutMsg) -> Result<MsgRef> {
            anyhow::bail!("mock")
        }
        fn edit(&self, _: &MsgRef, _: OutMsg) -> Result<()> {
            anyhow::bail!("mock")
        }
        fn delete(&self, _: &MsgRef) -> Result<()> {
            anyhow::bail!("mock")
        }
        fn create_binding(&self, _: &str, _: BindingOpts) -> Result<BindingRef> {
            anyhow::bail!("mock")
        }
        fn remove_binding(&self, _: &BindingRef) -> Result<()> {
            anyhow::bail!("mock")
        }
        fn has_binding(&self, _: &str) -> bool {
            false
        }
        fn record_binding(&self, _: &str, _: BindingRef, _: String) {}
        fn take_binding(&self, _: &str) -> Option<BindingRef> {
            None
        }
        fn attach_registry(&self, _: crate::agent::AgentRegistry) {}
    }

    #[test]
    fn default_create_topic_returns_not_supported() {
        let ch = MockChannel::new();
        let err = ch.create_topic("test").expect_err("should be NotSupported");
        assert!(
            matches!(err, ChannelError::NotSupported(_)),
            "expected NotSupported, got: {err}"
        );
        assert!(err.to_string().contains("create_topic"));
    }

    #[test]
    fn default_notify_returns_not_supported() {
        let ch = MockChannel::new();
        let err = ch
            .notify("inst", NotifySeverity::Warn, "msg", false)
            .expect_err("should be NotSupported");
        assert!(
            matches!(err, ChannelError::NotSupported(_)),
            "expected NotSupported, got: {err}"
        );
        assert!(err.to_string().contains("notify"));
    }

    #[test]
    fn channel_error_display() {
        let ns = ChannelError::NotSupported("op".into());
        assert_eq!(ns.to_string(), "operation not supported: op");

        let other = ChannelError::Other(anyhow::anyhow!("boom"));
        assert_eq!(other.to_string(), "boom");
    }

    #[test]
    fn channel_error_from_anyhow() {
        let err: ChannelError = anyhow::anyhow!("test").into();
        assert!(matches!(err, ChannelError::Other(_)));
    }

    #[test]
    fn topic_ref_fields() {
        let tr = TopicRef {
            id: "42".into(),
            channel_kind: ChannelKind::Telegram,
        };
        assert_eq!(tr.id, "42");
        assert_eq!(tr.channel_kind, ChannelKind::Telegram);
    }

    #[test]
    fn notify_severity_variants() {
        // Ensure all variants exist and are distinct.
        assert_ne!(NotifySeverity::Info, NotifySeverity::Warn);
        assert_ne!(NotifySeverity::Warn, NotifySeverity::Error);
        assert_ne!(NotifySeverity::Info, NotifySeverity::Error);
    }
}
