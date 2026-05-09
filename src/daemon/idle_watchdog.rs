//! Sprint 59 Wave 1 PR-2 (#10 dev 60min idle + #12 cross-vantage 30min
//! fleet guard) — engineering anti-stall watchdog cluster.
//!
//! Tied surface to PR-1's task-stall scanner: both run on the
//! supervisor's TICK loop, both emit inbox events when thresholds
//! are crossed, both fail-open on IO errors. The vantage differs:
//!
//! - **PR-1 (`anti_stall.rs`)**: per-task ETA-based stall detection.
//! - **PR-2 (this module)**: per-agent + fleet-wide idle detection,
//!   independent of task ETAs. Catches the Wave 3 PR-1 stall pattern
//!   where dev was idle-waiting for a dispatch that never explicitly
//!   referenced a task_id (no ETA → PR-1 wouldn't fire).
//!
//! ## Vantages
//!
//! ### #10 — dev 60min idle watchdog (P0)
//! Scans the watched-agent's last_active timestamp. When elapsed
//! exceeds `DEV_IDLE_THRESHOLD_SECS = 3600` (60 min), emits an
//! inbox ping to `lead` so the orchestrator can re-dispatch /
//! unblock. Default watched agent: `dev`. Tunable via
//! `AGEND_IDLE_WATCHDOG_AGENT` env var.
//!
//! ### #12 — cross-vantage 30min fleet-idle guard (P1)
//! Scans the entire fleet's last_active timestamps. When EVERY
//! tracked agent has been idle > `FLEET_IDLE_THRESHOLD_SECS = 1800`
//! (30 min) AND at least one agent has had recent activity (i.e.
//! a sidecar exists), emits an inbox ping to `general` so the
//! operator-facing aggregator surfaces the fleet stall. The
//! "at least one tracked" guard distinguishes "fleet really
//! stalled" from "fleet not yet started" / "all sidecars stale".
//!
//! ## Activity tracking sidecar
//!
//! `<home>/agent-activity/<agent>.json` — touched whenever the agent
//! sends a message via the unified `send` handler. Per-agent flock
//! for concurrency. Forward-compat preserved via `#[serde(default)]`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const ACTIVITY_DIR: &str = "agent-activity";
const SCHEMA_VERSION: u32 = 1;

/// Lead-spec threshold for the dev watchdog (vantage #10): agent
/// silent > 60 min → ping lead.
pub(crate) const DEV_IDLE_THRESHOLD_SECS: i64 = 60 * 60;

/// Lead-spec threshold for the fleet watchdog (vantage #12): every
/// tracked agent silent > 30 min → ping general.
pub(crate) const FLEET_IDLE_THRESHOLD_SECS: i64 = 30 * 60;

/// Scan throttle in supervisor TICK iterations. 30 × 10 s = 5 min
/// — matches PR-1's anti-stall cadence so both watchdogs scan in
/// the same wall-clock window without interleaving overhead.
pub(crate) const TICKS_PER_IDLE_SCAN: u64 = 30;

/// On-disk shape for a single agent's activity sidecar.
/// `#[serde(default)]` on each field per Sprint 58 Wave 1 PR-2
/// forward-compat contract.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct ActivitySidecar {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    last_active_at: String,
}

fn activity_dir(home: &Path) -> PathBuf {
    home.join(ACTIVITY_DIR)
}

fn activity_path(home: &Path, agent: &str) -> PathBuf {
    activity_dir(home).join(format!("{agent}.json"))
}

