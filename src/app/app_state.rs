//! #2453: root `AppState` / bounded `RestartState` owners for `run_app`'s
//! durable render-loop state — re-homed from `mod.rs` (grandfathered
//! anti-monolith ratchet: the parent file may not grow). `pub(super)` is the
//! minimum visibility `run_app` needs; nothing outside `app` sees these.

use super::ui_state::UiState;
use super::{CommitPending, RestartProbe, RunOutcome};
use std::collections::HashMap;

/// #2453 R2: bounded typed owner for the app owner-restart in-flight state.
/// At most one probe at a time (the gate CAS-serializes at the handler).
pub(super) struct RestartState {
    /// Read after the loop to drive the in-place re-exec in `run()`.
    pub(super) restart_outcome: RunOutcome,
    pub(super) restart_probe: Option<RestartProbe>,
    /// #2453 R2 P0-2: the commit-pending state after a passing probe. The loop
    /// NEVER blocks on the transport ack (that would freeze the UI on a wedged
    /// API writer — codex R3): it parks the `flush_ack` receiver here and polls
    /// it each tick.
    pub(super) restart_commit_pending: Option<CommitPending>,
}

/// #2453: root owner of `run_app`'s durable render-loop state. The only
/// mutable lifecycle locals permitted OUTSIDE this struct are `attach_jobs`
/// and `attach_workers` (startup/teardown-scoped). Channels, registries, and
/// RAII guards deliberately stay loose — they are wiring, not loop state.
pub(super) struct AppState {
    /// The five cohesive key/UI-interaction fields, owned by one type.
    /// `name_counter` counts auto-dedup agent names.
    pub(super) ui: UiState,
    /// Remote agent roster (Attached mode). Mirrors `*.port` files the daemon
    /// publishes for each live agent; periodic sync diffs this against the
    /// filesystem so hot-reload-added agents auto-materialize as tabs.
    pub(super) known_remote_agents: std::collections::HashSet<String>,
    /// Placeholder forwarder senders, keyed by pane id, retained until the
    /// matching AttachOutcome is applied (or the pane is closed first).
    pub(super) pending_fwd: HashMap<usize, crossbeam_channel::Sender<Vec<u8>>>,
    /// Flag to trigger resize pass after layout changes (split, close, zoom,
    /// tab switch). Starts true so restored split panes get correct sizes
    /// before first draw.
    pub(super) needs_resize: bool,
    /// Throttle for Attached-mode remote agent discovery. 2s is short enough
    /// that a fleet.yaml reload (daemon tick is 10s) feels timely but long
    /// enough that the readdir cost is trivial.
    pub(super) last_remote_sync: std::time::Instant,
    /// #1479: throttled, change-gated session.json persistence. Graceful exit
    /// already saves; this periodically persists the current layout so a
    /// kill -9 / power loss preserves what's on screen.
    pub(super) last_session_save: std::time::Instant,
    /// Caches the last session.json write to skip no-op rewrites (#1479).
    pub(super) last_session_json: Option<String>,
    /// #t-84833-10 redraw-storm frame cap: rate-limits `terminal.draw` to
    /// ≤1/FRAME_INTERVAL.
    pub(super) last_draw: Option<std::time::Instant>,
    /// Tracks whether anything changed since the last draw (set by every
    /// select! arm) so an idle loop keeps the cheap ~50ms refresh cadence
    /// instead of busy-drawing at 30 fps (#t-84833-10).
    pub(super) dirty: bool,
    /// #84833-15 R2 perf: stamps the last notification-queue disk scan so it
    /// runs at most once per `NOTIF_SYNC_INTERVAL` instead of once per wakeup
    /// (see `should_sync_notifications`). Mirrors `last_draw`'s frame cap.
    pub(super) last_notif_sync: Option<std::time::Instant>,
    /// #2524 P2b / #2313: mirrors `last_notif_sync` for the decision-badge
    /// throttle.
    pub(super) last_decision_sync: Option<std::time::Instant>,
    /// #2524 P2b / #2313: fleet-wide pending-decision total, refreshed
    /// alongside `last_decision_sync`; read at render time by both `render()`
    /// call sites (mirrors how `binary_stale` is snapshotted once per draw).
    pub(super) pending_decisions_total: usize,
    /// #freeze-4 (t-…2324) restart-flood boot phase: until the pre-restart
    /// backlog flood is drained, the loop runs a bounded "loading" phase — a
    /// TIME-capped drain ([`render::drain_all_panes_until`]) that clears the
    /// flood fast while still yielding to input every frame — instead of
    /// letting the steady-state 64 KiB/frame cap trickle it out over ~1s of
    /// interactive freeze. Exits once all deferred attaches are applied AND
    /// every pane's rx is drained, or after `MAX_BOOT_CATCHUP`.
    pub(super) booting: bool,
    /// #2453 R2: app owner-restart in-flight state (bounded typed sub-owner).
    pub(super) restart: RestartState,
}
