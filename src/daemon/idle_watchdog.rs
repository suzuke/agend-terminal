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
//! ## Standby-role exemption (#1438 / #1491C)
//!
//! Per-agent (dev-vantage) idle alerts skip **team orchestrators** (leads)
//! automatically — a lead with no in-flight task is expected to be quiet, so
//! flagging it is pure noise. Other standby roles (e.g. a reviewer waiting for
//! the next CI handoff) are exempted by the operator setting
//! `idle_watchdog_enabled: false` on that instance in fleet.yaml (the scan
//! already filters on that flag). Operator decision (#1491): orchestrators are
//! the only role reliably derivable from fleet.yaml — reviewer `role:` is
//! often unset — so reviewers use the explicit opt-out.
//!
//! ## Vantages
//!
//! ### #10 — dev 60min idle watchdog (P0)
//! Scans the watched-agent's last_active timestamp. When elapsed
//! exceeds `dev_idle_threshold_secs() = 3600` (60 min), emits an
//! inbox ping to `lead` so the orchestrator can re-dispatch /
//! unblock. Default watched agent: `dev`. Tunable via fleet.yaml
//! `watchdog.idle_watchdog_agent` (env `AGEND_IDLE_WATCHDOG_AGENT` is a
//! deprecated fallback).
//!
//! ### #12 — cross-vantage 30min fleet-idle guard (P1)
//! Scans the entire fleet's last_active timestamps. When EVERY
//! tracked agent has been idle > `fleet_idle_threshold_secs() = 1800`
//! (30 min) AND at least one agent has had recent activity (i.e.
//! a sidecar exists), emits an inbox ping to `lead` (#1563; was
//! `general`) so the orchestrator surfaces / re-dispatches the
//! fleet stall. Overridable via fleet.yaml `watchdog.fleet_recipient`
//! (env `AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT` is a deprecated fallback).
//! The
//! "at least one tracked" guard distinguishes "fleet really
//! stalled" from "fleet not yet started" / "all sidecars stale".
//!
//! ## Activity tracking sidecar
//!
//! `<home>/agent-activity/<agent>.json` — touched whenever the agent
//! sends a message via the unified `send` handler. Per-agent flock
//! for concurrency. Forward-compat preserved via `#[serde(default)]`.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};

const ACTIVITY_DIR: &str = "agent-activity";
const SCHEMA_VERSION: u32 = 1;

/// Lead-spec threshold for the dev watchdog (vantage #10): agent
/// silent > 60 min → ping lead. Overridable via runtime-config.json (#1085).
pub(crate) fn dev_idle_threshold_secs() -> i64 {
    crate::runtime_config::get().dev_idle_threshold_secs
}

/// Lead-spec threshold for the fleet watchdog (vantage #12): every
/// tracked agent silent > 30 min → ping lead (#1563; recipient was `general`).
/// Threshold overridable via runtime-config.json (#1085).
pub(crate) fn fleet_idle_threshold_secs() -> i64 {
    crate::runtime_config::get().fleet_idle_threshold_secs
}

/// #1438: max TTL for a fleet-idle ack (seconds). Backstop so an ack never
/// suppresses forever when the board never progresses. Overridable via
/// runtime-config.json.
pub(crate) fn fleet_idle_ack_ttl_secs() -> i64 {
    crate::runtime_config::get().fleet_idle_ack_ttl_secs
}

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
pub(crate) struct IdleWatchdogTracker {
    /// Cadence gate — throttles scans to once per [`TICKS_PER_IDLE_SCAN`]
    /// supervisor ticks (fire-on-Nth).
    gate: crate::daemon::cadence_gate::CadenceGate,
    /// (vantage, agent_or_fleet_marker) → last alert ts.
    last_alerted_at: HashMap<(&'static str, String), chrono::DateTime<chrono::Utc>>,
    /// #1739 boot-seed latch for the DEV vantage. First scan seeds the dev
    /// entries of `last_alerted_at` (stamped now) WITHOUT emitting, so a restart
    /// doesn't re-page about agents already idle before the restart. The fleet
    /// vantage is unaffected — it has its own persisted snooze.
    seeded: bool,
}

impl Default for IdleWatchdogTracker {
    fn default() -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_interval(TICKS_PER_IDLE_SCAN),
            last_alerted_at: HashMap::new(),
            seeded: false,
        }
    }
}

impl IdleWatchdogTracker {
    /// Increment tick counter and run scans every
    /// [`TICKS_PER_IDLE_SCAN`] calls.
    pub(crate) fn maybe_scan(&mut self, home: &Path) -> bool {
        if !self.gate.fire() {
            return false;
        }
        let seeding = !self.seeded;
        self.seeded = true;
        scan_and_emit(home, &mut self.last_alerted_at, seeding);
        true
    }
}

/// Pure scan logic: detects + emits idle alerts at both vantages.
/// Exposed for tests so they can invoke without 30-tick wait.
pub(crate) fn scan_and_emit(
    home: &Path,
    last_alerted: &mut HashMap<(&'static str, String), chrono::DateTime<chrono::Utc>>,
    seeding: bool,
) {
    if !crate::runtime_config::get().idle_watchdog_enabled {
        return;
    }
    if is_fleet_idle_snoozed(home) {
        return;
    }
    let now = chrono::Utc::now();
    // #1739: only the DEV vantage gets boot-seeded (it used the in-memory
    // `last_alerted` dedup that re-fired on restart). The fleet vantage has its
    // own persisted snooze, so it is left as-is.
    scan_dev_vantage(home, &now, last_alerted, seeding);
    scan_fleet_vantage(home, &now, last_alerted);
}

/// #1256: check if the task board has any tasks (indicating active use).
/// When the board has tasks, agents without assigned work should not
/// trigger idle alerts — their silence is expected.
fn task_board_is_active(home: &Path) -> bool {
    crate::task_events::replay(home)
        .map(|s| !s.tasks.is_empty())
        .unwrap_or(false)
}

/// Look up the current in-progress task for an agent (if any).
fn current_agent_task(home: &Path, agent: &str) -> Option<String> {
    let state = crate::task_events::replay(home).ok()?;
    state
        .tasks
        .values()
        .find(|t| {
            matches!(
                t.status,
                crate::task_events::TaskStatus::InProgress
                    | crate::task_events::TaskStatus::Claimed
            ) && t.owner.as_ref().map(|n| n.as_str()) == Some(agent)
        })
        .map(|t| format!("{} — {}", t.id, t.title))
}

/// Parse an RFC3339 task-board timestamp to UTC.
fn parse_ts(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&chrono::Utc))
}

