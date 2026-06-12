//! W1.1 (#2050 REFACTOR-PLAN, survey 01-R2): the 12 periodic trackers that
//! historically ran inline in the supervisor thread's `run_loop`
//! (`supervisor.rs:266`, lines 384–395) wrapped as [`PerTickHandler`]s so the
//! daemon has ONE periodic-work pipeline (`build_default_handlers`) instead of
//! two parallel mechanisms (the trait handler set + the supervisor inline
//! calls). This is the #694 extraction finished for the supervisor side and
//! the unification survey 01-R2 / S3 flagged.
//!
//! ## Pure relocation (zero behavior change)
//!
//! Each wrapper owns its tracker behind a `Mutex` (the trait's `run(&self)`
//! requires interior mutability; the original `run_loop` held them as `&mut`
//! locals) and calls `maybe_scan` / `maybe_sweep` EVERY tick. The trackers
//! self-throttle internally via their own `TICKS_PER_SCAN` counter — so wrapping
//! them with NO additional handler-level cadence gate preserves the exact scan
//! cadence. Hoisting that cadence onto the handler (the `should_fire` pattern the
//! other handlers use) is deliberately deferred to W2.4 (`CadenceGate`); W1.1 is
//! relocation only.
//!
//! The main loop ticks at the same 10s interval the supervisor did
//! (`daemon/mod.rs` `build_tick_infrastructure` tick producer ≡ `supervisor::TICK`),
//! and the trackers take no lock held by the loop, so moving them from the
//! supervisor thread to the main-loop handler thread is behavior-preserving on
//! unix (both `run_core` and app mode). The one platform delta — Windows
//! `run_core` (headless `start --foreground`), whose supervisor is
//! `#[cfg(unix)]`-gated and thus never ran these trackers — is documented in the
//! W1.1 PR body: the universal handler set now runs them there too, matching
//! app mode (the live daemon, which already ran them on every platform). That
//! closes a latent #1720-class wiring gap rather than introducing one.

use super::{PerTickHandler, TickContext};
use crate::daemon::anti_stall::AntiStallTracker;
use crate::daemon::auto_release::AutoReleaseTracker;
use crate::daemon::canonical_drift::CanonicalDriftTracker;
use crate::daemon::conflict_notify::ConflictNotifyTracker;
use crate::daemon::decision_timeout::DecisionTimeoutTracker;
use crate::daemon::dispatch_idle::team_nudge::DispatchIdleNudgeTracker;
use crate::daemon::dispatch_idle::DispatchIdleTracker;
use crate::daemon::helper_staleness_watchdog::HelperStalenessWatchdogTracker;
use crate::daemon::idle_watchdog::IdleWatchdogTracker;
use crate::daemon::mcp_registry_watcher::{DaemonBinaryStale, McpRegistryWatcherTracker};
use crate::daemon::retention::RetentionSupervisor;
use crate::daemon::waiting_on_stale::WaitingOnStaleTracker;
use parking_lot::Mutex;
use std::collections::HashSet;

/// Sprint 59 Wave 1 PR-1 (#9): per-task ETA stall scan, throttled to 5min.
pub(crate) struct AntiStallHandler {
    tracker: Mutex<AntiStallTracker>,
}
impl AntiStallHandler {
    pub(crate) fn new() -> Self {
        Self {
            tracker: Mutex::new(AntiStallTracker::default()),
        }
    }
}
impl PerTickHandler for AntiStallHandler {
    fn name(&self) -> &'static str {
        "anti_stall"
    }
    fn run(&self, ctx: &TickContext<'_>) {
        self.tracker.lock().maybe_scan(ctx.home);
    }
}

/// Sprint 59 Wave 1 PR-2 (#10+#12): per-agent + fleet idle thresholds, 5min.
pub(crate) struct IdleWatchdogHandler {
    tracker: Mutex<IdleWatchdogTracker>,
}
impl IdleWatchdogHandler {
    pub(crate) fn new() -> Self {
        Self {
            tracker: Mutex::new(IdleWatchdogTracker::default()),
        }
    }
}
impl PerTickHandler for IdleWatchdogHandler {
    fn name(&self) -> &'static str {
        "idle_watchdog"
    }
    fn run(&self, ctx: &TickContext<'_>) {
        self.tracker.lock().maybe_scan(ctx.home);
    }
}

/// Sprint 59 Wave 1 PR-4-recover (B): operator-decision auto-default on
/// timeout, 5min throttle.
pub(crate) struct DecisionTimeoutHandler {
    tracker: Mutex<DecisionTimeoutTracker>,
}
impl DecisionTimeoutHandler {
    pub(crate) fn new() -> Self {
        Self {
            tracker: Mutex::new(DecisionTimeoutTracker::default()),
        }
    }
}
impl PerTickHandler for DecisionTimeoutHandler {
    fn name(&self) -> &'static str {
        "decision_timeout"
    }
    fn run(&self, ctx: &TickContext<'_>) {
        self.tracker.lock().maybe_scan(ctx.home);
    }
}

