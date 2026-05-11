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
//! out of date relative to the install path's MCP tool registry —
//! emit an inbox alert to `general` + `lead` so operators can plan a
//! restart on their schedule instead of being surprised by a missing
//! tool mid-session. This is the same notification shape as
//! `helper_staleness_watchdog`, applied to the daemon binary itself.
//!
//! ## Pattern parallel
//!
//! Identical scaffolding to the four prior trackers: TICKS_PER_*_SCAN
//! = 30 (≈5 min cadence), `inbox::enqueue` emit path, fail-open IO,
//! per-key re-alert suppression. With this module the supervisor
//! coexists with five trackers: AntiStallTracker (#567),
//! IdleWatchdogTracker (#568), DecisionTimeoutTracker (#572),
//! HelperStalenessWatchdog (#576), and this one.

use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

/// Scan throttle — matches the four prior trackers so all five fire
/// in the same wall-clock window without interleaving overhead.
pub(crate) const TICKS_PER_REGISTRY_SCAN: u64 = 30;

/// Re-alert window. Once the operator has been notified that the
/// daemon binary is newer than the running process, suppress repeat
/// alerts for 6h so a routine `cargo install` cycle pages once and
/// chronic-stale state still re-pages every 6h.
pub(crate) const RE_ALERT_THRESHOLD_SECS: i64 = 6 * 60 * 60;

const RECIPIENTS: &[&str] = &["general", "lead"];

/// Per-supervisor-loop tracker — captures the process-start time on
/// first init so subsequent scans can detect a daemon binary that has
/// been replaced post-startup.
#[derive(Debug)]
pub(crate) struct McpRegistryWatcherTracker {
    tick_count: u64,
    /// Process start time. Captured at tracker init so the comparison
    /// against the daemon binary's mtime reflects "has the binary
    /// been refreshed since this process started".
    started_at: SystemTime,
    /// Last alert timestamp keyed by daemon-exe path. Per-path so a
    /// daemon-binary swap (rare, but possible if the operator changes
    /// `which agend-terminal` mid-cycle) doesn't suppress the alert.
    last_alerted_at: HashMap<String, chrono::DateTime<chrono::Utc>>,
}

impl Default for McpRegistryWatcherTracker {
    fn default() -> Self {
        Self {
            tick_count: 0,
            started_at: SystemTime::now(),
            last_alerted_at: HashMap::new(),
        }
    }
}

impl McpRegistryWatcherTracker {
    /// Increment tick counter and run the scan every
    /// [`TICKS_PER_REGISTRY_SCAN`] calls.
    pub(crate) fn maybe_scan(&mut self, home: &Path) -> bool {
        self.tick_count = self.tick_count.saturating_add(1);
        if self.tick_count < TICKS_PER_REGISTRY_SCAN {
            return false;
        }
        self.tick_count = 0;
        let daemon_exe = std::env::current_exe().ok();
        scan_and_emit(
            home,
            daemon_exe.as_deref(),
            self.started_at,
            &mut self.last_alerted_at,
        );
        true
    }
}

/// Pure scan logic — exposed for tests so they can invoke without
/// the 30-tick wall-clock wait and with controlled daemon-exe path
/// + process-start time.
pub(crate) fn scan_and_emit(
    home: &Path,
    daemon_exe: Option<&Path>,
    started_at: SystemTime,
    last_alerted: &mut HashMap<String, chrono::DateTime<chrono::Utc>>,
) {
    let Some(exe) = daemon_exe else {
        // Daemon binary path undeterminable — same fail-open posture
        // as helper_staleness_watchdog. No alert.
        return;
    };
    let Ok(binary_mtime) = std::fs::metadata(exe).and_then(|m| m.modified()) else {
        return;
    };
    if binary_mtime <= started_at {
        // Binary is older than (or equal to) process start — no
        // post-start replacement has occurred.
        return;
    }
    let key = exe.to_string_lossy().to_string();
    let now = chrono::Utc::now();
    if let Some(prev) = last_alerted.get(&key) {
        let since_alert = now.signed_duration_since(*prev).num_seconds();
        if since_alert < RE_ALERT_THRESHOLD_SECS {
            return;
        }
    }
    emit_registry_stale_alert(home, exe);
    last_alerted.insert(key, now);
}

