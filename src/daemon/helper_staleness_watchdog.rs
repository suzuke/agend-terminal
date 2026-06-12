//! Sprint 59 Wave 2 PR-3 (#13 deployment-cadence proactive helper-
//! staleness notification) — fourth supervisor tracker. Closes the
//! "main merged but daemon-side helpers lag" loop that hit 4-5 cycles
//! across Sprint 56-58.
//!
//! Sprint 58 Wave 2 PR-1 (#11) shipped `cli::check_helper_staleness`
//! as a Shape-B passive doctor warn — operator-pull. This module
//! adds the proactive vantage: the supervisor periodically reuses the
//! same `cli::classify_helper_staleness` classification and pings
//! `general` + `lead` when a helper goes stale, so operators see the
//! signal without first running `agend-terminal doctor`.
//!
//! Pattern parallel to `idle_watchdog.rs` / `anti_stall.rs` /
//! `decision_timeout.rs` — all four trackers share the supervisor's
//! TICK loop, all emit via `inbox::enqueue`, all fail-open on IO
//! errors. Throttle:
//!
//! - **Scan**: every `TICKS_PER_STALENESS_SCAN = 30` ticks (≈ 5 min,
//!   identical cadence to the existing trackers).
//! - **Re-alert suppression**: same helper won't re-page within
//!   `RE_ALERT_THRESHOLD_SECS = 6 * 3600`. Wave 2 closes long before
//!   that window so a single rebuild → restart cycle never produces a
//!   redundant page; chronic-stale state still re-pages every 6 hours
//!   so operators don't miss it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Scan throttle — matches `idle_watchdog::TICKS_PER_IDLE_SCAN` so
/// all four trackers fire in the same wall-clock window.
pub(crate) const TICKS_PER_STALENESS_SCAN: u64 = 30;

/// Re-alert window. Stale state that lingers across a full window
/// re-pages once; transient stale across a single rebuild cycle pages
/// only once. 6h chosen to be strictly larger than the longest
/// observed deployment-cadence cycle (Wave 3 PR-2 multi-day) without
/// being so long that genuine drift goes unnoticed.
pub(crate) const RE_ALERT_THRESHOLD_SECS: i64 = 6 * 60 * 60;

/// Recipient list — both go via inbox enqueue. Downstream telegram
/// routing (if the recipient vantage is telegram-bound) is handled by
/// the existing channel pipeline.
const RECIPIENTS: &[&str] = &["general", "lead"];

/// Helpers tracked. Mirrors `cli::check_helper_staleness` so a single
/// classification logic governs both the operator-pull doctor and the
/// proactive scanner.
const TRACKED_HELPERS: &[&str] = &["agend-git", "agend-mcp-bridge"];

pub(crate) struct HelperStalenessWatchdogTracker {
    /// Cadence gate — throttles scans to once per [`TICKS_PER_STALENESS_SCAN`]
    /// supervisor ticks (fire-on-Nth).
    gate: crate::daemon::cadence_gate::CadenceGate,
    /// Per-helper last-alert timestamp. Per-helper (not per-recipient)
    /// so a sequential rebuild that fixes one helper but not the other
    /// still produces fresh alerts for the lagging one.
    last_alerted_at: HashMap<String, chrono::DateTime<chrono::Utc>>,
    /// #1739 boot-seed latch. First scan seeds `last_alerted_at` with
    /// currently-stale helpers (stamped now) WITHOUT emitting, so a restart
    /// doesn't re-page about staleness the operator already saw. Only helpers
    /// newly stale after boot (or past RE_ALERT_THRESHOLD) emit.
    seeded: bool,
}

impl Default for HelperStalenessWatchdogTracker {
    fn default() -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_interval(TICKS_PER_STALENESS_SCAN),
            last_alerted_at: HashMap::new(),
            seeded: false,
        }
    }
}

impl HelperStalenessWatchdogTracker {
    /// Increment tick counter and run the scan every
    /// [`TICKS_PER_STALENESS_SCAN`] calls.
    pub(crate) fn maybe_scan(&mut self, home: &Path) -> bool {
        if !self.gate.fire() {
            return false;
        }
        let daemon_exe = std::env::current_exe().ok();
        let seeding = !self.seeded;
        self.seeded = true;
        scan_and_emit(
            home,
            daemon_exe.as_deref(),
            &mut self.last_alerted_at,
            seeding,
        );
        true
    }
}

