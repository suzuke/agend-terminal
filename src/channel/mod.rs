//! Channel abstraction — platform-neutral surface for messaging backends.
//!
//! This module defines the trait + types that `src/telegram.rs` (and future
//! Discord / Slack / Matrix adapters) implement. The public contract is
//! documented in `docs/FEATURE-channels.md`.
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
//!   in this module; the UX layer sits on top and
//!   can rename as needed without touching the transport contract.
//! - **`ChannelCapabilities::Default`** — conservative "nothing supported".
//!   Concrete adapters must opt-in per capability. This makes a new adapter's
//!   feature matrix explicit at review time.

// Entire module is scaffold-only in this PR — consumed in PR2 (T1 main
// atomic cut-over). Silences dead-code on type defs and unused-imports
// on the `pub use` re-exports below.
#![allow(dead_code, unused_imports)]

pub mod auth;
pub mod binding;
pub mod caps;
pub mod contract;
pub mod dedup;
#[cfg(feature = "discord")]
pub mod discord;
pub mod event;
/// #1642: shared sync→async bridge (`block_on_value`) used by both telegram and
/// discord — deduped from per-channel copies.
pub(crate) mod shared_async;
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
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Supporting types for trait methods added in PR-AE3
// ---------------------------------------------------------------------------

/// Telegram connection status for status bar display.
#[derive(Clone, Copy)]
pub enum TelegramStatus {
    /// No Telegram channel config in fleet.yaml.
    NotConfigured,
    /// Configured but token env var is missing.
    NoToken,
    /// Configured and token present (polling should be active).
    Connected,
}

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

// Sprint 55 P0-A r1: HashMap registry keyed by `Channel::kind()` to
// realize the Variant-extend multi-channel routing contract per design
// doc + reviewer Finding #1. Production keeps single-channel-fleet
// behavior intact (one entry → `active_channel()` returns it); the
// post-discord-broadens future state has telegram + discord both
// registered and `lookup_channel_by_name` routes deterministically.
static CHANNELS: OnceLock<parking_lot::RwLock<HashMap<&'static str, Arc<dyn Channel>>>> =
    OnceLock::new();

fn channels_registry() -> &'static parking_lot::RwLock<HashMap<&'static str, Arc<dyn Channel>>> {
    CHANNELS.get_or_init(|| parking_lot::RwLock::new(HashMap::new()))
}

/// Register a channel by its `kind()` discriminator. Same-kind
/// re-registration replaces the prior entry — this keeps the production
/// single-bootstrap-call contract intact and enables test isolation
/// across cases.
pub fn register_active_channel(channel: Arc<dyn Channel>) {
    let kind = channel.kind();
    channels_registry().write().insert(kind, channel);
}

/// Get THE active channel when exactly one is registered — preserves
/// pre-P0-A single-channel-fleet semantic. Returns `None` when zero OR
/// multiple channels are registered: in the multi-channel fleet,
/// callers MUST disambiguate via [`lookup_channel_by_name`] with the
/// explicit kind.
///
/// Sprint 56+ may add a `primary_channel()` concept with operator-
/// designated default; deferred from P0-A scope.
pub fn active_channel() -> Option<Arc<dyn Channel>> {
    let g = channels_registry().read();
    if g.len() == 1 {
        g.values().next().cloned()
    } else {
        None
    }
}

/// Sprint 55 P0-A — look up a registered channel by its `kind()`
/// discriminator (e.g. `"telegram"`, `"discord"`). Real HashMap lookup;
/// delivers the Variant-extend multi-channel routing contract.
pub fn lookup_channel_by_name(name: &str) -> Option<Arc<dyn Channel>> {
    channels_registry().read().get(name).cloned()
}

/// #1744-M6: the set of channels an operator-facing **escalation P0** must reach.
///
/// [`active_channel`] returns `None` whenever ZERO **or MULTIPLE** channels are
/// registered, so once a fleet runs telegram + discord every `if let Some(ch) =
/// active_channel()` escalation silently no-ops — exactly when an orchestrator-
/// down P0 must get through. This resolver instead returns ALL registered
/// channels (one per `kind()`): a P0 is delivered to every surface, because for
/// a leaderless-orchestrator alert "deliver twice" beats "deliver never". With a
/// single channel it returns just that one (unchanged single-fleet behavior);
/// with none, an empty vec (caller logs the drop).
///
/// Scope discipline (#1744-M6): this is for **Error-severity escalation P0s
/// only** — the crash / hung / AuthError / retry-exhausted / stall pages. Non-P0
/// notices (e.g. the Info "agent ready" recovery ping) deliberately keep
/// [`active_channel`] so the multi-channel fan-out can never leak into routine
/// traffic. An operator-designated `primary_channel()` is deferred (a routing
/// nicety, not a reachability fix).
pub fn resolve_escalation_channels() -> Vec<Arc<dyn Channel>> {
    channels_registry().read().values().cloned().collect()
}

