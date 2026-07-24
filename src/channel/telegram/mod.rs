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
pub(crate) mod poll_supervisor;
pub(crate) mod reply;
pub(crate) mod ux_sink;

pub(crate) use adapter::*;
pub(crate) use bootstrap::*;
pub(crate) use bot_api::*;
pub(crate) use creds::*;
#[allow(unused_imports)]
pub(crate) use error::*;
#[allow(unused_imports)]
pub(crate) use inbound::*;
#[allow(unused_imports)]
pub(crate) use notify::*;
#[allow(unused_imports)]
pub(crate) use reply::*;
#[allow(unused_imports)]
pub(crate) use send::*;
#[allow(unused_imports)]
pub(crate) use state::*;
pub(crate) use topic_registry::*;
