//! Normalised inbound events and outbound payload shapes.
//!
//! Per `docs/PLAN-channel-abstraction.md` §3.3, all channels emit the same
//! `ChannelEvent` enum regardless of platform. The unification is the whole
//! point: Telegram's `forum_topic_closed`, Discord's `channelDelete`,
//! and Slack's `channel_deleted` all map to
//! [`ChannelEvent::BindingRevoked`] with an appropriate [`RevokeReason`].
//!
//! **Status (T1 prep scaffold):** types carry minimum-viable fields so the
//! trait compiles. Expansion (reactions, attachment metadata, typing events)
//! lands alongside concrete adapter ports in T1b / Stage B.

use super::BindingRef;
use chrono::{DateTime, Utc};

/// Normalised inbound event — all adapters emit the same variants.
#[derive(Debug, Clone)]
pub enum ChannelEvent {
    /// A user sent a message in a bound "place".
    MessageIn {
        binding: BindingRef,
        from: User,
        payload: MsgPayload,
        ts: DateTime<Utc>,
    },
    /// A user clicked an inline button.
    ButtonClick {
        binding: BindingRef,
        from: User,
        data: String,
    },
    /// The binding is no longer usable (topic deleted / channel removed /
    /// topic closed). Core code should drop any references to this binding.
    BindingRevoked {
        binding: BindingRef,
        reason: RevokeReason,
    },
    /// The underlying transport came online.
    Connected { kind: String, who: String },
    /// The underlying transport went offline.
    Disconnected {
        kind: String,
        reason: Option<String>,
    },
}

/// Why a binding is no longer usable. Maps across platforms:
///
/// | Platform | Native event | Reason |
/// |---|---|---|
/// | Telegram | `forum_topic_closed` | [`RevokeReason::Closed`] |
/// | Telegram | error-driven fallback (topic deleted) | [`RevokeReason::Deleted`] |
/// | Discord | `channelDelete` | [`RevokeReason::Deleted`] |
/// | Slack | `channel_archive` | [`RevokeReason::Archived`] |
/// | Slack | `channel_deleted` | [`RevokeReason::Deleted`] |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeReason {
    Closed,
    Deleted,
    Archived,
    Unknown,
}

/// User identity at the transport layer. Fields are intentionally minimal —
/// richer identity (display name / avatar / roles) is a UX-layer concern.
#[derive(Debug, Clone)]
pub struct User {
    /// Stable, platform-scoped id (Telegram `user.id` as string, Discord
    /// snowflake, Slack `U…`).
    pub id: String,
    /// Optional human-visible handle (may be None on platforms without
    /// usernames, e.g. anonymous SMS).
    pub handle: Option<String>,
}

/// Payload of an inbound message. TODO: expand with attachments, replies,
/// forwarded-from metadata as adapter impls land.
#[derive(Debug, Clone)]
pub struct MsgPayload {
    pub text: String,
    // TODO(T1b+): attachments, reply-to metadata, inline entities.
}

/// Outbound message — the payload passed to `Channel::send` / `Channel::edit`.
/// TODO: expand with buttons, attachments, reply-to once adapters consume it.
#[derive(Debug, Clone, Default)]
pub struct OutMsg {
    pub text: String,
    // TODO(T1b+): buttons, attachments, reply_to, parse mode override.
}

impl OutMsg {
    /// Shorthand for a plain-text outbound message.
    pub fn text(t: impl Into<String>) -> Self {
        Self { text: t.into() }
    }
}

/// Opaque handle to a sent message. Analogous to [`BindingRef`] —
/// the adapter owns the inner shape; core code just hands it back
/// for `edit` / `delete`.
#[derive(Debug, Clone)]
pub struct MsgRef {
    pub binding: BindingRef,
    /// Platform-specific message id (Telegram `message_id` as string,
    /// Discord snowflake, Slack `ts`). Stored as a string so the core
    /// doesn't pick a platform's integer width.
    pub id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revoke_reason_is_copy() {
        // Copy derive keeps pattern matching ergonomic.
        let r = RevokeReason::Deleted;
        let r2 = r;
        assert_eq!(r, r2);
    }

    #[test]
    fn out_msg_text_helper() {
        let m = OutMsg::text("hello");
        assert_eq!(m.text, "hello");
    }
}
