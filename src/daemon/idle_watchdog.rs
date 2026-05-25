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
use std::sync::atomic::{AtomicI64, Ordering};

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

/// Remove an agent's activity sidecar (file + lock). Called from
/// `full_delete_instance` so deleted agents stop appearing in the
/// fleet_idle_watchdog tracking list. Best-effort: IO failures are
/// logged and swallowed (matches the delete-path cleanup contract).
pub(crate) fn remove_agent_activity(home: &Path, agent: &str) {
    if agent.is_empty() {
        return;
    }
    let path = activity_path(home, agent);
    if path.exists() {
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!(agent, error = %e, "remove_agent_activity: sidecar delete failed");
        } else {
            tracing::debug!(agent, "remove_agent_activity: sidecar removed");
        }
    }
    let lock_path = activity_dir(home).join(format!(".{agent}.lock"));
    let _ = std::fs::remove_file(&lock_path);
}

/// Boot-time GC: remove activity sidecars for agents not present in
/// `fleet.yaml`. Prevents ghost entries from accumulating across
/// daemon restarts when instances are deleted while the daemon is
/// down (or if eager cleanup on delete_instance missed one).
pub(crate) fn gc_stale_activity_sidecars(home: &Path) {
    let live: std::collections::HashSet<String> =
        match crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) {
            Ok(cfg) => cfg.instances.keys().cloned().collect(),
            Err(_) => return,
        };
    let dir = activity_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut removed = 0u32;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(agent) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !live.contains(agent) {
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
            let _ = std::fs::remove_file(dir.join(format!(".{agent}.lock")));
        }
    }
    if removed > 0 {
        tracing::info!(removed, "gc_stale_activity_sidecars: cleaned ghost entries");
    }
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

/// #1084: snooze sidecar path.
fn snooze_path(home: &Path) -> PathBuf {
    home.join("fleet-idle-snooze.json")
}

/// #1084: on-disk shape for the fleet-idle snooze sidecar.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct FleetIdleSnooze {
    #[serde(default)]
    pub snoozed_until: String,
    #[serde(default)]
    pub actor: String,
}

/// #1084: check whether fleet-idle watchdog is currently snoozed.
pub(crate) fn is_fleet_idle_snoozed(home: &Path) -> bool {
    let content = match std::fs::read_to_string(snooze_path(home)) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let snooze: FleetIdleSnooze = match serde_json::from_str(&content) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let until = match chrono::DateTime::parse_from_rfc3339(&snooze.snoozed_until) {
        Ok(dt) => dt.with_timezone(&chrono::Utc),
        Err(_) => return false,
    };
    chrono::Utc::now() < until
}

/// Read and parse the snooze sidecar. Returns `None` if the file is
/// missing, malformed, or the snooze has expired.
pub(crate) fn get_snooze_state(home: &Path) -> Option<FleetIdleSnooze> {
    let content = std::fs::read_to_string(snooze_path(home)).ok()?;
    let snooze: FleetIdleSnooze = serde_json::from_str(&content).ok()?;
    let until = chrono::DateTime::parse_from_rfc3339(&snooze.snoozed_until)
        .ok()?
        .with_timezone(&chrono::Utc);
    if chrono::Utc::now() < until {
        Some(snooze)
    } else {
        None
    }
}

/// #1084: snooze fleet-idle watchdog until the given timestamp.
pub(crate) fn snooze_fleet_idle(
    home: &Path,
    until: chrono::DateTime<chrono::Utc>,
    actor: &str,
) -> anyhow::Result<FleetIdleSnooze> {
    let snooze = FleetIdleSnooze {
        snoozed_until: until.to_rfc3339(),
        actor: actor.to_string(),
    };
    let body = serde_json::to_string_pretty(&snooze)?;
    crate::store::atomic_write(&snooze_path(home), body.as_bytes())?;
    Ok(snooze)
}

