//! Sprint 60 W1 PR-2 (#P0-2 daemon hot-reload tool registry) —
//! 5th supervisor tracker. Closes the Sprint 59 PR-5 → PR-4
//! chicken-and-egg loop where adding a new MCP tool to the registry
//! required a daemon restart to make the tool callable, and the
//! restart was itself the friction the new tool was meant to remove.
//!
//! ## Why this isn't true compiled-code hot-reload
//!
//! The Rust binary holds both the tool registry (`src/mcp/tools.rs`
//! `tool_definitions()`) and the dispatch handlers in the same
//! linked image — adding a new tool changes both, and an in-process
//! reload of either alone produces a broken state (registered tool
//! with no handler, or vice versa). True hot-swap of compiled code
//! is out of scope for a monolithic Rust daemon.
//!
//! What this module does instead: detect when the on-disk daemon
//! binary at `current_exe()` has been replaced (e.g. by a fresh
//! `cargo install --path . --force`) AFTER the running process
//! started. When that happens, the running process is by definition
//! out of date relative to the install path's MCP tool registry.
//!
//! ## Routing — #1027: TUI status bar instead of agent inbox
//!
//! Prior incarnation: enqueued an inbox alert for `general` + `lead`
//! every 6h while stale, modelled on `helper_staleness_watchdog`.
//! Operator-filed #1027 made the case that this routing is wrong:
//! agents cannot restart the daemon, so the alert just noised their
//! inboxes without delivering an actionable signal to the audience
//! that *can* act (the operator looking at the TUI).
//!
//! Current behaviour: this tracker flips a shared `Arc<AtomicBool>`
//! that the TUI status bar reads on every frame. The indicator stays
//! visible until process restart — sticky-true semantics rather than
//! pulse + dedup. Restarting the daemon resets the flag because the
//! tracker re-captures `started_at` from a fresh process and the
//! freshly-built binary mtime is then ≤ `started_at` (the steady
//! state). No inbox emit on any path.
//!
//! ## Pattern parallel
//!
//! Identical tick-cadence scaffolding to the four prior trackers:
//! `TICKS_PER_REGISTRY_SCAN = 30` (≈5 min cadence), fail-open IO. The
//! per-key re-alert dedup window is gone — sticky-true is its own
//! deduplication. With this module the supervisor coexists with five
//! trackers: AntiStallTracker (#567), IdleWatchdogTracker (#568),
//! DecisionTimeoutTracker (#572), HelperStalenessWatchdog (#576), and
//! this one.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

/// Shared daemon→TUI flag for "running daemon binary is older than the
/// on-disk binary" — read by the status bar so the operator sees a
/// stable indicator instead of a one-shot inbox message routed to
/// agents who cannot act on it (#1027).
pub type DaemonBinaryStale = Arc<AtomicBool>;

/// Scan throttle — matches the four prior trackers so all five fire
/// in the same wall-clock window without interleaving overhead.
pub(crate) const TICKS_PER_REGISTRY_SCAN: u64 = 30;

/// Per-supervisor-loop tracker — captures the process-start time on
/// first init so subsequent scans can detect a daemon binary that has
/// been replaced post-startup.
pub(crate) struct McpRegistryWatcherTracker {
    /// Cadence gate — throttles scans to once per [`TICKS_PER_REGISTRY_SCAN`]
    /// supervisor ticks (fire-on-Nth).
    gate: crate::daemon::cadence_gate::CadenceGate,
    /// Process start time. Captured at tracker init so the comparison
    /// against the daemon binary's mtime reflects "has the binary
    /// been refreshed since this process started".
    started_at: SystemTime,
}

impl Default for McpRegistryWatcherTracker {
    fn default() -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_interval(TICKS_PER_REGISTRY_SCAN),
            started_at: SystemTime::now(),
        }
    }
}

impl McpRegistryWatcherTracker {
    /// Increment tick counter and run the scan every
    /// [`TICKS_PER_REGISTRY_SCAN`] calls.
    pub(crate) fn maybe_scan(&mut self, binary_stale: &AtomicBool) -> bool {
        if !self.gate.fire() {
            return false;
        }
        let daemon_exe = std::env::current_exe().ok();
        scan_and_set_flag(daemon_exe.as_deref(), self.started_at, binary_stale);
        true
    }
}

