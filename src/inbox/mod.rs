//! Per-agent message inbox — append-only JSONL with disk resilience.
//!
//! Messages stored as one JSON object per line in {home}/inbox/{name}.jsonl.
//!
//! Resilience layers:
//! - **Readonly mode**: when available disk space < 1 GiB (customizable via AGEND_LOW_DISK_THRESHOLD in bytes), enqueue returns an
//!   error while drain continues to work (let agents consume backlog).
//! - **Append durability**: each enqueue is an in-place flock'd append + fsync
//!   (NOT tmp+rename — that path is used only by the read-modify-write
//!   rewriters: drain/sweep/clear/supersede). A crash can leave a half-written
//!   trailing line, which read paths skip and `recover_half_writes` quarantines
//!   at startup.
//! - **Half-write recovery**: on startup, stale `.tmp` files and corrupt
//!   JSONL lines are moved to `inbox.recovery/` for forensics.

mod disk;
pub mod message;
pub mod notify;
pub mod storage;

// Re-export public API surface so callers using `crate::inbox::X` continue to work.

// Types
pub use message::{
    BroadcastContext, CiHandoffClass, CiHandoffSettlement, ForceMeta, InboxMessage, MessageStatus,
    NotifySource, ReplyTargetContext,
};

// Disk health
pub use disk::{check_disk_space, recover_half_writes};

// Storage CRUD (pub)
pub use storage::{
    ack, ack_by_correlation, clear_compact, describe_message, drain, enqueue, find_message,
    get_thread, has_drained_blocker_for_correlation, inbox_agent_names, mark_ci_watch_superseded,
    obligation_reason, reclaim_stale_delivering, settle_delivering_for_session_reset,
    sweep_expired, unread_count, unread_count_after_discharge, unread_obligation_summary,
    UnreadObligationSummary,
};
// Storage CRUD (pub(crate))
pub(crate) use storage::inbox_path_resolved;

// Notification & PTY injection (pub)
#[cfg(test)]
pub use notify::deliver;
pub use notify::{
    compose_aware_inject, enqueue_with_idle_hint, format_event_header,
    inject_notification_with_submit, notify_agent, notify_agent_with_attachments, notify_system,
    stamp_message_id, wake_persisted_pointer, AGENT_MSG_PREFIX, SYSTEM_MSG_PREFIX,
};
// Notification & PTY injection (pub(crate))
pub(crate) use notify::build_excerpt;

// Items below are only consumed by test code (inbox/tests.rs, poller_tests, etc.)
#[cfg(test)]
pub(crate) use notify::{
    attachment_body_placeholder, enqueue_with_idle_hint_with_emitter,
    should_suppress_911_reinject_with_ledger, summarize_attachments_for_header,
    with_captured_pointer_wakes,
};
#[cfg(test)]
pub use notify::{
    format_header, format_notification_for_inject, pointer_only_inject, HEADER_PREFIX,
    PENDING_HEADER_PREFIX,
};

/// Settle the SENDER's own parent inbox row after a confirmed-successful
/// parented inter-agent send, so an answered obligation stops cycling
/// delivering→unread (reclaim TTL) and re-nagging via poll-reminder. Mirrors
/// the channel targeted-reply path (`mcp/handlers/channel.rs` #2622 Fork C).
/// Kind-independent, sender-scoped, idempotent. NO warn on the returned bool
/// (`false` = already-read/not-found/no-transition, all benign); genuine
/// lock/write failures are already warned inside `storage::settle_read_by_id`.
pub(crate) fn settle_parent_after_successful_send(
    home: &std::path::Path,
    sender: &str,
    parent_id: Option<&str>,
) {
    if let Some(pid) = parent_id {
        let _ = storage::settle_read_by_id(home, sender, pid);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests.rs"]
mod tests;
