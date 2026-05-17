//! L2: fixup-team-specific dispatch-idle automation.
//!
//! Two responsibilities, both hard-coded for the fixup team:
//! 1. **Threshold injection** at dispatch time — when fixup-team
//!    members `send(kind=task|query)` without explicit
//!    `expect_reply_within_secs`, inject the default 600s (10min)
//!    so the L1 tracker engages by default for fixup orchestration.
//! 2. **Auto-nudge** when the L1 watchdog fires — scan exceeded
//!    sidecars where the dispatcher belongs to the fixup team and the
//!    nudge has not yet been emitted, then send a status-request
//!    message to the dispatchee (NOT team-wide — target-specific,
//!    matching the L1 sidecar's `target` field).
//!
//! Cross-team isolation: this is the ONLY file that knows the string
//! "fixup". Other teams who want the same automation add a sibling
//! `dispatch_idle/<team>_nudge.rs` module following this exact shape.
//!
//! Defer config-driven thresholds to L2.1 (TeamConfig schema bump)
//! when a second team requests its own default.

use std::path::Path;

use super::{list_pending, pending_path, PendingDispatch};

/// Fixup team name as it appears in fleet.yaml. Single source of truth.
pub(crate) const FIXUP_TEAM_NAME: &str = "fixup";

/// Default dispatch-idle threshold for fixup-team members when no
/// explicit `expect_reply_within_secs` is set on the dispatch. 600s
/// (10 min) per the original watchdog spec.
pub(crate) const FIXUP_DEFAULT_THRESHOLD_SECS: i64 = 600;

/// Scan throttle: 6 ticks × 10s = ~60s — matches L1 cadence.
pub(crate) const TICKS_PER_SCAN: u64 = 6;

/// Resolve the threshold the dispatcher's send should record against.
/// Returns:
/// - `Some(explicit)` when the caller provided one.
/// - `Some(FIXUP_DEFAULT_THRESHOLD_SECS)` when no explicit value and
///   the dispatcher is a fixup-team member.
/// - `None` otherwise (cross-team-safe default-disabled).
pub(crate) fn resolve_threshold_for_dispatch(
    home: &Path,
    dispatcher: &str,
    explicit_threshold_secs: Option<i64>,
) -> Option<i64> {
    if let Some(explicit) = explicit_threshold_secs {
        if explicit > 0 {
            return Some(explicit);
        }
    }
    let team = crate::teams::find_team_for(home, dispatcher)?;
    if team.name == FIXUP_TEAM_NAME {
        Some(FIXUP_DEFAULT_THRESHOLD_SECS)
    } else {
        None
    }
}

/// Per-loop scheduler state for the auto-nudge tracker.
#[derive(Debug, Default)]
pub(crate) struct DispatchIdleFixupNudgeTracker {
    tick_count: u64,
}

impl DispatchIdleFixupNudgeTracker {
    pub(crate) fn maybe_scan(&mut self, home: &Path) -> bool {
        self.tick_count = self.tick_count.saturating_add(1);
        if self.tick_count < TICKS_PER_SCAN {
            return false;
        }
        self.tick_count = 0;
        scan_and_nudge(home);
        true
    }
}

fn is_fixup_member(home: &Path, agent: &str) -> bool {
    crate::teams::find_team_for(home, agent)
        .map(|t| t.name == FIXUP_TEAM_NAME)
        .unwrap_or(false)
}

fn write_dispatch_sidecar(home: &Path, d: &PendingDispatch) -> bool {
    let body = match serde_json::to_string_pretty(d) {
        Ok(s) => s,
        Err(_) => return false,
    };
    crate::store::atomic_write(&pending_path(home, &d.dispatch_id), body.as_bytes()).is_ok()
}

fn emit_nudge(home: &Path, d: &PendingDispatch) -> bool {
    let elapsed = chrono::DateTime::parse_from_rfc3339(&d.issued_at)
        .map(|t| {
            chrono::Utc::now()
                .signed_duration_since(t.with_timezone(&chrono::Utc))
                .num_seconds()
        })
        .unwrap_or(0);
    let text = format!(
        "[fixup-watchdog] dispatched by '{dispatcher}' {elapsed}s ago \
         (threshold {threshold_secs}s, correlation_id={corr}). \
         Please status: BUSY / progress / VERIFIED-if-ready.",
        dispatcher = d.dispatcher,
        elapsed = elapsed,
        threshold_secs = d.threshold_secs,
        corr = d.correlation_id.as_deref().unwrap_or(""),
    );
    let msg = crate::inbox::InboxMessage {
        schema_version: 0,
        id: None,
        from: "system:fixup-watchdog".to_string(),
        text,
        kind: Some("dispatch_idle_nudge".to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        channel: None,
        read_at: None,
        thread_id: None,
        parent_id: None,
        delivery_mode: Some("inbox_fallback".to_string()),
        task_id: d.correlation_id.clone(),
        force_meta: None,
        correlation_id: d.correlation_id.clone(),
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
    match crate::inbox::enqueue(home, &d.target, msg) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                error = %e,
                target = %d.target,
                dispatch_id = %d.dispatch_id,
                "fixup_nudge: enqueue failed"
            );
            false
        }
    }
}