/// Pure scan logic — exposed for tests so they can invoke without
/// the 30-tick wall-clock wait and with controlled daemon-exe path
/// + process-start time.
///
/// Sticky-true semantics: once a post-start binary refresh is observed
/// the flag stays true for the lifetime of the running process. The
/// only way back to false is a daemon restart (which re-creates the
/// tracker with a fresh `started_at`). This matches the operator's
/// mental model — the indicator means "this running daemon is out of
/// date; restart to pick up the new binary", and a restart is the
/// only thing that genuinely resolves the staleness.
pub(crate) fn scan_and_set_flag(
    daemon_exe: Option<&Path>,
    started_at: SystemTime,
    binary_stale: &AtomicBool,
) {
    let Some(exe) = daemon_exe else {
        // Daemon binary path undeterminable — same fail-open posture
        // as helper_staleness_watchdog. No flag change.
        return;
    };
    let Ok(binary_mtime) = std::fs::metadata(exe).and_then(|m| m.modified()) else {
        return;
    };
    if binary_mtime <= started_at {
        // Binary is older than (or equal to) process start — no
        // post-start replacement has occurred. Leave the flag alone
        // (sticky-true semantics: do NOT clear if a later scan sees
        // the binary roll back, which can happen if the operator
        // re-installs an older version on top of a newer one).
        return;
    }
    binary_stale.store(true, Ordering::SeqCst);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-mcp-registry-watcher-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Plant a fake daemon binary at `path` with the file's mtime
    /// strictly newer than `older_than` so the watcher classifies it
    /// as a post-startup refresh. Mirrors the mtime-ordering pattern
    /// from `cli::helper_staleness_tests` and the PR-3 sibling tracker.
    fn plant_post_start_binary(home: &Path) -> std::path::PathBuf {
        let path = home.join("agend-terminal-fake");
        std::fs::write(&path, b"fake-daemon-binary").unwrap();
        path
    }

    /// #1027 T1: post-start binary replacement must flip the shared
    /// `binary_stale` flag to true (the new contract). The status bar
    /// reads this flag instead of agents picking up a routed inbox
    /// message.
    #[test]
    fn scan_sets_flag_when_binary_replaced() {
        let home = tmp_home("flag-post-start");
        let started_at = SystemTime::now() - Duration::from_secs(10);
        std::thread::sleep(Duration::from_millis(20));
        let exe = plant_post_start_binary(&home);
        let flag = AtomicBool::new(false);
        scan_and_set_flag(Some(&exe), started_at, &flag);
        assert!(
            flag.load(Ordering::SeqCst),
            "post-start binary must flip flag to true"
        );
    }

    /// #1027 T2: binary older than process-start must leave the flag
    /// untouched (fresh daemon — nothing to surface).
    #[test]
    fn scan_leaves_flag_false_when_binary_fresh() {
        let home = tmp_home("flag-pre-start");
        let exe = plant_post_start_binary(&home);
        std::thread::sleep(Duration::from_millis(20));
        let started_at = SystemTime::now();
        let flag = AtomicBool::new(false);
        scan_and_set_flag(Some(&exe), started_at, &flag);
        assert!(
            !flag.load(Ordering::SeqCst),
            "binary older than process-start must keep flag=false"
        );
    }

    /// #1027 T3 anti-regression: even when stale binary detected, the
    /// new code path MUST NOT enqueue inbox messages — that was the
    /// behavior we are removing in this issue.
    #[test]
    fn scan_does_not_emit_inbox_when_setting_flag() {
        let home = tmp_home("flag-no-inbox");
        let started_at = SystemTime::now() - Duration::from_secs(10);
        std::thread::sleep(Duration::from_millis(20));
        let exe = plant_post_start_binary(&home);
        let flag = AtomicBool::new(false);
        scan_and_set_flag(Some(&exe), started_at, &flag);
        let general = crate::inbox::drain(&home, "general");
        let lead = crate::inbox::drain(&home, "lead");
        assert!(
            !general
                .iter()
                .any(|m| m.kind.as_deref() == Some("mcp_registry_watcher")),
            "scan_and_set_flag must not enqueue general inbox: {general:?}"
        );
        assert!(
            !lead
                .iter()
                .any(|m| m.kind.as_deref() == Some("mcp_registry_watcher")),
            "scan_and_set_flag must not enqueue lead inbox: {lead:?}"
        );
    }

    /// #1027 sticky-true: once stale, repeated scans against the same
    /// binary must NOT clear the flag back to false even if mtime
    /// comparison subsequently differs. The flag clears only via
    /// process restart (fresh tracker with a fresh `started_at`).
    #[test]
    fn scan_does_not_clear_flag_once_set() {
        let home = tmp_home("flag-sticky");
        let exe = plant_post_start_binary(&home);
        let flag = AtomicBool::new(true);
        // Scan against a started_at strictly later than the binary's
        // mtime: binary_mtime <= started_at → the function returns
        // early without touching the flag. Sticky-true semantics
        // require the pre-set `true` to survive.
        let later_started_at = SystemTime::now() + Duration::from_secs(60);
        scan_and_set_flag(Some(&exe), later_started_at, &flag);
        assert!(
            flag.load(Ordering::SeqCst),
            "flag must remain true once set; got cleared"
        );
    }

    #[test]
    fn tracker_throttles_until_full_tick_window() {
        let mut tracker = McpRegistryWatcherTracker::default();
        let flag = AtomicBool::new(false);
        for i in 0..(TICKS_PER_REGISTRY_SCAN - 1) {
            assert!(
                !tracker.maybe_scan(&flag),
                "scan must not run before tick window completes (i={i})"
            );
        }
        // Last tick triggers the scan.
        assert!(tracker.maybe_scan(&flag));
        // Counter reset → next scan requires another full window.
        assert!(!tracker.maybe_scan(&flag));
    }
}
