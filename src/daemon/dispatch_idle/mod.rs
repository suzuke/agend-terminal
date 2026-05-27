//! L1: cross-team-safe dispatch-idle watchdog.
//!
//! Tracks `send(kind=task)` calls that carry an explicit
//! `expect_reply_within_secs` opt-in. When the expected reply hasn't
//! arrived inside the threshold window, fires a
//! `dispatch_idle_threshold_exceeded` event to the dispatcher's inbox.
//!
//! Cross-team contract: this file MUST stay team-name-free.
//! `no_team_name_strings_in_l1` is the load-bearing invariant test —
//! any L1 grep hit for "fixup" / "reviewer" / "lead" fails CI.
//! Teams opt in by setting `expect_reply_within_secs` on their own
//! dispatches; team-specific automation lives in sibling modules
//! (see `fixup_nudge`).
//!
//! Pattern lineage: closest mirror is `decision_timeout` (threshold +
//! resolve + sidecar lifecycle); hook-site precedent is #870 in
//! `auto_release` (handle_send post-enqueue).

pub(crate) mod fixup_nudge;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const PENDING_DIR: &str = "pending-dispatches";
const SCHEMA_VERSION: u32 = 1;

/// #1018: correlation_id sentinels that the watchdog treats as
/// placeholder values (no real task-board cross-reference possible).
/// Sidecars carrying any of these are cleared on the watchdog tick
/// instead of firing `dispatch_idle_threshold_exceeded` — they
/// generate noise because the same value is reused across multiple
/// parallel dispatches so `mark_resolved`'s first-match clears only
/// one slot while sibling sidecars linger.
///
/// Empty string is implicit via `correlation_id.is_none()`.
const PLACEHOLDER_CORRELATION_IDS: &[&str] = &["t-pending", "t-tbd"];

/// #1018: status values that count as "task still live" — sidecar
/// targeting a real task whose status matches one of these stays
/// armed. Anything else (`done`, `cancelled`, `verified`, etc.) means
/// the task is closed and the sidecar should be cleared.
const LIVE_TASK_STATUSES: &[&str] = &["open", "claimed", "in_progress", "blocked"];

/// Scan throttle in supervisor ticks. 6 ≈ 60s at the 10s tick rate —
/// faster than the 30-tick siblings because the threshold the watchdog
/// is gating (single-digit minutes for orchestrator-class dispatches)
/// demands ≤60s fire-time accuracy. Mirrors auto_release's
/// responsiveness-critical cadence rationale.
pub(crate) const TICKS_PER_SCAN: u64 = 6;

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PendingDispatch {
    #[serde(default)]
    pub(crate) schema_version: u32,
    #[serde(default)]
    pub(crate) dispatch_id: String,
    /// The `send.from` identity (who is waiting for a reply).
    #[serde(default)]
    pub(crate) dispatcher: String,
    /// The `send.target` identity (who is expected to reply).
    #[serde(default)]
    pub(crate) target: String,
    /// `task_id` / thread correlation id from the original dispatch.
    #[serde(default)]
    pub(crate) correlation_id: Option<String>,
    /// Original dispatch kind: `"task"` (#1268: query excluded).
    #[serde(default)]
    pub(crate) expected_kind: String,
    #[serde(default)]
    pub(crate) threshold_secs: i64,
    #[serde(default)]
    pub(crate) issued_at: String,
    /// `pending` | `exceeded` | `resolved` | `cancelled`.
    #[serde(default = "default_status")]
    pub(crate) status: String,
    /// L2-owned dedup field: timestamp of the last nudge emitted for
    /// this dispatch. `None` = no nudge sent yet. L1 never reads this
    /// field; it lives in the L1 schema to keep the sidecar a single
    /// source of truth (avoids parallel L2 sidecar bookkeeping).
    #[serde(default)]
    pub(crate) nudge_sent_at: Option<String>,
}

fn default_status() -> String {
    "pending".to_string()
}

pub(crate) fn pending_dir(home: &Path) -> PathBuf {
    home.join(PENDING_DIR)
}

pub(crate) fn pending_path(home: &Path, dispatch_id: &str) -> PathBuf {
    pending_dir(home).join(format!("{dispatch_id}.json"))
}

fn dispatch_lock_path(home: &Path, dispatch_id: &str) -> PathBuf {
    pending_dir(home).join(format!("{dispatch_id}.lock"))
}

/// Generate a deterministic-format dispatch id (`disp-<unix_micros>-<seq>`).
fn next_dispatch_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ts = chrono::Utc::now().format("%Y%m%d%H%M%S%6f");
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("disp-{ts}-{seq}")
}

/// Record an outbound dispatch. Returns the new dispatch_id, or `None`
/// if the inputs are invalid / the disk write failed.
///
/// Called from `handle_send` post-enqueue: any failure here is silently
/// best-effort (the message has already been delivered; watchdog
/// coverage is a defence-in-depth layer that must never block the
/// dispatch primitive).
pub(crate) fn record_dispatch(
    home: &Path,
    dispatcher: &str,
    target: &str,
    correlation_id: Option<&str>,
    expected_kind: &str,
    threshold_secs: i64,
) -> Option<String> {
    if dispatcher.is_empty() || target.is_empty() || threshold_secs <= 0 {
        return None;
    }
    if !matches!(expected_kind, "task") {
        return None;
    }
    let dir = pending_dir(home);
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    let dispatch_id = next_dispatch_id();
    let payload = PendingDispatch {
        schema_version: SCHEMA_VERSION,
        dispatch_id: dispatch_id.clone(),
        dispatcher: dispatcher.to_string(),
        target: target.to_string(),
        correlation_id: correlation_id.map(String::from),
        expected_kind: expected_kind.to_string(),
        threshold_secs,
        issued_at: chrono::Utc::now().to_rfc3339(),
        status: "pending".to_string(),
        nudge_sent_at: None,
    };
    let body = match serde_json::to_string_pretty(&payload) {
        Ok(s) => s,
        Err(_) => return None,
    };
    if crate::store::atomic_write(&pending_path(home, &dispatch_id), body.as_bytes()).is_err() {
        return None;
    }
    Some(dispatch_id)
}

/// Read all pending dispatch sidecars from disk. Forward-compat: skips
/// any sidecar whose `schema_version` is unknown.
pub(crate) fn list_pending(home: &Path) -> Vec<PendingDispatch> {
    let dir = pending_dir(home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(d) = serde_json::from_str::<PendingDispatch>(&content) else {
            continue;
        };
        if d.schema_version != SCHEMA_VERSION {
            continue;
        }
        out.push(d);
    }
    out.sort_by(|a, b| a.issued_at.cmp(&b.issued_at));
    out
}

