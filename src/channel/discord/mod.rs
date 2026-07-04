//! Discord adapter — behind the `discord` feature gate.
//!
//! The full outbound REST surface (send/edit/delete/create_binding/
//! remove_binding), gateway protocol parsing (HELLO/IDENTIFY/HEARTBEAT/READY/
//! MESSAGE_CREATE mapping), binding lifecycle, and capability matrix shipped
//! 2026-04-29 (PR1-4, #316-319). #2562 P0 adds the piece that was missing
//! since then: [`start_gateway`] actually opens the live WebSocket to
//! Discord's gateway (via `twilight_gateway::Shard`) and feeds real events
//! through the mapping functions above — see `DISCORD-COMPLETION-SPIKE.md`
//! for the full gap analysis. Bootstrap wiring (constructing a `DiscordChannel`
//! from `ChannelConfig::Discord` and calling `start_gateway`) is #2562 P1.
//!
//! #2562 P5: split from a single `discord.rs` into per-concern files,
//! mirroring `channel::telegram`'s module layout.

pub(crate) mod adapter;
pub(crate) mod bootstrap;
pub(crate) mod gateway;
pub(crate) mod inbound;
pub(crate) mod keepalive;
pub(crate) mod protocol;
pub(crate) mod send;
pub(crate) mod state;

#[cfg(test)]
mod tests;

pub(crate) use adapter::*;
pub(crate) use bootstrap::*;
pub(crate) use gateway::*;
pub(crate) use inbound::*;
pub(crate) use keepalive::*;
pub(crate) use protocol::*;
pub(crate) use send::*;
pub(crate) use state::*;