fn helper_path(home: &Path, name: &str) -> PathBuf {
    let bin_dir = home.join("bin");
    if cfg!(windows) {
        bin_dir.join(format!("{name}.exe"))
    } else {
        bin_dir.join(name)
    }
}

/// Pure scan logic — exposed for tests so they can invoke without
/// the 30-tick wall-clock wait and with a controlled daemon-exe path.
pub(crate) fn scan_and_emit(
    home: &Path,
    daemon_exe: Option<&Path>,
    last_alerted: &mut HashMap<String, chrono::DateTime<chrono::Utc>>,
    seeding: bool,
) {
    let now = chrono::Utc::now();
    for name in TRACKED_HELPERS {
        let hp = helper_path(home, name);
        if crate::cli::classify_helper_staleness(daemon_exe, &hp)
            != crate::cli::HelperStaleness::Stale
        {
            // Fresh / NotInstalled / UndeterminableDaemonPath — no
            // proactive page. Operator-pull doctor still surfaces
            // these; the proactive scanner only cares about Stale.
            continue;
        }
        if let Some(prev) = last_alerted.get(*name) {
            let since_alert = now.signed_duration_since(*prev).num_seconds();
            if since_alert < RE_ALERT_THRESHOLD_SECS {
                continue;
            }
        }
        // #1739 boot-seed: first scan records the stale helper without emitting.
        if !seeding {
            // #event-bus Step 2 (legacy-zero): the bus is the sole delivery path.
            crate::daemon::event_bus::global().emit(
                home,
                crate::daemon::event_bus::EventKind::HelperStale {
                    helper_name: (*name).to_string(),
                },
            );
        }
        last_alerted.insert((*name).to_string(), now);
    }
}

/// #event-bus pattern #5: the stale-helper notification text. Shared by the
/// legacy direct deliver AND the event-bus subscriber so both rebuild the
/// BYTE-IDENTICAL text.
fn helper_staleness_text(helper_name: &str) -> String {
    format!(
        "[helper_staleness_watchdog] helper '{helper_name}' is older \
         than the daemon binary. Run `cargo install --path . --force` \
         then restart the daemon (`agend-terminal stop` → \
         `agend-terminal start`) so the refreshed helpers are loaded. \
         (Sprint 59 Wave 2 PR-3 #13 — proactive vantage; operator-pull \
         `agend-terminal doctor` reports the same state.)"
    )
}

/// #event-bus pattern #5: deliver the stale-helper alert to the hardcoded
/// `RECIPIENTS` (general + lead). Shared by the legacy path AND the subscriber
/// ([`handle_event`]), so the two are byte-identical by construction.
fn deliver_helper_staleness(home: &Path, helper_name: &str) {
    let text = helper_staleness_text(helper_name);
    for recipient in RECIPIENTS {
        if let Err(e) = crate::inbox::notify_system(
            home,
            recipient,
            "system:helper_staleness_watchdog",
            "helper_staleness_watchdog",
            text.clone(),
            Some(helper_name),
            None,
        ) {
            tracing::warn!(
                error = %e,
                recipient,
                helper = helper_name,
                "helper_staleness_watchdog: enqueue failed"
            );
        } else {
            tracing::info!(
                recipient,
                helper = helper_name,
                "helper_staleness_watchdog: emitted inbox alert"
            );
        }
    }
}

/// #event-bus pattern #5: subscriber — rebuild the alert from the event.
fn handle_event(event: &crate::daemon::event_bus::Event) -> bool {
    if let crate::daemon::event_bus::EventKind::HelperStale { helper_name } = &event.kind {
        deliver_helper_staleness(&event.home, helper_name);
        true
    } else {
        false
    }
}

