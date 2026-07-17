//! Per-tick handlers — first cut of #694 BLOCK 1.
//!
//! The daemon main loop (`run_core` in `src/daemon/mod.rs`) historically
//! inlined every periodic concern in a single 200-line block. This module
//! introduces a thin trait, [`PerTickHandler`], so each periodic concern
//! lives in its own file, owned state and all. The trait is deliberately
//! minimal — pattern relocation, not abstraction.
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
//! Execution: [`crate::daemon::build_default_handlers`] builds the ordered
//! `Vec<Box<dyn PerTickHandler>>` once at startup, and each tick the loop
//! runs it through [`run_handlers_with_panic_guard`] (a panicking handler is
//! isolated, never aborting the tick). Further subsystems move behind the
//! same trait by extending that Vec.
//!
//! #2549 operator-approved deletions (d-20260703021554626467-13): removed
//! `ProgressBackstopHandler` + `ProgressMirrorHandler` (the `progress_mode`
//! runtime-config key they served was retired along with them — see
//! `runtime_config.rs`) and `CrossBoardDepDetectiveHandler` (its underlying
//! `tasks::reconcile_stale_cross_board_claims` logic is untouched, only the
//! per-tick wrapper is gone). `RecoveryDispatcherHandler` converged to
//! Stage1-only (Stage2/3 skeleton removed). 40 → 37 handlers in
//! `build_default_handlers`.

use crate::agent::{AgentRegistry, ExternalRegistry};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

pub(crate) mod assignment_reconcile;
pub(crate) mod backend_exit_detection;
pub(crate) mod canonical_heartbeat;
pub(crate) mod check_schedules;
pub(crate) mod checkout_txn_recover;
pub(crate) mod ci_watch_poll;
pub(crate) mod context_alert;
pub(crate) mod context_handoff;
pub(crate) mod context_thresholds;
pub(crate) mod ephemeral_reap;
pub(crate) mod external_liveness;
pub(crate) mod gc_tick;
pub(crate) mod handoff_timeout;
pub(crate) mod hang_detection;
pub(crate) mod hourly_gc;
pub(crate) mod inbox_maintenance;
pub(crate) mod inbox_stuck;
pub(crate) mod inject_delivery;
pub(crate) mod log_rotation;
pub(crate) mod notification_flush;
pub(crate) mod notification_watchdogs;
pub(crate) mod offline_unread_alert;
pub(crate) mod poll_reminder;
pub(crate) mod pr_state_scan;
pub(crate) mod reclaim;
pub(crate) mod reconcile_backups_gc;
pub(crate) mod recovery_dispatcher;
pub(crate) mod respawn_watchdog;
pub(crate) mod shadow_observe;
pub(crate) mod snapshot;
pub(crate) mod supervisor_trackers;
pub(crate) mod thread_dump;
pub(crate) mod tmp_review_gc;
pub(crate) mod watchdog;
pub(crate) mod workspace_boundary_sweep;
pub(crate) mod worktree_registry_sweep;

pub(crate) use assignment_reconcile::AssignmentReconcileHandler;
pub(crate) use backend_exit_detection::BackendExitDetectionHandler;
pub(crate) use check_schedules::CheckSchedulesHandler;
pub(crate) use ci_watch_poll::CiWatchPollHandler;
pub(crate) use context_thresholds::ContextThresholdsHandler;
pub(crate) use ephemeral_reap::EphemeralReapHandler;
pub(crate) use external_liveness::ExternalLivenessHandler;
pub(crate) use hang_detection::HangDetectionHandler;
pub(crate) use hourly_gc::HourlyGcHandler;
pub(crate) use inbox_maintenance::InboxMaintenanceHandler;
pub(crate) use inject_delivery::InjectDeliveryHandler;
pub(crate) use log_rotation::LogRotationHandler;
pub(crate) use notification_flush::NotificationFlushHandler;
pub(crate) use notification_watchdogs::NotificationWatchdogsHandler;
pub(crate) use pr_state_scan::PrStateScanHandler;
pub(crate) use reclaim::ReclaimHandler;
pub(crate) use recovery_dispatcher::RecoveryDispatcherHandler;
pub(crate) use respawn_watchdog::RespawnWatchdogHandler;
pub(crate) use shadow_observe::ShadowObserveHandler;
pub(crate) use snapshot::SnapshotRotationHandler;
pub(crate) use supervisor_trackers::{
    AntiStallHandler, AutoReleaseHandler, CanonicalDriftHandler, ConflictNotifyHandler,
    DecisionTimeoutHandler, DispatchIdleHandler, HelperStalenessHandler, IdleWatchdogHandler,
    McpRegistryHandler, RetentionHandler, WaitingOnStaleHandler,
};
pub(crate) use thread_dump::ThreadDumpHandler;
pub(crate) use watchdog::WatchdogHandler;
pub(crate) use worktree_registry_sweep::WorktreeRegistrySweepHandler;

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
    fn name(&self) -> &'static str;
    fn run(&self, ctx: &TickContext<'_>);
}