fn write_dispatch(home: &Path, d: &PendingDispatch) -> bool {
    let body = match serde_json::to_string_pretty(d) {
        Ok(s) => s,
        Err(_) => return false,
    };
    crate::store::atomic_write(&pending_path(home, &d.dispatch_id), body.as_bytes()).is_ok()
}

/// #1018 (A): is the correlation_id an explicit placeholder sentinel
/// (`t-pending`, `t-tbd`, or empty/whitespace-only string) that the
/// watchdog should clear without firing a notification?
///
/// **Note on `None`**: `correlation_id == None` is NOT treated as a
/// placeholder — it means the dispatch never had an upstream
/// correlation in the first place, and #947 (load-bearing operator
/// contract) requires the nudge to still fire with `dispatch_id` as
/// the fallback. Only sidecars that explicitly carry a placeholder
/// STRING are eligible for silent cleanup.
fn is_placeholder_correlation(corr: Option<&str>) -> bool {
    let Some(c) = corr else {
        return false;
    };
    let c = c.trim();
    if c.is_empty() {
        return true;
    }
    PLACEHOLDER_CORRELATION_IDS.contains(&c)
}

/// #1018 (A): is `agent` a known target in the current fleet registry?
/// Falls back to "yes" (treats unknown fleet.yaml state as live) on
/// any read/parse error so a transient I/O glitch doesn't flush real
/// pending dispatches.
fn target_in_fleet(home: &Path, agent: &str) -> bool {
    let Ok(fleet) = crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home)) else {
        return true; // fail-open
    };
    fleet.resolve_instance(agent).is_some()
}

/// #1018 (A): is `task_id` still live on the task board? Returns
/// `Some(true)` for live (one of `LIVE_TASK_STATUSES`); `Some(false)`
/// for a definitively closed task (`done`, `cancelled`, etc.);
/// `None` when the task can't be found at all (treat as live —
/// fail-open).
fn task_still_live(home: &Path, task_id: &str) -> Option<bool> {
    if task_id.is_empty() {
        return None;
    }
    let path = home.join("tasks").join(format!("{task_id}.json"));
    let Ok(content) = std::fs::read_to_string(&path) else {
        return None;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return None;
    };
    let status = value.get("status").and_then(|v| v.as_str())?;
    Some(LIVE_TASK_STATUSES.contains(&status))
}

/// #1018 (B) eager cleanup: when a task transitions to a terminal
/// state (done / cancelled), scan pending sidecars and delete any
/// whose `correlation_id` matches the closed task_id. Prevents the
/// watchdog firing later on a dispatch whose work has already been
/// reported via the task board instead of via `kind=report`. Returns
/// the count of sidecars deleted (for callers that want to log).
pub(crate) fn cleanup_pending_for_task_id(home: &Path, task_id: &str) -> usize {
    if task_id.is_empty() || is_placeholder_correlation(Some(task_id)) {
        return 0;
    }
    let mut count = 0usize;
    for d in list_pending(home) {
        if d.status != "pending" {
            continue;
        }
        if d.correlation_id.as_deref() != Some(task_id) {
            continue;
        }
        let path = pending_path(home, &d.dispatch_id);
        if std::fs::remove_file(&path).is_ok() {
            count += 1;
            tracing::debug!(
                target: "dispatch_idle",
                dispatch_id = %d.dispatch_id,
                task_id = %task_id,
                "#1018 cleared stale sidecar — task_id closed"
            );
        }
    }
    if count > 0 {
        tracing::info!(
            target: "dispatch_idle",
            task_id = %task_id,
            count,
            "#1018 cleared pending sidecars on task closure"
        );
    }
    count
}

/// #1018 (C) eager cleanup: when an instance is deleted, scan pending
/// sidecars and delete any whose `target` matches the deleted instance
/// name. The deleted instance can never deliver a `kind=report`, so
/// every sidecar targeting it would fire watchdog noise indefinitely.
/// Returns the count of sidecars deleted (best-effort; failures are
/// silently skipped, matching the rest of `full_delete_instance`'s
/// cleanup contract).
pub(crate) fn cleanup_pending_for_instance(home: &Path, instance_name: &str) -> usize {
    if instance_name.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    for d in list_pending(home) {
        if d.status != "pending" {
            continue;
        }
        if d.target != instance_name {
            continue;
        }
        let path = pending_path(home, &d.dispatch_id);
        if std::fs::remove_file(&path).is_ok() {
            count += 1;
            tracing::debug!(
                target: "dispatch_idle",
                dispatch_id = %d.dispatch_id,
                instance = %instance_name,
                "#1018 cleared stale sidecar — target instance deleted"
            );
        }
    }
    if count > 0 {
        tracing::info!(
            target: "dispatch_idle",
            instance = %instance_name,
            count,
            "#1018 cleared pending sidecars on instance deletion"
        );
    }
    count
}

/// Resolve a pending dispatch by `correlation_id` (NOT by sender —
/// decision_timeout's sender-keyed semantic is wrong here because a
/// single dispatcher can have multiple pending dispatches outstanding,
/// each with a distinct correlation_id). Returns the resolved
/// dispatch_id, or `None` if no matching pending entry exists.
pub(crate) fn mark_resolved(home: &Path, correlation_id: &str) -> Option<String> {
    if correlation_id.is_empty() {
        return None;
    }
    let matched = list_pending(home)
        .into_iter()
        .find(|d| d.status == "pending" && d.correlation_id.as_deref() == Some(correlation_id));
    let d = matched?;
    let id = d.dispatch_id.clone();
    // #1340: flock + re-read to serialize against concurrent scan_and_emit
    let _lock = crate::store::acquire_file_lock(&dispatch_lock_path(home, &id)).ok()?;
    let path = pending_path(home, &id);
    let content = std::fs::read_to_string(&path).ok()?;
    let mut current: PendingDispatch = serde_json::from_str(&content).ok()?;
    if current.status != "pending" {
        return None;
    }
    current.status = "resolved".to_string();
    if write_dispatch(home, &current) {
        Some(id)
    } else {
        None
    }
}

