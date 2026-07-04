use super::*;
use crate::agent::AgentRegistry;
use crate::channel::{
    BindingRef, Channel, ChannelCapabilities, ChannelError, ChannelEvent, MarkdownDialect,
    MentionStyle, MsgRef, OutMsg, RateBudget,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::mpsc;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Mutable state for the Discord adapter.
pub struct DiscordState {
    /// Instance → channel_id binding registry.
    pub instance_to_channel: HashMap<String, u64>,
    /// Reverse: channel_id → instance name.
    pub channel_to_instance: HashMap<u64, String>,
    /// Submit key per instance (PTY metadata, unused by Discord but
    /// stored to satisfy the `record_binding` contract).
    pub submit_keys: HashMap<String, String>,
    /// Agent registry wired post-bootstrap.
    pub registry: Option<AgentRegistry>,
    /// User allowlist (Discord user snowflakes). `None` = fail-closed.
    pub user_allowlist: Option<Vec<i64>>,
    /// twilight HTTP client for REST API calls. `None` only in test
    /// harness — production `new` always populates it.
    pub http_client: Option<std::sync::Arc<twilight_http::Client>>,
    /// Guild (server) snowflake for binding creation.
    pub guild_id: u64,
}