/// #1744-M6: dispatch one operator-facing **escalation P0** to EVERY registered
/// channel (telegram, discord, …) via [`gated_notify`]. Centralizes the
/// iterate-all fan-out so every orchestrator-down page (crash / hung / AuthError
/// / retry-exhausted / stall) gets through even in a multi-channel fleet, where
/// [`active_channel`] would return `None` and silently drop the alert. Returns
/// the number of channels dispatched to; `0` (no channel registered) is logged
/// as a dropped P0. Reserve for Error-severity P0s — routine notices keep
/// [`active_channel`] so this fan-out never leaks into non-escalation traffic.
pub fn notify_all_escalation_channels(
    instance: &str,
    severity: NotifySeverity,
    message: &str,
    silent: bool,
) -> usize {
    let channels = resolve_escalation_channels();
    if channels.is_empty() {
        tracing::debug!(agent = %instance, "no channel registered — escalation P0 dropped");
        return 0;
    }
    for ch in &channels {
        let _ = gated_notify(ch.as_ref(), instance, severity, message, silent);
    }
    channels.len()
}

/// Sprint 55 P0-A — test-only: clear all registered channels.
#[cfg(test)]
pub fn reset_active_channel_for_test() {
    channels_registry().write().clear();
}

/// active_channel() multi-channel blind-spot fix (t-20260703164240502572-50899-11):
/// find which registered channel (if any) already has a binding recorded for
/// `instance` — read-only, unlike [`Channel::take_binding`]. Where `active_channel`
/// would return `None` once a fleet runs 2+ channels, this always finds the one
/// the instance actually belongs to, because [`Channel::has_binding`] is checked
/// per-channel rather than requiring exactly one channel to be registered fleet-wide.
pub fn channel_for_instance(instance: &str) -> Option<Arc<dyn Channel>> {
    channels_registry()
        .read()
        .values()
        .find(|ch| ch.has_binding(instance))
        .cloned()
}

/// active_channel() multi-channel blind-spot fix (t-20260703164240502572-50899-11):
/// drop `name`'s binding on EVERY registered channel, not just "the active" one.
/// `active_channel()` returns `None` once a fleet runs 2+ channels, so a caller
/// gating cleanup on it (as `drop_active_binding` and app-mode teardown used to)
/// silently skips the drop entirely in a multi-channel fleet — the binding (and
/// whatever platform resource it references) leaks. [`Channel::take_binding`] is
/// a safe no-op on a channel where `name` has no recorded binding (it returns
/// `None` and touches nothing), so iterating every registered channel reaches
/// whichever one `name` actually happens to be bound to.
pub fn drop_binding_on_all_channels(name: &str) {
    for ch in channels_registry().read().values() {
        let _ = ch.take_binding(name);
    }
}

/// #966: Outcome of [`ensure_topic_for`] — explicit enum (no
/// `Option<String>`) so callers must handle each variant. Mirrors the
/// #962 surface-failures discipline: silent `let _ = ...` patterns
/// upgrade to explicit branches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopicOutcome {
    /// Topic created (or reused — idempotent via channel-side registry).
    /// Inner string is the topic_id as channel-native format (telegram:
    /// stringified i32; discord: stringified u64).
    Created(String),
    /// No channel registered at call-time. NOT an error — the instance
    /// is operator-functional without a channel; just no telegram surface.
    NoChannel,
    /// Channel exists but `create_topic` returned `Err`. Caller should
    /// `tracing::warn!` and surface to operator; the instance creation
    /// should still proceed because topic creation is an enhancement,
    /// not a hard precondition. (handle_spawn returns success even when
    /// telegram is misconfigured — same contract.)
    Failed(String),
}

