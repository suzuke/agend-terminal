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
    /// #986 round-4: the worker is spawned lazily on the FIRST `run()` (which
    /// carries `ctx.home`, needed for the worker's `auto_arm`). `Once` guarantees
    /// exactly one worker across the handler's lifetime — the single scanner+worker
    /// owner in every mode.
    worker_spawn: std::sync::Once,
}

impl PrStateScanHandler {
    pub(crate) fn new() -> Self {
        Self {
            gh_cache: GhPollCache::new(),
            worker_spawn: std::sync::Once::new(),
        }
    }
}

impl PerTickHandler for PrStateScanHandler {
    fn name(&self) -> &'static str {
        "pr_state_scan"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        // #986: spawn the single background gh-poll worker once (first tick), now
        // that we have `home`. The scanner then reads the worker's cached snapshot
        // (non-blocking) instead of the live `gh pr list` subprocess that blocked
        // this tick.
        self.worker_spawn.call_once(|| {
            crate::daemon::pr_state::gh_poll::spawn_gh_poll_worker(
                ctx.home.to_path_buf(),
                self.gh_cache.clone(),
            );
        });
        let poller = SnapshotGhPoller::new(self.gh_cache.clone());
        crate::daemon::pr_state::scan_and_emit_with(ctx.home, ctx.registry, &poller);
    }
}
