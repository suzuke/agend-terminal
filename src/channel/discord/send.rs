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
// Outbound request body construction
// ---------------------------------------------------------------------------

/// Build the JSON body for `POST /channels/{id}/messages` per Discord spec.
/// Ref: https://discord.com/developers/docs/resources/message#create-message-jsonform-params
///
/// This is the canonical shape our adapter transmits. The test suite
/// asserts this against the spec-quoted example (§3.5.10 outbound
/// request boundary).
pub(crate) fn build_create_message_body(text: &str) -> serde_json::Value {
    serde_json::json!({ "content": text })
}