/// #1084: resume fleet-idle watchdog (delete snooze sidecar).
pub(crate) fn resume_fleet_idle(home: &Path) {
    let _ = std::fs::remove_file(snooze_path(home));
}

/// #1076: epoch seconds when fleet-idle was last acked. 0 = no ack.
/// In-memory — daemon restart clears ack (correct semantics: restart
/// means new fleet lifecycle, stale ack should not carry over).
static FLEET_ACKED_AT: AtomicI64 = AtomicI64::new(0);

/// #1076: ack fleet-idle watchdog. Suppresses fleet alerts until at
/// least one tracked agent becomes active after the ack timestamp,
/// then auto-clears so the next all-idle window triggers normally.
pub(crate) fn ack_fleet_idle() -> i64 {
    let ts = chrono::Utc::now().timestamp();
    FLEET_ACKED_AT.store(ts, Ordering::Relaxed);
    ts
}

/// #1076: read current fleet ack state.
pub(crate) fn fleet_ack_status() -> Option<i64> {
    let ts = FLEET_ACKED_AT.load(Ordering::Relaxed);
    if ts > 0 {
        Some(ts)
    } else {
        None
    }
}

/// #1076: clear fleet ack (used by tests).
#[cfg(test)]
fn clear_fleet_ack() {
    FLEET_ACKED_AT.store(0, Ordering::Relaxed);
}

