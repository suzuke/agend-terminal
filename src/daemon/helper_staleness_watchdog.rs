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

#[derive(Debug, Default)]
pub(crate) struct HelperStalenessWatchdogTracker {
    tick_count: u64,
    /// Per-helper last-alert timestamp. Per-helper (not per-recipient)
    /// so a sequential rebuild that fixes one helper but not the other
    /// still produces fresh alerts for the lagging one.
    last_alerted_at: HashMap<String, chrono::DateTime<chrono::Utc>>,
}

impl HelperStalenessWatchdogTracker {
    /// Increment tick counter and run the scan every
    /// [`TICKS_PER_STALENESS_SCAN`] calls.
    pub(crate) fn maybe_scan(&mut self, home: &Path) -> bool {
        self.tick_count = self.tick_count.saturating_add(1);
        if self.tick_count < TICKS_PER_STALENESS_SCAN {
            return false;
        }
        self.tick_count = 0;
        let daemon_exe = std::env::current_exe().ok();
        scan_and_emit(home, daemon_exe.as_deref(), &mut self.last_alerted_at);
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
        emit_staleness_alert(home, name);
        last_alerted.insert((*name).to_string(), now);
    }
}

fn emit_staleness_alert(home: &Path, helper_name: &str) {
    let text = format!(
        "[helper_staleness_watchdog] helper '{helper_name}' is older \
         than the daemon binary. Run `cargo install --path . --force` \
         then restart the daemon (`agend-terminal stop` → \
         `agend-terminal start`) so the refreshed helpers are loaded. \
         (Sprint 59 Wave 2 PR-3 #13 — proactive vantage; operator-pull \
         `agend-terminal doctor` reports the same state.)"
    );
    for recipient in RECIPIENTS {
        let msg = crate::inbox::InboxMessage {
            schema_version: 0,
            id: None,
            from: "system:helper_staleness_watchdog".to_string(),
            text: text.clone(),
            kind: Some("helper_staleness_watchdog".to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            channel: None,
            read_at: None,
            thread_id: None,
            parent_id: None,
            delivery_mode: Some("inbox_fallback".to_string()),
            task_id: None,
            force_meta: None,
            correlation_id: Some(helper_name.to_string()),
            reviewed_head: None,
            attachments: Vec::new(),
            in_reply_to_msg_id: None,
            in_reply_to_excerpt: None,
            superseded_by: None,
            from_id: None,
            broadcast_context: None,
        };
        if let Err(e) = crate::inbox::enqueue(home, recipient, msg) {
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
        scan_and_emit(&home, Some(&daemon), &mut last_alerted);

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
        scan_and_emit(&home, Some(&daemon), &mut last_alerted);

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
        scan_and_emit(&home, Some(&daemon), &mut last_alerted);

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
        scan_and_emit(&home, None, &mut last_alerted);
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
}
