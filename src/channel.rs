//! Channel adapter trait — abstraction layer for messaging platforms.
//!
//! Implementations: Telegram (telegram.rs), future: Discord, Slack, LINE.

use std::path::Path;

/// A message received from a channel.
#[allow(dead_code)]
pub struct IncomingMessage {
    pub from: String,
    pub text: String,
    pub channel_name: String,
    pub instance_name: String,
}

/// Result of sending a message.
#[allow(dead_code)]
pub enum SendResult {
    Sent,
    Failed(String),
}

/// Channel adapter trait — implement for each messaging platform.
#[allow(dead_code)]
pub trait ChannelAdapter: Send + Sync {
    /// Platform name (e.g., "telegram", "discord").
    fn name(&self) -> &str;

    /// Send a reply from an agent to the channel.
    fn send_reply(&self, instance_name: &str, text: &str) -> SendResult;

    /// React to a message with an emoji.
    fn react(&self, instance_name: &str, emoji: &str) -> SendResult {
        let _ = (instance_name, emoji);
        SendResult::Failed("react not supported".into())
    }

    /// Edit a previously sent message.
    fn edit_message(&self, instance_name: &str, message_id: &str, text: &str) -> SendResult {
        let _ = (instance_name, message_id, text);
        SendResult::Failed("edit not supported".into())
    }

    /// Start polling for inbound messages (called once at startup).
    /// Should spawn its own thread/runtime if needed.
    fn start_polling(&self, home: &Path);

    /// Stop polling (called on shutdown).
    fn stop(&self);
}
