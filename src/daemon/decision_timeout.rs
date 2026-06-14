//! Sprint 59 Wave 1 PR-4 ((B) Decision default with timeout) —
//! daemon-side pending operator-decision tracking + auto-default
//! on timeout.
//!
//! Caps the engineering anti-stall arc (Sprint 58 Wave 4 PR-1
//! structural + Sprint 59 Wave 1 PR-1 task watchdog + Wave 1 PR-2
//! idle watchdog + Wave 1 PR-3 narrative discipline) by making
//! "general 等 operator" stalls auto-resolve: the agent calls
//! `reply` with `default_action` + `timeout_secs`, and the daemon
//! either (a) sees the operator's override reply within the
//! window and marks the decision resolved, or (b) fires the
//! default action on timeout, surfacing the auto-execute via an
//! inbox event so general can proceed without waiting for the
//! operator's explicit response.
//!
//! ## Sidecar shape
//!
//! `<home>/pending-decisions/<decision_id>.json`:
//! ```json
//! {
//!   "schema_version": 1,
//!   "decision_id": "d-...",
//!   "sender": "general",
//!   "default_action": "proceed-with-lean",
//!   "timeout_secs": 1800,
//!   "issued_at": "2026-05-09T09:30:00Z",
//!   "status": "pending" | "timeout" | "resolved" | "cancelled"
//! }
//! ```
//!
//! ## Override / resolution
//!
//! Operator's telegram reply lands via the existing channel path;
//! the agent then calls `reply` AGAIN (without `default_action`)
//! to send the operator's resolution downstream. That second
//! `reply` call invokes [`mark_resolved_for_sender`] which flips
//! the most-recent pending decision for that sender to `resolved`,
//! suppressing the timeout firing.
//!
//! ## Failure modes
//!
//! All fail-open — IO errors are logged, scan retries next tick.
//! Forward-compat preserved per Sprint 58 Wave 1 PR-2 contract.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const PENDING_DIR: &str = "pending-decisions";
const SCHEMA_VERSION: u32 = 1;

/// Scheduler throttle in supervisor TICK iterations. 30 × 10 s = 5
/// min — matches Wave 1 PR-1/PR-2 cadence.
pub(crate) const TICKS_PER_DECISION_SCAN: u64 = 30;

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PendingDecision {
    #[serde(default)]
    pub(crate) schema_version: u32,
    #[serde(default)]
    pub(crate) decision_id: String,
    #[serde(default)]
    pub(crate) sender: String,
    #[serde(default)]
    pub(crate) default_action: String,
    #[serde(default)]
    pub(crate) timeout_secs: i64,
    #[serde(default)]
    pub(crate) issued_at: String,
    /// `pending` | `timeout` | `resolved` | `cancelled`
    #[serde(default = "default_status")]
    pub(crate) status: String,
}

fn default_status() -> String {
    "pending".to_string()
}

fn pending_dir(home: &Path) -> PathBuf {
    home.join(PENDING_DIR)
}

fn pending_path(home: &Path, decision_id: &str) -> PathBuf {
    pending_dir(home).join(format!("{decision_id}.json"))
}

fn decision_lock_path(home: &Path, decision_id: &str) -> PathBuf {
    pending_dir(home).join(format!("{decision_id}.lock"))
}

/// Generate a deterministic-format decision ID (`d-<unix_micros>-<seq>`).
fn next_decision_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ts = chrono::Utc::now().format("%Y%m%d%H%M%S%6f");
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("d-{ts}-{seq}")
}

/// Record a pending operator decision. Returns the decision_id so
/// the caller can include it in the response payload (operator can
/// reference it for explicit override).
pub(crate) fn record_pending_decision(
    home: &Path,
    sender: &str,
    default_action: &str,
    timeout_secs: i64,
) -> Option<String> {
    if sender.is_empty() || default_action.is_empty() || timeout_secs <= 0 {
        return None;
    }
    let dir = pending_dir(home);
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    // #1092: cancel any existing pending decisions for the same sender
    // before recording a new one. Prevents stacked pendings where
    // mark_resolved_for_sender only clears the latest, leaving older
    // ones to timeout-fire unexpectedly.
    for d in list_pending(home) {
        if d.sender == sender && d.status == "pending" {
            let _ = std::fs::remove_file(pending_path(home, &d.decision_id));
        }
    }
    let decision_id = next_decision_id();
    let payload = PendingDecision {
        schema_version: SCHEMA_VERSION,
        decision_id: decision_id.clone(),
        sender: sender.to_string(),
        default_action: default_action.to_string(),
        timeout_secs,
        issued_at: chrono::Utc::now().to_rfc3339(),
        status: "pending".to_string(),
    };
    let body = match serde_json::to_string_pretty(&payload) {
        Ok(s) => s,
        Err(_) => return None,
    };
    if crate::store::atomic_write(&pending_path(home, &decision_id), body.as_bytes()).is_err() {
        return None;
    }
    Some(decision_id)
}