/// #1047: reset the timer on a pending sidecar when the dispatchee sends
/// a non-report message (kind=update/query) with matching correlation_id.
/// The sidecar stays live (future silence still fires), but the threshold
/// clock restarts from now. Returns the refreshed dispatch_id, or `None`
/// if no matching pending sidecar exists.
pub(crate) fn refresh_issued_at(home: &Path, correlation_id: &str) -> Option<String> {
    if correlation_id.is_empty() {
        return None;
    }
    let matched = list_pending(home)
        .into_iter()
        .find(|d| d.status == "pending" && d.correlation_id.as_deref() == Some(correlation_id));
    let d = matched?;
    let id = d.dispatch_id.clone();
    // #1340: flock + re-read to serialize against concurrent scan_and_emit
    let _lock = crate::store::acquire_file_lock(&dispatch_lock_path(home, &id)).ok()?;
    let path = pending_path(home, &id);
    let content = std::fs::read_to_string(&path).ok()?;
    let mut current: PendingDispatch = serde_json::from_str(&content).ok()?;
    if current.status != "pending" {
        return None;
    }
    current.issued_at = chrono::Utc::now().to_rfc3339();
    if write_dispatch(home, &current) {
        Some(id)
    } else {
        None
    }
}

/// PR2 L3 visibility — per-instance dispatch metadata view.
/// Pending sidecars where this instance is the **dispatcher**: outbound
/// dispatches it's still waiting for replies on.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub(crate) struct DispatchedWaitingFor {
    pub correlation_id: Option<String>,
    pub target: String,
    pub threshold_secs: i64,
    pub elapsed_secs: i64,
}

/// PR2 L3 visibility — per-instance dispatch metadata view.
/// Pending sidecars where this instance is the **target**: inbound
/// dispatches it owes a reply on.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub(crate) struct PendingResponseTo {
    pub correlation_id: Option<String>,
    pub dispatcher: String,
    pub threshold_secs: i64,
    pub elapsed_secs: i64,
}

/// PR2 L3 helper: return the per-instance dispatch-idle metadata for
/// `agent`, split into outbound (as dispatcher) and inbound (as target)
/// views.
///
/// Only `status == "pending"` sidecars surface — `resolved`,
/// `exceeded`, and `cancelled` entries are filtered out so the
/// operator-facing view stays focused on live work.
pub(crate) fn pending_for_instance(
    home: &Path,
    agent: &str,
) -> (Vec<DispatchedWaitingFor>, Vec<PendingResponseTo>) {
    let now = chrono::Utc::now();
    let mut as_dispatcher = Vec::new();
    let mut as_target = Vec::new();
    if agent.is_empty() {
        return (as_dispatcher, as_target);
    }
    for d in list_pending(home) {
        if d.status != "pending" {
            continue;
        }
        let elapsed_secs = chrono::DateTime::parse_from_rfc3339(&d.issued_at)
            .map(|t| {
                now.signed_duration_since(t.with_timezone(&chrono::Utc))
                    .num_seconds()
            })
            .unwrap_or(0);
        if d.dispatcher == agent {
            as_dispatcher.push(DispatchedWaitingFor {
                correlation_id: d.correlation_id.clone(),
                target: d.target.clone(),
                threshold_secs: d.threshold_secs,
                elapsed_secs,
            });
        }
        if d.target == agent {
            as_target.push(PendingResponseTo {
                correlation_id: d.correlation_id.clone(),
                dispatcher: d.dispatcher.clone(),
                threshold_secs: d.threshold_secs,
                elapsed_secs,
            });
        }
    }
    (as_dispatcher, as_target)
}

/// Per-tick scan: flip eligible pending entries to `exceeded` and emit
/// the inbox event to the dispatcher. Exposed `pub(crate)` for tests.
pub(crate) fn scan_and_emit(home: &Path) {
    let now = chrono::Utc::now();
    for d in list_pending(home) {
        if d.status != "pending" {
            continue;
        }
        let issued = match chrono::DateTime::parse_from_rfc3339(&d.issued_at) {
            Ok(t) => t.with_timezone(&chrono::Utc),
            Err(_) => continue,
        };
        let elapsed_secs = now.signed_duration_since(issued).num_seconds();
        if elapsed_secs <= d.threshold_secs {
            continue;
        }

        // #1018 (A): tick-time validation before firing. Stale sidecars
        // (placeholder correlation_id / deleted target instance / closed
        // task_id) are deleted silently — operator already received the
        // canonical signal via task board / instance lifecycle, no need
        // to surface a second-class "idle threshold" notification.
        if let Some(reason) = stale_sidecar_reason(home, &d) {
            let path = pending_path(home, &d.dispatch_id);
            let _ = std::fs::remove_file(&path);
            tracing::debug!(
                target: "dispatch_idle",
                dispatch_id = %d.dispatch_id,
                target = %d.target,
                correlation_id = ?d.correlation_id,
                reason,
                "#1018 cleared stale sidecar at tick"
            );
            continue;
        }

        // #1340: flock + re-read to serialize against concurrent mark_resolved
        let _lock = match crate::store::acquire_file_lock(&dispatch_lock_path(home, &d.dispatch_id))
        {
            Ok(l) => l,
            Err(_) => continue,
        };
        let path = pending_path(home, &d.dispatch_id);
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut current: PendingDispatch = match serde_json::from_str(&content) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if current.status != "pending" {
            continue;
        }

        emit_exceeded_event(home, &current, elapsed_secs);
        current.status = "exceeded".to_string();
        if !write_dispatch(home, &current) {
            tracing::warn!(dispatch_id = %d.dispatch_id, "dispatch-idle exceeded status write failed");
        }
    }
}

/// #1018 (A): classify whether a sidecar is stale (eligible for
/// silent cleanup) at the watchdog tick. Returns `Some(reason)` for
/// stale; `None` for live (proceed with normal exceeded-event emit).
///
/// Three stale classes:
/// - placeholder correlation_id (lead-side hygiene bug — sidecars
///   sharing `t-pending` etc. across parallel dispatches)
/// - target instance no longer in fleet (deleted)
/// - correlation_id is a real task_id that's already done/cancelled
///
/// Fail-open semantics: any read/parse error in the lookup paths
/// treats the sidecar as live (preserves the existing behavior under
/// transient I/O glitches).
fn stale_sidecar_reason(home: &Path, d: &PendingDispatch) -> Option<&'static str> {
    if is_placeholder_correlation(d.correlation_id.as_deref()) {
        return Some("placeholder_correlation_id");
    }
    if !target_in_fleet(home, &d.target) {
        return Some("target_not_in_fleet");
    }
    if let Some(corr) = d.correlation_id.as_deref() {
        if let Some(false) = task_still_live(home, corr) {
            return Some("task_closed");
        }
    }
    None
}

