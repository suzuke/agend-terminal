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

/// Default recipient for the auto-default emission (operator-
/// proceed signal). Tunable via `AGEND_DECISION_TIMEOUT_RECIPIENT`.
fn timeout_recipient() -> String {
    std::env::var("AGEND_DECISION_TIMEOUT_RECIPIENT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "general".to_string())
}

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
    let mut latest = pending.pop()?;
    latest.status = "resolved".to_string();
    let id = latest.decision_id.clone();
    if write_decision(home, &latest) {
        Some(id)
    } else {
        None
    }
}

/// Per-loop scheduler state.
#[derive(Debug, Default)]
pub(crate) struct DecisionTimeoutTracker {
    tick_count: u64,
}

impl DecisionTimeoutTracker {
    pub(crate) fn maybe_scan(&mut self, home: &Path) -> bool {
        self.tick_count = self.tick_count.saturating_add(1);
        if self.tick_count < TICKS_PER_DECISION_SCAN {
            return false;
        }
        self.tick_count = 0;
        scan_and_emit(home);
        true
    }
}

/// Pure scan: detect timed-out pending decisions, flip their status,
/// and emit the auto-default inbox event. Exposed for tests.
pub(crate) fn scan_and_emit(home: &Path) {
    let now = chrono::Utc::now();
    for mut d in list_pending(home) {
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
        emit_timeout_event(home, &d, elapsed_secs);
        d.status = "timeout".to_string();
        let _ = write_decision(home, &d);
    }
}

fn emit_timeout_event(home: &Path, d: &PendingDecision, elapsed_secs: i64) {
    let text = format!(
        "[decision_timeout] decision {decision_id} from '{sender}' timed out \
         after {elapsed_secs}s (threshold {timeout_secs}s). Auto-default: \
         '{default_action}'. Operator can reverse via reply.",
        decision_id = d.decision_id,
        sender = d.sender,
        elapsed_secs = elapsed_secs,
        timeout_secs = d.timeout_secs,
        default_action = d.default_action,
    );
    let recipient = timeout_recipient();
    let msg = crate::inbox::InboxMessage {
        schema_version: 0,
        id: None,
        from: "system:decision_timeout".to_string(),
        text,
        kind: Some("decision_timeout".to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        channel: None,
        read_at: None,
        thread_id: None,
        parent_id: None,
        delivery_mode: Some("inbox_fallback".to_string()),
        task_id: None,
        force_meta: None,
        correlation_id: Some(d.decision_id.clone()),
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
    if let Err(e) = crate::inbox::enqueue(home, &recipient, msg) {
        tracing::warn!(error = %e, recipient, "decision_timeout: enqueue failed");
    }
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

    #[test]
    fn timeout_recipient_honors_env_override() {
        let _g = env_lock();
        std::env::set_var("AGEND_DECISION_TIMEOUT_RECIPIENT", "alice");
        let r = timeout_recipient();
        std::env::remove_var("AGEND_DECISION_TIMEOUT_RECIPIENT");
        assert_eq!(r, "alice");
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
}
