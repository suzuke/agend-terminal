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
pub mod event;

pub use binding::BindingRef;
pub use caps::{ChannelCapabilities, MarkdownDialect, MentionStyle, NativeSeeAllHint, RateBudget};
pub use event::{ChannelEvent, MsgPayload, MsgRef, OutMsg, RevokeReason, User};

use anyhow::Result;

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