/// Vantage #12 — fleet-wide idle threshold check. Triggers when
/// EVERY tracked agent has been silent > FLEET threshold AND at
/// least one agent is tracked (avoid false-positive on empty fleet).
fn scan_fleet_vantage(
    home: &Path,
    now: &chrono::DateTime<chrono::Utc>,
    last_alerted: &mut HashMap<(&'static str, String), chrono::DateTime<chrono::Utc>>,
) {
    // #1084: skip entire fleet vantage when snoozed.
    if is_fleet_idle_snoozed(home) {
        return;
    }
    let raw_pairs = enumerate_agent_activity(home);
    if raw_pairs.is_empty() {
        return;
    }
    // #1022: filter ghost agents — only consider instances present in
    // fleet.yaml. If fleet.yaml is unreadable, fall back to unfiltered
    // (fail-open: better to alert on ghosts than miss a real stall).
    let pairs: Vec<(String, chrono::DateTime<chrono::Utc>)> =
        if let Ok(cfg) = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) {
            raw_pairs
                .into_iter()
                .filter(|(name, _)| cfg.instances.contains_key(name))
                .collect()
        } else {
            raw_pairs
        };
    if pairs.is_empty() {
        return;
    }
    // #1076: ack cooldown — if fleet idle was acked, suppress until at
    // least one tracked agent becomes active after the ack timestamp.
    let acked_epoch = FLEET_ACKED_AT.load(Ordering::Relaxed);
    if acked_epoch > 0 {
        if let Some(acked_dt) = chrono::DateTime::from_timestamp(acked_epoch, 0) {
            let any_active_since = pairs.iter().any(|(_, ts)| *ts > acked_dt);
            if !any_active_since {
                return;
            }
            FLEET_ACKED_AT.store(0, Ordering::Relaxed);
        }
    }
    // All agents must exceed the threshold for "fleet idle".
    let all_idle = pairs
        .iter()
        .all(|(_, ts)| now.signed_duration_since(*ts).num_seconds() > FLEET_IDLE_THRESHOLD_SECS);
    if !all_idle {
        return;
    }
    // #1141: suppress alert when no work is expected — empty task board
    // + no pending dispatches means silence is normal.
    if !has_expected_work(home) {
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

/// #1141: Check if there's work the fleet should be doing.
/// Returns true if open/claimed/in_progress tasks exist OR pending dispatches exist.
/// Fail-open: if no task board file exists, assumes work may be expected.
fn has_expected_work(home: &Path) -> bool {
    // Check pending dispatch sidecars first (cheap).
    let pending = crate::daemon::dispatch_idle::list_pending(home);
    if pending.iter().any(|d| d.status == "pending") {
        return true;
    }
    // Only suppress when we can confirm the task board is empty.
    // If the event log doesn't exist, we can't determine → fail-open.
    let log_path = home.join("task_events.jsonl");
    if !log_path.exists() {
        return true;
    }
    match crate::task_events::replay(home) {
        Ok(state) => state.tasks.values().any(|r| {
            matches!(
                r.status,
                crate::task_events::TaskStatus::Open
                    | crate::task_events::TaskStatus::Claimed
                    | crate::task_events::TaskStatus::InProgress
                    | crate::task_events::TaskStatus::Blocked
            )
        }),
        Err(_) => true, // can't read board → fail-open
    }
}

fn emit_idle_alert(
    home: &Path,
    recipient: &str,
    kind: &str,
    text: &str,
    correlation_agent: Option<&str>,
) {
    let mut msg = crate::inbox::InboxMessage::new_system(format!("system:{kind}"), kind, text)
        .with_delivery_mode("inbox_fallback");
    if let Some(agent) = correlation_agent {
        msg = msg.with_correlation_id(agent);
    }
    if let Err(e) = crate::inbox::enqueue_with_idle_hint(home, recipient, msg) {
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

    // ── #1022 ghost-agent cleanup tests ───────────────────────────

    #[test]
    fn remove_agent_activity_deletes_sidecar() {
        let home = tmp_home("remove-activity");
        touch_agent_activity(&home, "doomed");
        assert!(activity_path(&home, "doomed").exists());
        remove_agent_activity(&home, "doomed");
        assert!(
            !activity_path(&home, "doomed").exists(),
            "sidecar must be deleted after remove_agent_activity"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn remove_agent_activity_noop_for_missing_agent() {
        let home = tmp_home("remove-missing");
        remove_agent_activity(&home, "nonexistent");
    }

    #[test]
    fn gc_stale_activity_sidecars_removes_ghosts_keeps_live() {
        let home = tmp_home("gc-ghosts");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  dev:\n    backend: claude\n  lead:\n    backend: claude\n",
        )
        .unwrap();
        let now = chrono::Utc::now();
        write_activity_at(&home, "dev", now);
        write_activity_at(&home, "lead", now);
        write_activity_at(&home, "demo-lead", now);
        write_activity_at(&home, "conflict-test-1", now);
        assert_eq!(enumerate_agent_activity(&home).len(), 4);
        gc_stale_activity_sidecars(&home);
        let remaining: Vec<String> = enumerate_agent_activity(&home)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(remaining.len(), 2, "only live agents remain: {remaining:?}");
        assert!(remaining.contains(&"dev".to_string()));
        assert!(remaining.contains(&"lead".to_string()));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_stale_activity_sidecars_noop_without_fleet_yaml() {
        let home = tmp_home("gc-no-fleet");
        let now = chrono::Utc::now();
        write_activity_at(&home, "orphan", now);
        gc_stale_activity_sidecars(&home);
        assert_eq!(
            enumerate_agent_activity(&home).len(),
            1,
            "without fleet.yaml, gc must not delete anything"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn fleet_scan_excludes_ghost_agents_from_alert() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT");
        let home = tmp_home("fleet-ghost-filter");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  dev:\n    backend: claude\n  lead:\n    backend: claude\n",
        )
        .unwrap();
        let stale = chrono::Utc::now() - chrono::Duration::seconds(FLEET_IDLE_THRESHOLD_SECS + 60);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        write_activity_at(&home, "demo-lead", stale);
        write_activity_at(&home, "conflict-test-1", stale);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);
        let general = crate::inbox::drain(&home, "general");
        let fleet_msg = general
            .iter()
            .find(|m| m.kind.as_deref() == Some("fleet_idle_watchdog"))
            .expect("fleet alert must fire for stale live agents");
        assert!(
            !fleet_msg.text.contains("demo-lead"),
            "ghost agent 'demo-lead' must not appear in alert: {}",
            fleet_msg.text
        );
        assert!(
            !fleet_msg.text.contains("conflict-test-1"),
            "ghost agent 'conflict-test-1' must not appear in alert: {}",
            fleet_msg.text
        );
        assert!(
            fleet_msg.text.contains("dev"),
            "live agent 'dev' must appear in alert"
        );
        assert!(
            fleet_msg.text.contains("lead"),
            "live agent 'lead' must appear in alert"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #1084 snooze tests ──────────────────────────────────────────

    #[test]
    fn snooze_suppresses_fleet_idle_alert() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT");
        let home = tmp_home("snooze-suppress");
        let stale = chrono::Utc::now() - chrono::Duration::seconds(FLEET_IDLE_THRESHOLD_SECS + 60);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        // Snooze for 1 hour from now
        let until = chrono::Utc::now() + chrono::Duration::hours(1);
        snooze_fleet_idle(&home, until, "test").unwrap();
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);
        let general = crate::inbox::drain(&home, "general");
        assert!(
            !general
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "#1084: snoozed fleet must NOT emit alert: {general:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn expired_snooze_resumes_fleet_alert() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT");
        let home = tmp_home("snooze-expired");
        let stale = chrono::Utc::now() - chrono::Duration::seconds(FLEET_IDLE_THRESHOLD_SECS + 60);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        // Snooze with PAST timestamp (already expired)
        let past = chrono::Utc::now() - chrono::Duration::seconds(10);
        snooze_fleet_idle(&home, past, "test").unwrap();
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);
        let general = crate::inbox::drain(&home, "general");
        assert!(
            general
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "#1084: expired snooze must resume alerting: {general:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn snooze_does_not_suppress_dev_idle_alert() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT");
        let home = tmp_home("snooze-dev-unaffected");
        let stale = chrono::Utc::now() - chrono::Duration::seconds(DEV_IDLE_THRESHOLD_SECS + 60);
        write_activity_at(&home, "dev", stale);
        // Snooze fleet
        let until = chrono::Utc::now() + chrono::Duration::hours(1);
        snooze_fleet_idle(&home, until, "test").unwrap();
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);
        let lead = crate::inbox::drain(&home, "lead");
        assert!(
            lead.iter()
                .any(|m| m.kind.as_deref() == Some("dev_idle_watchdog")),
            "#1084: fleet snooze must NOT affect dev vantage: {lead:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resume_clears_snooze() {
        let home = tmp_home("snooze-resume");
        let until = chrono::Utc::now() + chrono::Duration::hours(1);
        snooze_fleet_idle(&home, until, "test").unwrap();
        assert!(is_fleet_idle_snoozed(&home));
        resume_fleet_idle(&home);
        assert!(!is_fleet_idle_snoozed(&home));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn fleet_scan_no_alert_when_only_ghosts_stale() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT");
        let home = tmp_home("fleet-only-ghosts");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  dev:\n    backend: claude\n",
        )
        .unwrap();
        let recent = chrono::Utc::now() - chrono::Duration::seconds(60);
        let stale = chrono::Utc::now() - chrono::Duration::seconds(FLEET_IDLE_THRESHOLD_SECS + 60);
        write_activity_at(&home, "dev", recent);
        write_activity_at(&home, "ghost-1", stale);
        write_activity_at(&home, "ghost-2", stale);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);
        let general = crate::inbox::drain(&home, "general");
        assert!(
            !general
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "active live agent + stale ghosts must NOT trigger fleet alert: {general:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #1076 fleet-idle ack tests ───────────────────────────────

    #[test]
    fn fleet_scan_suppressed_after_ack() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT");
        let home = tmp_home("ack-suppress");
        let stale = chrono::Utc::now() - chrono::Duration::seconds(FLEET_IDLE_THRESHOLD_SECS + 60);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        ack_fleet_idle();
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);
        let general = crate::inbox::drain(&home, "general");
        assert!(
            !general
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "acked fleet idle must NOT trigger alert: {general:?}"
        );
        clear_fleet_ack();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn fleet_scan_resumes_after_post_ack_activity() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT");
        let home = tmp_home("ack-resume");
        let stale = chrono::Utc::now() - chrono::Duration::seconds(FLEET_IDLE_THRESHOLD_SECS + 60);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        // Ack in the past so post-ack activity can be simulated.
        let past_ack =
            chrono::Utc::now() - chrono::Duration::seconds(FLEET_IDLE_THRESHOLD_SECS + 120);
        FLEET_ACKED_AT.store(past_ack.timestamp(), Ordering::Relaxed);
        // Simulate one agent becoming active AFTER ack, then going idle again.
        let post_ack_active = past_ack + chrono::Duration::seconds(30);
        write_activity_at(&home, "dev", post_ack_active);
        write_activity_at(&home, "lead", stale);
        let mut last_alerted = HashMap::new();
        // dev's last_active > acked_at → ack auto-clears.
        // Both agents' activity timestamps are well past threshold
        // from real Utc::now(), so the alert should fire.
        scan_and_emit(&home, &mut last_alerted);
        let general = crate::inbox::drain(&home, "general");
        assert!(
            general
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "post-ack activity + re-idle must trigger fleet alert: {general:?}"
        );
        assert_eq!(
            FLEET_ACKED_AT.load(Ordering::Relaxed),
            0,
            "ack must auto-clear after post-ack activity detected"
        );
        clear_fleet_ack();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dev_vantage_unaffected_by_fleet_ack() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT");
        let home = tmp_home("ack-dev-unaffected");
        let stale = chrono::Utc::now() - chrono::Duration::seconds(DEV_IDLE_THRESHOLD_SECS + 60);
        write_activity_at(&home, "dev", stale);
        ack_fleet_idle();
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);
        let lead = crate::inbox::drain(&home, "lead");
        assert!(
            lead.iter()
                .any(|m| m.kind.as_deref() == Some("dev_idle_watchdog")),
            "fleet ack must NOT suppress dev vantage: {lead:?}"
        );
        clear_fleet_ack();
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1141: fleet idle suppressed when task board is empty (all done) and no pending dispatches.
    #[test]
    fn fleet_idle_suppressed_when_no_work_expected() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT");
        let home = tmp_home("1141-no-work");
        let stale = chrono::Utc::now() - chrono::Duration::seconds(FLEET_IDLE_THRESHOLD_SECS + 60);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        // Create an empty task_events.jsonl (board exists, no open tasks).
        std::fs::write(home.join("task_events.jsonl"), "").unwrap();
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);
        let general = crate::inbox::drain(&home, "general");
        assert!(
            !general
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "#1141: fleet idle must be suppressed when no work expected: {general:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1141: fleet idle fires when open task exists and agents are idle.
    #[test]
    fn fleet_idle_fires_when_open_task_exists() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT");
        let home = tmp_home("1141-open-task");
        let stale = chrono::Utc::now() - chrono::Duration::seconds(FLEET_IDLE_THRESHOLD_SECS + 60);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        // Create a task on the board (open, assigned to dev).
        crate::task_events::append(
            &home,
            &crate::task_events::InstanceName("lead".to_string()),
            crate::task_events::TaskEvent::Created {
                task_id: crate::task_events::TaskId("t-test-1141".to_string()),
                title: "test task".to_string(),
                description: String::new(),
                priority: "normal".to_string(),
                owner: Some(crate::task_events::InstanceName("dev".to_string())),
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
            },
        )
        .unwrap();
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted);
        let general = crate::inbox::drain(&home, "general");
        assert!(
            general
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "#1141: fleet idle must fire when open tasks exist: {general:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
