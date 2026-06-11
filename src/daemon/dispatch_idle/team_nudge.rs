//! L2: generic per-team dispatch-idle automation.
//!
//! Two responsibilities, for ANY team (t-dehardcode-fixup-nudge-multiteam — was
//! hard-coded to the "fixup" team):
//! 1. **Threshold injection** at dispatch time — when a team member
//!    `send(kind=task|query)` without an explicit `expect_reply_within_secs`,
//!    inject the default [`DEFAULT_DISPATCH_THRESHOLD_SECS`] so the L1 tracker
//!    engages by default for that team's orchestration.
//! 2. **Auto-nudge** when the L1 watchdog fires — scan exceeded sidecars where the
//!    dispatcher belongs to ANY team and the nudge has not yet been emitted, then
//!    send a status-request message to the dispatchee (NOT team-wide —
//!    target-specific, matching the L1 sidecar's `target` field), stamped
//!    per-team `[<team>-watchdog]` so the operator sees which team.
//!
//! Solo agents (no team) are intentionally NOT tracked here — without a team
//! there is no orchestrator / SLA context. Per-team threshold config is a
//! deferred follow-up (TeamConfig schema bump); today the threshold is a single
//! global default.

use std::path::Path;

use super::{list_pending, pending_path, DispatchStatus, PendingDispatch};

/// Default dispatch-idle threshold for ANY team member when no explicit
/// `expect_reply_within_secs` is set on the dispatch. 600s (10 min) per the
/// original watchdog spec. Per-team override is a deferred follow-up.
pub(crate) const DEFAULT_DISPATCH_THRESHOLD_SECS: i64 = 600;

/// Scan throttle: 6 ticks × 10s = ~60s — matches L1 cadence.
pub(crate) const TICKS_PER_SCAN: u64 = 6;

/// Resolve the threshold the dispatcher's send should record against.
/// Returns:
/// - `Some(explicit)` when the caller provided one.
/// - `Some(DEFAULT_DISPATCH_THRESHOLD_SECS)` when no explicit value and the
///   dispatcher belongs to ANY team.
/// - `None` for a teamless (solo) dispatcher (no orchestration context → no
///   default tracking).
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
    // Any team member's dispatch gets the default threshold (was gated to
    // team.name == "fixup"; #t-dehardcode-fixup-nudge-multiteam generalised it).
    crate::teams::find_team_for(home, dispatcher).map(|_team| DEFAULT_DISPATCH_THRESHOLD_SECS)
}

/// Per-loop scheduler state for the auto-nudge tracker.
#[derive(Debug, Default)]
pub(crate) struct DispatchIdleNudgeTracker {
    tick_count: u64,
}

impl DispatchIdleNudgeTracker {
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

/// The dispatcher's team name, if it belongs to one. Gates the auto-nudge AND
/// supplies the per-team `[<team>-watchdog]` label. `None` for a teamless (solo)
/// dispatcher → not nudged.
fn dispatcher_team(home: &Path, agent: &str) -> Option<String> {
    crate::teams::find_team_for(home, agent).map(|t| t.name)
}

/// [M2] Under the sidecar lock, stamp `nudge_sent_at` iff the sidecar still
/// exists AND is still `Exceeded` AND not already nudged; returns `true` when
/// stamped. Replaces the prior unconditional `atomic_write`, which — after the
/// slow `emit_nudge` — could RECREATE (resurrect) a sidecar that `mark_resolved`
/// had just deleted (report arrived mid-flight), leaking it forever. `with_json_state`
/// returns `Ok(None)` for a missing file and NEVER recreates it, and re-reads the
/// status under the lock (no lost update). Mirrors the L1 `scan_and_emit`
/// flock+re-read discipline.
fn stamp_nudge_sent(home: &Path, dispatch_id: &str) -> bool {
    matches!(
        crate::store::with_json_state::<PendingDispatch, _, _>(
            &pending_path(home, dispatch_id),
            |cur| {
                if cur.status == DispatchStatus::Exceeded && cur.nudge_sent_at.is_none() {
                    cur.nudge_sent_at = Some(chrono::Utc::now().to_rfc3339());
                    true
                } else {
                    false
                }
            },
        ),
        Ok(Some(true))
    )
}

fn emit_nudge(home: &Path, d: &PendingDispatch, team: &str) -> bool {
    let elapsed = chrono::DateTime::parse_from_rfc3339(&d.issued_at)
        .map(|t| {
            chrono::Utc::now()
                .signed_duration_since(t.with_timezone(&chrono::Utc))
                .num_seconds()
        })
        .unwrap_or(0);
    // #1866: informational tone, not an alarm. This fires once per dispatch
    // (deduped by `nudge_sent_at`) and only after the state-aware gate
    // (`target_is_working` + `set_waiting_on`) failed to detect activity, so it
    // is a gentle check-in, NOT a "you're stuck" page. An agent legitimately
    // heads-down (e.g. waiting on its own long build/CI) can ignore it and keep
    // working, or call `set_waiting_on(<reason>)` to suppress future nudges while
    // blocked. Per-team label so the operator sees which team's dispatch is quiet.
    let text = format!(
        "[{team}-watchdog] FYI: '{dispatcher}' dispatch has been quiet {elapsed}s \
         (threshold {threshold_secs}s, correlation_id={corr}). No action needed if \
         you're mid-task — a status (BUSY / progress / VERIFIED-if-ready) or \
         set_waiting_on(<reason>) keeps the board accurate.",
        team = team,
        dispatcher = d.dispatcher,
        elapsed = elapsed,
        threshold_secs = d.threshold_secs,
        corr = d.correlation_id.as_deref().unwrap_or(""),
    );
    // #947: same fallback as emit_exceeded_event — see that site for
    // design rationale (blend semantic acceptable due to `disp-`
    // prefix convention).
    let corr = d
        .correlation_id
        .clone()
        .unwrap_or_else(|| d.dispatch_id.clone());
    match crate::inbox::notify_system(
        home,
        &d.target,
        // Generic sender across all teams (was `system:fixup-watchdog`).
        "system:dispatch-watchdog",
        "dispatch_idle_nudge",
        text,
        Some(&corr),
        d.correlation_id.as_deref(),
    ) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                error = %e,
                target = %d.target,
                dispatch_id = %d.dispatch_id,
                "team_nudge: enqueue failed"
            );
            false
        }
    }
}