/// Scan exceeded sidecars and emit nudges. Exposed `pub(crate)` for
/// tests.
pub(crate) fn scan_and_nudge(home: &Path) {
    for mut d in list_pending(home) {
        if d.status != "exceeded" {
            continue;
        }
        if d.nudge_sent_at.is_some() {
            continue;
        }
        if !is_fixup_member(home, &d.dispatcher) {
            continue;
        }
        if !emit_nudge(home, &d) {
            continue;
        }
        d.nudge_sent_at = Some(chrono::Utc::now().to_rfc3339());
        let _ = write_dispatch_sidecar(home, &d);
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
    use crate::daemon::dispatch_idle::{pending_dir, pending_path, PendingDispatch};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-dispatch-idle-fixup-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// Write a fleet.yaml that puts `dispatcher` into the fixup team.
    fn write_fleet_with_fixup_member(home: &Path, member: &str) {
        let yaml = format!(
            "schema_version: 1\n\
             teams:\n  fixup:\n    members: [{member}]\n    orchestrator: {member}\n"
        );
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();
    }

    /// Write an exceeded sidecar directly (no nudge yet).
    fn write_exceeded_sidecar(
        home: &Path,
        dispatcher: &str,
        target: &str,
        correlation_id: &str,
        elapsed_secs: i64,
    ) -> String {
        let dir = pending_dir(home);
        std::fs::create_dir_all(&dir).unwrap();
        let id = format!("disp-test-{correlation_id}");
        let issued = (chrono::Utc::now() - chrono::Duration::seconds(elapsed_secs)).to_rfc3339();
        let payload = PendingDispatch {
            schema_version: 1,
            dispatch_id: id.clone(),
            dispatcher: dispatcher.to_string(),
            target: target.to_string(),
            correlation_id: Some(correlation_id.to_string()),
            expected_kind: "task".to_string(),
            threshold_secs: 600,
            issued_at: issued,
            status: "exceeded".to_string(),
            nudge_sent_at: None,
        };
        std::fs::write(
            pending_path(home, &id),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .unwrap();
        id
    }

    /// 9. First scan nudges; second scan does NOT re-nudge (dedup via
    /// `nudge_sent_at` field).
    #[test]
    fn nudge_dedup_via_nudge_sent_at() {
        let home = tmp_home("dedup");
        write_fleet_with_fixup_member(&home, "fixup-lead");
        write_exceeded_sidecar(&home, "fixup-lead", "fixup-reviewer", "t-dedup", 700);
        scan_and_nudge(&home);
        let first_count = crate::inbox::drain(&home, "fixup-reviewer")
            .iter()
            .filter(|m| m.kind.as_deref() == Some("dispatch_idle_nudge"))
            .count();
        assert_eq!(first_count, 1, "first scan must send exactly one nudge");
        scan_and_nudge(&home);
        let second_count = crate::inbox::drain(&home, "fixup-reviewer")
            .iter()
            .filter(|m| m.kind.as_deref() == Some("dispatch_idle_nudge"))
            .count();
        assert_eq!(
            second_count, 0,
            "second scan must NOT re-nudge (dedup via nudge_sent_at)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// 10. Nudge must target the sidecar's `target` field (the
    /// dispatchee), NOT team-wide. Parallel dev-1 + dev-2 dispatches
    /// must not cross-pollinate.
    #[test]
    fn nudge_targets_dispatchee_not_team() {
        let home = tmp_home("target-precision");
        // Multiple fixup members; only fixup-reviewer should get the
        // nudge for the exceeded sidecar.
        let yaml = "schema_version: 1\n\
                    teams:\n  fixup:\n    members: [fixup-lead, fixup-reviewer, fixup-dev, fixup-dev-2]\n\
                            orchestrator: fixup-lead\n";
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();
        write_exceeded_sidecar(&home, "fixup-lead", "fixup-reviewer", "t-precise", 700);
        scan_and_nudge(&home);
        let to_reviewer = crate::inbox::drain(&home, "fixup-reviewer")
            .iter()
            .filter(|m| m.kind.as_deref() == Some("dispatch_idle_nudge"))
            .count();
        let to_dev = crate::inbox::drain(&home, "fixup-dev")
            .iter()
            .filter(|m| m.kind.as_deref() == Some("dispatch_idle_nudge"))
            .count();
        let to_dev2 = crate::inbox::drain(&home, "fixup-dev-2")
            .iter()
            .filter(|m| m.kind.as_deref() == Some("dispatch_idle_nudge"))
            .count();
        assert_eq!(to_reviewer, 1, "the dispatchee receives exactly one nudge");
        assert_eq!(to_dev, 0, "other team members are NOT nudged");
        assert_eq!(to_dev2, 0, "other team members are NOT nudged");
        std::fs::remove_dir_all(&home).ok();
    }

    /// 11. Cross-team isolation contract: if the dispatcher is not in
    /// the fixup team, this module does not nudge. Other teams must
    /// supply their own `<team>_nudge.rs`.
    #[test]
    fn nudge_skips_non_fixup_dispatcher() {
        let home = tmp_home("non-fixup");
        // No fleet.yaml fixup team → dispatcher isn't a fixup member.
        let yaml = "schema_version: 1\n\
                    teams:\n  research:\n    members: [research-lead, research-dev]\n\
                            orchestrator: research-lead\n";
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();
        write_exceeded_sidecar(&home, "research-lead", "research-dev", "t-cross", 700);
        scan_and_nudge(&home);
        let inbox = crate::inbox::drain(&home, "research-dev");
        assert!(
            !inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_nudge")),
            "fixup_nudge must NOT nudge non-fixup-team dispatchees: {inbox:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
