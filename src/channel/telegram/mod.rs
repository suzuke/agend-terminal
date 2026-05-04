//! Telegram adapter — runs in dedicated thread with tokio runtime.
//!
//! Inbound: Telegram message → inbox + PTY notification
//! Outbound: reply(text) → Telegram send_message to topic

// PR 3a sub-modules
pub(crate) mod error;
pub(crate) mod inbound;
pub(crate) mod send;
pub(crate) mod state;
pub(crate) mod topic_registry;

// PR 3b sub-modules
pub(crate) mod adapter;
pub(crate) mod bootstrap;
pub(crate) mod bot_api;
pub(crate) mod creds;
pub(crate) mod notify;
pub(crate) mod reply;

pub(crate) use adapter::*;
pub(crate) use bootstrap::*;
pub(crate) use bot_api::*;
pub(crate) use creds::*;
pub(crate) use error::*;
pub(crate) use inbound::*;
pub(crate) use notify::*;
pub(crate) use reply::*;
pub(crate) use send::*;
pub(crate) use state::*;
pub(crate) use topic_registry::*;