/// #t-watchdog-boot-suppress: boot-grace window for the stale-backlog
/// NOTIFICATION watchdogs (`PollReminder` / `InboxStuck` / `HandoffTimeout`).
/// Those handlers keep their dedup state IN-MEMORY (empty at boot) and their
/// cadence counter resets to 0 each boot (so `should_fire` is true on tick 0).
/// On a daemon restart they would therefore re-fire for the ENTIRE current
/// backlog of unread / unclaimed items on the very first tick — including
/// freshly-respawned agents that simply haven't drained their inbox yet (a
/// false "stuck" alert). Suppressing these handlers for the first
/// `NOTIFICATION_BOOT_GRACE` after construction (≈ daemon boot) closes all three
/// causes at once: the in-memory dedup reset, the counter reset, and the
/// post-restart drain false-positive. 3 min is comfortably longer than agent
/// spawn + inbox drain (~tens of seconds) and far shorter than the watchdogs'
/// own 30-min stuck thresholds, so a genuinely-stuck item still alerts shortly
/// after the grace ends. Mirrors the boot-suppress precedent
/// `pr_state::suppress_stale_terminal_replay`.
pub(crate) const NOTIFICATION_BOOT_GRACE: std::time::Duration = std::time::Duration::from_secs(180);

/// True while still within [`NOTIFICATION_BOOT_GRACE`] of `created_at` (the
/// handler's construction instant ≈ daemon boot). The notification watchdogs
/// early-return from their `run` while this holds.
pub(crate) fn in_boot_grace(created_at: std::time::Instant) -> bool {
    created_at.elapsed() < NOTIFICATION_BOOT_GRACE
}

/// Releases an in-flight re-entrancy flag on drop — so a panic inside an
/// offloaded sweep's background thread still clears the guard. Without it, the
/// manual `in_flight.store(false)` at the end of the spawned closure is skipped
/// when the closure unwinds, leaving `in_flight` stuck `true` forever and the
/// sweep permanently skipped. Shared by the per-tick offload handlers
/// (`worktree_registry_sweep`, `hourly_gc`). #P1-2607 / #2614 follow-up
/// (reviewer non-blocking suggestion).
pub(crate) struct ClearOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl ClearOnDrop {
    /// Takes the shared flag; call AFTER the loop-side re-entrancy `swap(true)`.
    /// The flag is released (`store(false)`) when this guard drops — on normal
    /// completion OR an unwinding panic.
    pub(crate) fn new(in_flight: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self(in_flight)
    }
}

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

// ── #941: per-handler timing observability ─────────────────────────────
//
// `HANDLER_TIMING` accumulates per-handler wall-clock stats so the
// periodic thread-dump (see `thread_dump::ThreadDumpHandler`) can
// surface "which handler is slow". Zero overhead when
// `AGEND_DAEMON_THREAD_DUMP_SECS` is unset: `record_handler_timing`
// early-returns after one cached atomic load.
//
// `RwLock<HashMap>` is the right shape: many writers (one per handler
// per tick — sequential, never contended in practice) + few readers
// (the periodic dump). HashMap key is `&'static str` so no allocation
// per record.

#[derive(Debug, Clone, Default)]
pub(crate) struct HandlerStats {
    pub last_run_at: Option<chrono::DateTime<chrono::Utc>>,
    pub last_duration_ms: u64,
    pub max_duration_ms: u64,
    pub run_count: u64,
}

static HANDLER_TIMING: std::sync::OnceLock<std::sync::RwLock<HashMap<&'static str, HandlerStats>>> =
    std::sync::OnceLock::new();

/// Record one handler's run duration. Called by the main loop's
/// per-handler timing wrapper. No-op when thread-dump is disabled.
pub(crate) fn record_handler_timing(name: &'static str, elapsed: std::time::Duration) {
    if !crate::sync_audit::thread_dump_enabled() {
        return;
    }
    let lock = HANDLER_TIMING.get_or_init(|| std::sync::RwLock::new(HashMap::new()));
    let Ok(mut guard) = lock.write() else {
        return;
    };
    let stats = guard.entry(name).or_default();
    stats.last_run_at = Some(chrono::Utc::now());
    stats.last_duration_ms = elapsed.as_millis() as u64;
    if elapsed.as_millis() as u64 > stats.max_duration_ms {
        stats.max_duration_ms = elapsed.as_millis() as u64;
    }
    stats.run_count = stats.run_count.saturating_add(1);
}

/// Snapshot the current handler-timing map for the periodic dump
/// handler. Cloned because the dump runs on the same thread as the
/// recorders and we want to avoid holding the read lock across
/// formatting.
pub(crate) fn snapshot_handler_timings() -> HashMap<String, HandlerStats> {
    let Some(lock) = HANDLER_TIMING.get() else {
        return HashMap::new();
    };
    let Ok(guard) = lock.read() else {
        return HashMap::new();
    };
    guard
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
}