fn emit_registry_stale_alert(home: &Path, daemon_exe: &Path) {
    let text = format!(
        "[mcp_registry_watcher] daemon binary at '{}' has been refreshed \
         AFTER the running process started. The MCP tool registry in \
         memory may lag the binary's compiled-in registry — restart \
         the daemon (`agend-terminal stop` → `agend-terminal start`) \
         to pick up any newly-registered tools or handler updates. \
         (Sprint 60 W1 PR-2 #P0-2: closes the PR-5 → PR-4 chicken-and-\
         egg loop where new MCP tools required a restart that itself \
         was the friction the tool was meant to remove.)",
        daemon_exe.display()
    );
    for recipient in RECIPIENTS {
        let msg = crate::inbox::InboxMessage {
            schema_version: 0,
            id: None,
            from: "system:mcp_registry_watcher".to_string(),
            text: text.clone(),
            kind: Some("mcp_registry_watcher".to_string()),
            timestamp: chrono::Utc::now().to_rfc3339(),
            channel: None,
            read_at: None,
            thread_id: None,
            parent_id: None,
            delivery_mode: Some("inbox_fallback".to_string()),
            task_id: None,
            force_meta: None,
            correlation_id: Some(daemon_exe.to_string_lossy().to_string()),
            reviewed_head: None,
            attachments: Vec::new(),
            in_reply_to_msg_id: None,
            in_reply_to_excerpt: None,
            superseded_by: None,
            from_id: None,
            broadcast_context: None,
            sequencing: None,
            eta_minutes: None,
            reporting_cadence: None,
            worktree_binding_required: None,
        };
        if let Err(e) = crate::inbox::enqueue(home, recipient, msg) {
            tracing::warn!(
                error = %e,
                recipient,
                exe = %daemon_exe.display(),
                "mcp_registry_watcher: enqueue failed"
            );
        } else {
            tracing::info!(
                recipient,
                exe = %daemon_exe.display(),
                "mcp_registry_watcher: emitted inbox alert"
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
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

    #[test]
    fn scan_emits_alert_when_binary_replaced_after_startup() {
        let home = tmp_home("post-start-replace");
        // Pretend the process started 10 seconds ago.
        let started_at = SystemTime::now() - Duration::from_secs(10);
        std::thread::sleep(Duration::from_millis(20));
        let exe = plant_post_start_binary(&home);
        let mut last_alerted: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
        scan_and_emit(&home, Some(&exe), started_at, &mut last_alerted);

        let general = crate::inbox::drain(&home, "general");
        let lead = crate::inbox::drain(&home, "lead");
        assert!(
            general
                .iter()
                .any(|m| m.kind.as_deref() == Some("mcp_registry_watcher")),
            "general inbox missing mcp_registry_watcher alert: {general:?}"
        );
        assert!(
            lead.iter()
                .any(|m| m.kind.as_deref() == Some("mcp_registry_watcher")),
            "lead inbox missing mcp_registry_watcher alert: {lead:?}"
        );
        assert!(last_alerted.contains_key(&exe.to_string_lossy().to_string()));
    }

    #[test]
    fn no_alert_when_binary_older_than_startup() {
        let home = tmp_home("binary-older");
        // Plant the binary FIRST, then claim startup is ~now (well
        // after the binary mtime). The watcher must not emit because
        // the binary is older than process-start.
        let exe = plant_post_start_binary(&home);
        std::thread::sleep(Duration::from_millis(20));
        let started_at = SystemTime::now();
        let mut last_alerted: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
        scan_and_emit(&home, Some(&exe), started_at, &mut last_alerted);

        assert!(
            last_alerted.is_empty(),
            "binary older than startup must not alert"
        );
        let general = crate::inbox::drain(&home, "general");
        assert!(
            !general
                .iter()
                .any(|m| m.kind.as_deref() == Some("mcp_registry_watcher")),
            "no inbox emit when binary older than startup: {general:?}"
        );
    }

    #[test]
    fn throttle_suppresses_re_alert_within_window() {
        let home = tmp_home("throttle");
        let started_at = SystemTime::now() - Duration::from_secs(10);
        std::thread::sleep(Duration::from_millis(20));
        let exe = plant_post_start_binary(&home);
        let key = exe.to_string_lossy().to_string();
        let now = chrono::Utc::now();
        let mut last_alerted: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
        last_alerted.insert(
            key.clone(),
            now - chrono::Duration::seconds(RE_ALERT_THRESHOLD_SECS / 2),
        );
        let snapshot = last_alerted.clone();
        scan_and_emit(&home, Some(&exe), started_at, &mut last_alerted);

        assert_eq!(
            last_alerted.get(&key),
            snapshot.get(&key),
            "throttle window must suppress re-alert"
        );
        let general = crate::inbox::drain(&home, "general");
        assert!(
            !general
                .iter()
                .any(|m| m.kind.as_deref() == Some("mcp_registry_watcher")),
            "throttle window must suppress inbox emit: {general:?}"
        );
    }

    #[test]
    fn throttle_re_alerts_after_window_expires() {
        let home = tmp_home("re-alert");
        let started_at = SystemTime::now() - Duration::from_secs(10);
        std::thread::sleep(Duration::from_millis(20));
        let exe = plant_post_start_binary(&home);
        let key = exe.to_string_lossy().to_string();
        let now = chrono::Utc::now();
        let mut last_alerted: HashMap<String, chrono::DateTime<chrono::Utc>> = HashMap::new();
        // Pre-populate OUTSIDE the suppression window.
        last_alerted.insert(
            key.clone(),
            now - chrono::Duration::seconds(RE_ALERT_THRESHOLD_SECS + 60),
        );
        scan_and_emit(&home, Some(&exe), started_at, &mut last_alerted);

        let updated = last_alerted.get(&key).copied().unwrap();
        let drift = now.signed_duration_since(updated).num_seconds().abs();
        assert!(drift < 60, "post-window timestamp must be near now");
        let general = crate::inbox::drain(&home, "general");
        assert!(
            general
                .iter()
                .any(|m| m.kind.as_deref() == Some("mcp_registry_watcher")),
            "post-window scan must re-emit"
        );
    }

    #[test]
    fn tracker_throttles_until_full_tick_window() {
        let mut tracker = McpRegistryWatcherTracker::default();
        let home = tmp_home("tick-throttle");
        for i in 0..(TICKS_PER_REGISTRY_SCAN - 1) {
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