/// Sprint 59 Wave 2 PR-3 (#13): deployment-cadence helper-staleness ping.
pub(crate) struct HelperStalenessHandler {
    tracker: Mutex<HelperStalenessWatchdogTracker>,
}
impl HelperStalenessHandler {
    pub(crate) fn new() -> Self {
        Self {
            tracker: Mutex::new(HelperStalenessWatchdogTracker::default()),
        }
    }
}
impl PerTickHandler for HelperStalenessHandler {
    fn name(&self) -> &'static str {
        "helper_staleness"
    }
    fn run(&self, ctx: &TickContext<'_>) {
        self.tracker.lock().maybe_scan(ctx.home);
    }
}

/// Sprint 60 W1 PR-2 (#P0-2): daemon-binary hot-reload detector. Flips the
/// shared TUI status-bar flag (`DaemonBinaryStale`) the render loop reads —
/// the handler holds the SAME `Arc` app's TUI created (threaded through
/// `build_default_handlers`), so the move off the supervisor thread keeps the
/// status-bar wiring intact. `run_core` (headless) passes a throwaway `Arc`,
/// exactly as before.
pub(crate) struct McpRegistryHandler {
    tracker: Mutex<McpRegistryWatcherTracker>,
    binary_stale: DaemonBinaryStale,
}
impl McpRegistryHandler {
    pub(crate) fn new(binary_stale: DaemonBinaryStale) -> Self {
        Self {
            tracker: Mutex::new(McpRegistryWatcherTracker::default()),
            binary_stale,
        }
    }
}
impl PerTickHandler for McpRegistryHandler {
    fn name(&self) -> &'static str {
        "mcp_registry"
    }
    fn run(&self, _ctx: &TickContext<'_>) {
        self.tracker.lock().maybe_scan(&self.binary_stale);
    }
}

/// `set_waiting_on` staleness scan, 5min.
pub(crate) struct WaitingOnStaleHandler {
    tracker: Mutex<WaitingOnStaleTracker>,
}
impl WaitingOnStaleHandler {
    pub(crate) fn new() -> Self {
        Self {
            tracker: Mutex::new(WaitingOnStaleTracker::default()),
        }
    }
}
impl PerTickHandler for WaitingOnStaleHandler {
    fn name(&self) -> &'static str {
        "waiting_on_stale"
    }
    fn run(&self, ctx: &TickContext<'_>) {
        self.tracker.lock().maybe_scan(ctx.home);
    }
}

/// Phase A: per-tick git-conflict observation + 30min escalation. Needs the
/// registry. The supervisor used to prune this tracker's per-agent state
/// (`retain_active`, `supervisor.rs:430`, the #1923 G5 cleanup-on-delete);
/// that prune moves here with the tracker so a deleted agent's conflict state
/// still drops out next tick.
pub(crate) struct ConflictNotifyHandler {
    tracker: Mutex<ConflictNotifyTracker>,
}
impl ConflictNotifyHandler {
    pub(crate) fn new() -> Self {
        Self {
            tracker: Mutex::new(ConflictNotifyTracker::default()),
        }
    }
}
impl PerTickHandler for ConflictNotifyHandler {
    fn name(&self) -> &'static str {
        "conflict_notify"
    }
    fn run(&self, ctx: &TickContext<'_>) {
        self.tracker.lock().maybe_scan(ctx.home, ctx.registry);
        // #1923 G5 cleanup-on-delete (was supervisor.rs:425-430): prune
        // per-agent conflict state for agents no longer in the registry. Runs
        // every tick, unconditionally, exactly as the supervisor `.retain` did.
        let live: HashSet<String> = {
            let reg = crate::agent::lock_registry(ctx.registry);
            reg.values().map(|h| h.name.to_string()).collect()
        };
        self.tracker.lock().retain_active(&live);
    }
}

/// #852 residual PR-B: per-tick canonical-drift (detached-HEAD residue) scan.
pub(crate) struct CanonicalDriftHandler {
    tracker: Mutex<CanonicalDriftTracker>,
}
impl CanonicalDriftHandler {
    pub(crate) fn new() -> Self {
        Self {
            tracker: Mutex::new(CanonicalDriftTracker::default()),
        }
    }
}
impl PerTickHandler for CanonicalDriftHandler {
    fn name(&self) -> &'static str {
        "canonical_drift"
    }
    fn run(&self, ctx: &TickContext<'_>) {
        self.tracker.lock().maybe_scan(ctx.home);
    }
}

/// #870: per-tick auto-release scan (faster ~30s cadence, internal).
pub(crate) struct AutoReleaseHandler {
    tracker: Mutex<AutoReleaseTracker>,
}
impl AutoReleaseHandler {
    pub(crate) fn new() -> Self {
        Self {
            tracker: Mutex::new(AutoReleaseTracker::default()),
        }
    }
}
impl PerTickHandler for AutoReleaseHandler {
    fn name(&self) -> &'static str {
        "auto_release"
    }
    fn run(&self, ctx: &TickContext<'_>) {
        self.tracker.lock().maybe_scan(ctx.home);
    }
}

