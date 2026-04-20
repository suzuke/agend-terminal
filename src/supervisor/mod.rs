//! Self-healing supervisor.
//!
//! The supervisor is the stable layer that owns the `agend-terminal` daemon
//! child process, handles hot upgrades (binary swap + restart + rollback on
//! failure), and keeps the fleet pulled up after crashes that take down the
//! daemon itself.
//!
//! ## Layering
//!
//! ```text
//! agend-supervisor            ← frozen, small, upgraded rarely
//!   └── agend-terminal daemon ← hot-swappable binary
//!         └── agent PTYs      ← respawned by the daemon after upgrade
//! ```
//!
//! ## Modules
//!
//! - [`ipc`]   — NDJSON protocol types for `$AGEND_HOME/run/supervisor.sock`.
//! - [`paths`] — filesystem layout under `$AGEND_HOME/bin/` used by upgrade.
//! - [`server`]— supervisor's main loop (used by the `agend-supervisor` bin).
//! - [`client`]— CLI-side helpers to talk to the supervisor (used by the
//!   `agend-terminal upgrade` subcommand).
//! - [`self_test`] — in-daemon smoke test triggered by `AGEND_SELF_TEST=1`.

pub mod cli;
pub mod client;
pub mod ipc;
pub mod paths;
pub mod self_test;
pub mod server;
