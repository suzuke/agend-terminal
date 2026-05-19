//! CI watch — poll CI providers, fan out terminal events to subscribers.
//!
//! Module split (issue #701): the original 4829-LOC `ci_watch.rs` was a single
//! file mixing HTTP clients, poll-loop state machine, sweep/TTL helpers, and
//! a 2792-LOC test block. This module preserves the public API verbatim via
//! `pub use` re-exports while splitting implementation across:
//! - [`registry`] — watch-file helpers (paths, subscriber parsing, atomic writes)
//! - [`provider`] — `CiProvider` trait + GitHub / GitLab / Bitbucket impls
//! - [`sweep`] — TTL GC + rate-limit stall fan-out
//! - [`poller`] — poll loop + dedup helpers + `ci_check_repo` + tests
//! - [`watcher`] — top-level entry point + provider factory

pub(crate) mod migration;
mod poller;
mod provider;
mod registry;
mod sweep;
mod watcher;

/// Watch TTL in hours. Used for both absolute expiry and inactivity threshold.
pub const WATCH_TTL_HOURS: i64 = 72;

// Pre-#701 callers reached these names via `crate::daemon::ci_watch::X`.
// The re-exports preserve that path even when the only in-tree use of
// some items is via the trait object inside `watcher::check_ci_watches`.
#[allow(unused_imports)]
pub use poller::{emit_ci_conflict_alert, watch_start_check_mergeable};
#[allow(unused_imports)]
pub use provider::{
    detect_provider_from_remote, github_token_warning, github_token_warning_from_env,
    BitbucketCiProvider, CiPollResult, CiProvider, CiRun, GitHubCiProvider, GitLabCiProvider,
    MergeableState, PrState,
};
#[allow(unused_imports)]
pub use registry::{ci_watches_dir, remove_watch, watch_filename};
#[allow(unused_imports)]
pub use sweep::{gc_stale_watches, startup_sweep};
pub use watcher::check_ci_watches;

pub(crate) use registry::parse_subscribers;