/// Read all pending decisions from disk. Skips malformed / future-
/// version entries (forward-compat preserved).
pub(crate) fn list_pending(home: &Path) -> Vec<PendingDecision> {
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
        let Ok(d) = serde_json::from_str::<PendingDecision>(&content) else {
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

fn write_decision(home: &Path, d: &PendingDecision) -> bool {
    let body = match serde_json::to_string_pretty(d) {
        Ok(s) => s,
        Err(_) => return false,
    };
    crate::store::atomic_write(&pending_path(home, &d.decision_id), body.as_bytes()).is_ok()
}

/// Operator override path: when an agent calls `reply` again WITHOUT
/// `default_action` (i.e. a real operator-supplied resolution), flip
/// the most-recent pending decision for that sender to `resolved`.
/// Returns the decision_id that was resolved, if any.
///
/// Single-pending-per-sender semantic: the most recent pending entry
/// is the one being resolved. Older pending entries from the same
/// sender (rare, would only happen if multiple defaults are stacked
/// without resolution between) are left alone.
pub(crate) fn mark_resolved_for_sender(home: &Path, sender: &str) -> Option<String> {
    let mut pending: Vec<PendingDecision> = list_pending(home)
        .into_iter()
        .filter(|d| d.sender == sender && d.status == "pending")
        .collect();
    if pending.is_empty() {
        return None;
    }
    pending.sort_by(|a, b| a.issued_at.cmp(&b.issued_at));
    let candidate = pending.pop()?;
    let id = candidate.decision_id.clone();
    // #1116: flock + re-read to serialize against concurrent scan_and_emit
    let _lock = crate::store::acquire_file_lock(&decision_lock_path(home, &id)).ok()?;
    let path = pending_path(home, &id);
    let content = std::fs::read_to_string(&path).ok()?;
    let mut current: PendingDecision = serde_json::from_str(&content).ok()?;
    if current.status != "pending" {
        return None;
    }
    current.status = "resolved".to_string();
    if write_decision(home, &current) {
        Some(id)
    } else {
        None
    }
}

/// Per-loop scheduler state.
pub(crate) struct DecisionTimeoutTracker {
    /// Cadence gate — throttles scans to once per [`TICKS_PER_DECISION_SCAN`]
    /// supervisor ticks (fire-on-Nth).
    gate: crate::daemon::cadence_gate::CadenceGate,
}

impl Default for DecisionTimeoutTracker {
    fn default() -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new_interval(TICKS_PER_DECISION_SCAN),
        }
    }
}

impl DecisionTimeoutTracker {
    pub(crate) fn maybe_scan(&mut self, home: &Path) -> bool {
        if !self.gate.fire() {
            return false;
        }
        scan_and_emit(home);
        true
    }
}

/// Pure scan: detect timed-out pending decisions, flip their status,
/// and emit the auto-default inbox event. Exposed for tests.
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
        if elapsed_secs <= d.timeout_secs {
            continue;
        }
        // #1116: flock + re-read to serialize against concurrent mark_resolved.
        // #1629: do the RMW (re-read → flip status → write) UNDER the flock, then
        // drop it and emit lock-free. emit_timeout_event self-IPCs (notify_system
        // → enqueue_with_idle_hint → loopback api::call); it must never run while a
        // flock is held (#1617 lock-while-blocking class). The emit reads no mutated
        // field (status is not in the message), so flipping before emit is neutral.
        let to_emit: Option<PendingDecision> = {
            let _lock =
                match crate::store::acquire_file_lock(&decision_lock_path(home, &d.decision_id)) {
                    Ok(l) => l,
                    Err(_) => continue,
                };
            let path = pending_path(home, &d.decision_id);
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let mut current: PendingDecision = match serde_json::from_str(&content) {
                Ok(d) => d,
                Err(_) => continue,
            };
            if current.status != "pending" {
                continue;
            }
            current.status = "timeout".to_string();
            let _ = write_decision(home, &current);
            Some(current)
        };
        if let Some(current) = to_emit {
            emit_timeout_event(home, &current, elapsed_secs);
        }
    }
}

