//! #852 residual PR-B: runtime canonical-drift detection.
//!
//! Boot-time hygiene (`canonical_hygiene::run_hygiene`, originally added in
//! the #852 PR-C boot-only path) catches the canonical HEAD state at daemon
//! startup but cannot detect drift that happens AFTER boot. A long-lived
//! daemon may run for hours/days while the canonical accrues detached-HEAD
//! residue from agent activity that slips past the shim (or activity
//! pre-dating #858's tightened deny matrix). This per-tick tracker mirrors
//! `conflict_notify.rs` and `waiting_on_stale.rs`: 5-minute throttled
//! cadence + reuse of the boot-time hygiene helper.
//!
//! Spike Q2 sticky-point resolution: supervisor today doesn't carry
//! `FleetConfig` in scope. Reloading `FleetConfig::load` inside the tracker
//! on every fire (5-min cadence) is cheap I/O and avoids threading a new
//! arg through the supervisor loop signature. The boot-time path stays
//! authoritative for the helper API; this module is a thin adapter.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Scan throttle: 30 ticks × 10s = 5 min cadence (matches
/// `waiting_on_stale` + `conflict_notify`).
const TICKS_PER_SCAN: u64 = 30;

#[derive(Debug, Default)]
pub(crate) struct CanonicalDriftTracker {
    tick_count: u64,
    /// Per-canonical-path last-action timestamp. Reserved for future
    /// per-path dedup / re-alert suppression (mirror of
    /// `waiting_on_stale::WaitingOnStaleTracker::last_alerted_at`).
    /// Not read in this PR — the boot-time helper handles all log
    /// suppression today; the field is wired through so a future PR
    /// can add per-path throttling without re-flexing the struct
    /// shape.
    #[allow(dead_code)]
    last_action_at: HashMap<PathBuf, chrono::DateTime<chrono::Utc>>,
}

impl CanonicalDriftTracker {
    /// Per-tick entry. Increments the tick counter; on the throttled
    /// boundary, fires a drift scan and returns `true`. Returns `false`
    /// otherwise (pre-throttle tick OR post-fire reset).
    pub(crate) fn maybe_scan(&mut self, home: &Path) -> bool {
        self.tick_count = self.tick_count.saturating_add(1);
        if self.tick_count < TICKS_PER_SCAN {
            return false;
        }
        self.tick_count = 0;
        run_drift_scan(home);
        true
    }
}

/// Reload `FleetConfig` + dispatch to the canonical-hygiene helper.
/// Best-effort: a missing or unparseable fleet.yaml is logged at warn
/// and skipped (per boot-time semantics — daemon must never crash
/// because hygiene couldn't read config). Warn (not debug) because
/// a runtime fleet.yaml load failure indicates an actual operator-
/// visible config regression and should not be silenced.
fn run_drift_scan(home: &Path) {
    let path = crate::fleet::fleet_yaml_path(home);
    match crate::fleet::FleetConfig::load(&path) {
        Ok(config) => crate::bootstrap::canonical_hygiene::run_hygiene(&config),
        Err(e) => tracing::warn!(
            error = %e,
            path = %path.display(),
            "#852 canonical_drift: fleet.yaml load failed; skipping scan"
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("agend-test-canonical-drift-{tag}-{id}"))
    }

    /// Pure tick-gate contract: 29 calls return false, the 30th fires
    /// (returns true), the 31st returns false again. Matches the
    /// `WaitingOnStale` / `ConflictNotify` cadence contract so the
    /// supervisor loop's per-tick call sequence behaves uniformly.
    #[test]
    fn tracker_throttles_to_tick_per_scan() {
        let home = tmp_home("throttle");
        std::fs::create_dir_all(&home).unwrap();
        let mut tracker = CanonicalDriftTracker::default();
        for i in 0..29 {
            assert!(
                !tracker.maybe_scan(&home),
                "tick {i} (pre-throttle) must return false"
            );
        }
        assert!(
            tracker.maybe_scan(&home),
            "30th tick must fire scan and return true"
        );
        assert!(
            !tracker.maybe_scan(&home),
            "31st tick must reset counter and return false"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Smoke: the runtime scan must not panic when fleet.yaml is absent
    /// (the most-common fresh-daemon / test-harness state). Boot-time
    /// helper logs + skips on FleetConfig::load error; the runtime path
    /// inherits the same best-effort discipline.
    #[test]
    fn runtime_scan_calls_canonical_hygiene_no_panic_on_empty_fleet() {
        let home = tmp_home("empty");
        std::fs::create_dir_all(&home).unwrap();
        // No fleet.yaml written — FleetConfig::load returns Err and the
        // tracker logs + returns without touching the canonical.
        run_drift_scan(&home);
        let _ = std::fs::remove_dir_all(&home);
    }
}
