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

/// Scan cadence: 6 ticks × 10s = 60s. Faster than the original 5-min HEAD-only
/// cadence because the strict-policy dirty detector (L2) should surface a
/// worktree-discipline violation promptly. The combined scan is one cheap
/// `git status --porcelain` per canonical and the HEAD-hygiene side effect is
/// idempotent, so a 60s cadence is well within budget.
const TICKS_PER_SCAN: u64 = 6;

/// Re-alert cooldown (minutes): while a canonical stays dirty with the SAME change
/// set, re-notify the operator at most once per this window. A CHANGED set
/// (different fingerprint) notifies immediately regardless of cooldown.
const REALERT_COOLDOWN_MINS: i64 = 30;

/// Per-canonical re-alert suppression state — the `(fingerprint, last_notified_at)`
/// pair that implements the dirty-fingerprint throttle.
struct DirtyAlertState {
    fingerprint: u64,
    last_notified_at: chrono::DateTime<chrono::Utc>,
}

pub(crate) struct CanonicalDriftTracker {
    /// Cadence gate — throttles scans to once per [`TICKS_PER_SCAN`]
    /// supervisor ticks (fire-on-Nth).
    gate: crate::daemon::cadence_gate::CadenceGate,
    /// Per-canonical re-alert suppression (L2 strict-policy throttling). Keyed by
    /// canonical path; the stored fingerprint + timestamp suppress repeat
    /// notifications while the same dirty set persists, and re-arm (entry removed)
    /// when the canonical goes clean — so a new dirty state always notifies.
    last_dirty: HashMap<PathBuf, DirtyAlertState>,
}

impl Default for CanonicalDriftTracker {
    fn default() -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_interval(TICKS_PER_SCAN),
            last_dirty: HashMap::new(),
        }
    }
}

impl CanonicalDriftTracker {
    /// Per-tick entry. Increments the tick counter; on the throttled
    /// boundary, fires a drift scan and returns `true`. Returns `false`
    /// otherwise (pre-throttle tick OR post-fire reset).
    pub(crate) fn maybe_scan(&mut self, home: &Path) -> bool {
        if !self.gate.fire() {
            return false;
        }
        self.scan_and_notify(home);
        true
    }