/// #event-bus pattern #2: the timeout notification text, built from the exact
/// fields carried by `EventKind::DecisionTimeout`, so the legacy direct enqueue
/// and the bus subscriber produce a BYTE-IDENTICAL message.
fn decision_timeout_text(
    decision_id: &str,
    sender: &str,
    elapsed_secs: i64,
    timeout_secs: i64,
    default_action: &str,
) -> String {
    format!(
        "[decision_timeout] decision {decision_id} from '{sender}' timed out \
         after {elapsed_secs}s (threshold {timeout_secs}s). Auto-default: \
         '{default_action}'. Operator can reverse via reply.",
        decision_id = decision_id,
        sender = sender,
        elapsed_secs = elapsed_secs,
        timeout_secs = timeout_secs,
        default_action = default_action,
    )
}

/// #event-bus pattern #2: the actual delivery (notify_system to the timeout
/// recipient). Shared by BOTH the legacy gate-off path and the bus subscriber, so
/// the two are identical by construction — the parity test proves the event
/// carries enough to call this the same way.
fn deliver_timeout(
    home: &Path,
    decision_id: &str,
    sender: &str,
    elapsed_secs: i64,
    timeout_secs: i64,
    default_action: &str,
) {
    let text = decision_timeout_text(
        decision_id,
        sender,
        elapsed_secs,
        timeout_secs,
        default_action,
    );
    let recipient = crate::fleet::watchdog::resolve_decision_timeout_recipient(home);
    if let Err(e) = crate::inbox::notify_system(
        home,
        &recipient,
        "system:decision_timeout",
        "decision_timeout",
        text,
        Some(decision_id),
        None,
    ) {
        tracing::warn!(error = %e, recipient, "decision_timeout: enqueue failed");
    }
}

/// #event-bus pattern #2: bus subscriber — deliver on a `DecisionTimeout` event
/// (the gate-ON path). Registered once at daemon startup via [`register_subscriber`].
fn handle_event(event: &crate::daemon::event_bus::Event) -> bool {
    if let crate::daemon::event_bus::EventKind::DecisionTimeout {
        decision_id,
        sender,
        elapsed_secs,
        timeout_secs,
        default_action,
    } = &event.kind
    {
        deliver_timeout(
            &event.home,
            decision_id,
            sender,
            *elapsed_secs,
            *timeout_secs,
            default_action,
        );
        true
    } else {
        false
    }
}

/// #event-bus pattern #2: register the decision_timeout delivery subscriber on
/// the global bus. Call ONCE at daemon startup. Home-agnostic — the home travels
/// on each event.
pub fn register_subscriber() {
    crate::daemon::event_bus::global().subscribe(handle_event);
}

