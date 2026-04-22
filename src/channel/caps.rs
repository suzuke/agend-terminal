//! Channel capabilities — feature matrix consulted by core and UX layers.
//!
//! Per `docs/PLAN-channel-abstraction.md` §3.4, this struct lets core code
//! query "does this channel emit deletion events?" / "what markdown dialect?"
//! via data rather than `if channel.kind() == "telegram"` branches.
//!
//! The struct is split into two regions:
//!
//! - **Transport-layer caps** — consumed by the transport path (rate-limit,
//!   markdown rendering, msg size split).
//! - **UX-layer caps** — consumed by the UX renderer (see
//!   `PLAN-channel-ux-layer.md` §6). Transport path **never** branches on
//!   UX caps directly.
//!
//! **Status (T1 prep scaffold):** fields are defined; readers for the UX
//! region land with the UX renderer in a later PR. The `Default` impl is
//! conservative — everything disabled, MarkdownDialect::None, small rate
//! budget — so adding a new adapter forces explicit opt-in per capability.

/// Declared feature matrix for a channel.
#[derive(Debug, Clone)]
pub struct ChannelCapabilities {
    // ── Transport-layer capabilities ─────────────────────────────────
    /// Does the server push a native deletion event when a binding is
    /// removed? Telegram=false (no event, detected via API error),
    /// Discord=true (`channelDelete`), Slack=true (`channel_deleted`).
    pub emits_deletion_events: bool,
    /// Does the channel support threads / forum topics natively?
    pub threads: bool,
    /// Does the channel support inline buttons?
    pub buttons: bool,
    /// Does the channel accept file attachments on outbound?
    pub attachments: bool,
    /// Markdown dialect for formatted outbound text.
    pub markdown: MarkdownDialect,
    /// Maximum bytes per single outbound message. Core splits at this
    /// boundary before calling `send`.
    pub max_msg_bytes: usize,
    /// Rate budget for outbound calls. Core wraps `send` / `edit` /
    /// `delete` in a shared token-bucket using this budget.
    pub rate_budget: RateBudget,

    // ── UX-layer capabilities (consumed by the UX renderer) ──────────
    /// Does the channel accept emoji reactions?
    pub react: bool,
    /// Does the channel support editing a sent message?
    pub edit: bool,
    /// Does the channel support "typing…" indicators?
    pub typing_indicator: bool,
    /// Does the transport push `MessageEdited` events (Discord yes,
    /// Telegram no)?
    pub receives_edit_events: bool,
    /// Mention syntax the UX renderer should use when referencing
    /// another user.
    pub mention_parsing_hint: MentionStyle,
    /// Can the bot observe read receipts on bound messages?
    pub bot_sees_read_receipts: bool,
    /// If the channel has a native "see all threads" view, a hint for
    /// the UX renderer about how to name / surface it.
    pub has_native_multi_thread_view: Option<NativeSeeAllHint>,
    /// Is the channel ephemeral by design (e.g. an in-TUI adapter where
    /// history does not persist across sessions)?
    pub ephemeral: bool,
}

impl Default for ChannelCapabilities {
    /// Conservative "nothing supported" default. New adapters explicitly
    /// opt-in per capability, which surfaces the feature matrix at
    /// review time.
    fn default() -> Self {
        Self {
            emits_deletion_events: false,
            threads: false,
            buttons: false,
            attachments: false,
            markdown: MarkdownDialect::None,
            max_msg_bytes: 4096,
            rate_budget: RateBudget::default(),

            react: false,
            edit: false,
            typing_indicator: false,
            receives_edit_events: false,
            mention_parsing_hint: MentionStyle::None,
            bot_sees_read_receipts: false,
            has_native_multi_thread_view: None,
            ephemeral: false,
        }
    }
}

/// Outbound markdown dialect. Core formats messages with the right
/// dialect before calling `send`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownDialect {
    /// Telegram MarkdownV2 (strict escape rules).
    MarkdownV2,
    /// Discord / GitHub-flavoured markdown.
    DiscordMd,
    /// Slack mrkdwn (limited subset).
    SlackMrkdwn,
    /// Plain text — no formatting.
    None,
}

/// Mention syntax the UX renderer should emit when tagging another user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionStyle {
    /// `@username` (Slack).
    AtUsername,
    /// `<@snowflake>` (Discord).
    AtSnowflake,
    /// No machine-readable mention syntax (e.g. SMS).
    None,
}

/// Hint for the UX renderer about a channel's native "see all threads"
/// view. Concrete shape kept minimal for the scaffold — expand with
/// per-platform details as the UX layer lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSeeAllHint {
    /// Human-visible label of the native view
    /// (e.g. "View as Messages" on Telegram).
    pub label: String,
}

/// Token-bucket rate budget for outbound calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateBudget {
    pub per_second: u32,
    pub per_minute: u32,
}

impl Default for RateBudget {
    /// Conservative default: 1/s, 20/min. Platform adapters override.
    fn default() -> Self {
        Self {
            per_second: 1,
            per_minute: 20,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_caps_are_conservative() {
        let c = ChannelCapabilities::default();
        assert!(!c.emits_deletion_events);
        assert!(!c.threads);
        assert!(!c.react);
        assert!(!c.edit);
        assert_eq!(c.markdown, MarkdownDialect::None);
        assert_eq!(c.mention_parsing_hint, MentionStyle::None);
        assert!(c.has_native_multi_thread_view.is_none());
    }

    #[test]
    fn rate_budget_default() {
        let r = RateBudget::default();
        assert_eq!(r.per_second, 1);
        assert_eq!(r.per_minute, 20);
    }
}