fn emit_exceeded_event(home: &Path, d: &PendingDispatch, elapsed_secs: i64) {
    let overshoot = elapsed_secs - d.threshold_secs;
    let text = format!(
        "[dispatch_idle_threshold_exceeded] dispatch {dispatch_id} from '{dispatcher}' → '{target}' \
         (kind={expected_kind}, correlation_id={corr}) idle for {elapsed_secs}s \
         (threshold {threshold_secs}s, exceeded by {overshoot}s).\n\n\
         Action checklist:\n\
         1. Check target agent's pane — is it stuck or just slow?\n\
         2. If stuck → force release worktree + redispatch\n\
         3. If slow but progressing → extend patience\n\
         4. If crashed → restart agent, reassign task",
        dispatch_id = d.dispatch_id,
        dispatcher = d.dispatcher,
        target = d.target,
        expected_kind = d.expected_kind,
        corr = d.correlation_id.as_deref().unwrap_or(""),
        elapsed_secs = elapsed_secs,
        threshold_secs = d.threshold_secs,
        overshoot = overshoot,
    );
    // #947: fall back to dispatch_id when upstream correlation_id is
    // None so the nudge is always traceable to its source sidecar.
    let corr = d
        .correlation_id
        .clone()
        .unwrap_or_else(|| d.dispatch_id.clone());
    if let Err(e) = crate::inbox::notify_system(
        home,
        &d.dispatcher,
        "system:dispatch_idle",
        "dispatch_idle_threshold_exceeded",
        text,
        Some(&corr),
        d.correlation_id.as_deref(),
    ) {
        tracing::warn!(
            error = %e,
            dispatcher = %d.dispatcher,
            dispatch_id = %d.dispatch_id,
            "dispatch_idle: enqueue failed"
        );
    }
    crate::event_log::log(
        home,
        "dispatch_idle_threshold_exceeded",
        &d.dispatcher,
        &format!(
            "dispatch_id={} target={} corr={} elapsed_secs={} threshold_secs={}",
            d.dispatch_id,
            d.target,
            d.correlation_id.as_deref().unwrap_or(""),
            elapsed_secs,
            d.threshold_secs,
        ),
    );
    crate::daemon::event_bus::global().emit_lazy(|| {
        crate::daemon::event_bus::EventKind::DispatchIdleExceeded {
            dispatcher: d.dispatcher.clone(),
            target: d.target.clone(),
            elapsed_secs,
        }
    });
}

/// Per-loop scheduler state.
#[derive(Debug, Default)]
pub(crate) struct DispatchIdleTracker {
    tick_count: u64,
}