fn emit_timeout_event(home: &Path, d: &PendingDecision, elapsed_secs: i64) {
    // #event-bus Step 2 (legacy-zero): the bus is the sole delivery path.
    crate::daemon::event_bus::global().emit(
        home,
        crate::daemon::event_bus::EventKind::DecisionTimeout {
            decision_id: d.decision_id.clone(),
            sender: d.sender.clone(),
            elapsed_secs,
            timeout_secs: d.timeout_secs,
            default_action: d.default_action.clone(),
        },
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-decision-timeout-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// Helper: write a back-dated pending decision so timeout
    /// scenarios don't require sleeping.
    fn write_pending_at(
        home: &Path,
        sender: &str,
        default_action: &str,
        timeout_secs: i64,
        issued_at: chrono::DateTime<chrono::Utc>,
    ) -> String {
        let dir = pending_dir(home);
        std::fs::create_dir_all(&dir).unwrap();
        let id = next_decision_id();
        let payload = PendingDecision {
            schema_version: SCHEMA_VERSION,
            decision_id: id.clone(),
            sender: sender.to_string(),
            default_action: default_action.to_string(),
            timeout_secs,
            issued_at: issued_at.to_rfc3339(),
            status: "pending".to_string(),
        };
        std::fs::write(
            pending_path(home, &id),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .unwrap();
        id
    }

    // ── Lead-spec named tests (per dispatch m-20260509093620227624-140) ──

    #[test]
    fn reply_with_default_action_and_timeout_records_pending_decision() {
        let home = tmp_home("record-pending");
        let id = record_pending_decision(&home, "general", "proceed-with-lean", 1800);
        assert!(id.is_some(), "must return decision_id on success");
        let pending = list_pending(&home);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].sender, "general");
        assert_eq!(pending[0].default_action, "proceed-with-lean");
        assert_eq!(pending[0].timeout_secs, 1800);
        assert_eq!(pending[0].status, "pending");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn reply_without_default_action_maintains_existing_blocking_behavior() {
        // Backwards-compat: empty default_action / 0 timeout_secs
        // → record_pending_decision returns None → caller skips
        // sidecar write → existing reply path runs unchanged.
        let home = tmp_home("no-default");
        assert!(record_pending_decision(&home, "general", "", 1800).is_none());
        assert!(record_pending_decision(&home, "general", "x", 0).is_none());
        assert!(record_pending_decision(&home, "general", "x", -10).is_none());
        assert!(record_pending_decision(&home, "", "x", 1800).is_none());
        let pending = list_pending(&home);
        assert!(pending.is_empty(), "no pending entries written");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn pending_decision_emits_timeout_after_timeout_secs_elapsed() {
        let _g = env_lock();
        std::env::remove_var("AGEND_DECISION_TIMEOUT_RECIPIENT");
        let home = tmp_home("emit-timeout");
        // Issued 2000s ago, timeout=1800 → elapsed > timeout → fires.
        let issued = chrono::Utc::now() - chrono::Duration::seconds(2000);
        let id = write_pending_at(&home, "general", "proceed", 1800, issued);
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "general");
        assert!(
            inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("decision_timeout")
                    && m.correlation_id.as_deref() == Some(&id)),
            "decision_timeout event must enqueue: {inbox:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn pending_decision_default_action_auto_executed_on_timeout() {
        // The auto-default surfaces in the alert text — operator
        // sees what action was taken so they can reverse if wrong.
        let _g = env_lock();
        std::env::remove_var("AGEND_DECISION_TIMEOUT_RECIPIENT");
        let home = tmp_home("auto-execute");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(2000);
        write_pending_at(&home, "general", "proceed-with-lean", 1800, issued);
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "general");
        let event = inbox
            .iter()
            .find(|m| m.kind.as_deref() == Some("decision_timeout"))
            .expect("alert");
        assert!(
            event.text.contains("proceed-with-lean"),
            "alert text must include default_action: {}",
            event.text
        );
        assert!(
            event.text.contains("reverse"),
            "alert text must explain operator can reverse"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn operator_override_via_reply_marks_decision_resolved_prevents_timeout_fire() {
        let _g = env_lock();
        std::env::remove_var("AGEND_DECISION_TIMEOUT_RECIPIENT");
        let home = tmp_home("operator-override");
        // Pending decision issued 2000s ago, timeout=1800 → would
        // fire on next scan. Operator override lands first.
        let issued = chrono::Utc::now() - chrono::Duration::seconds(2000);
        let id = write_pending_at(&home, "general", "proceed", 1800, issued);
        let resolved = mark_resolved_for_sender(&home, "general");
        assert_eq!(resolved.as_deref(), Some(id.as_str()));
        scan_and_emit(&home);
        // No timeout event fired (status was already resolved).
        let inbox = crate::inbox::drain(&home, "general");
        assert!(
            !inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("decision_timeout")),
            "resolved decision must NOT fire timeout: {inbox:?}"
        );
        // Sidecar status reflects resolved.
        let pending = list_pending(&home);
        assert_eq!(pending[0].status, "resolved");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn pending_decision_forward_compat_serde_default_for_unknown_fields() {
        // A future v2 reader adding fields with #[serde(default)]
        // can deserialize v1 files cleanly. Pin v1 surface
        // (status default, timestamp parsing) so the round-trip
        // remains stable.
        let home = tmp_home("forward-compat");
        let dir = pending_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        // Write a JSON without the `status` field — deserializer
        // must fill in via `default_status()`.
        let json = serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "decision_id": "d-test",
            "sender": "general",
            "default_action": "proceed",
            "timeout_secs": 1800,
            "issued_at": "2026-05-09T00:00:00Z",
        });
        std::fs::write(
            pending_path(&home, "d-test"),
            serde_json::to_string_pretty(&json).unwrap(),
        )
        .unwrap();
        let pending = list_pending(&home);
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].status, "pending",
            "missing status defaults to pending"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn decision_timeout_event_routes_to_general_inbox() {
        let _g = env_lock();
        std::env::remove_var("AGEND_DECISION_TIMEOUT_RECIPIENT");
        let home = tmp_home("routes-general");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(2000);
        write_pending_at(&home, "general", "proceed", 1800, issued);
        scan_and_emit(&home);
        let general = crate::inbox::drain(&home, "general");
        assert_eq!(
            general
                .iter()
                .filter(|m| m.kind.as_deref() == Some("decision_timeout"))
                .count(),
            1,
            "exactly one event in general's inbox"
        );
        // Other instances' inboxes should NOT receive it (unless
        // env-overridden, tested separately).
        let lead = crate::inbox::drain(&home, "lead");
        assert!(
            lead.iter()
                .all(|m| m.kind.as_deref() != Some("decision_timeout")),
            "non-target inboxes must NOT receive: {lead:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn pending_decision_status_lifecycle_pending_to_timeout_to_resolved() {
        let _g = env_lock();
        std::env::remove_var("AGEND_DECISION_TIMEOUT_RECIPIENT");
        let home = tmp_home("lifecycle");
        let issued = chrono::Utc::now() - chrono::Duration::seconds(2000);
        let id = write_pending_at(&home, "general", "proceed", 1800, issued);
        // Pending → timeout (after scan).
        scan_and_emit(&home);
        let after_scan = list_pending(&home);
        assert_eq!(after_scan[0].decision_id, id);
        assert_eq!(
            after_scan[0].status, "timeout",
            "scan flips status to timeout"
        );
        // Timeout → resolved (operator post-hoc reverse): manually
        // edit the file (matches what an operator override-after-
        // timeout would write).
        let mut d = after_scan[0].clone();
        d.status = "resolved".to_string();
        write_decision(&home, &d);
        let final_state = list_pending(&home);
        assert_eq!(final_state[0].status, "resolved");
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Defensive bonuses ──────────────────────────────────────────

    #[test]
    fn scan_skips_resolved_or_timeout_decisions() {
        let _g = env_lock();
        std::env::remove_var("AGEND_DECISION_TIMEOUT_RECIPIENT");
        let home = tmp_home("skip-non-pending");
        // Write a resolved + a timeout sidecar — neither should
        // fire again on subsequent scan.
        let issued = chrono::Utc::now() - chrono::Duration::seconds(2000);
        let id_resolved = write_pending_at(&home, "general", "x", 1800, issued);
        let id_timeout = write_pending_at(&home, "general", "y", 1800, issued);
        for (id, status) in [
            (id_resolved.as_str(), "resolved"),
            (id_timeout.as_str(), "timeout"),
        ] {
            let mut d = list_pending(&home)
                .into_iter()
                .find(|d| d.decision_id == id)
                .unwrap();
            d.status = status.to_string();
            write_decision(&home, &d);
        }
        // Post-status-update list_pending excludes them via
        // schema_version filter? No — list_pending returns all v1
        // entries regardless of status. Scan iterates them but
        // skips non-pending status.
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "general");
        assert!(
            !inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("decision_timeout")),
            "non-pending decisions must NOT fire: {inbox:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn scan_skips_decisions_within_timeout_window() {
        let _g = env_lock();
        std::env::remove_var("AGEND_DECISION_TIMEOUT_RECIPIENT");
        let home = tmp_home("within-window");
        // Issued just now, timeout=1800 → not timed out yet.
        let issued = chrono::Utc::now() - chrono::Duration::seconds(60);
        write_pending_at(&home, "general", "proceed", 1800, issued);
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "general");
        assert!(
            !inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("decision_timeout")),
            "in-window decision must NOT fire"
        );
        // Sidecar status remains pending.
        assert_eq!(list_pending(&home)[0].status, "pending");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn maybe_scan_throttles_to_once_per_30_ticks() {
        let home = tmp_home("scan-throttle");
        let mut tracker = DecisionTimeoutTracker::default();
        for _ in 1..TICKS_PER_DECISION_SCAN {
            assert!(!tracker.maybe_scan(&home));
        }
        assert!(tracker.maybe_scan(&home));
        std::fs::remove_dir_all(&home).ok();
    }

    // #1812-followup: recipient resolution (default / env / fleet.yaml
    // precedence) moved to `fleet::watchdog` tests. The §3.9 real-entry test
    // `fleet_decision_timeout_recipient_routes_via_bus` below proves a fleet.yaml
    // `watchdog.decision_timeout_recipient` value reaches the live emit path.

    /// §3.9 real-entry: a fleet.yaml `watchdog.decision_timeout_recipient` must
    /// reach the live bus→subscriber→`deliver_timeout` path — delivery lands in the
    /// configured recipient, NOT the built-in `general` default.
    #[test]
    fn fleet_decision_timeout_recipient_routes_via_bus() {
        let _g = env_lock();
        std::env::remove_var("AGEND_DECISION_TIMEOUT_RECIPIENT");
        let home = tmp_home("fleet-dec-route");
        std::fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "watchdog:\n  decision_timeout_recipient: arbiter\ninstances: {}\n",
        )
        .unwrap();
        let bus = crate::daemon::event_bus::EventBus::new();
        bus.subscribe(handle_event);
        bus.emit(
            &home,
            crate::daemon::event_bus::EventKind::DecisionTimeout {
                decision_id: "d-fleet".to_string(),
                sender: "general".to_string(),
                elapsed_secs: 2000,
                timeout_secs: 1800,
                default_action: "proceed".to_string(),
            },
        );
        assert!(
            !drained_payloads(&home, "arbiter").is_empty(),
            "fleet-configured recipient `arbiter` must receive the decision-timeout alert"
        );
        assert!(
            drained_payloads(&home, "general").is_empty(),
            "built-in default `general` must NOT receive once fleet.yaml overrides"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn mark_resolved_returns_none_when_no_pending_for_sender() {
        let home = tmp_home("no-pending-override");
        // No pending decisions → mark_resolved is a no-op.
        assert!(mark_resolved_for_sender(&home, "general").is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn list_pending_skips_future_schema_version_files() {
        // Forward-compat preservation: a future v2 sidecar is left
        // on disk but not consumed by the v1 scanner.
        let home = tmp_home("forward-version-skip");
        let dir = pending_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        let payload = serde_json::json!({
            "schema_version": SCHEMA_VERSION + 1,
            "decision_id": "d-future",
            "sender": "general",
            "default_action": "x",
            "timeout_secs": 1800,
            "issued_at": "2026-05-09T00:00:00Z",
            "status": "pending",
        });
        std::fs::write(
            pending_path(&home, "d-future"),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .unwrap();
        let pending = list_pending(&home);
        assert!(pending.is_empty(), "future-version sidecar must be skipped");
        // File preserved on disk.
        assert!(pending_path(&home, "d-future").exists());
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #1092: stacked pending characterization + fix tests ──────

    /// #1092 pre-fix characterization (now regression guard): same
    /// sender cannot stack two pending decisions. The second
    /// record_pending_decision call cancels the first.
    #[test]
    fn t1092_second_record_cancels_first_for_same_sender() {
        let _g = env_lock();
        std::env::remove_var("AGEND_DECISION_TIMEOUT_RECIPIENT");
        let home = tmp_home("1092-char-resolve");
        let old = chrono::Utc::now() - chrono::Duration::seconds(2000);
        let id_old = write_pending_at(&home, "general", "action-A", 1800, old);
        // Second record via production API cancels the manually-planted one.
        let id_new =
            record_pending_decision(&home, "general", "action-B", 1800).expect("second record");
        let pending = list_pending(&home);
        assert!(
            pending.iter().all(|d| d.decision_id != id_old),
            "#1092 fix: old pending must be removed when new one is recorded"
        );
        let new_entry = pending.iter().find(|d| d.decision_id == id_new).unwrap();
        assert_eq!(new_entry.status, "pending");
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1092 regression guard: with the fix, resolve + scan does NOT
    /// fire stale decisions because stacking is prevented at record time.
    #[test]
    fn t1092_no_stale_fire_with_resolve_after_record() {
        let _g = env_lock();
        std::env::remove_var("AGEND_DECISION_TIMEOUT_RECIPIENT");
        let home = tmp_home("1092-char-fire");
        let old = chrono::Utc::now() - chrono::Duration::seconds(2000);
        write_pending_at(&home, "general", "stale-action", 1800, old);
        // Second record cancels the stale one.
        let id_new =
            record_pending_decision(&home, "general", "current-action", 1800).expect("new record");
        // Resolve the current one.
        let resolved = mark_resolved_for_sender(&home, "general");
        assert_eq!(resolved.as_deref(), Some(id_new.as_str()));
        // Scan should NOT fire any timeout (stale was cancelled at record).
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "general");
        assert!(
            !inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("decision_timeout")),
            "#1092 fix: no stale fire after resolve: {inbox:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1092 fix: record_pending_decision auto-cancels prior pending
    /// from the same sender. Only one pending per sender at any time.
    #[test]
    fn t1092_record_cancels_prior_pending_same_sender() {
        let _g = env_lock();
        std::env::remove_var("AGEND_DECISION_TIMEOUT_RECIPIENT");
        let home = tmp_home("1092-fix-cancel");
        let id_a =
            record_pending_decision(&home, "general", "action-A", 1800).expect("first record");
        let id_b =
            record_pending_decision(&home, "general", "action-B", 1800).expect("second record");
        let pending = list_pending(&home);
        let a = pending.iter().find(|d| d.decision_id == id_a);
        let b = pending.iter().find(|d| d.decision_id == id_b);
        // After fix: A should be cancelled, B should be pending.
        assert!(
            a.is_none() || a.unwrap().status != "pending",
            "#1092 fix: prior pending must be cancelled when new one is recorded"
        );
        assert_eq!(
            b.unwrap().status,
            "pending",
            "#1092 fix: new pending must be live"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #1116: resolve-vs-scan race characterization ──────────────

    /// #1116 characterization: concurrent mark_resolved + scan_and_emit
    /// on the same timed-out decision must not BOTH succeed. Invariant:
    /// if resolve wins, no timeout event fires; if scan wins, resolve
    /// returns None.
    #[test]
    fn t1116_race_resolve_vs_scan_must_not_both_succeed() {
        let _g = env_lock();
        std::env::remove_var("AGEND_DECISION_TIMEOUT_RECIPIENT");
        for i in 0..100 {
            let h = tmp_home(&format!("1116-race-{i}"));
            let issued = chrono::Utc::now() - chrono::Duration::seconds(2000);
            write_pending_at(&h, "general", "proceed", 1800, issued);

            let h1 = h.clone();
            let h2 = h.clone();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let b1 = barrier.clone();
            let b2 = barrier.clone();

            let t1 = std::thread::spawn(move || {
                b1.wait();
                mark_resolved_for_sender(&h1, "general")
            });
            let t2 = std::thread::spawn(move || {
                b2.wait();
                scan_and_emit(&h2);
            });

            let resolved = t1.join().unwrap();
            t2.join().unwrap();

            let inbox = crate::inbox::drain(&h, "general");
            let timeout_fired = inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("decision_timeout"));

            assert!(
                !(resolved.is_some() && timeout_fired),
                "#1116 race iteration {i}: resolve succeeded AND timeout fired — \
                 read-modify-write not serialized"
            );

            std::fs::remove_dir_all(&h).ok();
        }
    }

    /// #1092 fix: after the fix, resolve + scan does NOT fire stale
    /// decisions because there's only ever one pending per sender.
    #[test]
    fn t1092_no_stale_fire_after_fix() {
        let _g = env_lock();
        std::env::remove_var("AGEND_DECISION_TIMEOUT_RECIPIENT");
        let home = tmp_home("1092-fix-no-fire");
        let old = chrono::Utc::now() - chrono::Duration::seconds(2000);
        // Record two decisions for same sender (second auto-cancels first).
        write_pending_at(&home, "general", "stale-action", 1800, old);
        let id_new =
            record_pending_decision(&home, "general", "current-action", 1800).expect("new record");
        // Resolve the current one.
        let resolved = mark_resolved_for_sender(&home, "general");
        assert_eq!(resolved.as_deref(), Some(id_new.as_str()));
        // Scan should NOT fire any timeout.
        scan_and_emit(&home);
        let inbox = crate::inbox::drain(&home, "general");
        assert!(
            !inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("decision_timeout")),
            "#1092 fix: no stale fire after resolve: {inbox:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1629 invariant (#1617 lock-while-blocking class): `emit_timeout_event`
    /// (self-IPC via notify_system → loopback api::call) must NEVER run while the
    /// #1116 decision flock is held. The RMW happens inside the `let to_emit = {
    /// ... }` flock block; the emit runs after the block (lock-free). Structural
    /// source-scan: brace-match the to_emit block and assert the emit call is NOT
    /// inside it and IS after. Needle is `concat`-built and the scan is
    /// prod-sliced so this test can't self-satisfy.
    #[test]
    fn emit_timeout_not_called_under_flock() {
        let src = include_str!("decision_timeout.rs");
        let cfg_test = ["#[cfg(", "test)]"].concat();
        let prod = match src.find(&cfg_test) {
            Some(i) => &src[..i],
            None => src,
        };
        let block_anchor = ["let to", "_emit"].concat();
        let astart = prod
            .find(&block_anchor)
            .expect("to_emit flock block present");
        let open_rel = prod[astart..].find('{').expect("flock block opens");
        let block_start = astart + open_rel;
        let mut depth = 0usize;
        let mut block_end = block_start;
        for (i, c) in prod[block_start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        block_end = block_start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        assert!(block_end > block_start, "flock block must close");
        let emit_needle = ["emit_timeout", "_event("].concat();
        let block_body = &prod[block_start..=block_end];
        assert!(
            !block_body.contains(&emit_needle),
            "emit_timeout_event must NOT run inside the #1116 decision flock block (#1617 class)"
        );
        assert!(
            prod[block_end..].contains(&emit_needle),
            "emit_timeout_event must run AFTER the decision flock is dropped"
        );
    }

    // ── #event-bus pattern #2: emit→subscriber vs legacy parity ──

    /// The comparable inbox payload (ignoring volatile id/timestamp).
    fn drained_payloads(
        home: &Path,
        recipient: &str,
    ) -> Vec<(String, Option<String>, String, Option<String>)> {
        crate::inbox::drain(home, recipient)
            .into_iter()
            .map(|m| (m.from, m.kind, m.text, m.correlation_id))
            .collect()
    }

    /// PARITY (gate-ON): the bus `emit`→subscriber path delivers payloads
    /// byte-identical (from/kind/text/correlation) to the legacy direct enqueue.
    /// Exercises the REAL bus emit→fan-out→subscriber wiring.
    #[test]
    fn gate_on_emit_subscriber_matches_legacy_direct_enqueue() {
        let _g = env_lock();
        std::env::remove_var("AGEND_DECISION_TIMEOUT_RECIPIENT");
        let (decision_id, sender, elapsed_secs, timeout_secs, default_action) = (
            "d-parity",
            "general",
            2000_i64,
            1800_i64,
            "proceed-with-lean",
        );

        // Legacy direct delivery (the gate-OFF path).
        let home_legacy = tmp_home("parity-legacy");
        deliver_timeout(
            &home_legacy,
            decision_id,
            sender,
            elapsed_secs,
            timeout_secs,
            default_action,
        );

        // Bus emit→subscriber delivery (the gate-ON path) — real fan-out.
        let home_bus = tmp_home("parity-bus");
        let bus = crate::daemon::event_bus::EventBus::new();
        bus.subscribe(handle_event);
        bus.emit(
            &home_bus,
            crate::daemon::event_bus::EventKind::DecisionTimeout {
                decision_id: decision_id.to_string(),
                sender: sender.to_string(),
                elapsed_secs,
                timeout_secs,
                default_action: default_action.to_string(),
            },
        );

        let recipient = crate::fleet::watchdog::resolve_decision_timeout_recipient(&home_legacy);
        let legacy = drained_payloads(&home_legacy, &recipient);
        let viabus = drained_payloads(&home_bus, &recipient);
        assert_eq!(
            legacy, viabus,
            "emit→subscriber payload must equal legacy direct enqueue"
        );
        assert!(
            !legacy.is_empty(),
            "parity test must actually deliver ≥1 message (else it proves nothing)"
        );
        std::fs::remove_dir_all(&home_legacy).ok();
        std::fs::remove_dir_all(&home_bus).ok();
    }

    /// #event-bus Step 2 (legacy-zero): `emit_timeout_event` emits to the global
    /// bus; the registered subscriber delivers via `deliver_timeout` to the event's
    /// home (this test's home).
    #[test]
    fn emit_timeout_event_delivers_via_bus() {
        let _g = env_lock();
        std::env::remove_var("AGEND_DECISION_TIMEOUT_RECIPIENT");
        let home = tmp_home("via-bus");
        let d = PendingDecision {
            decision_id: "d-gateoff".into(),
            sender: "general".into(),
            default_action: "proceed".into(),
            timeout_secs: 1800,
            status: "timeout".into(),
            ..Default::default()
        };
        emit_timeout_event(&home, &d, 2000);
        let recipient = crate::fleet::watchdog::resolve_decision_timeout_recipient(&home);
        assert!(
            !drained_payloads(&home, &recipient).is_empty(),
            "#event-bus Option A: gate-off must deliver via the legacy path (no regression)"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}

#[cfg(test)]
mod review_repro_panic_io_extra;
