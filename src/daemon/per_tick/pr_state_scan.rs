//! #972: per-tick scanner for the PR-state aggregator.
//!
//! Walks `<home>/pr-state/*.json` once per tick, fires
//! `[pr-ready-for-merge]` events for any newly-eligible PR (debounced
//! via `ready_emitted_for_sha`), and sweeps terminal-state files
//! (Merged / ClosedUnmerged) after emitting their close-loop events.
//!
//! Pure delegation to [`crate::daemon::pr_state::scan_and_emit`] —
//! kept as a distinct handler so the per-tick dispatch shape stays
//! uniform.

use super::{PerTickHandler, TickContext};
use crate::daemon::pr_state::gh_poll::{GhPollCache, SnapshotGhPoller};
use std::sync::Arc;

pub(crate) struct PrStateScanHandler {
    /// #986: shared snapshot cache fed by the background gh-poll worker; the
    /// scanner reads it via [`SnapshotGhPoller`] instead of blocking on `gh`.
    gh_cache: Arc<GhPollCache>,
}

impl PrStateScanHandler {
    pub(crate) fn new() -> Self {
        let gh_cache = GhPollCache::new();
        // #986: spawn the single background gh-poll worker ONCE — this handler is
        // constructed once at daemon boot, so the worker is daemon-lifetime. The
        // scanner thread then never blocks on the `gh pr list` subprocess.
        crate::daemon::pr_state::gh_poll::spawn_gh_poll_worker(gh_cache.clone());
        Self { gh_cache }
    }
}

impl PerTickHandler for PrStateScanHandler {
    fn name(&self) -> &'static str {
        "pr_state_scan"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        // #986: read the worker's cached snapshot (non-blocking) instead of the
        // live `gh pr list` subprocess that previously blocked this tick.
        let poller = SnapshotGhPoller::new(self.gh_cache.clone());
        crate::daemon::pr_state::scan_and_emit_with(ctx.home, ctx.registry, &poller);
    }
}