/// #1438: a task-board "progress" event happened after `since` — some task is
/// now Claimed/InProgress/InReview/Verified/Done with `updated_at` past the
/// ack timestamp. This is the "sprint resumed" signal that lifts a fleet-idle
/// ack. Mere task creation (status Open) does NOT count as progress.
fn board_progressed_since(home: &Path, since: &chrono::DateTime<chrono::Utc>) -> bool {
    let Ok(state) = crate::task_events::replay(home) else {
        return false;
    };
    state.tasks.values().any(|t| {
        matches!(
            t.status,
            crate::task_events::TaskStatus::Claimed
                | crate::task_events::TaskStatus::InProgress
                | crate::task_events::TaskStatus::InReview
                | crate::task_events::TaskStatus::Verified
                | crate::task_events::TaskStatus::Done
        ) && parse_ts(&t.updated_at).is_some_and(|u| u > *since)
    })
}

/// #1438: an agent that OWNS an active task (Open/Claimed/InProgress/Blocked)
/// resumed activity after `since`. Worker heartbeat — recovers the ack even
/// when the board update lags or the work is tracked off-board. Deliberately
/// scoped to task owners so on-demand chatter (e.g. `general` answering the
/// operator while owning no task) does NOT lift the ack — that on-any-activity
/// recovery was the #1438 ack-wash bug.
fn task_owner_active_since(
    home: &Path,
    since: &chrono::DateTime<chrono::Utc>,
    pairs: &[(String, chrono::DateTime<chrono::Utc>)],
) -> bool {
    let Ok(state) = crate::task_events::replay(home) else {
        return false;
    };
    let owners: HashSet<&str> = state
        .tasks
        .values()
        .filter(|t| {
            matches!(
                t.status,
                crate::task_events::TaskStatus::Open
                    | crate::task_events::TaskStatus::Claimed
                    | crate::task_events::TaskStatus::InProgress
                    | crate::task_events::TaskStatus::Blocked
            )
        })
        .filter_map(|t| t.owner.as_ref().map(|n| n.as_str()))
        .collect();
    if owners.is_empty() {
        return false;
    }
    pairs
        .iter()
        .any(|(name, ts)| ts > since && owners.contains(name.as_str()))
}