/// PR1 watchdog L1: cross-team-safe dispatch-idle scan (~60s, internal).
pub(crate) struct DispatchIdleHandler {
    tracker: Mutex<DispatchIdleTracker>,
}
impl DispatchIdleHandler {
    pub(crate) fn new() -> Self {
        Self {
            tracker: Mutex::new(DispatchIdleTracker::default()),
        }
    }
}
impl PerTickHandler for DispatchIdleHandler {
    fn name(&self) -> &'static str {
        "dispatch_idle"
    }
    fn run(&self, ctx: &TickContext<'_>) {
        self.tracker.lock().maybe_scan(ctx.home);
    }
}

/// PR1 watchdog L2: generic per-team auto-nudge on exceeded dispatches.
pub(crate) struct DispatchIdleNudgeHandler {
    tracker: Mutex<DispatchIdleNudgeTracker>,
}
impl DispatchIdleNudgeHandler {
    pub(crate) fn new() -> Self {
        Self {
            tracker: Mutex::new(DispatchIdleNudgeTracker::default()),
        }
    }
}
impl PerTickHandler for DispatchIdleNudgeHandler {
    fn name(&self) -> &'static str {
        "dispatch_idle_nudge"
    }
    fn run(&self, ctx: &TickContext<'_>) {
        self.tracker.lock().maybe_scan(ctx.home);
    }
}

/// Retention sweep (review-task / tmp GC supervisor).
pub(crate) struct RetentionHandler {
    tracker: Mutex<RetentionSupervisor>,
}
impl RetentionHandler {
    pub(crate) fn new() -> Self {
        Self {
            tracker: Mutex::new(RetentionSupervisor::default()),
        }
    }
}
impl PerTickHandler for RetentionHandler {
    fn name(&self) -> &'static str {
        "retention"
    }
    fn run(&self, ctx: &TickContext<'_>) {
        self.tracker.lock().maybe_sweep(ctx.home);
    }
}

/// The 12 supervisor trackers migrated by W1.1, in the relative order they ran
/// in the supervisor `run_loop` (supervisor.rs:384-395, pre-W1.1). The
/// completeness invariant below pins this exact list against the registered
/// handler set.
#[cfg(test)]
pub(crate) const MIGRATED_TRACKER_NAMES: &[&str] = &[
    "anti_stall",
    "idle_watchdog",
    "decision_timeout",
    "helper_staleness",
    "mcp_registry",
    "waiting_on_stale",
    "conflict_notify",
    "canonical_drift",
    "auto_release",
    "dispatch_idle",
    "dispatch_idle_nudge",
    "retention",
];

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::MIGRATED_TRACKER_NAMES;

    /// W1.1 completeness invariant (#2050; the #1002 / #1719 app-mode-wiring-
    /// drift class applied to the supervisor → handler migration).
    ///
    /// Every tracker moved off the supervisor `run_loop` MUST be registered in
    /// `build_default_handlers`. The existing `app_tick_handlers_cover_*`
    /// invariant CANNOT catch a dropped migration: a forgotten tracker is absent
    /// from BOTH the daemon and app sets, so their set-difference stays empty and
    /// that test stays green. This one pins the full expected set against the
    /// registered handler names directly, so dropping (or never adding) a tracker
    /// fails CI. It also asserts the 12 keep their original RELATIVE order —
    /// handler order in the `build_default_handlers` Vec is load-bearing.
    #[test]
    fn all_twelve_supervisor_trackers_registered_in_order() {
        let (crash_tx, _rx) = crossbeam_channel::bounded(1);
        let stale: crate::daemon::mcp_registry_watcher::DaemonBinaryStale =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let names: Vec<&str> = crate::daemon::build_default_handlers(crash_tx, true, stale)
            .iter()
            .map(|h| h.name())
            .collect();

        // Present.
        for n in MIGRATED_TRACKER_NAMES {
            assert!(
                names.contains(n),
                "supervisor tracker '{n}' must be registered as a PerTickHandler (W1.1) — got {names:?}"
            );
        }

        // Relative order preserved (subsequence of the registered Vec).
        let positions: Vec<usize> = MIGRATED_TRACKER_NAMES
            .iter()
            .map(|n| {
                names
                    .iter()
                    .position(|x| x == n)
                    .unwrap_or_else(|| panic!("'{n}' present"))
            })
            .collect();
        let mut sorted = positions.clone();
        sorted.sort_unstable();
        assert_eq!(
            positions, sorted,
            "the 12 migrated trackers must keep their original relative order (load-bearing); positions={positions:?}"
        );

        // No duplicate handler names (HANDLER_TIMING + the completeness invariant
        // both key on name; a collision would silently merge two handlers).
        let mut uniq = names.clone();
        uniq.sort_unstable();
        let before = uniq.len();
        uniq.dedup();
        assert_eq!(
            before,
            uniq.len(),
            "handler names must be unique — got {names:?}"
        );
    }
}