    /// Reload `FleetConfig`, run hygiene (HEAD-state side effects + L2 dirty
    /// detection), and notify the operator about dirty canonicals subject to
    /// per-fingerprint re-alert throttling. Best-effort: a missing/unparseable
    /// fleet.yaml is warn-logged and skipped (warn, not debug — a runtime
    /// fleet.yaml load failure is an operator-visible config regression).
    fn scan_and_notify(&mut self, home: &Path) {
        let path = crate::fleet::fleet_yaml_path(home);
        let config = match crate::fleet::FleetConfig::load(&path) {
            Ok(config) => config,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    "#852 canonical_drift: fleet.yaml load failed; skipping scan"
                );
                return;
            }
        };
        let reports = crate::bootstrap::canonical_hygiene::run_hygiene_with_dirty_report(&config);
        let now = chrono::Utc::now();
        let mut still_dirty = std::collections::HashSet::<PathBuf>::new();
        for report in &reports {
            still_dirty.insert(report.path.clone());
            if self.should_notify(&report.path, report.fingerprint, now) {
                crate::bootstrap::canonical_hygiene::notify_operator_of_canonical_dirty(report);
                self.last_dirty.insert(
                    report.path.clone(),
                    DirtyAlertState {
                        fingerprint: report.fingerprint,
                        last_notified_at: now,
                    },
                );
            } else {
                tracing::debug!(
                    canonical = %report.path.display(),
                    "canonical_drift: dirty unchanged within re-alert cooldown — suppressed"
                );
            }
        }
        // Re-arm: drop suppression entries for canonicals that are no longer dirty,
        // so the next time one goes dirty it notifies immediately.
        self.last_dirty.retain(|p, _| still_dirty.contains(p));
    }

    /// Throttle decision: notify on first-dirty (no prior entry), on a CHANGED
    /// dirty set (fingerprint differs), or once the re-alert cooldown has elapsed
    /// for an unchanged set; otherwise suppress.
    fn should_notify(
        &self,
        path: &Path,
        fingerprint: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> bool {
        match self.last_dirty.get(path) {
            None => true,
            Some(s) if s.fingerprint != fingerprint => true,
            Some(s) => {
                now.signed_duration_since(s.last_notified_at).num_minutes() >= REALERT_COOLDOWN_MINS
            }
        }
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

    /// Pure tick-gate contract: `TICKS_PER_SCAN - 1` calls return false, the
    /// `TICKS_PER_SCAN`-th fires (returns true), and the next resets and returns
    /// false again. Matches the `WaitingOnStale` / `ConflictNotify` cadence
    /// contract so the supervisor loop's per-tick call sequence behaves uniformly.
    #[test]
    fn tracker_throttles_to_tick_per_scan() {
        let home = tmp_home("throttle");
        std::fs::create_dir_all(&home).unwrap();
        let mut tracker = CanonicalDriftTracker::default();
        for i in 0..(TICKS_PER_SCAN - 1) {
            assert!(
                !tracker.maybe_scan(&home),
                "tick {i} (pre-throttle) must return false"
            );
        }
        assert!(
            tracker.maybe_scan(&home),
            "the TICKS_PER_SCAN-th tick must fire scan and return true"
        );
        assert!(
            !tracker.maybe_scan(&home),
            "the next tick must reset the counter and return false"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Smoke: the runtime scan must not panic when fleet.yaml is absent
    /// (the most-common fresh-daemon / test-harness state). FleetConfig::load
    /// returns Err and the tracker logs + returns without touching the canonical.
    #[test]
    fn runtime_scan_calls_canonical_hygiene_no_panic_on_empty_fleet() {
        let home = tmp_home("empty");
        std::fs::create_dir_all(&home).unwrap();
        CanonicalDriftTracker::default().scan_and_notify(&home);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Re-alert throttle: the same dirty set is suppressed within the cooldown, a
    /// CHANGED set (new fingerprint) re-alerts immediately, the cooldown boundary
    /// re-alerts, and going clean re-arms so the next dirty notifies fresh.
    #[test]
    fn dirty_fingerprint_throttle_suppresses_unchanged_and_realerts_on_change() {
        let mut tracker = CanonicalDriftTracker::default();
        let path = std::path::PathBuf::from("/canonical/repo");
        let t0 = chrono::Utc::now();

        // First sighting → notify.
        assert!(
            tracker.should_notify(&path, 0xAAAA, t0),
            "first-dirty must notify"
        );
        // Record that we notified.
        tracker.last_dirty.insert(
            path.clone(),
            DirtyAlertState {
                fingerprint: 0xAAAA,
                last_notified_at: t0,
            },
        );

        // Same fingerprint within cooldown → suppress.
        assert!(
            !tracker.should_notify(&path, 0xAAAA, t0 + chrono::Duration::minutes(10)),
            "same dirty set within cooldown must be suppressed"
        );
        // Changed fingerprint → notify immediately.
        assert!(
            tracker.should_notify(&path, 0xBBBB, t0 + chrono::Duration::minutes(10)),
            "a changed dirty set must re-alert immediately"
        );
        // Same fingerprint at the cooldown boundary → notify.
        assert!(
            tracker.should_notify(
                &path,
                0xAAAA,
                t0 + chrono::Duration::minutes(REALERT_COOLDOWN_MINS)
            ),
            "same dirty set after cooldown must re-alert"
        );

        // Re-arm: a path no longer dirty is dropped, so the next dirty notifies.
        let still_dirty = std::collections::HashSet::<std::path::PathBuf>::new();
        tracker.last_dirty.retain(|p, _| still_dirty.contains(p));
        assert!(
            tracker.should_notify(&path, 0xAAAA, t0 + chrono::Duration::minutes(1)),
            "after going clean, the next dirty must notify immediately (re-armed)"
        );
    }
}