/// #1002 Phase 1 — per-handler panic isolation for the main-loop
/// dispatch.
///
/// Drives the per-tick handler iteration in two steps:
/// 1. `std::panic::catch_unwind` wraps each `handler.run(...)` call so
///    a panic in handler N does not abort the iteration before
///    handlers N+1..end run on this tick.
/// 2. On a caught panic, log handler identity + panic payload (best-
///    effort `Debug`-formatting of `Box<dyn Any>`) so future
///    `grep "panic"` over `app.log` surfaces the silent failure that
///    #1002 was filed against.
///
/// The handler trait object itself is `Send + Sync`, but `tick_ctx`
/// holds borrowed references — `AssertUnwindSafe` is required.
/// Borrows are not invalidated by unwinding because the catch boundary
/// is per-handler (no shared mutable state crosses the boundary).
///
/// Test seam: `handlers` accepts any `&[Box<dyn PerTickHandler>]`, so
/// the unit test (`per_tick::tests::panicking_handler_does_not_skip_siblings`)
/// constructs a fixture vec with a panicking handler in the middle and
/// asserts the trailing handler runs.
///
/// PR4: the untracked entry is now a `#[cfg(test)]` convenience wrapper. Both
/// tick hosts (daemon `run_core` + owned app) call [`run_handlers_with_progress`]
/// directly (with `None` when diagnostics are off), so the untracked form has no
/// production caller — gating it to `cfg(test)` keeps the existing panic-isolation
/// tests (`per_tick` + `canonical_heartbeat`) unchanged with zero production dead
/// code.
#[cfg(test)]
pub(crate) fn run_handlers_with_panic_guard(
    handlers: &[Box<dyn PerTickHandler>],
    ctx: &TickContext<'_>,
) {
    run_handlers_with_progress(handlers, ctx, None);
}

/// PR4 companion: identical per-handler panic isolation to
/// [`run_handlers_with_panic_guard`], plus optional out-of-band stall tracking.
///
/// When `progress` is `Some`, the current [`Phase::Handler`](crate::daemon::tick_stall)
/// index is published BEFORE each handler runs and advanced AFTER its
/// `catch_unwind` — on both the Ok and the panic paths, so a wedged/panicked
/// handler never leaves stale identity (the next iteration's `enter_handler`, or
/// the closing `enter_post`, IS that transition). The daemon `run_core` and the
/// owned app pass `Some`; the untracked wrapper above and every existing caller
/// pass `None` and are byte-for-byte unaffected.
///
/// The tracker is written ONLY here, on the tick thread; the stall monitor reads
/// it lock-free (see [`crate::daemon::tick_stall`]).
pub(crate) fn run_handlers_with_progress(
    handlers: &[Box<dyn PerTickHandler>],
    ctx: &TickContext<'_>,
    progress: Option<&crate::daemon::tick_stall::TickProgress>,
) {
    for (index, handler) in handlers.iter().enumerate() {
        if let Some(p) = progress {
            p.enter_handler(index as u32);
        }
        let start = std::time::Instant::now();
        let name = handler.name();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handler.run(ctx);
        }));
        record_handler_timing(name, start.elapsed());
        if let Err(payload) = outcome {
            // Best-effort payload Debug; tracing crate stringifies via Debug.
            let payload_str = panic_payload_str(&payload);
            tracing::error!(
                handler = name,
                payload = %payload_str,
                "#1002 per_tick handler panicked — subsequent handlers \
                 in this tick continue; next tick re-invokes this handler"
            );
        }
        // PR4: the handler-identity transition is the NEXT iteration's
        // `enter_handler(index+1)`, or — after the last handler — the closing
        // `enter_post()` below; both run AFTER this catch_unwind (Ok or panic).
    }
    if let Some(p) = progress {
        p.enter_post();
    }
}

/// AUDIT2-007: run the crash-event dispatch arm under the same panic guard the
/// per-tick handlers get (#1002). `handle_clean_exit` / `handle_crash_respawn` do
/// telegram `block_on`, `escalation_persist` and fleet resolve — a panic there
/// would unwind out of `run_core` and take down the whole daemon, escalating one
/// agent's crash into a fleet-wide supervisor outage. Lives here (not in
/// `daemon/mod.rs`) to keep that grandfathered file under its ceiling.
pub(crate) fn dispatch_exit_event_guarded(
    exit_event: crate::agent::AgentExitEvent,
    home: &std::path::Path,
    ctx: &super::DaemonContext,
) {
    use crate::agent::AgentExitEvent;
    let guarded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match exit_event {
        AgentExitEvent::CleanExit(ref name) => {
            super::handle_clean_exit(home, name.as_str(), &ctx.registry, &ctx.configs);
        }
        AgentExitEvent::Crash(observation) => {
            super::crash_respawn::handle_crash_observation(home, &observation, ctx);
        }
    }));
    if let Err(payload) = guarded {
        tracing::error!(
            payload = %panic_payload_str(&payload),
            "AUDIT2-007 crash-event handler panicked — daemon loop continues instead of dying"
        );
    }
}