/// #966 hub fn: look up the registered channels AT CALL-TIME (not from a
/// cached `Option<Arc<dyn Channel>>` snapshot) and ensure a topic
/// exists for `name` on EVERY registered channel. Replaces three
/// replicated call sites:
///
/// - `src/api/handlers/instance.rs` (handle_spawn — MCP / deploy_template / api caller)
/// - `src/api/handlers/team.rs` (team mode)
/// - `src/app/tui_spawn.rs` (TUI Backend menu + command palette — #966 new)
///
/// **Why runtime lookup matters**: post-#945 Phase 1, telegram_init runs
/// on a background thread. App startup-time snapshots of
/// `Option<Arc<dyn Channel>>` are commonly `None`; the channel registers
/// ~6s later via `register_active_channel`. Cached-snapshot callers
/// silently no-op forever for that startup window's instances.
/// `ensure_topic_for` queries the registry fresh on every call so the
/// post-init channel is picked up automatically.
///
/// **Multi-channel fan-out** (t-20260703164240502572-50899-11): this used
/// to gate on the `active_channel()` singleton, which returns `None` once
/// 2+ channels are registered — in a telegram+discord fleet, new
/// instances got NO topic anywhere. The TUI's own pane-create hooks
/// (`app/telegram_hooks.rs`, `app/discord_hooks.rs`) already call
/// `lookup_channel_by_name` for each channel explicitly and unconditionally
/// — i.e. fan-out to every registered channel is the established norm, not
/// a new policy. This converges `ensure_topic_for` onto that precedent:
/// `create_topic` is attempted on every registered channel; the 3 callers
/// only consume a single [`TopicOutcome`], so the result is `Created` with
/// the first channel's topic id if ANY channel succeeded, `NoChannel` if
/// the registry was empty, or `Failed` only if the registry was non-empty
/// and EVERY channel failed.
pub fn ensure_topic_for(name: &str) -> TopicOutcome {
    let channels: Vec<Arc<dyn Channel>> = channels_registry().read().values().cloned().collect();
    if channels.is_empty() {
        return TopicOutcome::NoChannel;
    }
    let mut first_success: Option<String> = None;
    let mut errors: Vec<String> = Vec::new();
    for ch in channels {
        match ch.create_topic(name) {
            Ok(topic) => {
                if first_success.is_none() {
                    first_success = Some(topic.id);
                }
            }
            Err(e) => errors.push(format!("{}: {e}", ch.kind())),
        }
    }
    match first_success {
        Some(id) => TopicOutcome::Created(id),
        None => TopicOutcome::Failed(errors.join("; ")),
    }
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
/// The signature mirrors the contract in `docs/FEATURE-channels.md`. Events are
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

    /// Outbound notify gate predicate: returns `true` iff the channel
    /// has been configured with an explicit non-empty operator
    /// allowlist (i.e. the operator has opted in to receiving
    /// info-bearing notifications via this channel).
    ///
    /// Default: `false` (fail-closed for adapters without an allowlist
    /// concept). Concrete adapters override to expose their
    /// configuration state.
    ///
    /// Consumed by [`gated_notify`] — daemon notify call sites should
    /// route through that helper rather than calling [`Self::notify`]
    /// directly so the gate cannot be bypassed.
    fn outbound_authorized(&self) -> bool {
        false
    }

    /// Unified entry for **agent-callable** outbound operations.
    ///
    /// Fan-in for the MCP→Channel bridge surfaces (`reply`, `react`,
    /// `delegate_task` provenance side-channel). The `Edit` variant is
    /// retained for telegram-internal edit operations (e.g. reaction
    /// replacement) — Sprint 30 PR-3 removed the agent-callable
    /// `edit_message` MCP tool, but the channel-internal Edit op still
    /// drives existing telegram.rs code paths.
    ///
    /// Implementations must:
    /// 1. Check [`Self::outbound_authorized`] (PR #216 allowlist gate).
    /// 2. Dispatch to the platform-specific send.
    ///
    /// Default: `Err(NotSupported)` so adapters that haven't opted in
    /// fail closed.
    fn send_from_agent(
        &self,
        _agent: &str,
        _op: AgentOutboundOp,
    ) -> std::result::Result<MsgRef, ChannelError> {
        Err(ChannelError::NotSupported("send_from_agent".into()))
    }
}