/// #event-bus pattern #5: register the delivery subscriber at daemon startup.
/// Home-agnostic — the home travels on each event. Wired beside the other
/// patterns in `daemon::mod`.
pub fn register_subscriber() {
    crate::daemon::event_bus::global().subscribe(handle_event);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-helper-staleness-watchdog-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Plant a file at `path` with content + a mtime ordering pause so
    /// subsequent writes register as strictly newer. Mirrors the
    /// pattern used by `cli::helper_staleness_tests`.
    fn write_then_pause(path: &std::path::Path, content: &[u8], pause_ms: u64) {
        std::fs::write(path, content).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(pause_ms));
    }

    /// Stage a stale helper + a (later-mtime) daemon stand-in. Returns
    /// the daemon stand-in path so tests can inject it into
    /// `scan_and_emit` without relying on `current_exe()`.
    fn stage_stale_state(home: &Path, helper_name: &str) -> PathBuf {
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let helper = if cfg!(windows) {
            bin.join(format!("{helper_name}.exe"))
        } else {
            bin.join(helper_name)
        };
        // Helper FIRST → older mtime.
        write_then_pause(&helper, b"old-helper", 30);
        // Daemon stand-in AFTER → newer mtime.
        let daemon = home.join("agend-terminal-fake");
        write_then_pause(&daemon, b"newer-daemon", 10);
        daemon
    }

    #[test]
    fn scan_emits_alert_when_helper_is_stale() {
        let home = tmp_home("stale-emits");
        let daemon = stage_stale_state(&home, "agend-git");
        let mut last_alerted: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
        scan_and_emit(&home, Some(&daemon), &mut last_alerted, false);

        // Both recipients should receive an alert for the stale
        // helper. Other helpers (agend-mcp-bridge in this case) are
        // NotInstalled, so no alert for them.
        let general = crate::inbox::drain(&home, "general");
        let lead = crate::inbox::drain(&home, "lead");
        assert!(
            general
                .iter()
                .any(|m| m.kind.as_deref() == Some("helper_staleness_watchdog")
                    && m.correlation_id.as_deref() == Some("agend-git")),
            "general inbox missing helper_staleness alert: {general:?}"
        );
        assert!(
            lead.iter()
                .any(|m| m.kind.as_deref() == Some("helper_staleness_watchdog")
                    && m.correlation_id.as_deref() == Some("agend-git")),
            "lead inbox missing helper_staleness alert: {lead:?}"
        );
        assert!(last_alerted.contains_key("agend-git"));
    }

    #[test]
    fn boot_seed_suppresses_existing_stale_helper_then_no_reburst() {
        // #1739: the first scan after a fresh daemon start seeds an
        // already-stale helper into the dedup WITHOUT paging, and a subsequent
        // scan does not re-burst it.
        let home = tmp_home("stale-bootseed");
        let daemon = stage_stale_state(&home, "agend-git");
        let mut last_alerted: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
        // seeding scan: record the stale helper, do NOT page.
        scan_and_emit(&home, Some(&daemon), &mut last_alerted, true);
        assert!(
            last_alerted.contains_key("agend-git"),
            "boot-seed must record the stale helper in the dedup"
        );
        assert!(
            crate::inbox::drain(&home, "general").is_empty(),
            "boot-seed must NOT page for a restart-existing stale helper \
             (negative-probe: removing the `if !seeding` gate makes this fire)"
        );
        assert!(crate::inbox::drain(&home, "lead").is_empty());
        // next normal scan: the seeded helper stays suppressed within RE_ALERT.
        scan_and_emit(&home, Some(&daemon), &mut last_alerted, false);
        assert!(
            crate::inbox::drain(&home, "general").is_empty(),
            "seeded helper must remain suppressed on the next scan"
        );
    }

    #[test]
    fn throttle_suppresses_re_alert_within_window() {
        let home = tmp_home("throttle");
        let daemon = stage_stale_state(&home, "agend-git");

        let now = chrono::Utc::now();
        let mut last_alerted: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
        // Pre-populate within the suppression window.
        last_alerted.insert(
            "agend-git".to_string(),
            now - chrono::Duration::seconds(RE_ALERT_THRESHOLD_SECS / 2),
        );
        let snapshot = last_alerted.clone();
        scan_and_emit(&home, Some(&daemon), &mut last_alerted, false);

        // Timestamp should NOT have moved → throttle held.
        assert_eq!(
            last_alerted.get("agend-git"),
            snapshot.get("agend-git"),
            "throttle window should suppress re-alert"
        );
        // No inbox messages should have been enqueued either.
        let general = crate::inbox::drain(&home, "general");
        assert!(
            !general
                .iter()
                .any(|m| m.kind.as_deref() == Some("helper_staleness_watchdog")),
            "throttle window should suppress inbox emit: {general:?}"
        );
    }

    #[test]
    fn throttle_re_alerts_after_window_expires() {
        let home = tmp_home("re-alert");
        let daemon = stage_stale_state(&home, "agend-git");

        let now = chrono::Utc::now();
        let mut last_alerted: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
        // Pre-populate OUTSIDE the suppression window (older than the
        // threshold).
        last_alerted.insert(
            "agend-git".to_string(),
            now - chrono::Duration::seconds(RE_ALERT_THRESHOLD_SECS + 60),
        );
        scan_and_emit(&home, Some(&daemon), &mut last_alerted, false);

        // Timestamp should have advanced to ~now → re-alert fired.
        let updated = last_alerted.get("agend-git").copied().unwrap();
        let drift = now.signed_duration_since(updated).num_seconds().abs();
        assert!(drift < 60, "post-window timestamp should be near now");
        let general = crate::inbox::drain(&home, "general");
        assert!(
            general
                .iter()
                .any(|m| m.kind.as_deref() == Some("helper_staleness_watchdog")),
            "post-window scan should re-emit"
        );
    }

    #[test]
    fn no_alert_for_missing_helpers() {
        // Empty home → both helpers classify as NotInstalled → no
        // proactive page (operator-pull doctor still surfaces).
        let home = tmp_home("missing");
        let mut last_alerted: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
        scan_and_emit(&home, None, &mut last_alerted, false);
        assert!(
            last_alerted.is_empty(),
            "NotInstalled state must not emit proactive alerts"
        );
    }

    #[test]
    fn tracker_throttles_until_full_tick_window() {
        let mut tracker = HelperStalenessWatchdogTracker::default();
        let home = tmp_home("tick-throttle");
        for i in 0..(TICKS_PER_STALENESS_SCAN - 1) {
            assert!(
                !tracker.maybe_scan(&home),
                "scan must not run before tick window completes (i={i})"
            );
        }
        // Last tick triggers the scan.
        assert!(tracker.maybe_scan(&home));
        // Counter reset → next scan requires another full window.
        assert!(!tracker.maybe_scan(&home));
    }

    fn drained_payloads(
        home: &Path,
        recipient: &str,
    ) -> Vec<(String, Option<String>, String, Option<String>)> {
        crate::inbox::drain(home, recipient)
            .into_iter()
            .map(|m| (m.from, m.kind, m.text, m.correlation_id))
            .collect()
    }

    /// #event-bus pattern #5 PARITY (gate-ON): the bus `emit`→subscriber path
    /// delivers payloads byte-identical (from/kind/text/correlation) to the legacy
    /// direct enqueue — for BOTH hardcoded recipients (general + lead). Exercises
    /// the REAL bus emit→fan-out→subscriber wiring. No `env_lock`: `RECIPIENTS` is
    /// a hardcoded const, not env-derived.
    #[test]
    fn gate_on_emit_subscriber_matches_legacy_direct_enqueue() {
        let helper = "agend-git";

        // Legacy direct deliver (the gate-OFF path).
        let home_legacy = tmp_home("parity-legacy");
        std::fs::create_dir_all(home_legacy.join("inbox")).unwrap();
        deliver_helper_staleness(&home_legacy, helper);

        // Bus emit→subscriber (the gate-ON path) — real fan-out via a test bus.
        let home_bus = tmp_home("parity-bus");
        std::fs::create_dir_all(home_bus.join("inbox")).unwrap();
        let bus = crate::daemon::event_bus::EventBus::new();
        bus.subscribe(handle_event);
        bus.emit(
            &home_bus,
            crate::daemon::event_bus::EventKind::HelperStale {
                helper_name: helper.to_string(),
            },
        );

        for recipient in RECIPIENTS {
            let legacy = drained_payloads(&home_legacy, recipient);
            let viabus = drained_payloads(&home_bus, recipient);
            assert_eq!(
                legacy, viabus,
                "emit→subscriber payload must equal legacy for recipient {recipient}"
            );
            assert!(!legacy.is_empty(), "recipient {recipient} must be alerted");
        }
        let _ = std::fs::remove_dir_all(&home_legacy);
        let _ = std::fs::remove_dir_all(&home_bus);
    }

    /// #event-bus Step 2 (legacy-zero): the scan emits to the global bus; the
    /// registered subscriber delivers to both recipients (the home travels on the
    /// event → this test's home).
    #[test]
    fn scan_delivers_via_bus() {
        let home = tmp_home("via-bus");
        let daemon = stage_stale_state(&home, "agend-git");
        let mut last_alerted: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
        scan_and_emit(&home, Some(&daemon), &mut last_alerted, false);
        for recipient in RECIPIENTS {
            assert!(
                !drained_payloads(&home, recipient).is_empty(),
                "#event-bus Option A: gate-off must deliver via legacy to {recipient} (no regression)"
            );
        }
        let _ = std::fs::remove_dir_all(&home);
    }
}