/// Build the canonical per-tick handler pipeline. Shared by `run_core` (daemon)
/// and `app::run_app` (owned `agend-terminal app`) so both run the IDENTICAL set
/// — the single source of truth that closes the recurring "app hand-picks a
/// subset → silently drops a handler" class (#1002 / #982 / #1719). App filters
/// only an explicit allowlist (see `app::APP_TICK_ALLOWLIST`), and a completeness
/// invariant fails CI if a new handler lands here but neither runs in app nor is
/// allowlisted.
///
/// #2538: relocated verbatim from `daemon::mod` (that grandfathered file was at
/// its LOC ceiling with zero slack) — every `per_tick::X` reference below became
/// bare `X` (already in scope via this module's own re-exports) and the one
/// daemon-level reference (`watchdog::watchdog_dry_run_from_env`) became
/// `super::watchdog::...`; no other change.
pub(crate) fn build_default_handlers(
    daemon_binary_stale: crate::daemon::mcp_registry_watcher::DaemonBinaryStale,
) -> Vec<Box<dyn PerTickHandler>> {
    let watchdog_dry_run = super::watchdog::watchdog_dry_run_from_env();
    // #2127 Phase 1: the inbox-stuck sub-handler (inside NotificationWatchdogsHandler,
    // #2549 W3) and the reclaim handler share one dedup latch so reclaim can clear an
    // agent's repeat-alert entry. Construct the notification-watchdogs handler first,
    // clone its inbox-stuck latch, then move it into the vec at its original position
    // (order preserved) — same shape as before the W3 merge.
    let notification_watchdogs = NotificationWatchdogsHandler::new(30, 30, 12);
    let work_stuck_latch = notification_watchdogs.inbox_stuck_latch();
    // Vec order MUST match the pre-extraction call order (zero-behavior-change guarantee).
    vec![
        Box::new(HangDetectionHandler::new()),
        // #2538: foreground-identity vs configured-backend mismatch detection —
        // grouped with HangDetection (both are "detect an abnormal condition,
        // set health state" concerns). Ordering-independent of RecoveryDispatcher
        // below (that reacts only to `Hung`, never `Unhealthy`).
        Box::new(BackendExitDetectionHandler::new()),
        Box::new(RecoveryDispatcherHandler::new()),
        // #t-777-3: respawn-stuck watchdog — auto-Fresh-restart an agent whose
        // Resume spawn hung (corrupt-session `resume --last`), bounded by a
        // retry cap that escalates a P0 + pause. Recovers via the proven API
        // restart path so it works in BOTH run_core and the live app-mode daemon
        // (where the crash_tx→respawn machinery is inert). Disjoint state class
        // from the Hung ladder above (no crash_tx needed).
        Box::new(RespawnWatchdogHandler::new()),
        Box::new(WatchdogHandler::new(watchdog_dry_run)),
        Box::new(ExternalLivenessHandler::new()),
        // #t-…83936-4: canonical source_repo existence heartbeat — pages the
        // operator if a registered canonical repo vanishes (the 40-min-silent
        // deletion incident). 60-tick backstop; the real-time path is
        // binding_state's `worktree_resolves` (protection ①). Order-independent:
        // reads bound_source_repos + emits alerts, shares no state with others.
        Box::new(canonical_heartbeat::CanonicalHeartbeatHandler::new(60)),
        // #2413 (B): ShadowObserve MUST run immediately BEFORE SnapshotRotation so the
        // snapshot's operated `agent_state` promotion reads THIS tick's `observed_status`
        // (it was previously LAST in the list → the snapshot would read last tick's). The
        // reorder is confirm-first safe:
        //   - ShadowObserve only WRITES `observed_status` + `published_observed`; nothing
        //     else in this list writes those, so there is no write-write hazard, and no
        //     handler except SnapshotRotation (below) reads them.
        //   - Its INPUTS are order-independent of the per-tick sequence: `api_activity` is
        //     written by a BACKGROUND thread (`api_activity_probe::spawn`), `state` /
        //     productive-silence by the PTY-feed thread, hook Evidence by the socket
        //     thread — none are per-tick handlers, so moving ShadowObserve earlier does not
        //     stale them.
        //   - The state-transition handlers (HangDetection / RecoveryDispatcher /
        //     RespawnWatchdog / Watchdog) sit ABOVE this point in BOTH the old and new
        //     layout, so ShadowObserve observes the same post-transition `state` either way.
        // The only behaviour change is the intended one: the snapshot promotes from a fresh
        // (this-tick) `observed_status`.
        Box::new(ShadowObserveHandler::new()),
        Box::new(SnapshotRotationHandler::new()),
        Box::new(CheckSchedulesHandler::new()),
        Box::new(CiWatchPollHandler::new()),
        Box::new(PrStateScanHandler::new()),
        // t-…-17 C12: reconcile the durable reviewer-assignment authority every tick
        // (~10s). Registered right AFTER PrStateScanHandler so, within a tick, it
        // sees the terminal markers the scanner's A7 wire just wrote (the marker
        // write is idempotent + persistent, so ordering is an optimization, not a
        // correctness dependency — the reconciler's A10a restart-repair converges
        // regardless). Bounded work: one pass over the (few) active assignment
        // branches; a store with no reviewer assignments does a single empty
        // read_dir. Runs in app mode too (the live daemon is app-mode).
        Box::new(AssignmentReconcileHandler::new(1)),
        Box::new(InboxMaintenanceHandler::new(60)),
        // #2604: offline-target unread-obligation escalation. Same 60-tick
        // cadence as the inbox sweep it races — an offline/nonexistent target's
        // pending obligations get an operator P0 before `sweep_expired`'s 30-day
        // TTL silently drops them. Independent handler (self-owned dedup latch),
        // NOT folded into InboxMaintenanceHandler (that composite takes only
        // ctx.home; this needs ctx.registry to tell offline from online).
        Box::new(offline_unread_alert::OfflineUnreadAlertHandler::new(60)),
        // #2549 W3: PollReminder (30 ticks) + InboxStuck watchdog (#1491(A), 30
        // ticks — every agent receiving but not draining its inbox; notifies lead,
        // no auto-restart) + HandoffTimeout watchdog (#1491(B), 12 ticks — #1859
        // lowered the cadence to ~2min so the daemon-side RE-NUDGE of the
        // next_after_ci target is timely; the lead ESCALATION stays gated by its
        // own 10min age + 30min re-alert windows, so the faster scan doesn't
        // escalate sooner) collapsed into one registered handler — same three
        // cadences, same thread, mutually independent; each sub-check keeps its
        // own cadence gate / extra state unchanged and is panic-isolated from its
        // siblings inside `NotificationWatchdogsHandler::run` (per-check
        // catch_unwind, replacing the per-handler isolation the outer loop used to
        // provide for these 3 separately).
        Box::new(notification_watchdogs),
        // Daemon-side deferred-notification flush — every tick (~10s). The
        // #1513 busy-gate defers notifications into the queue whose only other
        // flusher is the TUI loop; headless `run_core` (`start --foreground`)
        // has no TUI, so without this handler deferred operator messages
        // strand forever (7 stranded Telegram messages, 2026-06-10). Idle
        // cost per instance: one read_dir of notification-queue/ plus a line
        // count of any existing queue files — trivial at fleet sizes.
        Box::new(NotificationFlushHandler::new(1)),
        Box::new(LogRotationHandler::new(360)),
        Box::new(ThreadDumpHandler::new()),
        // #2549 W1: GcTickHandler (worktree GC + stale ci-watch locks + target/
        // sweep), WorkspaceBoundarySweepHandler (#2158 item 2: hourly
        // stray-managed-worktree sweep, edge-triggered event-log + fleet
        // health count), TmpReviewGcHandler (#1747: slow-cadence backstop GC
        // for stale /tmp review worktrees, mtime > 2d), and
        // ReconcileBackupsGcHandler (#2234 (B) prereq: hourly retention GC for
        // <home>/reconcile-backups/, mtime-age ≥ 14d + per-agent newest-1
        // floor) collapsed into one registered handler — same 360-tick
        // cadence, same thread, mutually independent; each sub-sweep keeps
        // its own cadence gate / extra state unchanged and is panic-isolated
        // from its siblings inside `HourlyGcHandler::run` (per-sweep
        // catch_unwind, replacing the per-handler isolation the outer loop
        // used to provide for these 4 separately). All four run in app mode
        // too (not allowlisted out) since the live daemon is app-mode.
        Box::new(HourlyGcHandler::new(360)),
        // #2550 W5 PR-3: worktree-registry auto-cleanup (branches whose PRs
        // merged into main, via the runtime config registry — a different
        // mechanism from the marker-based GC candidates above). Extracted
        // out of InboxMaintenanceHandler (unrelated to inbox concerns,
        // semantically GC) but kept on its OWN 60-tick cadence — SAME value
        // as when it lived inside InboxMaintenanceHandler — per decision Q4:
        // folding it into HourlyGcHandler's 360-tick cadence would regress
        // its cleanup latency from ~10min to ~1h.
        Box::new(WorktreeRegistrySweepHandler::new(60)),
        // #1967 Phase-1 (PR1): reap ephemeral workers every ~1min (6 ticks) —
        // removes/terminates terminal, max-wall-TTL-expired (cost guard), or
        // already-dead workers. Runs in app mode too (not allowlisted out); the
        // live daemon is app-mode. Idle cost: one read of a usually-tiny JSON sidecar.
        Box::new(EphemeralReapHandler::new(6)),
        // #2755: retry stuck checkout-transaction rollbacks (backoff-governed;
        // shares the boot-repair callable — no dedicated worker).
        Box::new(checkout_txn_recover::CheckoutTxnRecoverHandler::new(60)),
        // #2549 W5: ContextAlertHandler (operator-directed: every 6 ticks
        // ~1min, ≥80% orchestrator alert) and ContextHandoffHandler (#2007
        // context-full safety net: every 6 ticks, 85% one-shot
        // [AGEND-HANDOFF] injection to the agent itself, 92% one-shot
        // operator escalation) collapsed into one registered handler. Both
        // read `handle.core.lock().state.resolved_context()` — the agent's
        // in-memory statusline-pattern reading (NOT a transcript-estimate
        // file; #1945-disable retired that fallback) — lock-free during the
        // read. Each keeps its own noise-budgeted latch/hysteresis state
        // (re-alertable vs one-shot-per-episode — genuinely different state
        // machines, not shared) and is panic-isolated from the other inside
        // `ContextThresholdsHandler::run`. Runs in app mode (live daemon).
        Box::new(ContextThresholdsHandler::new(6, 6)),
        // #2044 inject-delivery watchdog: every tick (~10s) verify that an
        // armed actionable wake produced a UserPromptSubmit; re-deliver once
        // + WARN if a dialog swallowed it. Cheap (iterates a usually-empty
        // map); claude-only in practice (arm self-gates on hook history).
        Box::new(InjectDeliveryHandler::new(1)),
        // ── W1.1 (#2050): the 12 trackers migrated from the supervisor
        // `run_loop` (supervisor.rs:384-395). Appended in their original
        // relative order; each self-throttles internally (TICKS_PER_SCAN), so
        // running them every tick here is the same cadence the supervisor ran.
        // They previously executed on the supervisor thread; the main loop
        // ticks at the identical 10s interval and holds no lock across them, so
        // this is behavior-preserving on unix (both run_core and app mode).
        // Cadence-hoist to the handler (`should_fire`) is W2.4, not W1.1.
        //
        // #2549 W2 (P2-2549-SPIKE.md §3d, decision `d-20260702044452277394-4`):
        // `DecisionTimeoutHandler` now also runs the former
        // `DecisionBoardTimeoutHandler`'s scan, and `DispatchIdleHandler` now
        // also runs the former `DispatchIdleNudgeHandler`'s scan — WRAPPER-layer
        // registration-slot merges only (40 → 38 handlers); the four underlying
        // tracker modules (dispatch_idle/mod.rs, dispatch_idle/team_nudge.rs,
        // decision_timeout.rs, decision_board_timeout.rs) are untouched. See
        // `supervisor_trackers.rs` for the per-scan panic-isolation wrapper.
        Box::new(AntiStallHandler::new()),
        Box::new(IdleWatchdogHandler::new()),
        Box::new(DecisionTimeoutHandler::new()),
        Box::new(HelperStalenessHandler::new()),
        Box::new(McpRegistryHandler::new(daemon_binary_stale)),
        Box::new(WaitingOnStaleHandler::new()),
        Box::new(ConflictNotifyHandler::new()),
        Box::new(CanonicalDriftHandler::new()),
        Box::new(AutoReleaseHandler::new()),
        Box::new(DispatchIdleHandler::new()),
        Box::new(RetentionHandler::new()),
        // #2127 Phase 1: reclaim board tasks from agents stuck in a non-recoverable
        // usage_limit window (operator decision d-…085112: Phase 1, grace=10min).
        // Every 30 ticks (~5min). Fires ONLY for UsageLimit/QuotaExceeded with a
        // remaining window > grace and no recent recovery — releases the agent's
        // claimed/in_progress tasks back to Open + clears the work-stuck latch.
        // Runs in both run_core and app mode (live daemon is app-mode).
        Box::new(ReclaimHandler::new(30, work_stuck_latch)),
    ]
}

