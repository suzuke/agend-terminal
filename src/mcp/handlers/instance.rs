//! `instance` — re-export harness for the instance-handler concept. The
//! handlers are split by concept across sibling modules:
//!
//! - [`super::instance_queries`] — read-only: list / describe.
//! - [`super::instance_state`] — lifecycle: create / delete / start / replace /
//!   restart, with the `spawn` and `lifecycle` submodules that used to be
//!   size-driven top-level files (`instance_spawn.rs` / `instance_lifecycle.rs`).
//! - [`super::instance_metadata`] — per-instance attributes & control:
//!   display-name / description / waiting-on / interrupt / pane / health.
//!
//! Callers (dispatch adapters, tests, `mod.rs`) keep using `instance::handle_*`
//! and `instance::resolve_team_layout` verbatim — this harness re-exports the
//! whole surface so the split is internal.

pub(super) use super::instance_metadata::*;
pub(super) use super::instance_queries::*;
pub(super) use super::instance_state::*;
