use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

/// Type-safe notification source — replaces raw string conventions.
pub enum NotifySource<'a> {
    /// Message from a channel user (Telegram, Discord, etc.).
    Channel(&'a str, crate::channel::ChannelKind),
    /// Message from another agent instance (e.g., "dev").
    Agent(&'a str),
    /// System message (e.g., "replace", "ci").
    System(&'a str),
}

impl fmt::Display for NotifySource<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Channel(user, kind) => {
                let kind_str = match kind {
                    crate::channel::ChannelKind::Telegram => "telegram",
                    crate::channel::ChannelKind::Discord => "discord",
                };
                write!(f, "user:{user} via {kind_str}")
            }
            Self::Agent(name) => write!(f, "from:{name}"),
            Self::System(label) => write!(f, "system:{label}"),
        }
    }
}

impl NotifySource<'_> {
    pub(crate) fn reply_hint(&self) -> Cow<'static, str> {
        match self {
            Self::Channel(_, _) => {
                "\n(Reply using the reply tool — do NOT respond with direct text)".into()
            }
            Self::Agent(sender) => {
                format!("\n(Reply using the send tool with instance=\"{sender}\")").into()
            }
            Self::System(_) => "".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InboxMessage {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub id: Option<String>,
    pub from: String,
    pub text: String,
    pub kind: Option<String>,
    pub timestamp: String,
    /// Channel source identity (typed). Additive field replacing the ad-hoc
    /// `kind: "telegram"` misuse from telegram adapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<crate::channel::ChannelKind>,
    #[serde(default)]
    pub read_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// How the message was delivered: "pty" (injected to agent PTY) or
    /// "inbox_fallback" (daemon down, written to inbox file only).
    /// Absent on legacy messages; backwards-compatible via `#[serde(default)]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Force metadata — set when delegate_task used force=true (overrides busy gate).
    /// Serde alias "interrupt_meta" for backwards-compat with Sprint 8-9 inbox JSONL.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "interrupt_meta"
    )]
    pub force_meta: Option<ForceMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_head: Option<String>,
    /// Inbound media attachments (additive). Channel adapters download media
    /// to a local path before enqueue. Empty for text-only messages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<crate::channel::event::Attachment>,
    /// If the user replied to a specific bot message, this is that message's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to_msg_id: Option<String>,
    /// Excerpt of the replied-to message (first 200 chars + author tag).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_reply_to_excerpt: Option<String>,
    /// ID of a newer message that supersedes this one (e.g. ci-watch SHA update).
    /// Messages with superseded_by set are excluded from drain by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// Sender's instance ID (UUIDv4) for audit trail. Display: name (id8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_id: Option<String>,
    /// Sprint 54 layer-5 broadcast visibility: populated when this message
    /// arrived via a `send` broadcast (team / targets / tags fan-out).
    /// Absent on unicast — same conditional pattern as `attachments`. Lets
    /// recipient agents distinguish "broadcast to N peers" from a direct
    /// 1-on-1 message at JSON-metadata vantage; the PTY-inject header
    /// surfaces the same context via `team=` / `broadcast=` fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broadcast_context: Option<BroadcastContext>,
    /// Dispatch schema fields (Issue #649, Phase 1)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequencing: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_minutes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reporting_cadence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_binding_required: Option<bool>,
    /// #1031: pull-request number associated with the message. Populated
    /// by the ci_watch poller on `[ci-pass]` / `[ci-ready-for-action]`
    /// events from the pr_state aggregator cache, so reviewers can post
    /// §3.12 verdict mirrors via `gh pr comment <N>` without a
    /// `gh pr list --head` lookup. Absent on legacy messages and
    /// non-CI events — `#[serde(default)]` ensures backwards-compat
    /// with pre-#1031 JSONL inboxes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    /// #1228: when true, signals that this report is the final/terminal
    /// deliverable for the correlated task. Auto-close fires only when
    /// this flag is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<bool>,
}

/// Metadata attached to a forced delegation (busy gate override).
/// Renamed from InterruptMeta in Sprint 10 (semantic correction).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForceMeta {
    #[serde(alias = "interrupted")]
    pub forced: bool,
    pub reason: String,
    #[serde(alias = "interrupted_at")]
    pub forced_at: String,
}

/// Sprint 54 layer-5 broadcast visibility: routing-time metadata captured at
/// `handle_broadcast` and threaded through the SEND path so the recipient
/// agent can tell broadcast from unicast. `team` is `Some` only for
/// team-based broadcasts; `targets`/`count` are populated for every fan-out
/// (including direct `targets=[…]` / `tags=[…]` modes). Routing behavior is
/// unaffected — this struct is metadata-only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BroadcastContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    #[serde(default)]
    pub targets: Vec<String>,
    #[serde(default)]
    pub count: usize,
}

impl InboxMessage {
    /// Latest schema version this binary can read and write.
    pub const CURRENT_VERSION: u32 = 1;

    pub fn new_system(
        from: impl Into<String>,
        kind: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            from: from.into(),
            text: text.into(),
            kind: Some(kind.into()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        }
    }

    pub fn with_correlation_id(mut self, id: impl Into<String>) -> Self {
        self.correlation_id = Some(id.into());
        self
    }

    pub fn with_reviewed_head(mut self, sha: impl Into<String>) -> Self {
        self.reviewed_head = Some(sha.into());
        self
    }

    pub fn with_delivery_mode(mut self, mode: impl Into<String>) -> Self {
        self.delivery_mode = Some(mode.into());
        self
    }
}

/// Status of a specific inbox message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageStatus {
    /// Message was read at the given timestamp.
    ReadAt(String, Option<String>), // (read_at, delivery_mode)
    /// Message exists but has not been read and has expired (>30d).
    UnreadExpired,
    /// Message not found.
    NotFound,
}
