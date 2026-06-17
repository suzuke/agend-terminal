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
pub use message::{BroadcastContext, ForceMeta, InboxMessage, MessageStatus, NotifySource};

// Disk health
pub use disk::{check_disk_space, recover_half_writes};

// Storage CRUD (pub)
pub use storage::{
    clear_compact, describe_message, drain, enqueue, find_message, get_thread,
    has_drained_blocker_for_correlation, mark_ci_watch_superseded, sweep_expired, unread_count,
};
// Storage CRUD (pub(crate))
pub(crate) use storage::inbox_path_resolved;

// Notification & PTY injection (pub)
pub use notify::{
    compose_aware_inject, deliver, enqueue_with_idle_hint, format_event_header,
    inject_notification_with_submit, notify_agent, notify_agent_with_attachments, notify_system,
    AGENT_MSG_PREFIX, SYSTEM_MSG_PREFIX,
};
// Notification & PTY injection (pub(crate))
pub(crate) use notify::build_excerpt;

// Items below are only consumed by test code (inbox/tests.rs, poller_tests, etc.)
#[cfg(test)]
pub(crate) use notify::{
    attachment_body_placeholder, enqueue_with_idle_hint_with_emitter,
    should_suppress_911_reinject_with_ledger, summarize_attachments_for_header,
};
#[cfg(test)]
pub use notify::{
    format_header, format_notification_for_inject, pointer_only_inject, HEADER_PREFIX,
    PENDING_HEADER_PREFIX,
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tests.rs"]
mod tests;
