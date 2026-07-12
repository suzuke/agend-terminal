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
// AUDIT2-001: re-exported so the `ci watch` MCP boundary can warn the operator
// when an agent-supplied `ci_provider_url` host will NOT receive the forge token.
pub(crate) use provider::host_receives_credentials;
mod registry;
mod sweep;
pub(crate) mod watch_state;
mod watcher;

/// Watch TTL in hours. Used for both absolute expiry and inactivity threshold.
pub const WATCH_TTL_HOURS: i64 = 72;

/// #1750 A2: absolute watch-age cap (anchored on the earliest `subscribed_at`,
/// which is never refreshed by polling) as a backstop against a watch that keeps
/// receiving "active" poll results and so never hits the refreshed `expires_at`
/// / inactivity TTL. A real per-push CI watch goes terminal (and is removed)
/// within minutes-to-an-hour; a watch alive this long never reached terminal and
/// is stale by definition. Generous (7 days) so it can only ever catch genuine
/// leaks, never a live watch.
pub const MAX_WATCH_AGE_HOURS: i64 = 7 * 24;

/// S1 exact-head watch: is `s` a full immutable commit SHA? Accepts a 40-hex
/// (SHA-1) or 64-hex (SHA-256, future object-format repos) hash, case-insensitive.
/// Rejects abbreviated / non-hex / empty — an exact-head protected watch requires
/// an UNAMBIGUOUS immutable target so a later main push can never alias it.
pub fn is_full_commit_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// S1: canonical (lowercase) form of a commit SHA for storage + comparison, so a
/// caller-supplied `HEAD_SHA` matches the provider's lowercase `head_sha`. Callers
/// MUST have validated with [`is_full_commit_sha`] first (this only lowercases).
pub fn normalize_head_sha(s: &str) -> String {
    s.to_ascii_lowercase()
}

// Pre-#701 callers reached these names via `crate::daemon::ci_watch::X`.
// The re-exports preserve that path even when the only in-tree use of
// some items is via the trait object inside `watcher::check_ci_watches`.
#[allow(unused_imports)]
pub use poller::{emit_ci_conflict_alert, watch_start_check_mergeable};
// #972: re-export only consumed by the in-crate pr_state tests; cfg(test)
// gates the clippy unused-imports rule when building production binary.
#[cfg(test)]
pub(crate) use poller::parse_review_class;
pub(crate) use poller::register_subscriber;
#[allow(unused_imports)]
pub use provider::{
    detect_provider_from_remote, github_token_warning, github_token_warning_from_env,
    BitbucketCiProvider, CiPollResult, CiProvider, CiRun, GitHubCiProvider, GitLabCiProvider,
    MergeableState, PrState,
};
#[allow(unused_imports)]
pub use registry::{
    ci_watches_dir, cleanup_watches_for_instance, has_instance_anywhere, reassign_next_after_ci,
    remove_watch, watch_filename, watch_filename_exact_head,
};
#[allow(unused_imports)]
pub use sweep::{gc_stale_watches, startup_sweep};
pub use watcher::check_ci_watches;

pub(crate) use registry::parse_subscribers;
#[allow(unused_imports)]
pub(crate) use watch_state::WatchState;