/// Touch agent activity — atomically write `last_active_at = now()`.
/// Best-effort; IO failures are logged and swallowed.
pub(crate) fn touch_agent_activity(home: &Path, agent: &str) {
    if agent.is_empty() {
        return;
    }
    let dir = activity_dir(home);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let lock_path = dir.join(format!(".{agent}.lock"));
    let _lock = match crate::store::acquire_file_lock(&lock_path) {
        Ok(l) => l,
        Err(_) => return,
    };
    let payload = ActivitySidecar {
        schema_version: SCHEMA_VERSION,
        agent: agent.to_string(),
        last_active_at: chrono::Utc::now().to_rfc3339(),
    };
    let body = match serde_json::to_string_pretty(&payload) {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = crate::store::atomic_write(&activity_path(home, agent), body.as_bytes());
}

/// Read the last activity timestamp for an agent. Returns `None` on
/// missing/corrupt sidecar OR future-version (forward-compat
/// preserved).
pub(crate) fn read_agent_last_active(
    home: &Path,
    agent: &str,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let content = std::fs::read_to_string(activity_path(home, agent)).ok()?;
    let sidecar: ActivitySidecar = serde_json::from_str(&content).ok()?;
    if sidecar.schema_version != SCHEMA_VERSION {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(&sidecar.last_active_at)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// Enumerate every (agent, last_active_at) pair the activity dir
/// holds. Used by the fleet-idle scanner. Stale/corrupt entries are
/// silently skipped.
fn enumerate_agent_activity(home: &Path) -> Vec<(String, chrono::DateTime<chrono::Utc>)> {
    let dir = activity_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(agent) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Some(ts) = read_agent_last_active(home, agent) {
            out.push((agent.to_string(), ts));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Per-loop watchdog state — throttles scans + dedups alerts.
#[derive(Debug, Default)]
pub(crate) struct IdleWatchdogTracker {
    tick_count: u64,
    /// (vantage, agent_or_fleet_marker) → last alert ts.
    last_alerted_at: HashMap<(&'static str, String), chrono::DateTime<chrono::Utc>>,
}

impl IdleWatchdogTracker {
    /// Increment tick counter and run scans every
    /// [`TICKS_PER_IDLE_SCAN`] calls.
    pub(crate) fn maybe_scan(&mut self, home: &Path) -> bool {
        self.tick_count = self.tick_count.saturating_add(1);
        if self.tick_count < TICKS_PER_IDLE_SCAN {
            return false;
        }
        self.tick_count = 0;
        scan_and_emit(home, &mut self.last_alerted_at);
        true
    }
}

/// Watched agent for the dev-vantage (#10). Defaults to `dev`;
/// tunable via env for tests + multi-agent fleets.
fn watched_dev_agent() -> String {
    std::env::var("AGEND_IDLE_WATCHDOG_AGENT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "dev".to_string())
}

/// Recipient for dev-vantage idle alerts. Defaults to `lead`.
fn dev_idle_recipient() -> String {
    std::env::var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "lead".to_string())
}

/// Recipient for fleet-vantage idle alerts. Defaults to `general`.
fn fleet_idle_recipient() -> String {
    std::env::var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "general".to_string())
}

/// Pure scan logic: detects + emits idle alerts at both vantages.
/// Exposed for tests so they can invoke without 30-tick wait.
pub(crate) fn scan_and_emit(
    home: &Path,
    last_alerted: &mut HashMap<(&'static str, String), chrono::DateTime<chrono::Utc>>,
) {
    let now = chrono::Utc::now();
    scan_dev_vantage(home, &now, last_alerted);
    scan_fleet_vantage(home, &now, last_alerted);
}

/// Vantage #10 — single agent (dev) idle threshold check.
fn scan_dev_vantage(
    home: &Path,
    now: &chrono::DateTime<chrono::Utc>,
    last_alerted: &mut HashMap<(&'static str, String), chrono::DateTime<chrono::Utc>>,
) {
    let agent = watched_dev_agent();
    let Some(last_active) = read_agent_last_active(home, &agent) else {
        return;
    };
    let elapsed_secs = now.signed_duration_since(last_active).num_seconds();
    if elapsed_secs <= DEV_IDLE_THRESHOLD_SECS {
        return;
    }
    let key = ("dev", agent.clone());
    if let Some(prev) = last_alerted.get(&key) {
        // Re-alert only after another full threshold has passed
        // (suppresses flooding while still surfacing extended
        // stalls every threshold-window).
        let since_alert = now.signed_duration_since(*prev).num_seconds();
        if since_alert < DEV_IDLE_THRESHOLD_SECS {
            return;
        }
    }
    emit_idle_alert(
        home,
        &dev_idle_recipient(),
        "dev_idle_watchdog",
        &format!(
            "[dev_idle_watchdog] agent '{agent}' has been silent for \
             {elapsed_secs}s (threshold {DEV_IDLE_THRESHOLD_SECS}s). \
             Possible dispatch protocol gap or unblock-needed state. \
             Consider: lead dispatch / unblock-ping / decision-log scan."
        ),
        Some(&agent),
    );
    last_alerted.insert(key, *now);
}

/// Vantage #12 — fleet-wide idle threshold check. Triggers when
/// EVERY tracked agent has been silent > FLEET threshold AND at
/// least one agent is tracked (avoid false-positive on empty fleet).
fn scan_fleet_vantage(
    home: &Path,
    now: &chrono::DateTime<chrono::Utc>,
    last_alerted: &mut HashMap<(&'static str, String), chrono::DateTime<chrono::Utc>>,
) {
    let pairs = enumerate_agent_activity(home);
    if pairs.is_empty() {
        return;
    }
    // All agents must exceed the threshold for "fleet idle".
    let all_idle = pairs
        .iter()
        .all(|(_, ts)| now.signed_duration_since(*ts).num_seconds() > FLEET_IDLE_THRESHOLD_SECS);
    if !all_idle {
        return;
    }
    let key = ("fleet", "*".to_string());
    if let Some(prev) = last_alerted.get(&key) {
        let since_alert = now.signed_duration_since(*prev).num_seconds();
        if since_alert < FLEET_IDLE_THRESHOLD_SECS {
            return;
        }
    }
    let oldest = pairs
        .iter()
        .map(|(_, ts)| now.signed_duration_since(*ts).num_seconds())
        .max()
        .unwrap_or(0);
    let agent_list: Vec<&str> = pairs.iter().map(|(n, _)| n.as_str()).collect();
    emit_idle_alert(
        home,
        &fleet_idle_recipient(),
        "fleet_idle_watchdog",
        &format!(
            "[fleet_idle_watchdog] all tracked agents silent > {FLEET_IDLE_THRESHOLD_SECS}s \
             (max elapsed {oldest}s). Tracked agents: {agent_list:?}. \
             Cross-vantage signal: investigate decision log + git status + inbox \
             for stalled sprint state."
        ),
        None,
    );
    last_alerted.insert(key, *now);
}

fn emit_idle_alert(
    home: &Path,
    recipient: &str,
    kind: &str,
    text: &str,
    correlation_agent: Option<&str>,
) {
    let msg = crate::inbox::InboxMessage {
        schema_version: 0,
        id: None,
        from: format!("system:{kind}"),
        text: text.to_string(),
        kind: Some(kind.to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        channel: None,
        read_at: None,
        thread_id: None,
        parent_id: None,
        delivery_mode: Some("inbox_fallback".to_string()),
        task_id: None,
        force_meta: None,
        correlation_id: correlation_agent.map(String::from),
        reviewed_head: None,
        attachments: Vec::new(),
        in_reply_to_msg_id: None,
        in_reply_to_excerpt: None,
        superseded_by: None,
        from_id: None,
        broadcast_context: None,
    };
    if let Err(e) = crate::inbox::enqueue(home, recipient, msg) {
        tracing::warn!(error = %e, recipient, kind, "idle_watchdog: enqueue failed");
    } else {
        tracing::info!(recipient, kind, "idle_watchdog: emitted inbox alert");
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
            "agend-idle-watchdog-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Process-global env mutex — same pattern as anti_stall::tests
    /// to serialize env-var-touching tests.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// Helper: write an activity sidecar with a back-dated timestamp
    /// so tests can simulate elapsed time without sleeping.
    fn write_activity_at(home: &Path, agent: &str, ts: chrono::DateTime<chrono::Utc>) {
        let dir = activity_dir(home);
        std::fs::create_dir_all(&dir).unwrap();
        let payload = ActivitySidecar {
            schema_version: SCHEMA_VERSION,
            agent: agent.to_string(),
            last_active_at: ts.to_rfc3339(),
        };
        std::fs::write(
            activity_path(home, agent),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .unwrap();
    }

    // ── Lead-spec named tests (per dispatch m-20260509091247441218-129) ──

    #[test]
    fn dev_watchdog_pings_lead_after_60min_no_progress() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT");
        let home = tmp_home("dev-pings");
        let stale = chrono::Utc::now() - chrono::Duration::seconds(DEV_IDLE_THRESHOLD_SECS + 60);
        write_activity_at(&home, "dev", stale);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);
        let lead = crate::inbox::drain(&home, "lead");
        assert_eq!(lead.len(), 1, "lead must receive idle alert: {lead:?}");
        assert_eq!(lead[0].kind.as_deref(), Some("dev_idle_watchdog"));
        assert!(lead[0].text.contains("dev"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dev_watchdog_no_ping_when_progress_within_window() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT");
        let home = tmp_home("dev-no-ping");
        let recent = chrono::Utc::now() - chrono::Duration::seconds(DEV_IDLE_THRESHOLD_SECS - 60);
        write_activity_at(&home, "dev", recent);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);
        let lead = crate::inbox::drain(&home, "lead");
        assert!(
            lead.is_empty(),
            "active dev must NOT trigger alert: {lead:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dev_watchdog_progress_reset_after_ping_resumes_normal_cycle() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT");
        let home = tmp_home("dev-reset");
        // First scan: dev stale → alert.
        let stale = chrono::Utc::now() - chrono::Duration::seconds(DEV_IDLE_THRESHOLD_SECS + 60);
        write_activity_at(&home, "dev", stale);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);
        let after_first = crate::inbox::drain(&home, "lead");
        assert_eq!(after_first.len(), 1, "first scan alerts");
        // Touch activity (simulate dev resuming work).
        touch_agent_activity(&home, "dev");
        // Second scan: dev fresh → no alert.
        scan_and_emit(&home, &mut last_alerted);
        let after_second = crate::inbox::drain(&home, "lead");
        assert!(
            after_second.is_empty(),
            "post-reset must not re-alert: {after_second:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn general_watchdog_detects_fleet_idle_when_sprint_active() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT");
        let home = tmp_home("fleet-idle");
        // Multiple tracked agents, ALL stale > 30min.
        let stale_dev =
            chrono::Utc::now() - chrono::Duration::seconds(FLEET_IDLE_THRESHOLD_SECS + 60);
        let stale_lead =
            chrono::Utc::now() - chrono::Duration::seconds(FLEET_IDLE_THRESHOLD_SECS + 120);
        let stale_reviewer =
            chrono::Utc::now() - chrono::Duration::seconds(FLEET_IDLE_THRESHOLD_SECS + 200);
        // Note: dev stale beyond DEV_IDLE_THRESHOLD too — but
        // FLEET_IDLE_THRESHOLD < DEV_IDLE_THRESHOLD, so dev vantage
        // alone might also fire. We assert the fleet vantage
        // separately fires.
        write_activity_at(&home, "dev", stale_dev);
        write_activity_at(&home, "lead", stale_lead);
        write_activity_at(&home, "reviewer", stale_reviewer);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);
        let general = crate::inbox::drain(&home, "general");
        assert!(
            general
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "general must receive fleet alert: {general:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn general_watchdog_distinguishes_heartbeat_lag_from_real_idle() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT");
        let home = tmp_home("fleet-not-idle");
        // ONE agent recently active → fleet NOT idle, no alert.
        // (The "real idle" gate requires EVERY agent to exceed the
        // threshold; one fresh entry pulls the fleet out of idle
        // state. Heartbeat lag = whole fleet appears stale; real
        // partial activity = at least one fresh entry.)
        let stale = chrono::Utc::now() - chrono::Duration::seconds(FLEET_IDLE_THRESHOLD_SECS + 60);
        let recent = chrono::Utc::now() - chrono::Duration::seconds(60);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        write_activity_at(&home, "general", recent);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);
        let general = crate::inbox::drain(&home, "general");
        assert!(
            !general
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "fleet partial-activity must NOT trigger fleet alert: {general:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn general_watchdog_auto_investigation_includes_decision_log_scan() {
        // Lead-spec name surface: the alert text must mention the
        // decision-log scan so operator + general both see the
        // intended next-step. (Auto-investigation = the alert
        // body's recommended-action text.)
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT");
        let home = tmp_home("fleet-investigation");
        let stale = chrono::Utc::now() - chrono::Duration::seconds(FLEET_IDLE_THRESHOLD_SECS + 60);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);
        let general = crate::inbox::drain(&home, "general");
        let fleet_msg = general
            .iter()
            .find(|m| m.kind.as_deref() == Some("fleet_idle_watchdog"))
            .expect("fleet alert");
        assert!(
            fleet_msg.text.contains("decision log"),
            "alert text must direct to decision log: {}",
            fleet_msg.text
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Defensive bonuses ──────────────────────────────────────────

    #[test]
    fn touch_agent_activity_updates_sidecar() {
        let home = tmp_home("touch");
        touch_agent_activity(&home, "dev");
        let read = read_agent_last_active(&home, "dev");
        assert!(read.is_some(), "post-touch must be readable");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn touch_with_empty_agent_is_noop() {
        let home = tmp_home("empty-agent");
        touch_agent_activity(&home, "");
        // No file created, no panic.
        let dir = activity_dir(&home);
        assert!(!dir.exists() || std::fs::read_dir(&dir).unwrap().next().is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn read_returns_none_for_future_schema_version() {
        let home = tmp_home("forward-version");
        let dir = activity_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        let payload = serde_json::json!({
            "schema_version": SCHEMA_VERSION + 1,
            "agent": "future",
            "last_active_at": "2026-05-09T08:45:00Z",
        });
        std::fs::write(
            activity_path(&home, "future"),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .unwrap();
        assert!(read_agent_last_active(&home, "future").is_none());
        // File preserved on disk (forward-compat).
        assert!(activity_path(&home, "future").exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dev_watchdog_dedups_repeat_alert_within_window() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT");
        let home = tmp_home("dev-dedup");
        let stale = chrono::Utc::now() - chrono::Duration::seconds(DEV_IDLE_THRESHOLD_SECS + 60);
        write_activity_at(&home, "dev", stale);
        let mut last_alerted = HashMap::new();
        // Two scans without intervening activity touch → dedup
        // suppresses the second alert.
        scan_and_emit(&home, &mut last_alerted);
        scan_and_emit(&home, &mut last_alerted);
        let lead = crate::inbox::drain(&home, "lead");
        assert_eq!(lead.len(), 1, "second scan must be deduped: {lead:?}");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn fleet_watchdog_no_alert_when_no_agents_tracked() {
        // Edge case: empty fleet (no sidecars) must NOT trigger
        // fleet alert (avoids false-positive on first daemon
        // start before any send has happened).
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT");
        let home = tmp_home("fleet-empty");
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);
        let general = crate::inbox::drain(&home, "general");
        assert!(general.is_empty(), "empty fleet must not alert");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn maybe_scan_throttles_to_once_per_30_ticks() {
        let home = tmp_home("scan-throttle");
        let mut tracker = IdleWatchdogTracker::default();
        for _ in 1..TICKS_PER_IDLE_SCAN {
            assert!(!tracker.maybe_scan(&home));
        }
        assert!(tracker.maybe_scan(&home));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn watched_dev_agent_honors_env_override() {
        let _g = env_lock();
        std::env::set_var("AGEND_IDLE_WATCHDOG_AGENT", "custom-dev-name");
        let agent = watched_dev_agent();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        assert_eq!(agent, "custom-dev-name");
    }

    #[test]
    fn dev_idle_recipient_honors_env_override() {
        let _g = env_lock();
        std::env::set_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT", "alice");
        let r = dev_idle_recipient();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT");
        assert_eq!(r, "alice");
    }
}