/// Scan exceeded sidecars and emit nudges. Exposed `pub(crate)` for
/// tests.
pub(crate) fn scan_and_nudge(home: &Path) {
    for d in list_pending(home) {
        if d.status != DispatchStatus::Exceeded {
            continue;
        }
        if d.nudge_sent_at.is_some() {
            continue;
        }
        // Nudge for ANY team's dispatch (was gated to the fixup team); a teamless
        // (solo) dispatcher has no orchestration context → skip.
        // #1923 G6: but STAMP `nudge_sent_at` first so the L2 team-nudge does not
        // retry this Exceeded sidecar every scan FOREVER (the dispatcher left its
        // team — or was always solo — so there is no orchestrator to escalate to).
        // `stamp_nudge_sent` only sets the L2 dedup guard under the sidecar lock;
        // it does NOT remove the sidecar, so a legitimate solo dispatcher's L1
        // lifecycle is untouched. (A dispatcher that left the FLEET entirely is
        // already retired in L1 by the #1923 G2 `dispatcher_not_in_fleet` check;
        // this covers the narrower "left the team but still in the fleet" window.)
        let Some(team) = dispatcher_team(home, &d.dispatcher) else {
            stamp_nudge_sent(home, &d.dispatch_id);
            continue;
        };
        if !emit_nudge(home, &d, &team) {
            continue;
        }
        // [M2] Stamp via a LOCKED RMW that bails (and never recreates) if the
        // sidecar was resolved/deleted during the slow `emit_nudge` above — see
        // `stamp_nudge_sent`. A failed stamp here is benign (the dispatch was
        // resolved): the report already arrived, so no nudge will recur.
        if !stamp_nudge_sent(home, &d.dispatch_id) {
            tracing::debug!(
                dispatch_id = %d.dispatch_id,
                "team-nudge: sidecar resolved or changed before stamp — not resurrected"
            );
        }
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
            status: DispatchStatus::Exceeded,
            nudge_sent_at: None,
            not_working_streak: 0,
            refresh_count: 0,
            long_running_escalated: false,
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

    /// #1923 G6: a sidecar whose dispatcher has NO team (solo, or left the team
    /// but still in the fleet) must have its L2 `nudge_sent_at` STAMPED — else the
    /// team-nudge scan retries the Exceeded sidecar every scan FOREVER (there is no
    /// orchestrator to escalate to). The sidecar is NOT removed (a solo dispatcher
    /// keeps its L1 lifecycle); only the L2 retry is stopped.
    #[test]
    fn no_team_dispatcher_stamps_nudge_sent_stops_retry_1923_g6() {
        let home = tmp_home("g6-no-team");
        // Dispatcher is in the fleet (so L1 #1923-G2 doesn't retire it) but in NO
        // team → `dispatcher_team` returns None.
        std::fs::write(
            home.join("fleet.yaml"),
            "schema_version: 1\ninstances:\n  solo-lead:\n    backend: claude\n",
        )
        .unwrap();
        let id = write_exceeded_sidecar(&home, "solo-lead", "dev", "t-g6", 700);
        scan_and_nudge(&home);
        let sidecar: PendingDispatch =
            serde_json::from_str(&std::fs::read_to_string(pending_path(&home, &id)).unwrap())
                .unwrap();
        assert!(
            sidecar.nudge_sent_at.is_some(),
            "#1923 G6: a no-team dispatcher's sidecar must be stamped to stop the L2 retry"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// [M2] §3.9: a sidecar resolved (deleted) before the nudge stamp must NOT be
    /// resurrected by the locked RMW. Simulates a report arriving (which deletes
    /// the sidecar) during the in-flight `emit_nudge`, then the stamp write.
    #[test]
    fn resolved_sidecar_not_resurrected_by_nudge_write() {
        let home = tmp_home("m2-resurrect");
        let id = write_exceeded_sidecar(&home, "fixup-lead", "fixup-reviewer", "t-m2", 700);
        let path = pending_path(&home, &id);
        assert!(path.exists(), "precondition: sidecar exists");
        // Report arrives → sidecar deleted.
        std::fs::remove_file(&path).unwrap();
        // In-flight nudge tries to stamp — must NOT recreate the file.
        assert!(
            !stamp_nudge_sent(&home, &id),
            "stamp on a deleted sidecar returns false"
        );
        assert!(
            !path.exists(),
            "[M2] a resolved sidecar must NOT be resurrected by the nudge write"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// [M2] end-to-end: the real `mark_resolved` delete + a racing nudge stamp
    /// leaves the sidecar gone (not resurrected).
    #[test]
    fn mark_resolved_then_nudge_write_does_not_resurrect() {
        let home = tmp_home("m2-e2e");
        let id = write_exceeded_sidecar(&home, "fixup-lead", "fixup-reviewer", "t-m2c", 700);
        let path = pending_path(&home, &id);
        crate::daemon::dispatch_idle::mark_resolved(&home, "t-m2c");
        assert!(!path.exists(), "mark_resolved deleted the sidecar");
        assert!(
            !stamp_nudge_sent(&home, &id),
            "[M2] in-flight nudge stamp must not resurrect a mark_resolved'd sidecar"
        );
        assert!(!path.exists(), "[M2] still gone after the nudge write");
        std::fs::remove_dir_all(&home).ok();
    }

    /// [M2] a live Exceeded sidecar IS stamped (once) by the locked RMW.
    #[test]
    fn live_exceeded_sidecar_stamped_once() {
        let home = tmp_home("m2-stamp");
        let id = write_exceeded_sidecar(&home, "fixup-lead", "fixup-reviewer", "t-m2b", 700);
        assert!(stamp_nudge_sent(&home, &id), "first stamp succeeds");
        let content = std::fs::read_to_string(pending_path(&home, &id)).unwrap();
        let d: PendingDispatch = serde_json::from_str(&content).unwrap();
        assert!(d.nudge_sent_at.is_some(), "nudge_sent_at persisted");
        assert!(
            !stamp_nudge_sent(&home, &id),
            "second stamp is a no-op (already nudged)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// [M2] §3.9: a sidecar cleared by `cleanup_pending_for_task_id` (#1018
    /// task-close) is NOT resurrected by a racing nudge stamp — the second
    /// sidecar-delete site now also deletes under the lock via
    /// `delete_sidecar_locked`.
    #[test]
    fn cleanup_for_task_id_then_nudge_write_does_not_resurrect() {
        let home = tmp_home("m2-taskclose");
        let id = write_exceeded_sidecar(&home, "fixup-lead", "fixup-reviewer", "t-m2task", 700);
        let path = pending_path(&home, &id);
        crate::daemon::dispatch_idle::cleanup_pending_for_task_id(&home, "t-m2task");
        assert!(!path.exists(), "task-close cleanup deleted the sidecar");
        assert!(
            !stamp_nudge_sent(&home, &id),
            "[M2] in-flight nudge must not resurrect a task-close-cleared sidecar"
        );
        assert!(!path.exists(), "[M2] still gone after the nudge write");
        std::fs::remove_dir_all(&home).ok();
    }

    /// [M2] §3.9: a sidecar cleared by `cleanup_pending_for_instance` (#1018
    /// instance-delete) is NOT resurrected by a racing nudge stamp — the third
    /// sidecar-delete site now also deletes under the lock.
    #[test]
    fn cleanup_for_instance_then_nudge_write_does_not_resurrect() {
        let home = tmp_home("m2-instdel");
        let id = write_exceeded_sidecar(&home, "fixup-lead", "fixup-reviewer", "t-m2inst", 700);
        let path = pending_path(&home, &id);
        crate::daemon::dispatch_idle::cleanup_pending_for_instance(&home, "fixup-reviewer");
        assert!(
            !path.exists(),
            "instance-delete cleanup deleted the sidecar"
        );
        assert!(
            !stamp_nudge_sent(&home, &id),
            "[M2] in-flight nudge must not resurrect an instance-delete-cleared sidecar"
        );
        assert!(!path.exists(), "[M2] still gone after the nudge write");
        std::fs::remove_dir_all(&home).ok();
    }

    /// [M2] §3.9: the L1 `scan_and_emit` tick-time stale-sidecar delete (#1018-A,
    /// the 4th delete site) is NOT resurrected by a racing nudge stamp. Drives the
    /// real `scan_and_emit` entry with a past-threshold Pending sidecar whose
    /// target is not in the fleet (→ stale → deleted under the lock).
    #[test]
    fn scan_and_emit_stale_delete_then_nudge_does_not_resurrect() {
        let home = tmp_home("m2-stale");
        // Fleet exists but does NOT contain the target → `target_not_in_fleet`.
        write_fleet_with_fixup_member(&home, "fixup-lead");
        let dir = pending_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        let id = "disp-test-stale".to_string();
        // Past-threshold so scan_and_emit progresses to the stale check.
        let issued = (chrono::Utc::now() - chrono::Duration::seconds(700)).to_rfc3339();
        let payload = PendingDispatch {
            schema_version: 1,
            dispatch_id: id.clone(),
            dispatcher: "fixup-lead".to_string(),
            target: "ghost-not-in-fleet".to_string(),
            correlation_id: Some("t-stale".to_string()),
            expected_kind: "task".to_string(),
            threshold_secs: 600,
            issued_at: issued,
            status: DispatchStatus::Pending, // scan_and_emit only scans Pending
            nudge_sent_at: None,
            not_working_streak: 0,
            refresh_count: 0,
            long_running_escalated: false,
        };
        let path = pending_path(&home, &id);
        std::fs::write(&path, serde_json::to_string_pretty(&payload).unwrap()).unwrap();

        // Real entry: must stale-delete (target not in fleet) under the lock.
        crate::daemon::dispatch_idle::scan_and_emit(&home);
        assert!(
            !path.exists(),
            "scan_and_emit must stale-delete the sidecar (target not in fleet)"
        );
        assert!(
            !stamp_nudge_sent(&home, &id),
            "[M2] in-flight stamp must not resurrect a stale-deleted sidecar"
        );
        assert!(!path.exists(), "[M2] still gone after the nudge write");
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

    /// t-dehardcode-fixup-nudge-multiteam: ANY team's dispatch now nudges (was
    /// gated to "fixup"). A non-fixup team's overdue dispatch fires the nudge,
    /// stamped with that team's `[<team>-watchdog]` label.
    #[test]
    fn nudge_fires_for_any_team_dispatcher_multiteam() {
        let home = tmp_home("non-fixup");
        let yaml = "schema_version: 1\n\
                    teams:\n  research:\n    members: [research-lead, research-dev]\n\
                            orchestrator: research-lead\n";
        std::fs::write(home.join("fleet.yaml"), yaml).unwrap();
        write_exceeded_sidecar(&home, "research-lead", "research-dev", "t-cross", 700);
        scan_and_nudge(&home);
        let inbox = crate::inbox::drain(&home, "research-dev");
        let nudge = inbox
            .iter()
            .find(|m| m.kind.as_deref() == Some("dispatch_idle_nudge"));
        assert!(
            nudge.is_some(),
            "any-team dispatch must now be nudged (multi-team): {inbox:?}"
        );
        assert!(
            nudge.unwrap().text.contains("[research-watchdog]"),
            "nudge must carry the per-team label `[research-watchdog]`: {:?}",
            nudge.unwrap().text
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// t-dehardcode-fixup-nudge-multiteam: a TEAMLESS (solo) dispatcher has no
    /// orchestration context → still NOT nudged (and `resolve_threshold_for_dispatch`
    /// returns None for it, so no sidecar is even tracked in production).
    #[test]
    fn nudge_skips_teamless_solo_dispatcher() {
        let home = tmp_home("solo");
        // fleet.yaml with NO teams → the dispatcher belongs to none.
        std::fs::write(home.join("fleet.yaml"), "schema_version: 1\nteams: {}\n").unwrap();
        write_exceeded_sidecar(&home, "solo-agent", "other-agent", "t-solo", 700);
        scan_and_nudge(&home);
        let inbox = crate::inbox::drain(&home, "other-agent");
        assert!(
            !inbox
                .iter()
                .any(|m| m.kind.as_deref() == Some("dispatch_idle_nudge")),
            "a teamless (solo) dispatcher must NOT be nudged: {inbox:?}"
        );
        // And the threshold helper declines a teamless dispatcher.
        assert_eq!(
            resolve_threshold_for_dispatch(&home, "solo-agent", None),
            None,
            "teamless dispatcher gets no default threshold"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #947 fallback contract: fixup_nudge correlation_id ──
    //
    // Mirror of the `emit_exceeded_event` fallback semantic. When the
    // upstream `send` omitted correlation_id, the fixup-watchdog nudge
    // falls back to `d.dispatch_id`. See dispatch_idle/mod.rs #947 test
    // block for the design rationale.

    /// Helper to plant an exceeded sidecar with optional correlation_id
    /// (the existing `write_exceeded_sidecar` always sets one — this
    /// variant tests the absent case).
    fn write_exceeded_sidecar_no_correlation(
        home: &Path,
        dispatcher: &str,
        target: &str,
        elapsed_secs: i64,
    ) -> String {
        let dir = pending_dir(home);
        std::fs::create_dir_all(&dir).unwrap();
        let id = "disp-test-947-no-corr".to_string();
        let issued = (chrono::Utc::now() - chrono::Duration::seconds(elapsed_secs)).to_rfc3339();
        let payload = PendingDispatch {
            schema_version: 1,
            dispatch_id: id.clone(),
            dispatcher: dispatcher.to_string(),
            target: target.to_string(),
            correlation_id: None,
            expected_kind: "task".to_string(),
            threshold_secs: 600,
            issued_at: issued,
            status: DispatchStatus::Exceeded,
            nudge_sent_at: None,
            not_working_streak: 0,
            refresh_count: 0,
            long_running_escalated: false,
        };
        std::fs::write(
            pending_path(home, &id),
            serde_json::to_string_pretty(&payload).unwrap(),
        )
        .unwrap();
        id
    }

    /// #947 test 3 — fixup_nudge with upstream correlation_id preserves it.
    #[test]
    fn fixup_nudge_with_upstream_correlation_preserves_it() {
        let home = tmp_home("947-fixup-upstream");
        write_fleet_with_fixup_member(&home, "fixup-lead");
        write_exceeded_sidecar(
            &home,
            "fixup-lead",
            "fixup-reviewer",
            "upstream-corr-xyz",
            700,
        );
        scan_and_nudge(&home);
        let inbox = crate::inbox::drain(&home, "fixup-reviewer");
        let nudge = inbox
            .iter()
            .find(|m| m.kind.as_deref() == Some("dispatch_idle_nudge"))
            .expect("fixup-watchdog must enqueue nudge");
        assert_eq!(
            nudge.correlation_id.as_deref(),
            Some("upstream-corr-xyz"),
            "fixup_nudge must preserve upstream correlation_id"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #947 test 4 — fixup_nudge without upstream falls back to dispatch_id.
    #[test]
    fn fixup_nudge_without_upstream_falls_back_to_dispatch_id() {
        let home = tmp_home("947-fixup-fallback");
        write_fleet_with_fixup_member(&home, "fixup-lead");
        let dispatch_id =
            write_exceeded_sidecar_no_correlation(&home, "fixup-lead", "fixup-reviewer", 700);
        scan_and_nudge(&home);
        let inbox = crate::inbox::drain(&home, "fixup-reviewer");
        let nudge = inbox
            .iter()
            .find(|m| m.kind.as_deref() == Some("dispatch_idle_nudge"))
            .expect("fixup-watchdog must enqueue nudge");
        assert_eq!(
            nudge.correlation_id.as_deref(),
            Some(dispatch_id.as_str()),
            "fixup_nudge must fall back to dispatch_id when upstream missing"
        );
        // dispatch_id format check: `disp-` prefix self-documents the value class.
        assert!(
            nudge
                .correlation_id
                .as_deref()
                .unwrap_or("")
                .starts_with("disp-"),
            "dispatch_id fallback must carry `disp-` prefix"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