/// Vantage #10 — per-agent idle threshold check. Iterates all fleet
/// instances, using each agent's `timeout_secs` (falling back to the
/// global `dev_idle_threshold_secs`). Legacy single-agent mode is
/// preserved when `AGEND_IDLE_WATCHDOG_AGENT` env var is set.
fn scan_dev_vantage(
    home: &Path,
    now: &chrono::DateTime<chrono::Utc>,
    last_alerted: &mut HashMap<(&'static str, String), chrono::DateTime<chrono::Utc>>,
    seeding: bool,
) {
    // #1812-followup: single-agent override now comes from fleet.yaml
    // `watchdog.idle_watchdog_agent` (env `AGEND_IDLE_WATCHDOG_AGENT` is a
    // deprecated fallback). `Some` → legacy single-agent mode; `None` → the
    // modern per-instance iteration below.
    let single_override = crate::fleet::watchdog::resolve_idle_watchdog_agent(home);

    let agents: Vec<(String, i64)> = if let Some(single) = single_override {
        vec![(single, dev_idle_threshold_secs())]
    } else if let Ok(cfg) = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) {
        // #1438/#1491(C): auto-exempt team orchestrators (leads) from idle
        // alerts — a lead with no in-flight task is expected to be quiet, so
        // flagging it idle is pure noise. Standby reviewers opt out via the
        // per-instance `idle_watchdog_enabled: false` (operator decision,
        // #1491). Orchestrators are exempted automatically because their role
        // (waiting on the team) is inherently a standby role.
        let orchestrators: std::collections::HashSet<&str> = cfg
            .teams
            .values()
            .filter_map(|t| t.orchestrator.as_deref())
            .collect();
        cfg.instances
            .iter()
            .filter(|(name, ic)| {
                ic.idle_watchdog_enabled
                    && !orchestrators.contains(name.as_str())
                    // #1563: an `OnDemand` coordinator is legitimately quiet
                    // between requests — exempt it from idle tracking (same
                    // knob that gates the supervisor stall-forward paths).
                    && ic.idle_expectation == crate::fleet::IdleExpectation::Active
            })
            .map(|(name, ic)| {
                let threshold = ic
                    .timeout_secs
                    .map(|s| s as i64)
                    .unwrap_or_else(dev_idle_threshold_secs);
                (name.clone(), threshold)
            })
            .collect()
    } else {
        // fleet.yaml unreadable AND no single-agent override → last-resort
        // single default. (The override layer above already consulted env.)
        vec![("dev".to_string(), dev_idle_threshold_secs())]
    };

    for (agent, threshold) in &agents {
        let Some(last_active) = read_agent_last_active(home, agent) else {
            continue;
        };
        let elapsed_secs = now.signed_duration_since(last_active).num_seconds();
        if elapsed_secs <= *threshold {
            continue;
        }
        let key = ("dev", agent.clone());
        if let Some(prev) = last_alerted.get(&key) {
            let since_alert = now.signed_duration_since(*prev).num_seconds();
            if since_alert < *threshold {
                continue;
            }
        }
        let task_info =
            current_agent_task(home, agent).unwrap_or_else(|| "(no active task)".to_string());
        // #1256: skip alert when task board is active and agent has no
        // assigned task — silence is expected when no work is assigned.
        if task_info == "(no active task)" && task_board_is_active(home) {
            continue;
        }
        // #1739 boot-seed: first scan records the idle agent without paging.
        if !seeding {
            route_idle_alert(
                home,
                &crate::fleet::watchdog::resolve_dev_idle_recipient(home),
                "dev_idle_watchdog",
                &format!(
                    "[dev_idle_watchdog] agent '{agent}' has been silent for \
                     {elapsed_secs}s (threshold {threshold}s). \
                     Current task: {task_info}. \
                     Possible dispatch protocol gap or unblock-needed state. \
                     Consider: lead dispatch / unblock-ping / decision-log scan.",
                ),
                Some(agent),
            );
        }
        last_alerted.insert(key, *now);
    }
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
    // #1084: snooze guard moved to scan_and_emit (#1240).
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
                .filter(|(name, _)| {
                    cfg.instances
                        .get(name)
                        // #1563: also drop `OnDemand` coordinators from the
                        // fleet-wide all-idle quorum — an exempt agent must not
                        // count toward "every tracked agent is silent".
                        .map(|ic| {
                            ic.idle_watchdog_enabled
                                && ic.idle_expectation == crate::fleet::IdleExpectation::Active
                        })
                        .unwrap_or(false)
                })
                .collect()
        } else {
            raw_pairs
        };
    if pairs.is_empty() {
        return;
    }
    // #1076 / #1438: ack cooldown. The ack lifts only on a genuine
    // "sprint resumed" signal — NOT on bare agent activity. Previously ANY
    // tracked agent active after the ack cleared it, so `general` answering
    // the operator (on-demand chatter) washed the ack and re-armed the alert
    // every ~30 min (#1438 ack-wash loop). Recovery now requires one of:
    //   (a) the task board progressed (a task claimed / advanced since ack),
    //   (b) a task-OWNING agent resumed activity (worker heartbeat — covers
    //       board-update lag / off-board work), or
    //   (c) the ack exceeded its max TTL (time backstop — never silent forever;
    //       on expiry the fleet is re-evaluated, not auto-alerted, because the
    //       all-idle + has_expected_work gates below still apply).
    let acked_epoch = FLEET_ACKED_AT.load(Ordering::Relaxed);
    if acked_epoch > 0 {
        if let Some(acked_dt) = chrono::DateTime::from_timestamp(acked_epoch, 0) {
            let ack_age = now.signed_duration_since(acked_dt).num_seconds();
            let recovered = board_progressed_since(home, &acked_dt)
                || task_owner_active_since(home, &acked_dt, &pairs)
                || ack_age > fleet_idle_ack_ttl_secs();
            if !recovered {
                return;
            }
            FLEET_ACKED_AT.store(0, Ordering::Relaxed);
        }
    }
    // All agents must exceed the threshold for "fleet idle".
    let all_idle = pairs
        .iter()
        .all(|(_, ts)| now.signed_duration_since(*ts).num_seconds() > fleet_idle_threshold_secs());
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
        if since_alert < fleet_idle_threshold_secs() {
            return;
        }
    }
    let oldest = pairs
        .iter()
        .map(|(_, ts)| now.signed_duration_since(*ts).num_seconds())
        .max()
        .unwrap_or(0);
    let agent_list: Vec<&str> = pairs.iter().map(|(n, _)| n.as_str()).collect();
    route_idle_alert(
        home,
        &crate::fleet::watchdog::resolve_fleet_idle_recipient(home),
        "fleet_idle_watchdog",
        &format!(
            "[fleet_idle_watchdog] all tracked agents silent > {}s \
             (max elapsed {oldest}s). Tracked agents: {agent_list:?}. \
             Cross-vantage signal: investigate decision log + git status + inbox \
             for stalled sprint state.",
            fleet_idle_threshold_secs()
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
    if pending
        .iter()
        .any(|d| d.status == crate::daemon::dispatch_idle::DispatchStatus::Pending)
    {
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
    let source = format!("system:{kind}");
    if let Err(e) = crate::inbox::notify_system(
        home,
        recipient,
        &source,
        kind,
        text,
        correlation_agent,
        None,
    ) {
        tracing::warn!(error = %e, recipient, kind, "idle_watchdog: enqueue failed");
    } else {
        tracing::info!(recipient, kind, "idle_watchdog: emitted inbox alert");
    }
}

/// #event-bus pattern #6 (Option A): gate-ON → emit `IdleAlert` (the subscriber
/// delivers via `emit_idle_alert`); gate-OFF (prod default) → the legacy direct
/// `emit_idle_alert`. No double-delivery, no gate-off regression. The recipient
/// is already resolved by the caller, so the legacy and bus paths deliver
/// identically. The legacy `else` is retired only at the final cutover.
fn route_idle_alert(
    home: &Path,
    recipient: &str,
    kind: &str,
    text: &str,
    correlation_agent: Option<&str>,
) {
    // #event-bus Step 2 (legacy-zero): the bus is the sole delivery path. The
    // recipient is already resolved by the caller and carried on the event.
    crate::daemon::event_bus::global().emit(
        home,
        crate::daemon::event_bus::EventKind::IdleAlert {
            recipient: recipient.to_string(),
            kind: kind.to_string(),
            text: text.to_string(),
            correlation_agent: correlation_agent.map(String::from),
        },
    );
}

/// #event-bus pattern #6: bus subscriber — deliver on an `IdleAlert` event via the
/// shared `emit_idle_alert`. Registered once at daemon startup.
fn handle_event(event: &crate::daemon::event_bus::Event) -> bool {
    if let crate::daemon::event_bus::EventKind::IdleAlert {
        recipient,
        kind,
        text,
        correlation_agent,
    } = &event.kind
    {
        emit_idle_alert(
            &event.home,
            recipient,
            kind,
            text,
            correlation_agent.as_deref(),
        );
        true
    } else {
        false
    }
}

/// #event-bus pattern #6: register the idle_watchdog delivery subscriber on the
/// global bus. Call ONCE at daemon startup. Home-agnostic — home travels on the event.
pub fn register_subscriber() {
    crate::daemon::event_bus::global().subscribe(handle_event);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // Test-only constants matching runtime defaults.
    #[allow(dead_code)]
    const DEV_IDLE_THRESHOLD_SECS: i64 = 3600;
    const FLEET_IDLE_THRESHOLD_SECS: i64 = 1800;
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

    /// #1438 test helper: create an Open task on the board owned by `owner`.
    fn seed_task(home: &Path, id: &str, owner: &str) {
        crate::task_events::append(
            home,
            &crate::task_events::InstanceName("lead".to_string()),
            crate::task_events::TaskEvent::Created {
                task_id: crate::task_events::TaskId(id.to_string()),
                title: "t".to_string(),
                description: String::new(),
                priority: "normal".to_string(),
                owner: Some(crate::task_events::InstanceName(owner.to_string())),
                due_at: None,
                depends_on: Vec::new(),
                routed_to: None,
                branch: None,
                bind: None,
                eta_secs: None,
                tags: vec![],
                parent_id: None,
            },
        )
        .unwrap();
    }

    /// #1438 test helper: advance a task to InProgress (board "progress" event).
    fn advance_task(home: &Path, id: &str, by: &str) {
        crate::task_events::append(
            home,
            &crate::task_events::InstanceName(by.to_string()),
            crate::task_events::TaskEvent::InProgress {
                task_id: crate::task_events::TaskId(id.to_string()),
                by: crate::task_events::InstanceName(by.to_string()),
            },
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
        let stale = chrono::Utc::now() - chrono::Duration::seconds(dev_idle_threshold_secs() + 60);
        write_activity_at(&home, "dev", stale);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let lead = crate::inbox::drain(&home, "lead");
        // #1563: the fleet recipient now also defaults to `lead`, so a
        // single-agent idle fleet co-fires a `fleet_idle_watchdog` alert here
        // too. Assert the DEV vantage specifically (filter by kind), not total.
        let dev_alerts: Vec<_> = lead
            .iter()
            .filter(|m| m.kind.as_deref() == Some("dev_idle_watchdog"))
            .collect();
        assert_eq!(
            dev_alerts.len(),
            1,
            "lead must receive one dev idle alert: {lead:?}"
        );
        assert!(dev_alerts[0].text.contains("dev"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn boot_seed_suppresses_existing_idle_dev_then_no_reburst() {
        // #1739: the first scan after a fresh daemon start seeds an already-idle
        // dev agent into the dedup WITHOUT paging lead, and a subsequent scan
        // does not re-burst it. (Only the dev vantage is seeded; the fleet
        // vantage is unaffected, so we filter by the `dev_idle_watchdog` kind.)
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT");
        let home = tmp_home("dev-bootseed");
        let stale = chrono::Utc::now() - chrono::Duration::seconds(dev_idle_threshold_secs() + 60);
        write_activity_at(&home, "dev", stale);
        let count_dev_alerts = |home: &Path| {
            crate::inbox::drain(home, "lead")
                .into_iter()
                .filter(|m| m.kind.as_deref() == Some("dev_idle_watchdog"))
                .count()
        };

        let mut last_alerted = HashMap::new();
        // seeding scan: record the idle dev, do NOT page.
        scan_and_emit(&home, &mut last_alerted, true);
        assert_eq!(
            count_dev_alerts(&home),
            0,
            "boot-seed must NOT page lead for a restart-existing idle dev \
             (negative-probe: removing the `if !seeding` gate makes this fire)"
        );
        assert!(
            last_alerted.keys().any(|(vantage, _)| *vantage == "dev"),
            "boot-seed must record the idle dev in the dev-vantage dedup"
        );
        // next normal scan: the seeded dev stays suppressed within threshold.
        scan_and_emit(&home, &mut last_alerted, false);
        assert_eq!(
            count_dev_alerts(&home),
            0,
            "seeded idle dev must remain suppressed on the next scan"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1438/#1491(C): a team orchestrator (lead) that has been silent past
    /// the threshold must NOT be idle-alerted — its standby is expected. A
    /// non-orchestrator member with the same silence still gets flagged.
    #[test]
    fn dev_watchdog_exempts_team_orchestrator() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT");
        let home = tmp_home("orch-exempt");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  lead:\n    backend: claude\n  worker:\n    backend: claude\n\
             teams:\n  t:\n    members: [lead, worker]\n    orchestrator: lead\n",
        )
        .unwrap();
        let stale = chrono::Utc::now() - chrono::Duration::seconds(dev_idle_threshold_secs() + 60);
        write_activity_at(&home, "lead", stale);
        write_activity_at(&home, "worker", stale);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        // Alerts are delivered to the dev recipient ("lead"); inspect them.
        let alerts = crate::inbox::drain(&home, "lead");
        let dev_alerts: Vec<&str> = alerts
            .iter()
            .filter(|m| m.kind.as_deref() == Some("dev_idle_watchdog"))
            .map(|m| m.text.as_str())
            .collect();
        assert!(
            dev_alerts.iter().any(|t| t.contains("'worker'")),
            "non-orchestrator worker must still be flagged: {dev_alerts:?}"
        );
        assert!(
            !dev_alerts.iter().any(|t| t.contains("'lead'")),
            "orchestrator 'lead' must be exempt from idle alerts: {dev_alerts:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dev_watchdog_no_ping_when_progress_within_window() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT");
        let home = tmp_home("dev-no-ping");
        let recent = chrono::Utc::now() - chrono::Duration::seconds(dev_idle_threshold_secs() - 60);
        write_activity_at(&home, "dev", recent);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let lead = crate::inbox::drain(&home, "lead");
        // #1563: assert the DEV vantage specifically — a `fleet_idle_watchdog`
        // alert may co-land on `lead` (the agent is past the shorter fleet
        // threshold), which is a separate vantage this test does not exercise.
        assert!(
            !lead
                .iter()
                .any(|m| m.kind.as_deref() == Some("dev_idle_watchdog")),
            "active dev (within dev window) must NOT trigger a dev idle alert: {lead:?}"
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
        let stale = chrono::Utc::now() - chrono::Duration::seconds(dev_idle_threshold_secs() + 60);
        write_activity_at(&home, "dev", stale);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let after_first = crate::inbox::drain(&home, "lead");
        // #1563: filter the DEV vantage (fleet_idle co-fires to lead now).
        assert_eq!(
            after_first
                .iter()
                .filter(|m| m.kind.as_deref() == Some("dev_idle_watchdog"))
                .count(),
            1,
            "first scan alerts (dev vantage)"
        );
        // Touch activity (simulate dev resuming work).
        touch_agent_activity(&home, "dev");
        // Second scan: dev fresh → no alert.
        scan_and_emit(&home, &mut last_alerted, false);
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
            chrono::Utc::now() - chrono::Duration::seconds(fleet_idle_threshold_secs() + 60);
        let stale_lead =
            chrono::Utc::now() - chrono::Duration::seconds(fleet_idle_threshold_secs() + 120);
        let stale_reviewer =
            chrono::Utc::now() - chrono::Duration::seconds(fleet_idle_threshold_secs() + 200);
        // Note: dev stale beyond DEV_IDLE_THRESHOLD too — but
        // FLEET_IDLE_THRESHOLD < DEV_IDLE_THRESHOLD, so dev vantage
        // alone might also fire. We assert the fleet vantage
        // separately fires.
        write_activity_at(&home, "dev", stale_dev);
        write_activity_at(&home, "lead", stale_lead);
        write_activity_at(&home, "reviewer", stale_reviewer);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let recipient = crate::inbox::drain(&home, "lead");
        assert!(
            recipient
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "lead (#1563 default fleet recipient) must receive fleet alert: {recipient:?}"
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
        let stale =
            chrono::Utc::now() - chrono::Duration::seconds(fleet_idle_threshold_secs() + 60);
        let recent = chrono::Utc::now() - chrono::Duration::seconds(60);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        write_activity_at(&home, "general", recent);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let recipient = crate::inbox::drain(&home, "lead");
        assert!(
            !recipient
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "fleet partial-activity must NOT trigger fleet alert: {recipient:?}"
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
        let stale =
            chrono::Utc::now() - chrono::Duration::seconds(fleet_idle_threshold_secs() + 60);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let recipient = crate::inbox::drain(&home, "lead");
        let fleet_msg = recipient
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
        let stale = chrono::Utc::now() - chrono::Duration::seconds(dev_idle_threshold_secs() + 60);
        write_activity_at(&home, "dev", stale);
        let mut last_alerted = HashMap::new();
        // Two scans without intervening activity touch → dedup
        // suppresses the second alert.
        scan_and_emit(&home, &mut last_alerted, false);
        scan_and_emit(&home, &mut last_alerted, false);
        let lead = crate::inbox::drain(&home, "lead");
        // #1563: filter the DEV vantage (fleet_idle co-fires to lead now); the
        // dedup contract is per-vantage → exactly one dev alert across 2 scans.
        assert_eq!(
            lead.iter()
                .filter(|m| m.kind.as_deref() == Some("dev_idle_watchdog"))
                .count(),
            1,
            "second scan must be deduped (dev vantage): {lead:?}"
        );
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
        scan_and_emit(&home, &mut last_alerted, false);
        let recipient = crate::inbox::drain(&home, "lead");
        assert!(recipient.is_empty(), "empty fleet must not alert");
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

    // #1812-followup: env-override + fleet-config precedence for the idle
    // watchdog recipients/agent now live in `fleet::watchdog` tests (the
    // resolution logic moved there). The §3.9 real-entry test
    // `fleet_dev_recipient_routes_idle_alert` below proves the fleet.yaml value
    // reaches the live scan path.

    /// §3.9 real-entry: a fleet.yaml `watchdog.dev_recipient` (+ single-agent
    /// `idle_watchdog_agent`) must reach the live `scan_and_emit` → dev-vantage →
    /// `route_idle_alert` path — the alert lands in the fleet-configured recipient,
    /// NOT the built-in `lead` default. Proves the config is read at the real call
    /// site, not just by the resolver in isolation.
    #[test]
    fn fleet_dev_recipient_routes_idle_alert() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT");
        let home = tmp_home("fleet-dev-route");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "watchdog:\n  idle_watchdog_agent: dev\n  dev_recipient: custom-arbiter\ninstances: {}\n",
        )
        .unwrap();
        let stale = chrono::Utc::now() - chrono::Duration::seconds(dev_idle_threshold_secs() + 60);
        write_activity_at(&home, "dev", stale);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let configured = crate::inbox::drain(&home, "custom-arbiter");
        assert_eq!(
            configured
                .iter()
                .filter(|m| m.kind.as_deref() == Some("dev_idle_watchdog"))
                .count(),
            1,
            "fleet-configured dev_recipient must receive the dev idle alert: {configured:?}"
        );
        let default_lead = crate::inbox::drain(&home, "lead");
        assert_eq!(
            default_lead
                .iter()
                .filter(|m| m.kind.as_deref() == Some("dev_idle_watchdog"))
                .count(),
            0,
            "built-in default `lead` must NOT receive the dev alert once fleet.yaml overrides it"
        );
        std::fs::remove_dir_all(&home).ok();
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
        let stale =
            chrono::Utc::now() - chrono::Duration::seconds(fleet_idle_threshold_secs() + 60);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        write_activity_at(&home, "demo-lead", stale);
        write_activity_at(&home, "conflict-test-1", stale);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let recipient = crate::inbox::drain(&home, "lead");
        let fleet_msg = recipient
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
        let stale =
            chrono::Utc::now() - chrono::Duration::seconds(fleet_idle_threshold_secs() + 60);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        // Snooze for 1 hour from now
        let until = chrono::Utc::now() + chrono::Duration::hours(1);
        snooze_fleet_idle(&home, until, "test").unwrap();
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let recipient = crate::inbox::drain(&home, "lead");
        assert!(
            !recipient
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "#1084: snoozed fleet must NOT emit alert: {recipient:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn expired_snooze_resumes_fleet_alert() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT");
        let home = tmp_home("snooze-expired");
        let stale =
            chrono::Utc::now() - chrono::Duration::seconds(fleet_idle_threshold_secs() + 60);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        // Snooze with PAST timestamp (already expired)
        let past = chrono::Utc::now() - chrono::Duration::seconds(10);
        snooze_fleet_idle(&home, past, "test").unwrap();
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let recipient = crate::inbox::drain(&home, "lead");
        assert!(
            recipient
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "#1084: expired snooze must resume alerting: {recipient:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn snooze_suppresses_dev_idle_alert() {
        // #1240: snooze now suppresses ALL alerts (both fleet + dev).
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT");
        let home = tmp_home("snooze-dev-suppressed");
        let stale = chrono::Utc::now() - chrono::Duration::seconds(dev_idle_threshold_secs() + 60);
        write_activity_at(&home, "dev", stale);
        // Snooze fleet
        let until = chrono::Utc::now() + chrono::Duration::hours(1);
        snooze_fleet_idle(&home, until, "test").unwrap();
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let lead = crate::inbox::drain(&home, "lead");
        assert!(
            !lead
                .iter()
                .any(|m| m.kind.as_deref() == Some("dev_idle_watchdog")),
            "#1240: snooze must suppress dev vantage alerts too: {lead:?}"
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
        let stale =
            chrono::Utc::now() - chrono::Duration::seconds(fleet_idle_threshold_secs() + 60);
        write_activity_at(&home, "dev", recent);
        write_activity_at(&home, "ghost-1", stale);
        write_activity_at(&home, "ghost-2", stale);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let recipient = crate::inbox::drain(&home, "lead");
        assert!(
            !recipient
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "active live agent + stale ghosts must NOT trigger fleet alert: {recipient:?}"
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
        let stale =
            chrono::Utc::now() - chrono::Duration::seconds(fleet_idle_threshold_secs() + 60);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        ack_fleet_idle();
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let recipient = crate::inbox::drain(&home, "lead");
        assert!(
            !recipient
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "acked fleet idle must NOT trigger alert: {recipient:?}"
        );
        clear_fleet_ack();
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1438: fleet ack resumes on TASK-BOARD PROGRESS, not bare agent activity.
    /// Previously this test asserted that any post-ack agent activity cleared
    /// the ack — encoding the #1438 ack-wash bug as if it were the spec.
    /// Rewritten: board progress is the resume trigger. The "bare activity must
    /// NOT resume" contract is pinned by `ack_survives_unrelated_agent_activity`.
    #[test]
    fn fleet_scan_resumes_on_board_progress() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT");
        let home = tmp_home("ack-resume-board");
        let stale =
            chrono::Utc::now() - chrono::Duration::seconds(fleet_idle_threshold_secs() + 60);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        // Ack in the past; agents stay idle (no post-ack activity at all).
        let past_ack =
            chrono::Utc::now() - chrono::Duration::seconds(fleet_idle_threshold_secs() + 120);
        FLEET_ACKED_AT.store(past_ack.timestamp(), Ordering::Relaxed);
        // Board PROGRESSES after the ack: a task is created then advanced now.
        seed_task(&home, "t-resume", "dev");
        advance_task(&home, "t-resume", "dev");
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let recipient = crate::inbox::drain(&home, "lead");
        assert!(
            recipient
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "post-ack board progress + still-idle agents must trigger alert: {recipient:?}"
        );
        assert_eq!(
            FLEET_ACKED_AT.load(Ordering::Relaxed),
            0,
            "ack must clear after board progress detected"
        );
        clear_fleet_ack();
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1438 (core regression): ack must SURVIVE unrelated agent activity.
    /// `general` answering the operator (on-demand chatter, owns no task) must
    /// NOT clear the ack — that on-any-activity recovery was the ack-wash bug
    /// that re-fired the alert every ~30 min.
    #[test]
    fn ack_survives_unrelated_agent_activity() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT");
        let home = tmp_home("ack-wash-regression");
        let stale =
            chrono::Utc::now() - chrono::Duration::seconds(fleet_idle_threshold_secs() + 120);
        write_activity_at(&home, "general", stale);
        write_activity_at(&home, "dev", stale);
        // Open task owned by dev → has_expected_work true (alert path armed).
        seed_task(&home, "t-open", "dev");
        // Ack now; then general (no owned task) chatters AFTER the ack.
        let acked = ack_fleet_idle();
        let post_ack =
            chrono::DateTime::from_timestamp(acked, 0).unwrap() + chrono::Duration::seconds(5);
        write_activity_at(&home, "general", post_ack);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let recipient = crate::inbox::drain(&home, "lead");
        assert!(
            !recipient
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "on-demand chatter must NOT wash the ack / re-fire alert: {recipient:?}"
        );
        assert_ne!(
            FLEET_ACKED_AT.load(Ordering::Relaxed),
            0,
            "ack must remain set after unrelated activity"
        );
        clear_fleet_ack();
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1438: ack releases when a task-OWNING agent resumes (worker heartbeat),
    /// even without an explicit board status-advance event — covers board-update
    /// lag / off-board work. Scoped to owners so chatter can't trigger it.
    #[test]
    fn ack_released_on_task_owner_heartbeat() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT");
        let home = tmp_home("ack-owner-heartbeat");
        let stale =
            chrono::Utc::now() - chrono::Duration::seconds(fleet_idle_threshold_secs() + 120);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        // dev owns an Open task (no status-advance after ack → board_progressed false).
        seed_task(&home, "t-owned", "dev");
        let past_ack =
            chrono::Utc::now() - chrono::Duration::seconds(fleet_idle_threshold_secs() + 120);
        FLEET_ACKED_AT.store(past_ack.timestamp(), Ordering::Relaxed);
        // dev (task owner) becomes active AFTER the ack.
        write_activity_at(&home, "dev", past_ack + chrono::Duration::seconds(30));
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        assert_eq!(
            FLEET_ACKED_AT.load(Ordering::Relaxed),
            0,
            "ack must clear when a task-owning agent resumes (heartbeat)"
        );
        clear_fleet_ack();
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1438: ack expires after its max TTL even with no board progress and no
    /// owner activity — the time backstop against permanent silence.
    #[test]
    fn ack_expires_after_max_ttl() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT");
        let home = tmp_home("ack-ttl");
        let stale = chrono::Utc::now() - chrono::Duration::seconds(fleet_idle_ack_ttl_secs() + 120);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        seed_task(&home, "t-ttl", "dev"); // work exists → alert path armed
                                          // Ack older than the max TTL; no progress, no owner activity since.
        let old_ack =
            chrono::Utc::now() - chrono::Duration::seconds(fleet_idle_ack_ttl_secs() + 60);
        FLEET_ACKED_AT.store(old_ack.timestamp(), Ordering::Relaxed);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        assert_eq!(
            FLEET_ACKED_AT.load(Ordering::Relaxed),
            0,
            "ack must expire and clear after exceeding max TTL"
        );
        clear_fleet_ack();
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1438: when the board's tasks are all Done and agents are idle, no alert
    /// fires (has_expected_work=false) — TTL expiry must not turn into permanent
    /// re-alert noise once the sprint is genuinely finished.
    #[test]
    fn done_board_idle_agents_no_alert() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT");
        let home = tmp_home("ack-done-board");
        let stale =
            chrono::Utc::now() - chrono::Duration::seconds(fleet_idle_threshold_secs() + 120);
        write_activity_at(&home, "dev", stale);
        write_activity_at(&home, "lead", stale);
        // Task created then marked Done → has_expected_work false.
        seed_task(&home, "t-done", "dev");
        crate::task_events::append(
            &home,
            &crate::task_events::InstanceName("dev".to_string()),
            crate::task_events::TaskEvent::Done {
                task_id: crate::task_events::TaskId("t-done".to_string()),
                by: crate::task_events::InstanceName("dev".to_string()),
                source: crate::task_events::DoneSource::OperatorManual {
                    authored_at: chrono::Utc::now().to_rfc3339(),
                    result: None,
                },
            },
        )
        .unwrap();
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let recipient = crate::inbox::drain(&home, "lead");
        assert!(
            !recipient
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "all-done board must not fire fleet idle alert: {recipient:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dev_vantage_unaffected_by_fleet_ack() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT");
        let home = tmp_home("ack-dev-unaffected");
        let stale = chrono::Utc::now() - chrono::Duration::seconds(dev_idle_threshold_secs() + 60);
        write_activity_at(&home, "dev", stale);
        ack_fleet_idle();
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
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
        scan_and_emit(&home, &mut last_alerted, false);
        let recipient = crate::inbox::drain(&home, "lead");
        assert!(
            !recipient
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "#1141: fleet idle must be suppressed when no work expected: {recipient:?}"
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
                tags: vec![],
                parent_id: None,
            },
        )
        .unwrap();
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let recipient = crate::inbox::drain(&home, "lead");
        assert!(
            recipient
                .iter()
                .any(|m| m.kind.as_deref() == Some("fleet_idle_watchdog")),
            "#1141: fleet idle must fire when open tasks exist: {recipient:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── per-agent timeout tests ──────────────────────────────────

    fn write_fleet_yaml(home: &Path, yaml: &str) {
        std::fs::write(crate::fleet::fleet_yaml_path(home), yaml).unwrap();
    }

    #[test]
    fn per_agent_timeout_uses_instance_override() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT");
        let home = tmp_home("per-agent-override");
        write_fleet_yaml(
            &home,
            "instances:\n  fast-reviewer:\n    backend: claude\n    timeout_secs: 300\n",
        );
        let stale = chrono::Utc::now() - chrono::Duration::seconds(400);
        write_activity_at(&home, "fast-reviewer", stale);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let lead = crate::inbox::drain(&home, "lead");
        assert!(
            lead.iter()
                .any(|m| m.kind.as_deref() == Some("dev_idle_watchdog")),
            "per-agent 300s threshold exceeded at 400s must trigger alert: {lead:?}"
        );
        assert!(
            lead[0].text.contains("fast-reviewer"),
            "alert must name the agent: {}",
            lead[0].text
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn per_agent_timeout_no_alert_within_override_window() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT");
        let home = tmp_home("per-agent-within");
        write_fleet_yaml(
            &home,
            "instances:\n  fast-reviewer:\n    backend: claude\n    timeout_secs: 300\n",
        );
        let recent = chrono::Utc::now() - chrono::Duration::seconds(200);
        write_activity_at(&home, "fast-reviewer", recent);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let lead = crate::inbox::drain(&home, "lead");
        assert!(
            lead.is_empty(),
            "200s < 300s threshold must NOT trigger alert: {lead:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn per_agent_timeout_falls_back_to_global_when_unset() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT");
        let home = tmp_home("per-agent-fallback");
        write_fleet_yaml(&home, "instances:\n  slow-dev:\n    backend: claude\n");
        // Stale beyond global threshold (3600s default)
        let stale = chrono::Utc::now() - chrono::Duration::seconds(dev_idle_threshold_secs() + 60);
        write_activity_at(&home, "slow-dev", stale);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let lead = crate::inbox::drain(&home, "lead");
        assert!(
            lead.iter()
                .any(|m| m.kind.as_deref() == Some("dev_idle_watchdog")),
            "agent without timeout_secs must use global threshold: {lead:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn per_agent_timeout_alert_includes_current_task() {
        let _g = env_lock();
        std::env::remove_var("AGEND_IDLE_WATCHDOG_AGENT");
        std::env::remove_var("AGEND_IDLE_WATCHDOG_DEV_RECIPIENT");
        let home = tmp_home("per-agent-task-info");
        write_fleet_yaml(
            &home,
            "instances:\n  dev:\n    backend: claude\n    timeout_secs: 300\n",
        );
        // Create an in-progress task owned by "dev"
        let event = crate::task_events::TaskEvent::Created {
            task_id: "t-test-1".into(),
            title: "fix the widget".into(),
            description: String::new(),
            priority: "P1".into(),
            owner: Some("dev".into()),
            due_at: None,
            depends_on: vec![],
            routed_to: None,
            branch: None,
            bind: None,
            eta_secs: None,
            tags: vec![],
            parent_id: None,
        };
        crate::task_events::append(&home, &"lead".into(), event).unwrap();
        let claim = crate::task_events::TaskEvent::InProgress {
            task_id: "t-test-1".into(),
            by: "dev".into(),
        };
        crate::task_events::append(&home, &"dev".into(), claim).unwrap();

        let stale = chrono::Utc::now() - chrono::Duration::seconds(400);
        write_activity_at(&home, "dev", stale);
        let mut last_alerted = HashMap::new();
        scan_and_emit(&home, &mut last_alerted, false);
        let lead = crate::inbox::drain(&home, "lead");
        assert!(!lead.is_empty(), "alert must fire");
        assert!(
            lead[0].text.contains("fix the widget"),
            "alert must include current task title: {}",
            lead[0].text
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // #event-bus pattern #6: the (from, kind, text, correlation_id) tuple a
    // drained alert carries — id/timestamp ignored so legacy-vs-bus compares clean.
    fn alert_payloads(
        home: &Path,
        recipient: &str,
    ) -> Vec<(String, Option<String>, String, Option<String>)> {
        crate::inbox::drain(home, recipient)
            .into_iter()
            .map(|m| (m.from, m.kind, m.text, m.correlation_id))
            .collect()
    }

    // gate-ON: emit(IdleAlert)→subscriber re-delivers BYTE-IDENTICALLY to the
    // legacy `emit_idle_alert` direct enqueue.
    #[test]
    fn gate_on_emit_subscriber_matches_legacy_idle_alert() {
        let recipient = "general";
        let kind = "fleet_idle_watchdog";
        let text = "fleet idle 1800s; all agents quiescent";
        let corr = Some("fixup-dev");

        let home_legacy = tmp_home("p6-parity-legacy");
        emit_idle_alert(&home_legacy, recipient, kind, text, corr);

        let home_bus = tmp_home("p6-parity-bus");
        let bus = crate::daemon::event_bus::EventBus::new();
        bus.subscribe(handle_event);
        bus.emit(
            &home_bus,
            crate::daemon::event_bus::EventKind::IdleAlert {
                recipient: recipient.to_string(),
                kind: kind.to_string(),
                text: text.to_string(),
                correlation_agent: corr.map(String::from),
            },
        );

        let legacy = alert_payloads(&home_legacy, recipient);
        let via_bus = alert_payloads(&home_bus, recipient);
        assert!(!legacy.is_empty(), "legacy alert must fire");
        assert_eq!(
            legacy, via_bus,
            "bus delivery must match legacy byte-for-byte"
        );

        std::fs::remove_dir_all(&home_legacy).ok();
        std::fs::remove_dir_all(&home_bus).ok();
    }

    // #event-bus Step 2 (legacy-zero): route_idle_alert emits to the global bus;
    // the registered subscriber delivers via emit_idle_alert to the event's home.
    #[test]
    fn route_idle_alert_delivers_via_bus() {
        let home = tmp_home("p6-via-bus");
        route_idle_alert(
            &home,
            "general",
            "fleet_idle_watchdog",
            "fleet idle 1800s",
            None,
        );
        let alerts = alert_payloads(&home, "general");
        assert_eq!(alerts.len(), 1, "gate-off must deliver via legacy path");
        assert_eq!(alerts[0].1.as_deref(), Some("fleet_idle_watchdog"));
        std::fs::remove_dir_all(&home).ok();
    }
}