/// Op-specific payload for [`Channel::send_from_agent`].
///
/// Variants:
///
/// - `Reply` — `reply` MCP tool (free-form text into bound topic)
/// - `React` — `react` MCP tool (emoji on existing message)
/// - `Edit` — channel-internal edit (replace text of bot-sent message);
///   retained after Sprint 30 PR-3 removed the `edit_message` MCP tool
///   for telegram-internal edit paths (reaction replacement, etc.)
/// - `InjectProvenance` — `delegate_task` provenance side-channel
///   (renders "@from delegated to @target" tag in target's topic)
#[derive(Debug, Clone)]
pub enum AgentOutboundOp {
    /// Free-form reply into the agent's bound topic. `task_id`/`correlation_id`
    /// carry the sending turn's task context when known (the `reply` MCP tool
    /// may pass them); they are recorded in the `sent_ledger` so a later
    /// operator reply-to can be correlated back to this message's task. Most
    /// interactive replies legitimately carry neither (→ `None`).
    Reply {
        text: String,
        task_id: Option<String>,
        correlation_id: Option<String>,
    },
    /// Emoji reaction on a previously-observed message. `message_id` is
    /// `None` when the agent reacts to its most recent inbound message
    /// (resolved via `metadata/{instance}.json` `last_message_id`).
    React {
        emoji: String,
        message_id: Option<String>,
    },
    /// Edit a bot-sent message in place.
    Edit {
        message_id: String,
        new_text: String,
    },
    /// Side-channel provenance render — daemon-internal only.
    /// `from` is the delegating agent's name; the trait method's `agent`
    /// arg is the receiving agent (whose topic gets the tag).
    InjectProvenance { from: String, task: String },
}

/// #1339 PR-2: mode-aware notification policy. The operator mode (#1575)
/// decides which severities are worth pinging the operator's channel for:
///
/// - `Active` — operator at the TUI; today's behavior: **every** severity
///   passes (hard `true`, no per-severity computation), so a future filter
///   bug can never silently drop an active-mode notice (reviewer-2 risk).
/// - `Away` — operator reachable via the channel but not at the TUI: routine
///   `Info` (e.g. the "ready again" recovery ping) is suppressed; `Warn` and
///   `Error` still go through.
/// - `Sleep` — operator unreachable: only `Error` (P0 — a crash, or an agent
///   stuck awaiting the operator, see #1552 in `daemon::supervisor`) breaks
///   through; `Info`/`Warn` are held.
///
/// CHECKED match on the severity axis for `Away`/`Sleep` (no `_` catch-all) —
/// adding a `NotifySeverity` variant fails to compile until its per-mode
/// policy is stated here, the regression-guard the single-chokepoint design
/// depends on.
fn should_notify_in_mode(
    severity: NotifySeverity,
    mode: crate::operator_mode::OperatorMode,
) -> bool {
    use crate::operator_mode::OperatorMode;
    match mode {
        OperatorMode::Active => true,
        OperatorMode::Away => match severity {
            NotifySeverity::Info => false,
            NotifySeverity::Warn | NotifySeverity::Error => true,
        },
        OperatorMode::Sleep => match severity {
            NotifySeverity::Info | NotifySeverity::Warn => false,
            NotifySeverity::Error => true,
        },
    }
}