/// Reduce a panic payload (`Box<dyn Any + Send>`) to a printable
/// string. Mirrors `std::panic::panic_any` conventions: `String` and
/// `&'static str` are the only payloads `panic!()` produces by
/// default; other payloads fall through to a placeholder.
pub(crate) fn panic_payload_str(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Test-only: build a LIVE `AgentHandle` (real openpty, child `cat`) with a
/// default `StateTracker` — so `core.state.resolved_context()` returns `None`
/// (no context statusline parsed). Used by the #latch-prune reverse-regression
/// tests: an agent present in the registry but WITHOUT a context reading this
/// tick must still appear in each handler's `live` set (the latch maps are
/// retained against ALL live agents, not just those with a reading), so its
/// latch is NOT wrongly pruned. Returns the master reader for the caller to
/// keep alive (binding `_reader`). Mirrors `supervisor::tests::mock_agent_handle`.
#[cfg(test)]
pub(crate) fn mock_live_agent_no_context(
    name: &str,
) -> (crate::agent::AgentHandle, Box<dyn std::io::Read + Send>) {
    use std::sync::Arc;
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows: 10,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");
    #[cfg(not(target_os = "windows"))]
    let cmd = portable_pty::CommandBuilder::new("cat");
    #[cfg(target_os = "windows")]
    let cmd = {
        let mut c = portable_pty::CommandBuilder::new("cmd");
        c.args(["/c", "findstr", ".*"]);
        c
    };
    let child = pair.slave.spawn_command(cmd).expect("spawn cat");
    drop(pair.slave);
    let reader = pair.master.try_clone_reader().expect("clone reader");
    let writer = pair.master.take_writer().expect("take writer");
    let pty_writer: crate::agent::PtyWriter = Arc::new(parking_lot::Mutex::new(writer));
    let core = Arc::new(crate::sync_audit::CoreMutex::new(crate::agent::AgentCore {
        vterm: crate::vterm::VTerm::with_pty_writer(80, 10, Arc::clone(&pty_writer)),
        subscribers: Vec::new(),
        state: crate::state::StateTracker::new(None),
        health: crate::health::HealthTracker::new(),
        api_activity: crate::agent::ApiActivity::default(),
        observed_status: None,
    }));
    let handle = crate::agent::AgentHandle {
        id: crate::types::InstanceId::default(),
        name: name.to_string().into(),
        backend_command: "claude".to_string(),
        pty_writer,
        pty_master: Arc::new(parking_lot::Mutex::new(pair.master)),
        published_state: crate::agent::published_state_of(&core),
        published_observed: crate::agent::published_observed_of(&core),
        core,
        child: Arc::new(parking_lot::Mutex::new(child)),
        submit_key: "\r".to_string(),
        inject_prefix: String::new(),
        typed_inject: false,
        spawned_at: std::time::Instant::now(),
        spawned_at_epoch_ms: 0,
        spawn_mode: crate::backend::SpawnMode::Fresh,
        generation: crate::agent::crash_disposition::SpawnGeneration::default(),
        deleted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    (handle, reader)
}

/// Test-only: build a LIVE `AgentHandle` (real openpty, child `cat`) whose
/// `StateTracker` has a REAL, fresh Claude context-percent reading (fed via
/// a synthetic statusline frame, same shape as `state::tests::CLAUDE_STATUSLINE_FRAME`)
/// — so `core.state.resolved_context()` resolves to `Some((pct, "pattern"))`.
/// Sibling of [`mock_live_agent_no_context`] (kept separate rather than
/// parameterizing it, to avoid touching that function's signature and its 6
/// existing call sites across `per_tick`'s test suites — #2549 W5). Used by
/// the `ContextAlertHandler`/`ContextHandoffHandler` merge's cross-
/// independence pin, which needs a live agent whose threshold-crossing
/// decision actually fires.
#[cfg(test)]
pub(crate) fn mock_live_agent_with_context(
    name: &str,
    pct: f32,
) -> (crate::agent::AgentHandle, Box<dyn std::io::Read + Send>) {
    use std::sync::Arc;
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(portable_pty::PtySize {
            rows: 10,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");
    #[cfg(not(target_os = "windows"))]
    let cmd = portable_pty::CommandBuilder::new("cat");
    #[cfg(target_os = "windows")]
    let cmd = {
        let mut c = portable_pty::CommandBuilder::new("cmd");
        c.args(["/c", "findstr", ".*"]);
        c
    };
    let child = pair.slave.spawn_command(cmd).expect("spawn cat");
    drop(pair.slave);
    let reader = pair.master.try_clone_reader().expect("clone reader");
    let writer = pair.master.take_writer().expect("take writer");
    let pty_writer: crate::agent::PtyWriter = Arc::new(parking_lot::Mutex::new(writer));
    let mut state = crate::state::StateTracker::new(Some(&crate::backend::Backend::ClaudeCode));
    state.feed(&format!(
        "  Model: Fable 5 | Ctx Used: {pct:.1}% | ⎇ b | (+0,-0)\n  ⏵⏵ bypass permissions on (shift+tab to cycle)"
    ));
    let core = Arc::new(crate::sync_audit::CoreMutex::new(crate::agent::AgentCore {
        vterm: crate::vterm::VTerm::with_pty_writer(80, 10, Arc::clone(&pty_writer)),
        subscribers: Vec::new(),
        state,
        health: crate::health::HealthTracker::new(),
        api_activity: crate::agent::ApiActivity::default(),
        observed_status: None,
    }));
    let handle = crate::agent::AgentHandle {
        id: crate::types::InstanceId::default(),
        name: name.to_string().into(),
        backend_command: "claude".to_string(),
        pty_writer,
        pty_master: Arc::new(parking_lot::Mutex::new(pair.master)),
        published_state: crate::agent::published_state_of(&core),
        published_observed: crate::agent::published_observed_of(&core),
        core,
        child: Arc::new(parking_lot::Mutex::new(child)),
        submit_key: "\r".to_string(),
        inject_prefix: String::new(),
        typed_inject: false,
        spawned_at: std::time::Instant::now(),
        spawned_at_epoch_ms: 0,
        spawn_mode: crate::backend::SpawnMode::Fresh,
        generation: crate::agent::crash_disposition::SpawnGeneration::default(),
        deleted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    (handle, reader)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use parking_lot::Mutex as PLMutex;
    use std::sync::Arc;

    /// #t-watchdog-boot-suppress: the boot-grace predicate is active for a
    /// just-built handler and expires past the window; the window itself is a
    /// sane value (not zeroed away, not absurdly long).
    #[test]
    fn in_boot_grace_active_then_expires() {
        use std::time::{Duration, Instant};
        assert!(
            in_boot_grace(Instant::now()),
            "a just-constructed handler is within boot-grace"
        );
        assert!(
            !in_boot_grace(Instant::now() - NOTIFICATION_BOOT_GRACE - Duration::from_secs(1)),
            "past the window → no longer in boot-grace"
        );
        assert!(
            (Duration::from_secs(60)..=Duration::from_secs(600)).contains(&NOTIFICATION_BOOT_GRACE),
            "grace must stay a sane window (≥1min to suppress the burst, ≤10min ≪ 30min stuck threshold)"
        );
    }

    /// Source-pin: every notification watchdog handler must wire the boot-grace
    /// gate, so a future edit can't silently drop it and reintroduce the
    /// restart-burst. (Cross-platform file read, survives rustfmt.)
    #[test]
    fn notification_handlers_wire_boot_grace() {
        for file in [
            "src/daemon/per_tick/poll_reminder.rs",
            "src/daemon/per_tick/inbox_stuck.rs",
            "src/daemon/per_tick/handoff_timeout.rs",
        ] {
            let src = std::fs::read_to_string(file)
                .or_else(|_| std::fs::read_to_string(format!("agend-terminal/{file}")))
                .unwrap_or_else(|_| panic!("source must be readable: {file}"));
            assert!(
                src.contains("new_with_boot_grace("),
                "{file} must build its cadence gate via CadenceGate::new_with_boot_grace \
                 (which bundles the boot-suppress window) — #t-watchdog-boot-suppress"
            );
        }
    }

    /// Mock handler with arbitrary `run` behaviour: either records a
    /// hit on a shared counter or panics with a fixed message.
    struct MockHandler {
        name_: &'static str,
        on_run: Box<dyn Fn(&TickContext<'_>) + Send + Sync>,
    }

    impl PerTickHandler for MockHandler {
        fn name(&self) -> &'static str {
            self.name_
        }
        fn run(&self, ctx: &TickContext<'_>) {
            (self.on_run)(ctx);
        }
    }

    #[test]
    fn panicking_handler_does_not_skip_siblings() {
        // #1002 Phase 1 RED→GREEN pin: a panic in handler N MUST NOT
        // abort handler N+1's invocation on this tick. Pre-fix, the
        // panic propagated up the for-loop and silently killed the
        // entire tick (the daemon's `run_core` had no `catch_unwind`
        // around `handler.run(&tick_ctx)`).
        let home = std::env::temp_dir().join(format!("agend-pertick-test-{}", std::process::id()));
        std::fs::create_dir_all(&home).ok();
        let registry: AgentRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(PLMutex::new(HashMap::new()));
        let configs: Arc<PLMutex<HashMap<String, crate::daemon::AgentConfig>>> =
            Arc::new(PLMutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        let before_count = Arc::new(PLMutex::new(0u32));
        let after_count = Arc::new(PLMutex::new(0u32));
        let before_clone = before_count.clone();
        let after_clone = after_count.clone();

        let handlers: Vec<Box<dyn PerTickHandler>> = vec![
            Box::new(MockHandler {
                name_: "before",
                on_run: Box::new(move |_| *before_clone.lock() += 1),
            }),
            Box::new(MockHandler {
                name_: "panicker",
                on_run: Box::new(|_| panic!("#1002 test panic")),
            }),
            Box::new(MockHandler {
                name_: "after",
                on_run: Box::new(move |_| *after_clone.lock() += 1),
            }),
        ];

        run_handlers_with_panic_guard(&handlers, &ctx);

        assert_eq!(*before_count.lock(), 1, "pre-panic handler ran");
        assert_eq!(
            *after_count.lock(),
            1,
            "post-panic handler MUST still run — catch_unwind isolates the panic"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn panic_payload_str_handles_common_panic_types() {
        let string_payload: Box<dyn std::any::Any + Send> = Box::new(String::from("string panic"));
        let static_payload: Box<dyn std::any::Any + Send> = Box::new("static str panic");
        let other_payload: Box<dyn std::any::Any + Send> = Box::new(42u32);

        assert_eq!(panic_payload_str(&string_payload), "string panic");
        assert_eq!(panic_payload_str(&static_payload), "static str panic");
        assert_eq!(
            panic_payload_str(&other_payload),
            "<non-string panic payload>"
        );
    }

    /// #P1-2607 / #2614 follow-up: `ClearOnDrop` releases the in-flight guard on
    /// BOTH normal completion and an unwinding panic — the panic path is the
    /// whole point (the old manual `store(false)` was skipped on unwind, wedging
    /// the guard `true` forever). Proves both.
    #[test]
    fn clear_on_drop_releases_flag_on_normal_and_panic() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        // Normal scope exit.
        let flag = Arc::new(AtomicBool::new(true));
        {
            let _g = ClearOnDrop::new(Arc::clone(&flag));
        }
        assert!(!flag.load(Ordering::Acquire), "guard clears on normal drop");

        // Unwinding panic — the guard must STILL clear.
        let flag = Arc::new(AtomicBool::new(true));
        let flag2 = Arc::clone(&flag);
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = ClearOnDrop::new(flag2);
            panic!("boom");
        }));
        assert!(res.is_err(), "the closure panicked");
        assert!(
            !flag.load(Ordering::Acquire),
            "guard MUST clear on an unwinding panic — the whole reason it exists"
        );
    }
}