impl DispatchIdleTracker {
    /// Per-tick entry. Increments the counter; on the throttled
    /// boundary, fires `scan_and_emit` and returns `true`. Returns
    /// `false` for all pre-boundary ticks.
    pub(crate) fn maybe_scan(&mut self, home: &Path) -> bool {
        self.tick_count = self.tick_count.saturating_add(1);
        if self.tick_count < TICKS_PER_SCAN {
            return false;
        }
        self.tick_count = 0;
        scan_and_emit(home);
        true
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::doc_lazy_continuation
)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-dispatch-idle-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Write a backdated pending sidecar directly (bypasses
    /// `record_dispatch`) so timeout scenarios don't require sleeping.
    fn write_pending_at(
        home: &Path,
        dispatcher: &str,
        target: &str,
        correlation_id: Option<&str>,
        expected_kind: &str,
        threshold_secs: i64,
        issued_at: chrono::DateTime<chrono::Utc>,
    ) -> String {
        let dir = pending_dir(home);
        std::fs::create_dir_all(&dir).unwrap();
        let id = next_dispatch_id();
        let payload = PendingDispatch {
            schema_version: SCHEMA_VERSION,
            dispatch_id: id.clone(),
            dispatcher: dispatcher.to_string(),
            target: target.to_string(),
            correlation_id: correlation_id.map(String::from),
            expected_kind: expected_kind.to_string(),
            threshold_secs,
            issued_at: issued_at.to_rfc3339(),
            status: "pending".to_string(),
            nudge_sent_at: None,
        };
        std::fs::write(
            pending_path(home, &id),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .unwrap();
        id
    }

    /// 1. Throttle contract — TICKS_PER_SCAN-1 calls return false, the
    /// next fires (returns true), and the counter resets.
    #[test]
    fn tracker_throttles_to_tick_per_scan() {
        let home = tmp_home("throttle");
        let mut tracker = DispatchIdleTracker::default();
        for i in 0..(TICKS_PER_SCAN - 1) {
            assert!(
                !tracker.maybe_scan(&home),
                "tick {i} (pre-throttle) must return false"
            );
        }
        assert!(
            tracker.maybe_scan(&home),
            "{}th tick must fire scan and return true",
            TICKS_PER_SCAN
        );
        assert!(
            !tracker.maybe_scan(&home),
            "post-fire tick must reset counter and return false"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// 2. `record_dispatch` writes a sidecar that `list_pending` can
    /// round-trip.
    #[test]
    fn record_and_list_pending_dispatch() {
        let home = tmp_home("record");
        let id = record_dispatch(&home, "lead", "reviewer", Some("t-abc"), "task", 600)
            .expect("record must return id");
        let pending = list_pending(&home);
        assert_eq!(pending.len(), 1);
        let p = &pending[0];
        assert_eq!(p.dispatch_id, id);
        assert_eq!(p.dispatcher, "lead");
        assert_eq!(p.target, "reviewer");
        assert_eq!(p.correlation_id.as_deref(), Some("t-abc"));
        assert_eq!(p.expected_kind, "task");
        assert_eq!(p.threshold_secs, 600);
        assert_eq!(p.status, "pending");
        std::fs::remove_dir_all(&home).ok();
    }

    /// 3. `scan_and_emit` flips exceeded entries and emits an inbox
    /// event to the dispatcher.
    #[test]
    fn fires_on_threshold_exceeded() {
        let home = tmp_home("fires");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let id = write_pending_at(&home, "alpha", "beta", Some("t-fires"), "task", 600, issued);
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "alpha");
        assert!(
            inbox.iter().any(
                |m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")
                    && m.correlation_id.as_deref() == Some("t-fires")
            ),
            "must emit dispatch_idle_threshold_exceeded event to dispatcher's inbox: {inbox:?}"
        );
        let pending = list_pending(&home);
        let p = pending.iter().find(|p| p.dispatch_id == id).unwrap();
        assert_eq!(p.status, "exceeded", "sidecar must flip pending→exceeded");
        std::fs::remove_dir_all(&home).ok();
    }

    /// 4. Load-bearing contract: `mark_resolved` keys on
    /// `correlation_id`, NOT on `dispatcher`. Decision_timeout's
    /// sender-keyed semantic would resolve the wrong sidecar when a
    /// single dispatcher has multiple in-flight dispatches.
    #[test]
    fn mark_resolved_keys_on_correlation_id_not_sender() {
        let home = tmp_home("resolve-by-corr");
        let now = chrono::Utc::now();
        let id_a = write_pending_at(&home, "lead", "dev-1", Some("t-aaa"), "task", 600, now);
        let id_b = write_pending_at(&home, "lead", "dev-2", Some("t-bbb"), "task", 600, now);
        let resolved = mark_resolved(&home, "t-aaa");
        assert_eq!(
            resolved.as_deref(),
            Some(id_a.as_str()),
            "must resolve the correlation_id-matching sidecar, not sender-matching"
        );
        let pending = list_pending(&home);
        let p_a = pending.iter().find(|p| p.dispatch_id == id_a).unwrap();
        let p_b = pending.iter().find(|p| p.dispatch_id == id_b).unwrap();
        assert_eq!(p_a.status, "resolved", "matched sidecar must flip");
        assert_eq!(
            p_b.status, "pending",
            "unmatched sidecar from same dispatcher must NOT flip"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// 5. After `mark_resolved`, the subsequent `scan_and_emit` does
    /// NOT fire an event (status was resolved before threshold check).
    #[test]
    fn mark_resolved_suppresses_fire() {
        let home = tmp_home("resolved-no-fire");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        write_pending_at(
            &home,
            "alpha",
            "beta",
            Some("t-suppress"),
            "task",
            600,
            issued,
        );
        let resolved = mark_resolved(&home, "t-suppress");
        assert!(resolved.is_some(), "mark_resolved must locate sidecar");
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "alpha");
        assert!(
            !inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
            "resolved dispatch must NOT fire timeout event: {inbox:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// 6. Load-bearing contract (parallel dev-1 + dev-2 configuration):
    /// two sidecars, only the exceeded one fires; the fresh one stays
    /// pending and does NOT pollute the exceeded one's event.
    #[test]
    fn parallel_dispatch_isolation() {
        let home = tmp_home("parallel-iso");
        let stale = chrono::Utc::now() - chrono::Duration::seconds(700);
        let fresh = chrono::Utc::now() - chrono::Duration::seconds(60);
        let id_stale =
            write_pending_at(&home, "lead", "dev-1", Some("t-stale"), "task", 600, stale);
        let id_fresh =
            write_pending_at(&home, "lead", "dev-2", Some("t-fresh"), "task", 600, fresh);
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "lead");
        let exceeded_events: Vec<_> = inbox
            .iter()
            .filter(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded"))
            .collect();
        assert_eq!(
            exceeded_events.len(),
            1,
            "exactly one exceeded event for the stale dispatch"
        );
        assert_eq!(
            exceeded_events[0].correlation_id.as_deref(),
            Some("t-stale"),
            "the event must reference the stale correlation_id"
        );
        let pending = list_pending(&home);
        let p_stale = pending.iter().find(|p| p.dispatch_id == id_stale).unwrap();
        let p_fresh = pending.iter().find(|p| p.dispatch_id == id_fresh).unwrap();
        assert_eq!(p_stale.status, "exceeded");
        assert_eq!(
            p_fresh.status, "pending",
            "fresh dispatch must remain pending"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// 7. Invariant: L1 file MUST stay team-name-free. Cross-team-safe
    /// design contract. If this test ever fails, the L1 primitive has
    /// leaked team-specific knowledge and a sibling module is the
    /// right home for that code.
    ///
    /// Two structural allowances: comment lines (any `// …` prefix) and
    /// the boilerplate `pub(crate) mod fixup_nudge;` declaration that
    /// wires the L2 submodule into the dispatch_idle module tree.
    /// Test-module contents are also exempt — placeholder names like
    /// "lead" / "reviewer" / "dev-1" are legitimate test inputs.
    #[test]
    fn no_team_name_strings_in_l1() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let l1_path = std::path::PathBuf::from(manifest).join("src/daemon/dispatch_idle/mod.rs");
        let body = std::fs::read_to_string(&l1_path)
            .expect("L1 file must be readable from CARGO_MANIFEST_DIR");
        let mut offenders: Vec<(usize, &str, String)> = Vec::new();
        let mut in_test_mod = false;
        for (lineno, line) in body.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.starts_with("mod tests") {
                in_test_mod = true;
            }
            if in_test_mod {
                continue;
            }
            // Allowlist: the L2 submodule declaration is structural,
            // not behavioral. Behaviour-side references stay forbidden.
            if trimmed == "pub(crate) mod fixup_nudge;" {
                continue;
            }
            for needle in ["fixup", "reviewer", "lead"] {
                if line.contains(needle) {
                    offenders.push((lineno + 1, needle, line.to_string()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "L1 file must stay team-name-free; offenders: {offenders:?}"
        );
    }

    /// 8. Forward-compat: a future v2 sidecar must be left on disk and
    /// skipped by the v1 list_pending. Sprint 58 Wave 1 PR-2 contract.
    #[test]
    fn forward_compat_serde() {
        let home = tmp_home("forward-compat");
        let dir = pending_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        let payload = serde_json::json!({
            "schema_version": SCHEMA_VERSION + 1,
            "dispatch_id": "disp-future",
            "dispatcher": "x",
            "target": "y",
            "expected_kind": "task",
            "threshold_secs": 600,
            "issued_at": "2026-05-09T00:00:00Z",
            "status": "pending",
        });
        std::fs::write(
            pending_path(&home, "disp-future"),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .unwrap();
        let pending = list_pending(&home);
        assert!(
            pending.is_empty(),
            "future-version sidecar must be skipped by v1 reader"
        );
        // File preserved on disk so a v2 reader could pick it up later.
        assert!(pending_path(&home, "disp-future").exists());
        std::fs::remove_dir_all(&home).ok();
    }

    // ── PR2 L3 visibility tests for pending_for_instance ──

    /// Dispatcher view: pending sidecars where this agent is the
    /// outbound dispatcher surface in `dispatched_waiting_for`.
    #[test]
    fn pending_for_instance_surfaces_dispatcher_view() {
        let home = tmp_home("pfi-dispatcher");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(120);
        write_pending_at(
            &home,
            "fixup-lead",
            "fixup-reviewer",
            Some("t-l3-disp"),
            "task",
            600,
            issued,
        );
        let (as_dispatcher, as_target) = pending_for_instance(&home, "fixup-lead");
        assert_eq!(as_dispatcher.len(), 1);
        assert_eq!(as_dispatcher[0].target, "fixup-reviewer");
        assert_eq!(
            as_dispatcher[0].correlation_id.as_deref(),
            Some("t-l3-disp")
        );
        assert_eq!(as_dispatcher[0].threshold_secs, 600);
        assert!(
            (110..=130).contains(&as_dispatcher[0].elapsed_secs),
            "elapsed_secs within 10s window of expected 120: {}",
            as_dispatcher[0].elapsed_secs
        );
        assert!(
            as_target.is_empty(),
            "dispatcher agent must NOT appear in its own target view"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Target view: pending sidecars where this agent owes a reply
    /// surface in `pending_response_to`.
    #[test]
    fn pending_for_instance_surfaces_target_view() {
        let home = tmp_home("pfi-target");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(120);
        write_pending_at(
            &home,
            "fixup-lead",
            "fixup-reviewer",
            Some("t-l3-target"),
            "task",
            600,
            issued,
        );
        let (_, as_target) = pending_for_instance(&home, "fixup-reviewer");
        assert_eq!(as_target.len(), 1);
        assert_eq!(as_target[0].dispatcher, "fixup-lead");
        assert_eq!(as_target[0].correlation_id.as_deref(), Some("t-l3-target"));
        assert_eq!(as_target[0].threshold_secs, 600);
        std::fs::remove_dir_all(&home).ok();
    }

    /// Cross-team-safe: a non-fixup agent (and any agent not on a
    /// sidecar) sees empty arrays. Non-fixup teams that haven't opted
    /// in to the watchdog never record sidecars (see L2 default
    /// threshold logic), so L3 is a no-op for them.
    #[test]
    fn pending_for_instance_empty_for_unaffected_agent() {
        let home = tmp_home("pfi-unaffected");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(60);
        write_pending_at(
            &home,
            "fixup-lead",
            "fixup-reviewer",
            Some("t-fixup"),
            "task",
            600,
            issued,
        );
        let (as_dispatcher, as_target) = pending_for_instance(&home, "research-dev");
        assert!(
            as_dispatcher.is_empty() && as_target.is_empty(),
            "unaffected agent surfaces empty arrays"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Stale filter: resolved / exceeded / cancelled sidecars do NOT
    /// surface. Only `status == "pending"` reaches L3.
    #[test]
    fn pending_for_instance_filters_stale_sidecars() {
        let home = tmp_home("pfi-stale-filter");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(60);
        // One pending (must surface)
        write_pending_at(&home, "lead", "dev", Some("t-pending"), "task", 600, issued);
        // Three non-pending (must be filtered).
        for (corr, status) in [
            ("t-resolved", "resolved"),
            ("t-exceeded", "exceeded"),
            ("t-cancelled", "cancelled"),
        ] {
            let id = write_pending_at(&home, "lead", "dev", Some(corr), "task", 600, issued);
            // Flip status on disk.
            let path = pending_path(&home, &id);
            let body = std::fs::read_to_string(&path).unwrap();
            let mut v: serde_json::Value = serde_json::from_str(&body).unwrap();
            v["status"] = serde_json::Value::String(status.to_string());
            std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap()).unwrap();
        }
        let (as_dispatcher, _) = pending_for_instance(&home, "lead");
        assert_eq!(
            as_dispatcher.len(),
            1,
            "only status=pending sidecars surface"
        );
        assert_eq!(
            as_dispatcher[0].correlation_id.as_deref(),
            Some("t-pending"),
            "non-pending entries must be filtered"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Wire shape: the serde-derived JSON for the L3 metadata uses
    /// stable snake_case field names. Pins the operator-visible
    /// schema so future renames are an intentional break, not a
    /// silent regression.
    #[test]
    fn pending_for_instance_serializes_with_stable_field_names() {
        let home = tmp_home("pfi-shape");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(60);
        write_pending_at(&home, "lead", "dev", Some("t-shape"), "task", 600, issued);
        let (as_dispatcher, as_target) = pending_for_instance(&home, "lead");
        let j = serde_json::to_value(&as_dispatcher[0]).unwrap();
        assert!(j.get("correlation_id").is_some());
        assert!(j.get("target").is_some());
        assert!(j.get("threshold_secs").is_some());
        assert!(j.get("elapsed_secs").is_some());
        assert!(
            as_target.is_empty(),
            "lead is dispatcher only — target view stays empty"
        );
        let (_, as_target_dev) = pending_for_instance(&home, "dev");
        let j2 = serde_json::to_value(&as_target_dev[0]).unwrap();
        assert!(j2.get("correlation_id").is_some());
        assert!(j2.get("dispatcher").is_some());
        assert!(j2.get("threshold_secs").is_some());
        assert!(j2.get("elapsed_secs").is_some());
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #947 fallback contract: dispatch_idle nudge's correlation_id ──
    //
    // Pre-#947 behavior: `emit_exceeded_event` cloned `d.correlation_id`
    // verbatim. When the original `send` omitted correlation_id, the
    // outbound nudge inherited `None` — operators couldn't backtrack
    // from the nudge to the source sidecar.
    //
    // Post-#947: when upstream correlation_id is None, fall back to
    // `d.dispatch_id` (format `disp-{ts}-{seq}`, self-documenting via
    // the `disp-` prefix). The schema field is reused; no new field.
    //
    // The blend (upstream-chain vs producer-record) is acceptable because
    // the prefix conventions (`disp-`, `t-`, `m-`) make value class
    // identifiable at grep time. If a future producer breaks the prefix
    // convention, file a follow-up to add `source_record_id: Option<String>`
    // for clean separation (option A from /tmp/dialectic-947-dev-primary.md).

    /// #947 test 1 — when upstream correlation_id is present, it is
    /// preserved (NOT replaced with dispatch_id). The fallback applies
    /// only when upstream is None.
    #[test]
    fn dispatch_idle_emit_with_upstream_correlation_preserves_it() {
        let home = tmp_home("947-upstream-preserved");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        write_pending_at(
            &home,
            "alpha",
            "beta",
            Some("upstream-corr-abc"),
            "task",
            600,
            issued,
        );
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "alpha");
        let nudge = inbox
            .iter()
            .find(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded"))
            .expect("exceeded nudge must enqueue");
        assert_eq!(
            nudge.correlation_id.as_deref(),
            Some("upstream-corr-abc"),
            "upstream correlation_id must be preserved verbatim"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #947 test 2 — when upstream correlation_id is None, fall back to
    /// `d.dispatch_id`. The nudge becomes traceable to its source sidecar.
    #[test]
    fn dispatch_idle_emit_without_upstream_falls_back_to_dispatch_id() {
        let home = tmp_home("947-fallback-dispid");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let dispatch_id = write_pending_at(&home, "alpha", "beta", None, "task", 600, issued);
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "alpha");
        let nudge = inbox
            .iter()
            .find(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded"))
            .expect("exceeded nudge must enqueue");
        assert_eq!(
            nudge.correlation_id.as_deref(),
            Some(dispatch_id.as_str()),
            "missing upstream correlation_id must fall back to dispatch_id"
        );
        // Format check: dispatch_id starts with `disp-` (self-documenting prefix).
        assert!(
            dispatch_id.starts_with("disp-"),
            "dispatch_id format must use `disp-` prefix: {dispatch_id}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #947 test 5 (e2e) — after the fix, dispatch_idle nudges ALWAYS
    /// carry a non-empty correlation_id, regardless of upstream presence.
    /// This is the load-bearing operator contract for reverse-lookup.
    #[test]
    fn dispatch_idle_nudge_correlation_id_always_non_empty() {
        let home = tmp_home("947-always-non-empty");
        let now = chrono::Utc::now();
        // Two pending dispatches: one with upstream, one without.
        write_pending_at(
            &home,
            "alpha",
            "beta",
            Some("with-chain"),
            "task",
            600,
            now - chrono::Duration::seconds(700),
        );
        write_pending_at(
            &home,
            "gamma",
            "delta",
            None,
            "task",
            600,
            now - chrono::Duration::seconds(800),
        );
        scan_and_emit(&home);
        let alpha_inbox = crate::inbox::drain(&home, "alpha");
        let gamma_inbox = crate::inbox::drain(&home, "gamma");
        for m in alpha_inbox
            .iter()
            .filter(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded"))
        {
            let c = m.correlation_id.as_deref().unwrap_or("");
            assert!(
                !c.is_empty(),
                "alpha nudge correlation_id must be non-empty: {m:?}"
            );
        }
        for m in gamma_inbox
            .iter()
            .filter(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded"))
        {
            let c = m.correlation_id.as_deref().unwrap_or("");
            assert!(
                !c.is_empty(),
                "gamma nudge correlation_id must be non-empty (fallback): {m:?}"
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #1018: stale-sidecar cleanup ───────────────────────────────────

    /// #1018 (A) — placeholder correlation_id classifier covers the
    /// known sentinels (`t-pending`, `t-tbd`) and the explicit-empty
    /// string variant. `None` is NOT a placeholder per #947 contract:
    /// dispatches without upstream correlation must still fire the
    /// threshold event with `dispatch_id` as fallback. Other strings
    /// (even short / suspicious-looking ones) are also NOT placeholders.
    #[test]
    fn t1018_a_placeholder_correlation_predicate() {
        assert!(is_placeholder_correlation(Some("t-pending")));
        assert!(is_placeholder_correlation(Some("t-tbd")));
        assert!(is_placeholder_correlation(Some("")));
        assert!(is_placeholder_correlation(Some("   ")));
        assert!(
            !is_placeholder_correlation(None),
            "None != placeholder — #947 fallback contract preserved"
        );
        assert!(!is_placeholder_correlation(Some(
            "t-20260520163333000054-1"
        )));
        assert!(!is_placeholder_correlation(Some("t-pending-real")));
        assert!(!is_placeholder_correlation(Some("real-id")));
    }

    /// #1018 (A) — sidecar with placeholder correlation_id is cleared
    /// at scan tick without firing the threshold event.
    #[test]
    fn t1018_a_placeholder_correlation_swept_silently() {
        let home = tmp_home("1018-placeholder");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let id = write_pending_at(
            &home,
            "fixup-lead",
            "fixup-dev-2",
            Some("t-pending"),
            "task",
            600,
            issued,
        );
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "fixup-lead");
        assert!(
            !inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
            "placeholder correlation_id MUST NOT fire threshold event: {inbox:?}"
        );
        let pending = list_pending(&home);
        assert!(
            pending.iter().all(|p| p.dispatch_id != id),
            "stale placeholder sidecar MUST be removed from disk"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1018 (A) — sidecar targeting a non-fleet agent is cleared
    /// silently (fleet.yaml exists but instance not listed).
    #[test]
    fn t1018_a_missing_target_in_fleet_swept_silently() {
        let home = tmp_home("1018-missing-target");
        // Empty fleet.yaml → resolve_instance returns None for any name.
        std::fs::write(crate::fleet::fleet_yaml_path(&home), "instances: {}\n").unwrap();
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let id = write_pending_at(
            &home,
            "fixup-lead",
            "ghost-agent",
            Some("t-real-task-123"),
            "task",
            600,
            issued,
        );
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "fixup-lead");
        assert!(
            !inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
            "missing target MUST NOT fire threshold event: {inbox:?}"
        );
        let pending = list_pending(&home);
        assert!(
            pending.iter().all(|p| p.dispatch_id != id),
            "stale missing-target sidecar MUST be removed from disk"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1018 (A) — sidecar correlation_id maps to a real task_id but
    /// that task is already `done` on the board. Cleared silently.
    #[test]
    fn t1018_a_closed_task_id_swept_silently() {
        let home = tmp_home("1018-closed-task");
        // Provide a fleet.yaml that includes the target so the
        // missing-target branch doesn't trip first.
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  fixup-dev-2:\n    backend: claude\n",
        )
        .unwrap();
        let task_id = "t-closed-12345";
        let tasks_dir = home.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{task_id}.json")),
            serde_json::to_string_pretty(&serde_json::json!({
                "id": task_id,
                "status": "done",
                "title": "test",
                "assignee": "fixup-dev-2"
            }))
            .unwrap(),
        )
        .unwrap();
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        let id = write_pending_at(
            &home,
            "fixup-lead",
            "fixup-dev-2",
            Some(task_id),
            "task",
            600,
            issued,
        );
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "fixup-lead");
        assert!(
            !inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
            "closed task_id MUST NOT fire threshold event: {inbox:?}"
        );
        let pending = list_pending(&home);
        assert!(
            pending.iter().all(|p| p.dispatch_id != id),
            "stale closed-task sidecar MUST be removed from disk"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1018 (A) anti-regression — real task_id + present target +
    /// task status `in_progress` MUST still fire the threshold event
    /// when overdue. Guards against over-rotation into clearing live
    /// sidecars.
    #[test]
    fn t1018_a_live_dispatch_still_fires() {
        let home = tmp_home("1018-live");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  fixup-dev-2:\n    backend: claude\n",
        )
        .unwrap();
        let task_id = "t-live-99";
        let tasks_dir = home.join("tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::write(
            tasks_dir.join(format!("{task_id}.json")),
            serde_json::to_string_pretty(&serde_json::json!({
                "id": task_id,
                "status": "in_progress",
                "title": "live work",
                "assignee": "fixup-dev-2"
            }))
            .unwrap(),
        )
        .unwrap();
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        write_pending_at(
            &home,
            "fixup-lead",
            "fixup-dev-2",
            Some(task_id),
            "task",
            600,
            issued,
        );
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "fixup-lead");
        assert!(
            inbox.iter().any(
                |m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")
                    && m.correlation_id.as_deref() == Some(task_id)
            ),
            "live overdue dispatch MUST still fire — got: {inbox:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1018 (B) — `cleanup_pending_for_task_id` deletes sidecars
    /// matching the closed task_id; leaves others untouched.
    #[test]
    fn t1018_b_cleanup_pending_for_task_id() {
        let home = tmp_home("1018-task-done-cleanup");
        let now = chrono::Utc::now();
        let id_match_1 = write_pending_at(
            &home,
            "fixup-lead",
            "dev-1",
            Some("t-target"),
            "task",
            600,
            now,
        );
        let id_match_2 = write_pending_at(
            &home,
            "fixup-lead",
            "dev-2",
            Some("t-target"),
            "task",
            600,
            now,
        );
        let id_other = write_pending_at(
            &home,
            "fixup-lead",
            "dev-1",
            Some("t-different"),
            "task",
            600,
            now,
        );

        let cleared = cleanup_pending_for_task_id(&home, "t-target");
        assert_eq!(cleared, 2, "must delete both sidecars for closed task");

        let pending = list_pending(&home);
        assert!(pending.iter().all(|p| p.dispatch_id != id_match_1));
        assert!(pending.iter().all(|p| p.dispatch_id != id_match_2));
        assert!(
            pending.iter().any(|p| p.dispatch_id == id_other),
            "unrelated task_id sidecar must NOT be cleared"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1018 (B) — `cleanup_pending_for_task_id` refuses to act on
    /// placeholder task_ids so a stray `task_id=t-pending` close
    /// can't wipe unrelated sidecars.
    #[test]
    fn t1018_b_cleanup_refuses_placeholder_task_id() {
        let home = tmp_home("1018-cleanup-placeholder");
        let now = chrono::Utc::now();
        let id = write_pending_at(
            &home,
            "fixup-lead",
            "dev-1",
            Some("t-pending"),
            "task",
            600,
            now,
        );
        let cleared = cleanup_pending_for_task_id(&home, "t-pending");
        assert_eq!(cleared, 0, "placeholder task_id MUST NOT trigger cleanup");
        // Sidecar still exists on disk.
        let pending = list_pending(&home);
        assert!(pending.iter().any(|p| p.dispatch_id == id));
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1018 (C) — `cleanup_pending_for_instance` deletes sidecars
    /// targeting the deleted instance; leaves dispatcher-side and
    /// other-target sidecars untouched.
    #[test]
    fn t1018_c_cleanup_pending_for_instance() {
        let home = tmp_home("1018-instance-delete-cleanup");
        let now = chrono::Utc::now();
        let id_target = write_pending_at(
            &home,
            "fixup-lead",
            "fixup-reviewer",
            Some("t-aaa"),
            "task",
            600,
            now,
        );
        let id_other_target = write_pending_at(
            &home,
            "fixup-lead",
            "fixup-dev-2",
            Some("t-bbb"),
            "task",
            600,
            now,
        );
        // Sidecar where the deleted instance is the DISPATCHER, not
        // the target. Must NOT be cleared by this cleanup (different
        // failure mode — dispatcher-side bookkeeping is operator's
        // responsibility via task board).
        let id_dispatcher_role = write_pending_at(
            &home,
            "fixup-reviewer",
            "fixup-dev-2",
            Some("t-ccc"),
            "task",
            600,
            now,
        );

        let cleared = cleanup_pending_for_instance(&home, "fixup-reviewer");
        assert_eq!(cleared, 1, "must delete only target-matching sidecar");
        let pending = list_pending(&home);
        assert!(pending.iter().all(|p| p.dispatch_id != id_target));
        assert!(
            pending.iter().any(|p| p.dispatch_id == id_other_target),
            "different-target sidecar untouched"
        );
        assert!(
            pending.iter().any(|p| p.dispatch_id == id_dispatcher_role),
            "dispatcher-role sidecar untouched"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #1047: refresh_issued_at (kind=update/query timer reset) ──

    /// #1047 T1: dispatchee sends kind=update within threshold → timer
    /// resets → subsequent scan_and_emit does NOT fire at the original
    /// threshold boundary.
    #[test]
    fn t1047_refresh_issued_at_prevents_false_positive() {
        let home = tmp_home("1047-refresh");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(550);
        write_pending_at(&home, "lead", "dev", Some("t-1047-a"), "task", 600, issued);
        // Dispatchee sends update → timer resets.
        let refreshed = refresh_issued_at(&home, "t-1047-a");
        assert!(refreshed.is_some(), "refresh must locate sidecar");
        // Now 600s hasn't elapsed from the refreshed issued_at.
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "lead");
        assert!(
            !inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
            "#1047: refreshed sidecar must NOT fire: {inbox:?}"
        );
        let pending = list_pending(&home);
        assert!(
            pending.iter().any(|p| p.status == "pending"),
            "sidecar must remain pending after refresh"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1047 T2: dispatchee silent past threshold → fire (regression preserved).
    #[test]
    fn t1047_silent_dispatchee_still_fires() {
        let home = tmp_home("1047-silent");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
        write_pending_at(&home, "lead", "dev", Some("t-1047-b"), "task", 600, issued);
        // No refresh — dispatchee is silent.
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "lead");
        assert!(
            inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
            "#1047 regression: silent dispatchee must still fire: {inbox:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1047 T3: kind=report still fully closes sidecar (status=resolved).
    #[test]
    fn t1047_report_still_resolves() {
        let home = tmp_home("1047-report");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(550);
        write_pending_at(&home, "lead", "dev", Some("t-1047-c"), "task", 600, issued);
        let resolved = mark_resolved(&home, "t-1047-c");
        assert!(resolved.is_some(), "report must resolve sidecar");
        let pending = list_pending(&home);
        let d = pending
            .iter()
            .find(|p| p.correlation_id.as_deref() == Some("t-1047-c"))
            .unwrap();
        assert_eq!(d.status, "resolved", "kind=report must set status=resolved");
        std::fs::remove_dir_all(&home).ok();
    }
}
