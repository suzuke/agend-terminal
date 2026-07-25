//! Sprint 58 Wave 4 PR-1 (#2 engineering anti-stall): structural
//! gate ensuring every `send kind=task` dispatch carries an explicit
//! `task_id`. Closes the Wave 3 PR-1 dispatch protocol gap where
//! task-board IDs and `send` calls weren't structurally tied — a
//! lead could create a task entry but forget to dispatch with
//! `task_id=...`, leaving the recipient with no correlation handle
//! (incident reference: m-20260509031109840434-59 — dev idle-waited
//! 100+ min in Wave 3 PR-1 because of this gap).
//!
//! Extracted from `comms.rs` to keep that file under the 700 LOC
//! handler invariant (`tests/file_size_invariant.rs`).

use crate::identity::Sender;
use serde_json::{json, Value};

/// One-shot anti-stall gate + progress + idle-watchdog hook for
/// the unified `send` handler. Returns `Some(error json)` when
/// `request_kind=task` is rejected (Wave 4 PR-1 contract —
/// missing/malformed task_id), `None` otherwise. As side-effects
/// on the OK path:
/// - Wave 1 PR-1: touches the task's progress sidecar when
///   `task_id` or `correlation_id` is present, so the per-task
///   ETA scanner sees the task as alive.
/// - Wave 1 PR-2: touches the sender's agent-activity sidecar
///   so the dev-vantage 60min + fleet-vantage 30min idle
///   watchdogs see the agent as making forward progress.
///
/// Folding all behaviors into one helper keeps the comms.rs call
/// site a single line under the 700 LOC handler invariant.
pub(crate) fn enforce_send_invariants(
    home: &std::path::Path,
    args: &Value,
    sender: &Option<Sender>,
) -> Option<Value> {
    // Wave 4 PR-1 gate: kind=task without task_id rejected.
    // #1050: single-target + empty task_id exempt — handle_delegate_task
    // auto-creates after validation passes (avoids orphan tasks).
    // Malformed (non-empty but bad shape) task_ids still rejected everywhere.
    if args["request_kind"].as_str() == Some("task") {
        let is_broadcast = args.get("instances").is_some()
            || args.get("team").is_some()
            || args.get("tags").is_some();
        let auto_create_eligible =
            !is_broadcast && args["task_id"].as_str().unwrap_or("").is_empty();
        if !auto_create_eligible {
            if let Err(msg) = validate_task_id_present(args) {
                return Some(json!({"error": msg, "code": "task_id_required"}));
            }
        }
    }
    // Wave 1 PR-1 hooks (a)+(c): any send carrying task_id (or
    // correlation_id for kind=report verdict) touches the progress
    // sidecar.
    let task_id = args["task_id"]
        .as_str()
        .or_else(|| args["correlation_id"].as_str())
        .unwrap_or("");
    if !task_id.is_empty() {
        let source = if args["request_kind"].as_str() == Some("report") {
            crate::daemon::task_progress::ProgressSource::CiVerdict
        } else {
            crate::daemon::task_progress::ProgressSource::Broadcast
        };
        crate::daemon::task_progress::touch(home, task_id, source);
    }
    // Wave 1 PR-2 idle watchdog: touch sender's activity sidecar.
    if let Some(s) = sender {
        crate::daemon::idle_watchdog::touch_agent_activity(home, s.as_str());
    }
    None
}

/// Returns the rejection error message on failure (with operator-
/// actionable hint mentioning `task action=create`) so the send
/// handler can surface it as a structured `code: "task_id_required"`
/// response.
pub(super) fn validate_task_id_present(args: &Value) -> Result<(), String> {
    let task_id = args["task_id"].as_str().unwrap_or("");
    if task_id.is_empty() {
        return Err(
            "send kind=task requires 'task_id' — first call task action=create to obtain a \
             't-...' id, then send kind=task task_id=t-... (Sprint 58 Wave 4 anti-stall \
             contract)"
                .to_string(),
        );
    }
    if !is_valid_task_id_shape(task_id) {
        return Err(format!(
            "send kind=task: 'task_id' value '{task_id}' has invalid shape — task_id must \
             start with 't-' and contain only ASCII alphanumeric / hyphen / underscore \
             characters (length 4-128). Did you mean to call task action=create first?"
        ));
    }
    Ok(())
}

