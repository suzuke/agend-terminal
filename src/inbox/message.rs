use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

/// Durable identity for a CI handoff. `Protected` is used for exact-head
/// protected-ref watches; `Feature` preserves the ordinary feature-branch
/// notification path. Missing values on legacy rows are intentionally not
/// inferred by settlement code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiHandoffClass {
    Protected,
    Feature,
}

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
    /// #2299 three-state delivery: timestamp the message was DELIVERED to the agent
    /// (drained / injected) but not yet confirmed processed. The state machine is:
    ///   unread     = read_at.is_none() && delivering_at.is_none()
    ///   delivering = read_at.is_none() && delivering_at.is_some()  (in-flight)
    ///   processed  = read_at.is_some()                             (terminal)
    /// A `delivering` message is NOT re-delivered (drain skips it / unread_count
    /// excludes it); a reclaim-TTL sweep resets it to unread if it stays unconfirmed
    /// past the TTL (the agent's turn died before processing). Absent on legacy rows
    /// (`#[serde(default)]`) → reads as unread/processed exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivering_at: Option<String>,
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
    /// task66: typed report purpose. Missing durable rows deserialize only as
    /// LegacyUntyped, which is ordinary task-report data with zero code-review
    /// authority.
    #[serde(
        default,
        skip_serializing_if = "crate::review_receipt::ReportPurpose::is_legacy"
    )]
    pub report_purpose: crate::review_receipt::ReportPurpose,
    /// Constructed only by the authoritative API sink after validating the
    /// active assignment. API-down fallback and caller JSON never populate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validated_code_review: Option<crate::review_receipt::ValidatedCodeReviewReceipt>,
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
    /// Reply-to correlation (Telegram): when the operator quote-replied to a
    /// message the bot previously SENT and that message is found in the
    /// `sent_ledger`, this carries who sent it and its task context — so the
    /// agent knows exactly which prior message + task the operator is responding
    /// to. `None` when the quote isn't in the ledger (e.g. sent before a restart
    /// that predates the ledger, or not a bot message) → the agent still has
    /// `in_reply_to_excerpt` (graceful degrade).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_target: Option<ReplyTargetContext>,
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
    /// t-…-17 reviewer-assignment outbox: opaque delivery generation nonce carried
    /// from [`crate::mcp::handlers`]'s `SendEnvelope`, minted at dispatch (A1) and
    /// rotated on row-repair (A4). `#[serde(default, skip_serializing_if)]` keeps
    /// legacy rows deserializing to `None` AND serializing byte-identically when
    /// `None` — no schema break for pre-outbox inboxes. Distinct from any task/
    /// assignment id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_nonce: Option<String>,
    /// Typed subject delivered by the reviewer-assignment authority store.
    /// Legacy assignment rows omit it and cannot later authorize a receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_assignment: Option<crate::review_receipt::ReviewAssignmentEnvelope>,
    /// Durable CI-handoff episode token. It is minted before a ci-ready row is
    /// enqueued and copied byte-for-byte to the handoff track.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_handoff_episode: Option<String>,
    /// Durable CI-handoff class. Legacy rows omit this and settlement fails
    /// closed rather than guessing protected-vs-feature semantics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_handoff_class: Option<CiHandoffClass>,
}

/// Reply-to correlation context for a quoted bot message (resolved from the
/// `sent_ledger`). Kept as its own struct rather than reusing the top-level
/// `task_id`/`correlation_id` fields, which carry THIS message's own dispatch
/// context — overloading them would conflate "this inbound's task" with "the
/// quoted message's task".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplyTargetContext {
    /// Which agent originally sent the quoted message.
    pub sent_by_agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Excerpt of the original sent message.
    pub excerpt: String,
    /// When the quoted message was sent (RFC3339).
    pub sent_ts: String,
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
    /// #bughunt-r2 #3: message exists, is NOT yet read, and is still live
    /// (within the 30d retention window). The previous code returned
    /// `NotFound` for this — breaking delivery audit of an un-drained message.
    Unread {
        delivery_mode: Option<String>,
        correlation_id: Option<String>,
    },
    /// #2299: message has been DELIVERED to the agent (`delivering_at` set) but
    /// not yet confirmed processed (`read_at` None). Distinct from `Unread` so a
    /// delivery audit (`inbox message_id=…`) does not mistake an in-flight
    /// message for undelivered and re-send it.
    Delivering {
        delivery_mode: Option<String>,
        correlation_id: Option<String>,
    },
    /// Message exists but has not been read and has expired (>30d).
    UnreadExpired,
    /// Message not found.
    NotFound,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::InboxMessage;

    /// t-…-17 (T10): `delivery_nonce` is additive `serde(default, skip)` — a legacy
    /// row without the key deserializes to `None`, and a `None` nonce is OMITTED on
    /// serialize so the JSON is byte-identical to a pre-outbox message (no schema
    /// break, no spurious `"delivery_nonce": null`).
    #[test]
    fn delivery_nonce_serde_default_and_skip_byte_identical() {
        // absent → None (legacy row).
        let legacy =
            r#"{"schema_version":1,"from":"dev","text":"hi","timestamp":"2026-01-01T00:00:00Z"}"#;
        let m: InboxMessage = serde_json::from_str(legacy).unwrap();
        assert!(
            m.delivery_nonce.is_none(),
            "absent key must deserialize None"
        );

        // None → field OMITTED on serialize (not present-null).
        let v = serde_json::to_value(&m).unwrap();
        assert!(
            v.as_object().unwrap().get("delivery_nonce").is_none(),
            "a None nonce must be skipped, got: {v}"
        );

        // Some → present with the value.
        let mut m2 = m.clone();
        m2.delivery_nonce = Some("n-1".to_string());
        let v2 = serde_json::to_value(&m2).unwrap();
        assert_eq!(v2["delivery_nonce"], "n-1");

        // round-trip preserves the nonce.
        let back: InboxMessage = serde_json::from_value(v2).unwrap();
        assert_eq!(back.delivery_nonce.as_deref(), Some("n-1"));
    }
}
