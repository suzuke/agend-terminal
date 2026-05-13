//! Per-tick handlers — first cut of #694 BLOCK 1.
//!
//! The daemon main loop (`run_core` in `src/daemon/mod.rs`) historically
//! inlined every periodic concern in a single 200-line block. This module
//! introduces a thin trait, [`PerTickHandler`], so each periodic concern
//! can be moved into its own file, owned state and all, then invoked from
//! the main loop in the same position it occupied before. The trait is
//! deliberately minimal — pattern relocation, not abstraction.
//!
//! Cumulative extraction state (handlers grow per PR):
//!
//! - [`SnapshotRotationHandler`] (T-B2, was `mod.rs:644-680`) — owns the
//!   `last_snapshot_json` dedup string that used to live as a loop-local.
//! - [`PollReminderHandler`] (T-B2, was `mod.rs:748-758`) — owns the
//!   every-N tick counter that used to live as a function-local `static`.
//! - [`InboxMaintenanceHandler`] (T-B3, was `mod.rs:667-728`) — the
//!   every-60-tick composite of 6 sub-ops; counter moves from
//!   function-local `static AtomicU64` onto the struct.
//! - [`ExternalLivenessHandler`] (T-B3, was `mod.rs:647-658`) — picked
//!   over the watchdog block for blast-radius reasons documented in the
//!   T-B3 PR body. Stateless wrapper around the `externals.retain`
//!   liveness sweep.
//!
//! Follow-up PRs (T-B4+) will move further subsystems behind the same
//! trait. Until then, the daemon loop holds the handlers as named locals
//! and calls them at their original sites — a single `Vec<Box<dyn …>>`
//! iteration would reorder execution and is deferred until enough
//! handlers exist for the uniform iteration to be the natural shape.

use crate::agent::{AgentRegistry, ExternalRegistry};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

pub(crate) mod check_schedules;
pub(crate) mod ci_watch_poll;
pub(crate) mod external_liveness;
pub(crate) mod hang_detection;
pub(crate) mod inbox_maintenance;
pub(crate) mod poll_reminder;
pub(crate) mod snapshot;
pub(crate) mod watchdog;

pub(crate) use check_schedules::CheckSchedulesHandler;
pub(crate) use ci_watch_poll::CiWatchPollHandler;
pub(crate) use external_liveness::ExternalLivenessHandler;
pub(crate) use hang_detection::HangDetectionHandler;
pub(crate) use inbox_maintenance::InboxMaintenanceHandler;
pub(crate) use poll_reminder::PollReminderHandler;
pub(crate) use snapshot::SnapshotRotationHandler;
pub(crate) use watchdog::WatchdogHandler;

/// Shared per-tick context. Field types match what the daemon main loop
/// holds verbatim — the trait is pure relocation, not abstraction. New
/// fields are added as a handler's extraction lands; existing handlers
/// are unaffected because all fields are borrowed references.
pub(crate) struct TickContext<'a> {
    pub home: &'a Path,
    pub registry: &'a AgentRegistry,
    pub externals: &'a ExternalRegistry,
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