/// Outbound notify gate — only forwards to [`Channel::notify`] when the
/// channel reports [`Channel::outbound_authorized`] = `true`. When the
/// channel is unauthorised (no allowlist configured), the call is
/// dropped with a `tracing::debug!` log.
///
/// Closes the Sprint 20.5 cross-validation outbound info-leak finding:
/// daemon stall / recovery / crash / CI notices were calling
/// `ch.notify()` directly, which on Telegram pushes the message to the
/// bound group regardless of allowlist state — leaking PTY tails (40
/// lines per stall) to anyone added to an unconfigured group. The gate
/// fails-closed so legacy deployments with `user_allowlist == None`
/// stop emitting outbound info; operators must explicitly configure
/// `user_allowlist: [...]` (a Phase-2-aligned action) to opt in.
pub fn gated_notify(
    channel: &dyn Channel,
    instance: &str,
    severity: NotifySeverity,
    message: &str,
    silent: bool,
) -> std::result::Result<(), ChannelError> {
    // #1339 PR-2: mode gate at the single Telegram chokepoint. `get()` reads
    // the process-global operator-mode snapshot the daemon reloads each tick
    // (reload-coherent); outside the daemon it defaults to `Active`, so this is
    // a no-op for tests and one-off callers. Placed before the authorization
    // gate so a mode-suppressed notice is dropped without touching the channel.
    let mode = crate::operator_mode::get().mode;
    if !should_notify_in_mode(severity, mode) {
        tracing::debug!(
            instance,
            ?severity,
            ?mode,
            "gated_notify: suppressed by operator mode"
        );
        return Ok(());
    }

    if !channel.outbound_authorized() {
        // Sprint 22 P1.5 (Candidate 4 from PR #229 P1 dispatch): the
        // previous `tracing::debug!` was invisible at default
        // `RUST_LOG=info` so operators didn't see the gate firing when
        // stall / crash / CI notices were silently dropped.
        // `warn_once_user_allowlist_unconfigured` upgrades to FATAL
        // (`error!`) once-per-channel:instance pair so operators see an
        // operator-actionable copy-paste fleet.yaml stanza when the
        // allowlist is unconfigured. Sprint 23 P1 retired the
        // outbound-caps sister helper because that gate is now default-
        // open; the channel-level allowlist gate is still fail-closed
        // (different threat model — silent notification fan-out).
        auth::warn_once_user_allowlist_unconfigured(channel.kind(), instance);
        return Ok(());
    }
    channel.notify(instance, severity, message, silent)
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

    /// #1339 PR-2: the full (mode × severity) policy grid. Active passes
    /// everything (M2 hard-true); Away suppresses only Info; Sleep passes only
    /// Error (the P0 tier #1552 AwaitingOperator rides on).
    #[test]
    fn should_notify_in_mode_policy_grid() {
        use crate::operator_mode::OperatorMode::{Active, Away, Sleep};
        use NotifySeverity::{Error, Info, Warn};

        // Active — every severity passes, unconditionally.
        for sev in [Info, Warn, Error] {
            assert!(
                should_notify_in_mode(sev, Active),
                "Active must pass {sev:?}"
            );
        }

        // Away — Info suppressed; Warn + Error pass.
        assert!(!should_notify_in_mode(Info, Away), "Away suppresses Info");
        assert!(should_notify_in_mode(Warn, Away), "Away passes Warn");
        assert!(should_notify_in_mode(Error, Away), "Away passes Error");

        // Sleep — only Error breaks through.
        assert!(!should_notify_in_mode(Info, Sleep), "Sleep suppresses Info");
        assert!(!should_notify_in_mode(Warn, Sleep), "Sleep suppresses Warn");
        assert!(
            should_notify_in_mode(Error, Sleep),
            "Sleep passes Error (P0 — AwaitingOperator / crash)"
        );
    }

    /// Mock channel that records every `notify` call so tests can pin
    /// the gate's pass / drop semantics. Used by the [`gated_notify`]
    /// tests below.
    struct RecordingChannel {
        caps: ChannelCapabilities,
        outbound_ok: bool,
        notify_count: std::sync::atomic::AtomicUsize,
    }

    impl RecordingChannel {
        fn new(outbound_ok: bool) -> Self {
            Self {
                caps: ChannelCapabilities::default(),
                outbound_ok,
                notify_count: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn count(&self) -> usize {
            self.notify_count.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl Channel for RecordingChannel {
        fn kind(&self) -> &'static str {
            "recording"
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
        fn outbound_authorized(&self) -> bool {
            self.outbound_ok
        }
        fn notify(
            &self,
            _instance: &str,
            _severity: NotifySeverity,
            _message: &str,
            _silent: bool,
        ) -> std::result::Result<(), ChannelError> {
            self.notify_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn gated_notify_drops_when_channel_unauthorized() {
        // Fail-closed default: trait method returns false, gate drops
        // the call; underlying `notify` must NOT be invoked.
        //
        // #1339 PR-2: `Error` so the drop is provably the *authorization* gate
        // firing, not the mode gate — `Error` passes every operator mode, so it
        // always reaches the `outbound_authorized` check this test pins.
        let ch = RecordingChannel::new(false);
        let result = gated_notify(&ch, "agent1", NotifySeverity::Error, "leak-bait", false);
        assert!(matches!(result, Ok(())), "drop must be Ok, got {result:?}");
        assert_eq!(
            ch.count(),
            0,
            "unauthorized channel must NOT receive notify call"
        );
    }

    #[test]
    fn gated_notify_forwards_when_channel_authorized() {
        // Operator opted in (allowlist configured) → channel reports
        // outbound_authorized=true → gate forwards to notify.
        //
        // #1339 PR-2: use `Error` so this test isolates the *authorization*
        // gate from the *mode* gate — `Error` passes every operator mode, so a
        // concurrent operator_mode test leaving the process-global at
        // `Away`/`Sleep` can't drop this notice and flake the assertion. The
        // mode dimension is covered by `should_notify_in_mode_policy_grid`.
        let ch = RecordingChannel::new(true);
        let result = gated_notify(&ch, "agent1", NotifySeverity::Error, "ok", true);
        assert!(result.is_ok(), "authorised forward must succeed");
        assert_eq!(
            ch.count(),
            1,
            "authorised channel must receive exactly 1 notify call"
        );
    }

    #[test]
    fn default_outbound_authorized_is_fail_closed() {
        // Trait default is `false` so an adapter forgetting to opt in
        // doesn't accidentally pass the gate.
        let ch = MockChannel::new();
        assert!(
            !ch.outbound_authorized(),
            "default trait method must fail-closed"
        );
    }

    // ── #1744-M6: escalation channel resolver / multi-channel fan-out ──

    /// Serialize the process-global channel registry across the registry-touching
    /// tests in this module (mirrors `daemon::router` / `mcp::handlers::channel`).
    fn m6_registry_guard() -> parking_lot::MutexGuard<'static, ()> {
        static G: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
        G.lock()
    }

    /// Recording channel with a configurable `kind()` (the registry is keyed by
    /// kind, so a multi-channel test needs distinct kinds) and authorized=true.
    struct KindedRecording {
        kind: &'static str,
        caps: ChannelCapabilities,
        count: std::sync::atomic::AtomicUsize,
    }
    impl KindedRecording {
        fn arc(kind: &'static str) -> Arc<Self> {
            Arc::new(Self {
                kind,
                caps: ChannelCapabilities::default(),
                count: std::sync::atomic::AtomicUsize::new(0),
            })
        }
        fn count(&self) -> usize {
            self.count.load(std::sync::atomic::Ordering::Relaxed)
        }
    }
    impl Channel for KindedRecording {
        fn kind(&self) -> &'static str {
            self.kind
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
        fn outbound_authorized(&self) -> bool {
            true
        }
        fn notify(
            &self,
            _: &str,
            _: NotifySeverity,
            _: &str,
            _: bool,
        ) -> std::result::Result<(), ChannelError> {
            self.count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    /// #1744-M6: an escalation P0 must reach EVERY registered channel — the bug
    /// was `active_channel()` returning `None` with ≥2 channels, silently dropping
    /// the alert. `notify_all_escalation_channels` fans out to all of them.
    #[test]
    fn notify_all_escalation_channels_fans_out_to_every_channel_1744_m6() {
        let _g = m6_registry_guard();
        reset_active_channel_for_test();
        let tg = KindedRecording::arc("telegram-m6");
        let dc = KindedRecording::arc("discord-m6");
        register_active_channel(tg.clone());
        register_active_channel(dc.clone());

        // Precondition: this is exactly the case `active_channel()` drops.
        assert!(
            active_channel().is_none(),
            "active_channel() returns None with 2 channels — the M6 bug"
        );

        let sent = notify_all_escalation_channels("orch", NotifySeverity::Error, "P0", false);
        assert_eq!(sent, 2, "must dispatch to both channels");
        assert_eq!(tg.count(), 1, "telegram got the P0");
        assert_eq!(dc.count(), 1, "discord got the P0");

        reset_active_channel_for_test();
    }

    /// #1744-M6: single-channel returns 1 (unchanged single-fleet behavior); zero
    /// returns 0 (logged drop, no panic).
    #[test]
    fn notify_all_escalation_channels_single_and_zero_1744_m6() {
        let _g = m6_registry_guard();
        reset_active_channel_for_test();
        assert_eq!(
            notify_all_escalation_channels("a", NotifySeverity::Error, "p", false),
            0,
            "zero channels → 0 (drop logged)"
        );
        let only = KindedRecording::arc("solo-m6");
        register_active_channel(only.clone());
        assert_eq!(
            notify_all_escalation_channels("a", NotifySeverity::Error, "p", false),
            1
        );
        assert_eq!(only.count(), 1);
        reset_active_channel_for_test();
    }

    // ---- Phase 5b: Channel::send_from_agent default + AgentOutboundOp ----

    #[test]
    fn default_send_from_agent_returns_not_supported() {
        // Adapters that haven't implemented the Phase 5b agent-callable
        // outbound surface fail closed by default. New adapters (e.g.
        // Discord placeholder per dispatch) inherit `NotSupported`
        // automatically until they explicitly opt in with their own
        // gated impl.
        let ch = MockChannel::new();
        let err = ch
            .send_from_agent(
                "agent1",
                AgentOutboundOp::Reply {
                    text: "hi".to_string(),
                    task_id: None,
                    correlation_id: None,
                },
            )
            .expect_err("default must be NotSupported");
        assert!(
            matches!(err, ChannelError::NotSupported(_)),
            "expected NotSupported, got: {err}"
        );
        assert!(
            err.to_string().contains("send_from_agent"),
            "error must name the missing method: {err}"
        );
    }

    // ── t-20260703164240502572-50899-11: multi-channel behavior tests ──
    // Pins the two remaining bug classes from the active_channel() survey:
    // (a) delete/drop-binding must reach whichever channel actually holds
    //     the binding, not just "the" active_channel() singleton; (b)
    //     ensure_topic_for must fan out topic creation to every registered
    //     channel, not just one.

    /// Recording channel with real (stateful) `has_binding`/`take_binding`
    /// and a configurable `create_topic` outcome — `KindedRecording` above
    /// only mocks `notify`, and `MockChannel`/`RecordingChannel` hardcode
    /// `has_binding` to `false`, so none of the existing mocks can pin
    /// either fix.
    struct BindingTopicChannel {
        kind: &'static str,
        caps: ChannelCapabilities,
        bound: parking_lot::Mutex<std::collections::HashSet<String>>,
        topic_calls: std::sync::atomic::AtomicUsize,
        create_topic_ok: bool,
    }

    impl BindingTopicChannel {
        fn arc(kind: &'static str, create_topic_ok: bool) -> Arc<Self> {
            Arc::new(Self {
                kind,
                caps: ChannelCapabilities::default(),
                bound: parking_lot::Mutex::new(std::collections::HashSet::new()),
                topic_calls: std::sync::atomic::AtomicUsize::new(0),
                create_topic_ok,
            })
        }
        fn bind(&self, instance: &str) {
            self.bound.lock().insert(instance.to_string());
        }
        fn is_bound(&self, instance: &str) -> bool {
            self.bound.lock().contains(instance)
        }
        fn topic_calls(&self) -> usize {
            self.topic_calls.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl Channel for BindingTopicChannel {
        fn kind(&self) -> &'static str {
            self.kind
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
            Ok(())
        }
        fn has_binding(&self, instance: &str) -> bool {
            self.bound.lock().contains(instance)
        }
        fn record_binding(&self, instance: &str, _: BindingRef, _: String) {
            self.bound.lock().insert(instance.to_string());
        }
        fn take_binding(&self, instance: &str) -> Option<BindingRef> {
            if self.bound.lock().remove(instance) {
                Some(BindingRef::new(self.kind, None, ()))
            } else {
                None
            }
        }
        fn attach_registry(&self, _: crate::agent::AgentRegistry) {}
        fn create_topic(&self, name: &str) -> std::result::Result<TopicRef, ChannelError> {
            self.topic_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if self.create_topic_ok {
                // Mirrors the fixed Discord adapter contract
                // (t-20260703164240502572-50899-11 reviewer4 finding): a
                // successful create_topic must leave the instance bound, or
                // the channel it just created is unroutable and uncleanable.
                self.bound.lock().insert(name.to_string());
                Ok(TopicRef {
                    id: format!("{}-{name}", self.kind),
                    channel_kind: ChannelKind::Telegram,
                })
            } else {
                Err(ChannelError::Other(anyhow::anyhow!(
                    "mock create_topic failure"
                )))
            }
        }
    }

    /// Fix #1/#2 (`drop_active_binding` / app-teardown loop): both now call
    /// `drop_binding_on_all_channels`, which must clear the binding
    /// regardless of which registered channel actually holds it — the bug
    /// was gating cleanup on `active_channel()`, which returns `None` with
    /// 2+ channels registered and so silently skipped the drop entirely.
    #[test]
    fn drop_binding_on_all_channels_clears_binding_on_whichever_channel_holds_it() {
        let _g = m6_registry_guard();
        reset_active_channel_for_test();

        let tg = BindingTopicChannel::arc("telegram-drop", true);
        let dc = BindingTopicChannel::arc("discord-drop", true);
        register_active_channel(tg.clone());
        register_active_channel(dc.clone());
        assert!(
            active_channel().is_none(),
            "precondition: 2 channels registered"
        );

        // Instance is bound on discord only (e.g. it joined via Discord).
        dc.bind("agent-x");
        assert!(!tg.is_bound("agent-x"));
        assert!(dc.is_bound("agent-x"));

        drop_binding_on_all_channels("agent-x");

        assert!(
            !dc.is_bound("agent-x"),
            "binding must be cleared on the channel that actually held it"
        );
        assert!(
            !tg.is_bound("agent-x"),
            "no-op on the channel that never had a binding"
        );

        reset_active_channel_for_test();
    }

    /// Fix #3 (`ensure_topic_for`): must create a topic on EVERY registered
    /// channel in a multi-channel fleet, not just the `active_channel()`
    /// singleton (which returns `None` with 2+ channels — new instances
    /// used to get no topic anywhere).
    #[test]
    fn ensure_topic_for_fans_out_to_every_registered_channel() {
        let _g = m6_registry_guard();
        reset_active_channel_for_test();

        let tg = BindingTopicChannel::arc("telegram-topic", true);
        let dc = BindingTopicChannel::arc("discord-topic", true);
        register_active_channel(tg.clone());
        register_active_channel(dc.clone());
        assert!(
            active_channel().is_none(),
            "precondition: 2 channels registered"
        );

        let outcome = ensure_topic_for("agent-y");

        assert!(
            matches!(outcome, TopicOutcome::Created(_)),
            "expected Created, got {outcome:?}"
        );
        assert_eq!(tg.topic_calls(), 1, "telegram must get a create_topic call");
        assert_eq!(dc.topic_calls(), 1, "discord must get a create_topic call");

        reset_active_channel_for_test();
    }

    /// reviewer4 REJECTED finding on the first #2615 pass: fan-out alone isn't
    /// enough — a channel whose `create_topic` creates the platform resource
    /// but never calls `record_binding` (the real bug found in
    /// `DiscordChannel::create_topic`) leaves that channel unroutable
    /// (`channel_for_instance`/inbound can't find it) and uncleanable
    /// (`drop_binding_on_all_channels`'s `take_binding` finds nothing). Pins
    /// the now-fixed contract end-to-end: every channel `ensure_topic_for`
    /// fanned out to must have `has_binding(instance) == true` immediately
    /// after, and `drop_binding_on_all_channels` must clear all of them.
    #[test]
    fn ensure_topic_for_created_topic_is_bound_and_cleanable_on_every_channel() {
        let _g = m6_registry_guard();
        reset_active_channel_for_test();

        let tg = BindingTopicChannel::arc("telegram-bind", true);
        let dc = BindingTopicChannel::arc("discord-bind", true);
        register_active_channel(tg.clone());
        register_active_channel(dc.clone());

        ensure_topic_for("agent-w");

        assert!(
            tg.is_bound("agent-w"),
            "telegram must be routable/cleanable after create_topic"
        );
        assert!(
            dc.is_bound("agent-w"),
            "discord must be routable/cleanable after create_topic — this is \
             exactly the reviewer4 finding: create_topic alone left Discord \
             unbound"
        );

        drop_binding_on_all_channels("agent-w");

        assert!(!tg.is_bound("agent-w"), "cleanup must reach telegram");
        assert!(!dc.is_bound("agent-w"), "cleanup must reach discord");

        reset_active_channel_for_test();
    }

    /// Zero channels → `NoChannel` (happy path, unchanged). All-channels-fail
    /// → `Failed`, only when the registry is non-empty.
    #[test]
    fn ensure_topic_for_no_channel_and_all_fail_cases() {
        let _g = m6_registry_guard();
        reset_active_channel_for_test();

        assert_eq!(ensure_topic_for("agent-z"), TopicOutcome::NoChannel);

        let tg = BindingTopicChannel::arc("telegram-fail", false);
        let dc = BindingTopicChannel::arc("discord-fail", false);
        register_active_channel(tg.clone());
        register_active_channel(dc.clone());

        let outcome = ensure_topic_for("agent-z");
        assert!(
            matches!(outcome, TopicOutcome::Failed(_)),
            "all channels failing must produce Failed, got {outcome:?}"
        );
        assert_eq!(tg.topic_calls(), 1);
        assert_eq!(dc.topic_calls(), 1);

        reset_active_channel_for_test();
    }
}