/// Cheap shape check for task_id strings. The auto-generated form
/// is `t-<14-digit-timestamp>-<seq>`; operator-supplied entries
/// can use any reasonable identifier as long as it starts with the
/// `t-` prefix. We accept ASCII alphanumeric + `-_` after the
/// prefix to allow operator names like `t-sprint58-wave4-pr1`.
///
/// Rejects: empty, missing prefix, control characters, length
/// outside 4-128 (catches obvious typos without rejecting valid
/// IDs).
fn is_valid_task_id_shape(s: &str) -> bool {
    if s.len() < 4 || s.len() > 128 {
        return false;
    }
    let Some(rest) = s.strip_prefix("t-") else {
        return false;
    };
    if rest.is_empty() {
        return false;
    }
    rest.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── #3026: coalesce the per-send progress/activity sidecar writes ──────

    fn tmp_home_3026(tag: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("agend-3026-{}-{}-{}", std::process::id(), tag, id));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn send_3026(home: &std::path::Path, task_id: &str, agent: &str) {
        enforce_send_invariants(
            home,
            &json!({"request_kind": "update", "task_id": task_id}),
            &Sender::new(agent),
        );
    }

    /// #3026 RED — the real send gate touches BOTH sidecars on every accepted
    /// send (`task_progress::touch` + `idle_watchdog::touch_agent_activity`),
    /// each a mkdir + flock + `atomic_write` = two fsyncs. Back-to-back sends
    /// must pay that chain once, not twice.
    ///
    /// Deterministic with no sleep: the window is judged against the PERSISTED
    /// stamp, and both payloads embed `Utc::now()`, so an unchanged stamp after
    /// the second send is proof that no write occurred.
    #[test]
    fn repeat_send_inside_window_skips_both_writes_3026() {
        let home = tmp_home_3026("inside");
        send_3026(&home, "t-3026-in", "agent-3026");
        let progress_first =
            crate::daemon::task_progress::read_last_progress_at(&home, "t-3026-in")
                .expect("first send must create the progress sidecar");
        let activity_first =
            crate::daemon::idle_watchdog::read_agent_last_active(&home, "agent-3026")
                .expect("first send must create the activity sidecar");

        send_3026(&home, "t-3026-in", "agent-3026");

        assert_eq!(
            crate::daemon::task_progress::read_last_progress_at(&home, "t-3026-in"),
            Some(progress_first),
            "a second send inside the window must not rewrite the progress sidecar"
        );
        assert_eq!(
            crate::daemon::idle_watchdog::read_agent_last_active(&home, "agent-3026"),
            Some(activity_first),
            "a second send inside the window must not rewrite the activity sidecar"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #3026 — past the window BOTH writes must happen, and the worst-case
    /// staleness the window leaves stays orders of magnitude below the tightest
    /// consumer floor (idle watchdogs at 1800s / 3600s; anti-stall at
    /// `eta_secs * 1.5`).
    #[test]
    fn send_after_window_persists_both_and_stays_fresh_3026() {
        let home = tmp_home_3026("after");
        // Literal 6s on purpose: reading production's own constant here would
        // make the test blind to that constant changing.
        let stale = chrono::Utc::now() - chrono::Duration::seconds(6);
        crate::daemon::task_progress::seed_progress_at(&home, "t-3026-out", stale);
        crate::daemon::idle_watchdog::seed_activity_at(&home, "agent-3026", stale);

        send_3026(&home, "t-3026-out", "agent-3026");

        let progress = crate::daemon::task_progress::read_last_progress_at(&home, "t-3026-out")
            .expect("progress sidecar readable");
        let activity = crate::daemon::idle_watchdog::read_agent_last_active(&home, "agent-3026")
            .expect("activity sidecar readable");
        assert!(
            progress > stale,
            "a send past the window must persist progress: seeded={stale} after={progress}"
        );
        assert!(
            activity > stale,
            "a send past the window must persist activity: seeded={stale} after={activity}"
        );
        for (label, ts) in [("progress", progress), ("activity", activity)] {
            let lag = chrono::Utc::now().signed_duration_since(ts).num_seconds();
            assert!(
                lag <= 5,
                "{label} staleness must stay within the 5s window (lag={lag}s) — \
                 far below the 1800s idle floor"
            );
        }
        std::fs::remove_dir_all(&home).ok();
    }

    /// #3026 — the coalescing must live in the shared owners (`touch` /
    /// `touch_agent_activity`), NOT in this gate:
    /// `comms_delegate::handle_delegate_task` is a second production caller of
    /// `task_progress::touch` and has to inherit the behavior without its own
    /// patch. Calling the owner directly is what pins that placement.
    #[test]
    fn coalescing_lives_in_the_touch_owner_not_this_gate_3026() {
        let home = tmp_home_3026("owner");
        crate::daemon::task_progress::touch(
            &home,
            "t-3026-owner",
            crate::daemon::task_progress::ProgressSource::Broadcast,
        );
        let first = crate::daemon::task_progress::read_last_progress_at(&home, "t-3026-owner")
            .expect("sidecar created");
        crate::daemon::task_progress::touch(
            &home,
            "t-3026-owner",
            crate::daemon::task_progress::ProgressSource::CiVerdict,
        );
        assert_eq!(
            crate::daemon::task_progress::read_last_progress_at(&home, "t-3026-owner"),
            Some(first),
            "touch itself must coalesce so every caller inherits it"
        );

        crate::daemon::idle_watchdog::touch_agent_activity(&home, "agent-3026");
        let activity_first =
            crate::daemon::idle_watchdog::read_agent_last_active(&home, "agent-3026")
                .expect("sidecar created");
        crate::daemon::idle_watchdog::touch_agent_activity(&home, "agent-3026");
        assert_eq!(
            crate::daemon::idle_watchdog::read_agent_last_active(&home, "agent-3026"),
            Some(activity_first),
            "touch_agent_activity itself must coalesce so every caller inherits it"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn empty_task_id_rejected_with_actionable_hint() {
        let result = validate_task_id_present(&json!({}));
        let err = result.unwrap_err();
        assert!(
            err.contains("task_id"),
            "rejection must mention task_id: {err}"
        );
        assert!(
            err.contains("task action=create"),
            "rejection must guide caller to task action=create: {err}"
        );
        assert!(
            err.contains("t-"),
            "rejection must reference the t-... id prefix shape: {err}"
        );
    }

    #[test]
    fn empty_string_task_id_rejected() {
        let result = validate_task_id_present(&json!({"task_id": ""}));
        assert!(result.is_err(), "empty string must be rejected");
    }

    #[test]
    fn valid_auto_generated_task_id_accepted() {
        let result = validate_task_id_present(&json!({"task_id": "t-20260509040842727169-9"}));
        assert!(
            result.is_ok(),
            "auto-generated form must be accepted: {result:?}"
        );
    }

    #[test]
    fn valid_operator_named_task_id_accepted() {
        let result = validate_task_id_present(&json!({"task_id": "t-sprint58-wave4-pr1"}));
        assert!(
            result.is_ok(),
            "operator-named form must be accepted: {result:?}"
        );
    }

    #[test]
    fn task_id_without_t_prefix_rejected() {
        let result = validate_task_id_present(&json!({"task_id": "no-prefix"}));
        let err = result.unwrap_err();
        assert!(err.contains("invalid shape"), "shape error expected: {err}");
    }

    #[test]
    fn task_id_with_only_prefix_rejected() {
        let result = validate_task_id_present(&json!({"task_id": "t-"}));
        let err = result.unwrap_err();
        assert!(
            err.contains("invalid shape"),
            "empty body after prefix rejected: {err}"
        );
    }

    #[test]
    fn task_id_with_whitespace_rejected() {
        let result = validate_task_id_present(&json!({"task_id": "t-with spaces"}));
        let err = result.unwrap_err();
        assert!(err.contains("invalid shape"), "whitespace rejected: {err}");
    }

    #[test]
    fn task_id_with_slash_rejected() {
        let result = validate_task_id_present(&json!({"task_id": "t-bad/path"}));
        let err = result.unwrap_err();
        assert!(err.contains("invalid shape"), "slash rejected: {err}");
    }

    #[test]
    fn task_id_too_short_rejected() {
        let result = validate_task_id_present(&json!({"task_id": "x"}));
        let err = result.unwrap_err();
        // Single-character "x" is empty after prefix-strip OR fails
        // the length check; either path lands "invalid shape".
        assert!(err.contains("invalid shape"), "too-short rejected: {err}");
    }

    #[test]
    fn task_id_with_underscore_accepted() {
        // Defensive bonus: underscore allowed (operator-friendly).
        let result = validate_task_id_present(&json!({"task_id": "t-foo_bar_baz"}));
        assert!(result.is_ok(), "underscore must be accepted: {result:?}");
    }

    #[test]
    fn task_id_at_length_boundary_accepted() {
        // 128 chars total — at the upper bound.
        let id = format!("t-{}", "a".repeat(126));
        assert_eq!(id.len(), 128);
        let result = validate_task_id_present(&json!({"task_id": id}));
        assert!(result.is_ok(), "128-char boundary accepted: {result:?}");
    }

    #[test]
    fn task_id_over_length_boundary_rejected() {
        let id = format!("t-{}", "a".repeat(127));
        assert_eq!(id.len(), 129);
        let result = validate_task_id_present(&json!({"task_id": id}));
        assert!(result.is_err(), "129-char rejected");
    }
}
