//! Per-tick handlers — first cut of #694 BLOCK 1.
//!
//! The daemon main loop (`run_core` in `src/daemon/mod.rs`) historically
//! inlined every periodic concern in a single 200-line block. This module
//! introduces a thin trait, [`PerTickHandler`], so each periodic concern
//! can be moved into its own file, owned state and all, then invoked from
//! the main loop in the same position it occupied before. The trait is
//! deliberately minimal — pattern relocation, not abstraction.
//!
//! This first cut extracts two low-risk subsystems:
//!
//! - [`SnapshotRotationHandler`] (was `mod.rs:644-680`) — owns the
//!   `last_snapshot_json` dedup string that used to live as a loop-local
//!   in `run_core`.
//! - [`PollReminderHandler`] (was `mod.rs:748-758`) — owns the every-N
//!   tick counter that used to live as a function-local `static`.
//!
//! Follow-up PRs (T-B3+) will move further subsystems behind the same
//! trait. Until then, the daemon loop holds the handlers as named locals
//! and calls them at their original sites — a single `Vec<Box<dyn …>>`
//! iteration would reorder execution and is deferred until enough
//! handlers exist for the uniform iteration to be the natural shape.

use crate::agent::AgentRegistry;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

pub(crate) mod poll_reminder;
pub(crate) mod snapshot;

pub(crate) use poll_reminder::PollReminderHandler;
pub(crate) use snapshot::SnapshotRotationHandler;

/// Shared per-tick context. Field types match what the daemon main loop
/// holds verbatim — the trait is pure relocation, not abstraction. Future
/// handlers add fields as their extraction lands.
pub(crate) struct TickContext<'a> {
    pub home: &'a Path,
    pub registry: &'a AgentRegistry,
    pub configs: &'a Arc<Mutex<HashMap<String, super::AgentConfig>>>,
}

/// One periodic concern in the daemon main loop. `run` takes `&self`
/// because handlers are held by reference for the daemon's lifetime;
/// state that needs to mutate across ticks must use interior mutability
/// (`AtomicU64`, `Mutex<…>`, etc.).
pub(crate) trait PerTickHandler: Send + Sync {
    // Reserved for T-B3+ — once a Vec<Box<dyn PerTickHandler>> aggregator
    // exists, this will drive tracing spans / diagnostic dumps. The trait
    // commits to it now so per-handler PRs don't have to re-thread it.
    #[allow(dead_code)]
    fn name(&self) -> &'static str;
    fn run(&self, ctx: &TickContext<'_>);
}
