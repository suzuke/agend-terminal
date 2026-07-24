use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

// #78445-2: quota-wedge tests homed in a sibling `*_tests.rs` (LOC-exempt) so
// `mod.rs` stays under the anti-monolith ceiling. Submodule of `tests`, so its
// `use super::*` inherits both these helpers and the production items.
#[path = "tests/dispatch_idle_quota_78445_tests.rs"]
mod quota_78445_tests;

// #2760 REDs (non-task fail-open / canonical-orphan suppress) — same LOC-exempt
// split pattern as the quota tests above.
#[path = "tests/dispatch_idle_reds_2760_tests.rs"]
mod reds_2760_tests;

/// #1636: the `DispatchStatus` enum MUST serialize to / deserialize from the
/// exact lowercase wire strings the prior stringly-typed field used, so
/// existing on-disk sidecars + IPC payloads stay byte-compatible.
#[test]
fn dispatch_status_serde_roundtrip() {
    for (variant, wire) in [
        (DispatchStatus::Pending, "\"pending\""),
        (DispatchStatus::Resolved, "\"resolved\""),
        (DispatchStatus::Exceeded, "\"exceeded\""),
        (DispatchStatus::Cancelled, "\"cancelled\""),
    ] {
        // enum → string matches the legacy wire form
        assert_eq!(serde_json::to_string(&variant).unwrap(), wire);
        // string → enum (legacy on-disk values still load)
        assert_eq!(
            serde_json::from_str::<DispatchStatus>(wire).unwrap(),
            variant
        );
    }
    // `#[serde(default)]` on the field → a sidecar JSON with no `status`
    // key loads as Pending, matching the old `default_status` fn.
    let no_status = r#"{"dispatch_id":"d1","dispatcher":"a","target":"b"}"#;
    let d: PendingDispatch = serde_json::from_str(no_status).unwrap();
    assert_eq!(d.status, DispatchStatus::Pending);
    // A full sidecar round-trips with the status as a lowercase string.
    let s = serde_json::to_string(&d).unwrap();
    assert!(
        s.contains("\"status\":\"pending\""),
        "status must serialize as the lowercase wire string, got: {s}"
    );
    // An unknown status string fails to deserialize (strict, like the
    // pr_state enums) — list_pending's fail-open loader then skips it.
    assert!(serde_json::from_str::<DispatchStatus>("\"bogus\"").is_err());
}

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
        status: DispatchStatus::Pending,
        nudge_sent_at: None,
        not_working_streak: 0,
        refresh_count: 0,
        long_running_escalated: false,
        reported_at: None,
        quota_escalated: false,
        exceeded_at: None,
    };
    std::fs::write(
        pending_path(home, &id),
        serde_json::to_string_pretty(&payload).unwrap(),
    )
    .unwrap();
    id
}

/// t-dispatchidle-clear-on-report (1): a report clears EVERY sidecar with the
/// matching correlation_id, not just the first — so a duplicate left by a
/// re-dispatch can't survive to nudge after the report.
#[test]
fn mark_resolved_deletes_all_duplicate_sidecars_clearonreport() {
    let home = tmp_home("resolve-all-dups");
    let now = chrono::Utc::now();
    // Two sidecars, SAME correlation_id (the re-dispatch duplicate case).
    let dup_a = write_pending_at(&home, "lead", "dev", Some("t-dup"), "task", 600, now);
    let dup_b = write_pending_at(&home, "lead", "dev", Some("t-dup"), "task", 600, now);
    // An unrelated sidecar that must survive.
    let other = write_pending_at(&home, "lead", "dev", Some("t-other"), "task", 600, now);

    let resolved = mark_resolved(&home, "t-dup", "dev");
    assert!(resolved.is_some(), "must report a deletion");

    let pending = list_pending(&home);
    assert!(
        !pending
            .iter()
            .any(|p| p.dispatch_id == dup_a || p.dispatch_id == dup_b),
        "BOTH duplicate sidecars for t-dup must be deleted"
    );
    assert!(
        pending.iter().any(|p| p.dispatch_id == other),
        "the unrelated correlation's sidecar must survive"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// t-dispatchidle-clear-on-report (2): re-dispatching the SAME task
/// (dispatcher, target, correlation_id) refreshes the existing sidecar in
/// place instead of creating a duplicate.
#[test]
fn record_dispatch_dedups_redispatch_by_key_clearonreport() {
    let home = tmp_home("record-dedup");
    let first = record_dispatch(&home, "lead", "dev", Some("t-redispatch"), "task", 600);
    let second = record_dispatch(&home, "lead", "dev", Some("t-redispatch"), "task", 600);
    assert!(first.is_some() && second.is_some());
    assert_eq!(
        first, second,
        "re-dispatch must REFRESH the same sidecar (same dispatch_id), not create a new one"
    );
    let pending = list_pending(&home);
    let dups = pending
        .iter()
        .filter(|p| p.correlation_id.as_deref() == Some("t-redispatch"))
        .count();
    assert_eq!(
        dups, 1,
        "exactly ONE sidecar for the re-dispatched correlation"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// §3.9 #1916: a task REASSIGN (OwnerAssigned A→B) retargets the dispatch-idle
/// sidecar to B AND resets its idle clock — so the watchdog nudges B (the new
/// owner), not A, and B is not immediately nudged for a task it just received
/// (the #1866 principle: nudge reflects the current owner's idle time).
#[test]
fn reassign_retargets_sidecar_to_new_owner_and_resets_clock_1916() {
    let home = tmp_home("1916-retarget");
    // A's sidecar is already near-threshold (590s of a 600s window).
    let aged = chrono::Utc::now() - chrono::Duration::seconds(590);
    write_pending_at(
        &home,
        "lead",
        "agent-a",
        Some("t-reassign"),
        "task",
        600,
        aged,
    );

    let moved = reassign_pending_for_task(&home, "t-reassign", Some("agent-b"));
    assert_eq!(moved, 1, "exactly one sidecar retargeted");

    let pending = list_pending(&home);
    let s = pending
        .iter()
        .find(|p| p.correlation_id.as_deref() == Some("t-reassign"))
        .expect("#1916: sidecar must SURVIVE a reassign (retargeted, not deleted)");
    assert_eq!(
        s.target, "agent-b",
        "#1916: sidecar must target the reassigned owner B, not the former owner A"
    );
    assert_eq!(
        s.status,
        DispatchStatus::Pending,
        "#1916: revived to Pending so B gets a fresh window"
    );
    assert_eq!(s.not_working_streak, 0, "#1916: debounce streak reset");
    let issued = chrono::DateTime::parse_from_rfc3339(&s.issued_at)
        .expect("issued_at rfc3339")
        .with_timezone(&chrono::Utc);
    assert!(
        chrono::Utc::now()
            .signed_duration_since(issued)
            .num_seconds()
            < 60,
        "#1916: idle clock RESET on reassign — B must not inherit A's near-threshold age (#1866)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// §3.9 #1916: an ORPHAN (OwnerAssigned with owner=None — #1903 disband/delete
/// orphan) CLEARS the sidecar — there is no owner to nudge. It must NOT leave a
/// sidecar with target=None (which would nudge nobody / a placeholder forever).
#[test]
fn reassign_none_clears_orphaned_sidecar_1916() {
    let home = tmp_home("1916-orphan");
    record_dispatch(&home, "lead", "agent-a", Some("t-orphan"), "task", 600)
        .expect("dispatch recorded");

    let cleared = reassign_pending_for_task(&home, "t-orphan", None);
    assert_eq!(cleared, 1, "orphan (owner=None) clears the sidecar");
    assert!(
        list_pending(&home)
            .iter()
            .all(|p| p.correlation_id.as_deref() != Some("t-orphan")),
        "#1916: orphaned task's sidecar must be removed — nobody to nudge (never target=None)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// §3.9 #1866 (b) clear-on-handoff: re-dispatching the SAME (dispatcher,
/// target) to a NEW task (different correlation_id) retires the older still-
/// armed sidecar (the #1861 stale-handoff false-nudge), but must NOT clobber
/// a different dispatcher's parallel dispatch or a newer sidecar.
#[test]
fn record_dispatch_retires_stale_handoff_sidecar_1866() {
    let home = tmp_home("retire-handoff");
    let now = chrono::Utc::now();
    let older = now - chrono::Duration::seconds(700);
    // OLD dispatch (task A) lead→dev — still Pending (the stale handoff).
    let old_a = write_pending_at(&home, "lead", "dev", Some("t-A"), "task", 600, older);
    // A DIFFERENT dispatcher's parallel dispatch to dev — must survive.
    let parallel = write_pending_at(&home, "lead2", "dev", Some("t-A2"), "task", 600, older);
    // A NEWER lead→dev sidecar (issued after the re-dispatch) — must survive
    // (the "strictly older" boundary).
    let newer = write_pending_at(
        &home,
        "lead",
        "dev",
        Some("t-future"),
        "task",
        600,
        now + chrono::Duration::seconds(60),
    );

    // Re-dispatch dev to a NEW task B via the real entry point.
    let new_b = record_dispatch(&home, "lead", "dev", Some("t-B"), "task", 600)
        .expect("new dispatch recorded");

    let ids: Vec<String> = list_pending(&home)
        .into_iter()
        .map(|d| d.dispatch_id)
        .collect();
    assert!(
        !ids.contains(&old_a),
        "#1866 (b): the stale same-(dispatcher,target) older sidecar (task A) must be retired"
    );
    assert!(ids.contains(&new_b), "the new dispatch sidecar must exist");
    assert!(
        ids.contains(&parallel),
        "#1866 (b): a DIFFERENT dispatcher's dispatch must NOT be retired"
    );
    assert!(
        ids.contains(&newer),
        "#1866 (b): a NEWER sidecar must NOT be retired (older-only boundary)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// §3.9 #1866 (a) state-aware: an overdue dispatch whose target has RECENT
/// in-mem activity (heartbeat_at_ms advanced) is SUPPRESSED — past the
/// wall-clock threshold it stays Pending instead of firing.
#[test]
fn scan_suppresses_on_recent_heartbeat_1866() {
    let home = tmp_home("stateaware-hb");
    // Unique target → isolated process-global heartbeat_pair entry.
    let target = "dev-1866-hb";
    let id = write_pending_at(
        &home,
        "lead",
        target,
        Some("t-hb"),
        "task",
        600,
        chrono::Utc::now() - chrono::Duration::seconds(700),
    );
    // Target made MCP activity just now (heads-down inter-agent work).
    crate::daemon::heartbeat_pair::update_with(target, |p| {
        p.heartbeat_at_ms = crate::daemon::heartbeat_pair::now_ms();
    });

    scan_and_emit(&home);

    let d = list_pending(&home)
        .into_iter()
        .find(|d| d.dispatch_id == id)
        .expect("sidecar must still exist (suppressed, not swept)");
    assert_eq!(
        d.status,
        DispatchStatus::Pending,
        "#1866 (a): recent heartbeat must suppress the nudge despite the wall-clock threshold"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// §3.9 #1866 (a) the OTHER half: a fully-idle target (no recent heartbeat /
/// input, no pane activity) past threshold STILL fires — the new signals only
/// ADD suppression for provably-recent activity, never hide a real stuck.
#[test]
fn scan_still_fires_when_target_fully_idle_1866() {
    let home = tmp_home("stateaware-idle");
    let target = "dev-1866-idle"; // unique → stale (0) heartbeat_pair
                                  // Live fleet + task so the sidecar isn't swept as stale before it fires.
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        // #1923 G2: seed the dispatcher (`lead`) too — the new
        // dispatcher-in-fleet stale check requires it (prod always has it).
        format!("instances:\n  lead:\n    backend: claude\n  {target}:\n    backend: claude\n"),
    )
    .unwrap();
    let task_id = "t-idle-99";
    let tasks_dir = home.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join(format!("{task_id}.json")),
        serde_json::to_string_pretty(&serde_json::json!({
            "id": task_id, "status": "in_progress", "title": "w", "assignee": target
        }))
        .unwrap(),
    )
    .unwrap();
    let id = write_pending_at(
        &home,
        "lead",
        target,
        Some(task_id),
        "task",
        600,
        chrono::Utc::now() - chrono::Duration::seconds(700),
    );
    // NO heartbeat / input set → all activity signals stale → truly idle.

    scan_and_emit(&home);

    let d = list_pending(&home)
        .into_iter()
        .find(|d| d.dispatch_id == id)
        .expect("sidecar present");
    assert_eq!(
        d.status,
        DispatchStatus::Exceeded,
        "#1866 (a): a fully-idle target (all activity signals stale) must STILL fire"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #2031: L1 must STAMP `exceeded_at` when it flips a sidecar to `Exceeded`.
/// This is the signal L2's second-window tiering reads — if L1 stopped
/// stamping, L2 would fail-open to an immediate nudge and silently regress the
/// tiering, so pin it explicitly.
#[test]
fn scan_and_emit_stamps_exceeded_at_2031() {
    let home = tmp_home("2031-l1-stamp");
    let target = "dev-2031-stamp";
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!("instances:\n  lead:\n    backend: claude\n  {target}:\n    backend: claude\n"),
    )
    .unwrap();
    let task_id = "t-stamp-2031";
    let tasks_dir = home.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join(format!("{task_id}.json")),
        serde_json::to_string_pretty(&serde_json::json!({
            "id": task_id, "status": "in_progress", "title": "w", "assignee": target
        }))
        .unwrap(),
    )
    .unwrap();
    let id = write_pending_at(
        &home,
        "lead",
        target,
        Some(task_id),
        "task",
        600,
        chrono::Utc::now() - chrono::Duration::seconds(700),
    );
    // No snapshot → debounce fails open → fires on this single scan.
    scan_and_emit(&home);

    let d = list_pending(&home)
        .into_iter()
        .find(|d| d.dispatch_id == id)
        .expect("sidecar present");
    assert_eq!(d.status, DispatchStatus::Exceeded, "precondition: fired");
    assert!(
        d.exceeded_at.is_some(),
        "#2031: L1 must stamp exceeded_at on the Exceeded transition (L2 tiering depends on it)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// §3.9 #2008-p2: below the auto-extension cap, an ACTIVE target's deadline is
/// extended (refresh_count++) with NO alarm of any kind — the existing
/// activity-suppress, now counted toward the cap.
#[test]
fn below_cap_extends_active_target_without_alarm() {
    let home = tmp_home("p2-below-cap");
    let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
    let id = write_pending_at(&home, "lead", "dev", Some("t-below"), "task", 600, issued);
    write_target_snapshot(&home, "dev", "active"); // target_is_working

    scan_and_emit(&home);

    let d = list_pending(&home)
        .into_iter()
        .find(|d| d.dispatch_id == id)
        .expect("sidecar");
    assert_eq!(d.refresh_count, 1, "one activity-based extension counted");
    assert!(!d.long_running_escalated);
    assert_eq!(
        d.status,
        DispatchStatus::Pending,
        "still pending, not fired"
    );
    let elog = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert!(
        !elog.contains("dispatch_idle_long_running")
            && !elog.contains("dispatch_idle_threshold_exceeded"),
        "no alarm of any kind while extending an active target: {elog}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// §3.9 #2008-p2: at the cap, a still-ACTIVE target gets ONE "long-running —
/// confirm expected" escalation (latched) — NOT the stuck/Exceeded alarm, and
/// NOT repeated on the next scan (escalate-don't-repeat).
#[test]
fn cap_reached_escalates_long_running_once_then_latches() {
    let home = tmp_home("p2-cap-escalate");
    let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
    let id = write_pending_at(&home, "lead", "dev", Some("t-cap"), "task", 600, issued);
    for _ in 0..REFRESH_CAP {
        bump_refresh_count(&home, &id); // already AT the extension cap
    }
    write_target_snapshot(&home, "dev", "active"); // still working

    scan_and_emit(&home);

    let d = list_pending(&home)
        .into_iter()
        .find(|d| d.dispatch_id == id)
        .expect("sidecar");
    assert!(
        d.long_running_escalated,
        "cap → the escalate-once latch is set"
    );
    assert_eq!(
        d.status,
        DispatchStatus::Pending,
        "long-running is NOT the stuck/Exceeded path — the target is working"
    );
    let elog = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert_eq!(
        elog.matches("dispatch_idle_long_running").count(),
        1,
        "exactly one long-running escalation: {elog}"
    );
    assert!(
        !elog.contains("dispatch_idle_threshold_exceeded"),
        "no stuck alarm for a working target: {elog}"
    );

    // A second scan must NOT re-escalate (latched).
    scan_and_emit(&home);
    let elog2 = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert_eq!(
        elog2.matches("dispatch_idle_long_running").count(),
        1,
        "escalate-don't-repeat: still exactly one after a second scan: {elog2}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #t-116: a backend quota-wedged target (snapshot `agent_state=="usage_limit"`)
/// must escalate ONCE then latch — never the repeated per-threshold "stuck"
/// nudge that washed r5 (agy quota 6 days, pinged every 30 min). NOT the
/// Exceeded/stuck path (the agent is blocked, not stuck).
#[test]
fn quota_wedged_escalates_once_then_latches_t116() {
    let home = tmp_home("t116-quota-once");
    let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
    let id = write_pending_at(&home, "lead", "dev", Some("t-q"), "task", 600, issued);
    write_target_snapshot(&home, "dev", "usage_limit"); // backend quota hard-block

    scan_and_emit(&home);
    let d = list_pending(&home)
        .into_iter()
        .find(|d| d.dispatch_id == id)
        .expect("sidecar");
    assert!(d.quota_escalated, "quota-wedge → escalate-once latch set");
    assert_eq!(
        d.status,
        DispatchStatus::Pending,
        "quota-wedge is blocked, not stuck — must NOT flip to Exceeded"
    );
    let elog = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert_eq!(
        elog.matches("dispatch_idle_quota_wedged").count(),
        1,
        "exactly one quota escalation: {elog}"
    );
    assert!(
        !elog.contains("dispatch_idle_threshold_exceeded"),
        "no stuck alarm for a quota-wedged target: {elog}"
    );

    // Repeated scans while still quota-wedged: latched → NO re-nudge.
    scan_and_emit(&home);
    scan_and_emit(&home);
    let elog2 = std::fs::read_to_string(home.join("event-log.jsonl")).unwrap_or_default();
    assert_eq!(
        elog2.matches("dispatch_idle_quota_wedged").count(),
        1,
        "fire-once: still exactly one after repeat scans: {elog2}"
    );
    let d = list_pending(&home)
        .into_iter()
        .find(|d| d.dispatch_id == id)
        .expect("sidecar");
    assert_eq!(
        d.status,
        DispatchStatus::Pending,
        "still suppressed (Pending) across repeats, never Exceeded"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// §3.9 #2008-p2 (codex review): a re-dispatch of the SAME correlation is a NEW
/// episode — the in-place refresh must reset BOTH the extension cap counter and
/// the escalation latch, or the reborn dispatch inherits a stale
/// "already long-running, don't protect" state and is silently unguarded.
#[test]
fn redispatch_same_correlation_resets_cap_and_latch() {
    let home = tmp_home("p2-redispatch-reset");
    let id = record_dispatch(&home, "lead", "dev", Some("t-redisp"), "task", 600).expect("first");
    // Drive it to the latched, capped state (as a long-running escalation does).
    for _ in 0..REFRESH_CAP {
        bump_refresh_count(&home, &id);
    }
    set_long_running_escalated(&home, &id);
    let before = list_pending(&home)
        .into_iter()
        .find(|d| d.dispatch_id == id)
        .expect("sidecar");
    assert!(
        before.refresh_count >= REFRESH_CAP && before.long_running_escalated,
        "precondition: capped + latched"
    );

    // Re-dispatch the SAME correlation → in-place refresh.
    let id2 =
        record_dispatch(&home, "lead", "dev", Some("t-redisp"), "task", 600).expect("redispatch");
    assert_eq!(id2, id, "same correlation refreshes in place (one sidecar)");

    let after = list_pending(&home)
        .into_iter()
        .find(|d| d.dispatch_id == id)
        .expect("sidecar");
    assert_eq!(
        after.refresh_count, 0,
        "re-dispatch resets the extension cap counter"
    );
    assert!(
        !after.long_running_escalated,
        "re-dispatch clears the escalation latch (fresh episode is protected again)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// §3.9 #1961 (instrument-only): when a dispatch fires, the `#1961-fire-signals`
/// diagnostic is emitted at the fire point (so production can see which
/// work-aware suppress signal slipped) WHILE the fire behavior is byte-identical
/// (the dispatch still flips to Exceeded). Drops the instrument → the log
/// assertion fails; changes a gate → the Exceeded assertion fails.
#[test]
#[tracing_test::traced_test]
fn fire_signals_instrumented_zero_behavior_1961() {
    let home = tmp_home("1961-instrument");
    let target = "dev-1961-idle"; // unique → stale heartbeat_pair → idle
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!("instances:\n  lead:\n    backend: claude\n  {target}:\n    backend: claude\n"),
    )
    .unwrap();
    let task_id = "t-idle-1961";
    let tasks_dir = home.join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join(format!("{task_id}.json")),
        serde_json::to_string_pretty(&serde_json::json!({
            "id": task_id, "status": "in_progress", "title": "w", "assignee": target
        }))
        .unwrap(),
    )
    .unwrap();
    let id = write_pending_at(
        &home,
        "lead",
        target,
        Some(task_id),
        "task",
        600,
        chrono::Utc::now() - chrono::Duration::seconds(700),
    );

    scan_and_emit(&home);

    // Behavior unchanged: the dispatch still fires (flips to Exceeded).
    let d = list_pending(&home)
        .into_iter()
        .find(|d| d.dispatch_id == id)
        .expect("sidecar present");
    assert_eq!(
        d.status,
        DispatchStatus::Exceeded,
        "#1961: the instrument must NOT change the fire behavior"
    );
    // Instrument live: the fire-signals diagnostic is emitted at the fire point.
    assert!(
        logs_contain("#1961-fire-signals"),
        "#1961: the fire-signals diagnostic must be logged when a dispatch fires"
    );
    std::fs::remove_dir_all(&home).ok();
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
    assert_eq!(p.status, DispatchStatus::Pending);
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
    assert_eq!(
        p.status,
        DispatchStatus::Exceeded,
        "sidecar must flip pending→exceeded"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1658 helper: write a fleet snapshot setting `target`'s agent_state
/// (reuses [`mk_agent_snapshot`]).
fn write_target_snapshot(home: &std::path::Path, target: &str, state: &str) {
    crate::snapshot::save(home, &[mk_agent_snapshot(target, state)]);
}

/// #1658: with a snapshot showing the target NOT working, the signal
/// debounces — it requires DEBOUNCE_SCANS consecutive not-working scans past
/// threshold before firing (filters the #1516 instantaneous gate's
/// false-fire on a brief idle gap during active heads-down work).
#[test]
fn debounce_idle_requires_consecutive_scans_1658() {
    let home = tmp_home("debounce-idle");
    let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
    let id = write_pending_at(&home, "lead", "dev", Some("t-deb"), "task", 600, issued);
    write_target_snapshot(&home, "dev", "idle");

    // The first DEBOUNCE_SCANS-1 scans defer: no event, stays Pending, streak grows.
    for i in 1..DEBOUNCE_SCANS {
        scan_and_emit(&home);
        let p = list_pending(&home)
            .into_iter()
            .find(|p| p.dispatch_id == id)
            .unwrap();
        assert_eq!(p.status, DispatchStatus::Pending, "scan {i}: must defer");
        assert_eq!(p.not_working_streak, i, "scan {i}: streak must grow");
        assert!(
            crate::inbox::drain(&home, "lead").is_empty(),
            "scan {i}: must NOT emit yet"
        );
    }
    // The DEBOUNCE_SCANS-th consecutive not-working scan fires once.
    scan_and_emit(&home);
    let p = list_pending(&home)
        .into_iter()
        .find(|p| p.dispatch_id == id)
        .unwrap();
    assert_eq!(
        p.status,
        DispatchStatus::Exceeded,
        "the DEBOUNCE_SCANS-th idle scan must fire"
    );
    assert!(
        crate::inbox::drain(&home, "lead")
            .iter()
            .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
        "the firing scan must emit the dispatcher event"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1658: observing the target working resets the debounce streak, so a
/// momentary idle blip never accumulates to a false-fire.
#[test]
fn debounce_resets_streak_when_working_1658() {
    let home = tmp_home("debounce-reset");
    let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
    let id = write_pending_at(&home, "lead", "dev", Some("t-rst"), "task", 600, issued);

    // One idle scan → streak 1, deferred.
    write_target_snapshot(&home, "dev", "idle");
    scan_and_emit(&home);
    let p = list_pending(&home)
        .into_iter()
        .find(|p| p.dispatch_id == id)
        .unwrap();
    assert_eq!(p.not_working_streak, 1);
    assert_eq!(p.status, DispatchStatus::Pending);

    // Target resumes working → streak resets to 0, still no fire.
    write_target_snapshot(&home, "dev", "active");
    scan_and_emit(&home);
    let p = list_pending(&home)
        .into_iter()
        .find(|p| p.dispatch_id == id)
        .unwrap();
    assert_eq!(p.not_working_streak, 0, "working must reset the streak");
    assert_eq!(p.status, DispatchStatus::Pending);
    assert!(crate::inbox::drain(&home, "lead").is_empty());
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
    let resolved = mark_resolved(&home, "t-aaa", "dev-1");
    assert_eq!(
        resolved.as_deref(),
        Some(id_a.as_str()),
        "must resolve the correlation_id-matching sidecar, not sender-matching"
    );
    let pending = list_pending(&home);
    // A: the matched sidecar is DELETED on resolve (no longer flipped to
    // Resolved + left behind), so it must be absent from list_pending.
    assert!(
        !pending.iter().any(|p| p.dispatch_id == id_a),
        "matched sidecar must be deleted on resolve"
    );
    let p_b = pending.iter().find(|p| p.dispatch_id == id_b).unwrap();
    assert_eq!(
        p_b.status,
        DispatchStatus::Pending,
        "unmatched sidecar from same dispatcher must NOT be touched"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// A: `cleanup_pending_for_instance` deletes EVERY sidecar for the instance,
/// including non-Pending (Exceeded/Resolved) ones — previously it skipped
/// them, leaving resolved/exceeded sidecars to accumulate.
#[test]
fn cleanup_pending_deletes_non_pending_sidecars() {
    let home = tmp_home("cleanup-non-pending");
    let now = chrono::Utc::now();
    let id = write_pending_at(&home, "lead", "gone-agent", Some("t-x"), "task", 600, now);
    // Flip the sidecar to a terminal (non-Pending) status on disk.
    let path = pending_path(&home, &id);
    let mut pd: PendingDispatch =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    pd.status = DispatchStatus::Exceeded;
    std::fs::write(&path, serde_json::to_string_pretty(&pd).unwrap()).unwrap();

    let removed = cleanup_pending_for_instance(&home, "gone-agent");
    assert_eq!(removed, 1, "must delete the non-Pending (Exceeded) sidecar");
    assert!(!path.exists(), "Exceeded sidecar must be removed");
    std::fs::remove_dir_all(&home).ok();
}

/// codex probe #1 regression: a LATE report on a dispatch that already timed
/// out (Pending → Exceeded, idle nudge fired) must STILL delete the sidecar.
/// Pre-fix `mark_resolved` matched only `Pending`, so the Exceeded sidecar
/// leaked until the slow retention / terminal-sweep path.
#[test]
fn mark_resolved_clears_exceeded_sidecar() {
    let home = tmp_home("resolve-exceeded");
    let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
    let id = write_pending_at(&home, "lead", "dev", Some("t-late"), "task", 600, issued);
    // Flip to Exceeded on disk, as the idle scan would once the threshold passes.
    let path = pending_path(&home, &id);
    let mut pd: PendingDispatch =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    pd.status = DispatchStatus::Exceeded;
    std::fs::write(&path, serde_json::to_string_pretty(&pd).unwrap()).unwrap();

    let resolved = mark_resolved(&home, "t-late", "dev");
    assert_eq!(
        resolved.as_deref(),
        Some(id.as_str()),
        "late report must resolve the already-Exceeded sidecar"
    );
    assert!(
        !path.exists(),
        "late report must delete the Exceeded sidecar (not leak it)"
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
    let resolved = mark_resolved(&home, "t-suppress", "beta");
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
    let id_stale = write_pending_at(&home, "lead", "dev-1", Some("t-stale"), "task", 600, stale);
    let id_fresh = write_pending_at(&home, "lead", "dev-2", Some("t-fresh"), "task", 600, fresh);
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
    assert_eq!(p_stale.status, DispatchStatus::Exceeded);
    assert_eq!(
        p_fresh.status,
        DispatchStatus::Pending,
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
/// the boilerplate `pub(crate) mod team_nudge;` declaration that
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
        if trimmed == "pub(crate) mod team_nudge;" {
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

/// #1923 G2: a pending-dispatch sidecar whose DISPATCHER has left the fleet
/// (deleted / redeployed) is stale — its idle nudge would route to a ghost
/// dispatcher. `stale_sidecar_reason` must flag it `dispatcher_not_in_fleet`
/// (mirroring the existing `target_not_in_fleet` check); a live dispatcher is
/// not flagged.
#[test]
fn stale_sidecar_reason_flags_deleted_dispatcher_1923_g2() {
    let home = tmp_home("g2-dispatcher-stale");
    // fleet has the TARGET (`dev`) but NOT the dispatcher (it was deleted).
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  dev:\n    command: /bin/cat\n",
    )
    .expect("seed fleet.yaml");
    let mk = |dispatcher: &str| PendingDispatch {
        schema_version: SCHEMA_VERSION,
        dispatch_id: "d1".into(),
        dispatcher: dispatcher.into(),
        target: "dev".into(),
        correlation_id: Some("t-realtask".into()),
        expected_kind: "task".into(),
        threshold_secs: 600,
        issued_at: chrono::Utc::now().to_rfc3339(),
        status: DispatchStatus::Pending,
        nudge_sent_at: None,
        not_working_streak: 0,
        refresh_count: 0,
        long_running_escalated: false,
        reported_at: None,
        quota_escalated: false,
        exceeded_at: None,
    };
    assert_eq!(
        stale_sidecar_reason(&home, &mk("ghost-lead")),
        Some("dispatcher_not_in_fleet"),
        "#1923 G2: a sidecar whose dispatcher left the fleet is stale"
    );
    assert_ne!(
        stale_sidecar_reason(&home, &mk("dev")),
        Some("dispatcher_not_in_fleet"),
        "a live dispatcher must NOT be flagged stale"
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
    // #1608b: seed a REAL closed task on the event-sourced board (the path
    // `task_still_live` now reads), not a `tasks/<id>.json` file the board
    // never writes.
    {
        use crate::task_events::{append, DoneSource, InstanceName, TaskEvent, TaskId};
        let emitter = InstanceName::from("test:operator");
        let tid = TaskId(task_id.into());
        append(
            &home,
            &emitter,
            TaskEvent::Created {
                task_id: tid.clone(),
                title: "test".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: Some(InstanceName::from("fixup-dev-2")),
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
        append(
            &home,
            &emitter,
            TaskEvent::Done {
                task_id: tid,
                by: InstanceName::from("fixup-dev-2"),
                source: DoneSource::OperatorManual {
                    authored_at: chrono::Utc::now().to_rfc3339(),
                    result: None,
                },
            },
        )
        .unwrap();
    }
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
        // #1923 G2: seed the DISPATCHER too (not just the target) — the
        // dispatcher-in-fleet stale check now requires it, and in prod the
        // dispatcher is always a live fleet agent.
        "instances:\n  fixup-lead:\n    backend: claude\n  fixup-dev-2:\n    backend: claude\n",
    )
    .unwrap();
    let task_id = "t-live-99";
    // #1608b: seed a REAL LIVE task on the event-sourced board (the path
    // `task_still_live` reads via load_by_id → replay), NOT a
    // `tasks/{id}.json` file — that file is never written, so the old write
    // was ignored and the test only passed via the missing-task fail-open,
    // not because the task was recognized as live. A freshly-Created task is
    // `open`, which is in LIVE_TASK_STATUSES, so `task_still_live` returns
    // Some(true) and the overdue dispatch must still fire.
    {
        use crate::task_events::{append, InstanceName, TaskEvent, TaskId};
        let emitter = InstanceName::from("test:operator");
        append(
            &home,
            &emitter,
            TaskEvent::Created {
                task_id: TaskId(task_id.into()),
                title: "live work".into(),
                description: String::new(),
                priority: "normal".into(),
                owner: Some(InstanceName::from("fixup-dev-2")),
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
    let refreshed = refresh_issued_at(&home, "t-1047-a", "dev");
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
        pending.iter().any(|p| p.status == DispatchStatus::Pending),
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
    let resolved = mark_resolved(&home, "t-1047-c", "dev");
    assert!(resolved.is_some(), "report must resolve sidecar");
    let pending = list_pending(&home);
    assert!(
        !pending
            .iter()
            .any(|p| p.correlation_id.as_deref() == Some("t-1047-c")),
        "kind=report must resolve (delete) the sidecar"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #1516: agent_state gate (don't fire while target is working) ──

/// #1694②: map the state label to a productive-silence value so the
/// pre-existing #1516/#1658 state-based tests keep their intent under the
/// silence-clock gate — `active` = recently productive
/// (`silent_secs` 0 → working), anything else = productive-silent past any
/// window. New silence-specific tests use [`mk_agent_snapshot_silence`].
fn mk_agent_snapshot(name: &str, agent_state: &str) -> crate::snapshot::AgentSnapshot {
    let silent_secs = match agent_state {
        "active" => 0,
        _ => i64::MAX,
    };
    mk_agent_snapshot_silence(name, agent_state, silent_secs)
}

fn mk_agent_snapshot_silence(
    name: &str,
    agent_state: &str,
    silent_secs: i64,
) -> crate::snapshot::AgentSnapshot {
    // #1961 phase-2: pane-change signal FAIL-CLOSED (no recent change) in
    // the legacy fixtures so every pre-existing gate test exercises the
    // original three signals unchanged; pane-delta tests use
    // [`mk_agent_snapshot_pane`].
    mk_agent_snapshot_pane(name, agent_state, silent_secs, i64::MAX)
}

fn mk_agent_snapshot_pane(
    name: &str,
    agent_state: &str,
    silent_secs: i64,
    output_silent_secs: i64,
) -> crate::snapshot::AgentSnapshot {
    crate::snapshot::AgentSnapshot {
        name: name.to_string(),
        backend_command: "opencode".to_string(),
        args: vec![],
        working_dir: None,
        submit_key: "\r".to_string(),
        health_state: "healthy".to_string(),
        agent_state: agent_state.to_string(),
        silent_secs,
        output_silent_secs,
    }
}

/// #1961 phase-2 — THE production false-fire shape: the state-detector
/// mis-reads a code-writing agent as "idle", productive markers missed
/// (silent_secs=MAX), no MCP heartbeat — all three legacy gates slip. The
/// pane CONTENT is changing (token streaming → screen-hash delta), so the
/// classification-free 4th signal must suppress.
#[test]
fn pane_change_suppresses_when_all_state_signals_slip_1961() {
    const T: i64 = 600;
    let snap = crate::snapshot::FleetSnapshot {
        timestamp: "t".to_string(),
        agents: vec![mk_agent_snapshot_pane(
            "misread",
            "idle",
            i64::MAX, // detector says idle, markers missed
            10,       // …but the pane changed 10s ago (streaming)
        )],
    };
    assert!(
        target_is_working(Some(&snap), "misread", T),
        "#1961: a recently-changing pane must suppress even when every \
         classification-based signal reads idle/silent"
    );
}

/// #1961 phase-2 fail-toward-fire: a genuinely idle agent — pane completely
/// static past the window, all other signals idle — must STILL fire (the
/// new signal only ADDS suppression, never blocks a real stuck).
#[test]
fn truly_static_pane_still_fires_1961() {
    const T: i64 = 600;
    let snap = crate::snapshot::FleetSnapshot {
        timestamp: "t".to_string(),
        agents: vec![mk_agent_snapshot_pane(
            "stuck",
            "idle",
            i64::MAX, // not productive
            i64::MAX, // pane has not changed at all
        )],
    };
    assert!(
        !target_is_working(Some(&snap), "stuck", T),
        "#1961: a fully-static pane keeps firing — the pane signal must not \
         hide a real stuck"
    );
    // Old-format snapshot (field missing → serde default MAX) behaves the
    // same: fail-open to firing.
    let legacy = crate::snapshot::FleetSnapshot {
        timestamp: "t".to_string(),
        agents: vec![mk_agent_snapshot_silence("legacy", "idle", i64::MAX)],
    };
    assert!(
        !target_is_working(Some(&legacy), "legacy", T),
        "fail-closed fixture (= old-format default) must not suppress"
    );
}

/// #1694②: the gate reads the productive-SILENCE clock, not the
/// instantaneous thinking/tool_use state. Recently-productive
/// (`silent_secs < threshold`) → working (suppress); productive-silent past
/// the window → not working (fire); active-recovery states are exempt
/// regardless of silence.
#[test]
fn target_is_working_reads_silence_clock_1694() {
    const T: i64 = 600;
    let snap = crate::snapshot::FleetSnapshot {
        timestamp: "t".to_string(),
        agents: vec![
            // recently productive while NOT active → still working
            mk_agent_snapshot_silence("fresh_idle", "idle", 10),
            // #toolu-gap: a long LOCAL active span (9-min Bash) emits no pane
            // marker / MCP heartbeat → silent_secs high, but agent_state is
            // active → WORKING. A hung one is the hang_detector's job
            // (productive_silence_exceeds → Hung at silent>600s).
            mk_agent_snapshot_silence("long_active", "active", 700),
            // active-recovery exempt: ONLY server_rate_limit (bounded retry +
            // #1744 exhaustion backstop) → silent but exempt
            mk_agent_snapshot_silence("rate_limited", "server_rate_limit", 700),
            // api_error is NOT exempt (no exhaustion backstop) → silent = stuck
            mk_agent_snapshot_silence("api_err", "api_error", 700),
        ],
    };
    assert!(
        target_is_working(Some(&snap), "fresh_idle", T),
        "recently productive (silent<threshold) → working even when not thinking"
    );
    assert!(
        target_is_working(Some(&snap), "long_active", T),
        "#toolu-gap: long local active span (silent past window, no pane/heartbeat) \
         → WORKING (instantaneous state); hang_detector owns a genuinely hung one"
    );
    assert!(
        target_is_working(Some(&snap), "rate_limited", T),
        "ServerRateLimit → active-recovery exempt (suppress nudge)"
    );
    assert!(
        !target_is_working(Some(&snap), "api_err", T),
        "ApiError is NOT exempt (no exhaustion backstop) → silent past window fires"
    );
    assert!(
        !target_is_working(Some(&snap), "ghost", T),
        "absent → not working (fire)"
    );
    assert!(
        !target_is_working(None, "fresh_idle", T),
        "no snapshot → not working (fail-open fire)"
    );
}

/// Core #1516 regression: an overdue dispatch whose target is demonstrably
/// WORKING (Thinking/ToolUse) must NOT fire — the timer resets instead.
/// Pre-fix this false-fired (5× the night it landed).
#[test]
fn working_target_does_not_fire_1516() {
    let home = tmp_home("gate-working");
    let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
    let id = write_pending_at(&home, "lead", "gt1516w", Some("t-w"), "task", 600, issued);
    crate::snapshot::save(&home, &[mk_agent_snapshot("gt1516w", "active")]);

    scan_and_emit(&home);

    assert!(
        crate::inbox::drain(&home, "lead").is_empty(),
        "a working (thinking) target must NOT trigger an idle nudge"
    );
    let p = list_pending(&home)
        .into_iter()
        .find(|p| p.dispatch_id == id)
        .unwrap();
    assert_eq!(
        p.status,
        DispatchStatus::Pending,
        "sidecar stays pending (clock refreshed)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Real-stuck still caught: an overdue dispatch whose target is Idle (not
/// working) with no report still fires (Q4 — the gate only suppresses
/// demonstrable progress). #1658: now after the DEBOUNCE_SCANS debounce
/// window (was 1 scan) — the signal is delayed, not lost.
#[test]
fn idle_target_still_fires_1516() {
    let home = tmp_home("gate-idle");
    let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
    let id = write_pending_at(&home, "lead", "gt1516i", Some("t-i"), "task", 600, issued);
    crate::snapshot::save(&home, &[mk_agent_snapshot("gt1516i", "idle")]);

    // #1658: a snapshot present + not-working debounces — fires on the
    // DEBOUNCE_SCANS-th consecutive idle scan, not the first.
    for _ in 0..DEBOUNCE_SCANS {
        scan_and_emit(&home);
    }

    assert!(
        crate::inbox::drain(&home, "lead")
            .iter()
            .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
        "idle + overdue + no report must still fire"
    );
    let p = list_pending(&home)
        .into_iter()
        .find(|p| p.dispatch_id == id)
        .unwrap();
    assert_eq!(p.status, DispatchStatus::Exceeded);
    std::fs::remove_dir_all(&home).ok();
}

/// Graceful degradation: no snapshot at all → fall back to firing (the
/// pre-#1516 behavior; the gate never makes things worse).
#[test]
fn no_snapshot_falls_back_to_firing_1516() {
    let home = tmp_home("gate-nosnap");
    let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
    write_pending_at(&home, "lead", "gt1516n", Some("t-n"), "task", 600, issued);
    // No snapshot.json written.
    scan_and_emit(&home);
    assert!(
        crate::inbox::drain(&home, "lead")
            .iter()
            .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
        "no snapshot → must fall back to firing (no worse than pre-#1516)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1694② de-noise regression: an overdue dispatch whose target is NOT in
/// thinking/tool_use but is recently PRODUCTIVE (low `silent_secs`) must NOT
/// fire. Pre-#1694 the #1516 state gate fired here (state ≠ thinking) — the
/// exact "reminders became noise" complaint, e.g. a dev heads-down for 13 min
/// whose snapshot state isn't thinking at the scan instant but who is plainly
/// producing output.
#[test]
fn productive_but_not_thinking_suppressed_1694() {
    let home = tmp_home("silence-productive");
    let issued = chrono::Utc::now() - chrono::Duration::seconds(800);
    let id = write_pending_at(&home, "lead", "dev", Some("t-p"), "task", 600, issued);
    // Idle state label, but productive output 60s ago (well under the 600s
    // window) → the silence clock says "working".
    crate::snapshot::save(&home, &[mk_agent_snapshot_silence("dev", "idle", 60)]);

    for _ in 0..DEBOUNCE_SCANS + 1 {
        scan_and_emit(&home);
    }

    assert!(
        crate::inbox::drain(&home, "lead").is_empty(),
        "recently-productive target must NOT trigger an idle nudge"
    );
    let p = list_pending(&home)
        .into_iter()
        .find(|p| p.dispatch_id == id)
        .unwrap();
    assert_eq!(
        p.status,
        DispatchStatus::Pending,
        "sidecar stays pending (silence clock refreshed it)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #toolu-gap: a long LOCAL tool_use (e.g. a 9-min `Bash` run) emits no pane
/// marker / MCP heartbeat, so `silent_secs` climbs past the window — but the
/// agent is plainly WORKING (`agent_state=tool_use`). It must NOT fire (the
/// live noise dev-2 hit: `✻ Proofing… 9m`, not stuck, even shipped a PR). A
/// genuinely hung tool_use is the hang_detector's job, not dispatch-idle's.
#[test]
fn long_tool_use_silent_does_not_fire_toolu_gap() {
    let home = tmp_home("silence-tooluse");
    let issued = chrono::Utc::now() - chrono::Duration::seconds(800);
    let id = write_pending_at(&home, "lead", "dev", Some("t-tu"), "task", 600, issued);
    // active AND productive-silent past the window (no pane/heartbeat output).
    crate::snapshot::save(&home, &[mk_agent_snapshot_silence("dev", "active", 700)]);

    for _ in 0..DEBOUNCE_SCANS + 1 {
        scan_and_emit(&home);
    }

    assert!(
        crate::inbox::drain(&home, "lead").is_empty(),
        "long tool_use (silent past window) must NOT fire — it is working, not stuck"
    );
    let p = list_pending(&home)
        .into_iter()
        .find(|p| p.dispatch_id == id)
        .unwrap();
    assert_eq!(
        p.status,
        DispatchStatus::Pending,
        "sidecar stays pending (instantaneous tool_use state suppresses the nudge)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1694② finding #4: an overdue dispatch whose target is in an
/// active-recovery state (ServerRateLimit) must NOT fire even when
/// productive-silent — the auto-retry machinery owns the recovery, so
/// nudging is pure noise (and would re-create the very proxy-drop noise this
/// change removes).
#[test]
fn active_recovery_exempt_does_not_fire_1694() {
    let home = tmp_home("silence-recovery");
    let issued = chrono::Utc::now() - chrono::Duration::seconds(800);
    let id = write_pending_at(&home, "lead", "dev", Some("t-r"), "task", 600, issued);
    // Productive-silent (700s) AND in an auto-recovery state → exempt.
    crate::snapshot::save(
        &home,
        &[mk_agent_snapshot_silence("dev", "server_rate_limit", 700)],
    );

    for _ in 0..DEBOUNCE_SCANS + 1 {
        scan_and_emit(&home);
    }

    assert!(
        crate::inbox::drain(&home, "lead").is_empty(),
        "active-recovery (ServerRateLimit) target must NOT trigger an idle nudge"
    );
    let p = list_pending(&home)
        .into_iter()
        .find(|p| p.dispatch_id == id)
        .unwrap();
    assert_eq!(p.status, DispatchStatus::Pending);
    std::fs::remove_dir_all(&home).ok();
}

/// codex #1775 HIGH: `api_error` is NOT an exempt active-recovery state (it
/// has no retry-exhaustion backstop), so a wedged api_error agent that is
/// productive-silent past the window must still fire — dispatch-idle is its
/// only watchdog (hang_detector misses it: no BlockedReason → IdleLong, not
/// Hung). Contrast with [`active_recovery_exempt_does_not_fire_1694`]
/// (server_rate_limit, which IS exempt).
#[test]
fn stuck_api_error_silent_still_fires_1775() {
    let home = tmp_home("silence-apierror");
    let issued = chrono::Utc::now() - chrono::Duration::seconds(800);
    let id = write_pending_at(&home, "lead", "dev", Some("t-ae"), "task", 600, issued);
    // api_error AND productive-silent (700s > 600s window) → must fire.
    crate::snapshot::save(&home, &[mk_agent_snapshot_silence("dev", "api_error", 700)]);

    for _ in 0..DEBOUNCE_SCANS {
        scan_and_emit(&home);
    }

    assert!(
        crate::inbox::drain(&home, "lead")
            .iter()
            .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
        "stuck api_error (silent past window) must fire — no exhaustion backstop owns it"
    );
    let p = list_pending(&home)
        .into_iter()
        .find(|p| p.dispatch_id == id)
        .unwrap();
    assert_eq!(p.status, DispatchStatus::Exceeded);
    std::fs::remove_dir_all(&home).ok();
}

/// #1694② complement to [`active_recovery_exempt_does_not_fire_1694`]: a
/// genuinely stuck target — productive-silent past the window AND in a
/// non-recovery state — still fires after the debounce (the watchdog must
/// not be neutered by the de-noise change).
#[test]
fn productive_silent_non_recovery_still_fires_1694() {
    let home = tmp_home("silence-stuck");
    let issued = chrono::Utc::now() - chrono::Duration::seconds(800);
    let id = write_pending_at(&home, "lead", "dev", Some("t-s"), "task", 600, issued);
    // Productive-silent (700s > 600s window), ordinary state → not working → fire.
    crate::snapshot::save(&home, &[mk_agent_snapshot_silence("dev", "idle", 700)]);

    for _ in 0..DEBOUNCE_SCANS {
        scan_and_emit(&home);
    }

    assert!(
        crate::inbox::drain(&home, "lead")
            .iter()
            .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
        "productive-silent past window + overdue + no report must still fire"
    );
    let p = list_pending(&home)
        .into_iter()
        .find(|p| p.dispatch_id == id)
        .unwrap();
    assert_eq!(p.status, DispatchStatus::Exceeded);
    std::fs::remove_dir_all(&home).ok();
}

/// #absorb-blocked (the N=3 false-positive replay): a target that is
/// idle/silent (NOT "working") but has declared an ACTIVE `waiting_on`
/// (intentional block/queue, e.g. waiting on a dependency PR) must NOT fire —
/// the sidecar stays Pending, so neither the dispatcher `..._exceeded` event
/// NOR the downstream L2 `..._nudge` to the target is sent.
#[test]
fn blocked_target_with_waiting_on_is_absorbed() {
    let home = tmp_home("absorb-blocked");
    let target = "absorb-blocked-tgt";
    let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
    let id = write_pending_at(&home, "lead", target, Some("t-ab"), "task", 600, issued);
    // Idle + productive-silent past the window (would normally fire) ...
    crate::snapshot::save(&home, &[mk_agent_snapshot_silence(target, "idle", 700)]);
    // ... BUT the target declared an active waiting_on (set_waiting_on).
    crate::daemon::heartbeat_pair::update_with(target, |p| {
        p.waiting_on_since_ms = Some(crate::daemon::heartbeat_pair::now_ms());
    });

    for _ in 0..DEBOUNCE_SCANS + 1 {
        scan_and_emit(&home);
    }

    assert!(
        crate::inbox::drain(&home, "lead").is_empty(),
        "#absorb-blocked: an active-waiting_on target must NOT fire the exceeded event"
    );
    let p = list_pending(&home)
        .into_iter()
        .find(|p| p.dispatch_id == id)
        .unwrap();
    assert_eq!(
        p.status,
        DispatchStatus::Pending,
        "#absorb-blocked: sidecar stays Pending (absorbed) → the L2 target nudge is also suppressed"
    );
    // Global hygiene: clear this name's waiting_on (heartbeat_pair is process-global).
    crate::daemon::heartbeat_pair::update_with(target, |p| {
        p.waiting_on_since_ms = None;
    });
    std::fs::remove_dir_all(&home).ok();
}

/// #absorb-blocked boundary: once the target CLEARS its waiting_on
/// (`set_waiting_on("")` → `waiting_on_since_ms = None`), the absorb releases —
/// a still-overdue, still-silent target fires normally. We must not permanently
/// suppress a genuinely-stuck-after-unblock target.
#[test]
fn cleared_waiting_on_resumes_firing() {
    let home = tmp_home("absorb-cleared");
    let target = "absorb-cleared-tgt";
    let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
    let id = write_pending_at(&home, "lead", target, Some("t-ac"), "task", 600, issued);
    crate::snapshot::save(&home, &[mk_agent_snapshot_silence(target, "idle", 700)]);
    // Cleared (no active waiting_on) — the resume side of the boundary.
    crate::daemon::heartbeat_pair::update_with(target, |p| {
        p.waiting_on_since_ms = None;
    });

    for _ in 0..DEBOUNCE_SCANS {
        scan_and_emit(&home);
    }

    assert!(
        crate::inbox::drain(&home, "lead")
            .iter()
            .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
        "#absorb-blocked: a cleared (no waiting_on) overdue+silent target must still fire"
    );
    let p = list_pending(&home)
        .into_iter()
        .find(|p| p.dispatch_id == id)
        .unwrap();
    assert_eq!(p.status, DispatchStatus::Exceeded);
    std::fs::remove_dir_all(&home).ok();
}

/// #1629 invariant (#1617 lock-while-blocking class): `emit_exceeded_event`
/// (self-IPC via notify_system → loopback api::call) must NEVER run while the
/// #1340 dispatch flock is held. The RMW happens inside the `let to_emit = {
/// ... }` flock block; the emit runs after the block (lock-free). Structural
/// source-scan: brace-match the to_emit block and assert the emit call is NOT
/// inside it and IS after. Needle is `concat`-built and the scan is
/// prod-sliced so this test can't self-satisfy.
#[test]
fn emit_exceeded_not_called_under_flock() {
    let src = include_str!("mod.rs");
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
    let emit_needle = ["emit_exceeded", "_event("].concat();
    let block_body = &prod[block_start..=block_end];
    assert!(
        !block_body.contains(&emit_needle),
        "emit_exceeded_event must NOT run inside the #1340 dispatch flock block (#1617 class)"
    );
    assert!(
        prod[block_end..].contains(&emit_needle),
        "emit_exceeded_event must run AFTER the dispatch flock is dropped"
    );
}

// ── #event-bus pattern #3: emit→subscriber vs legacy parity ──
// No `env_lock` needed: the recipient is `dispatcher` (from the sidecar), not
// an env-derived value, so there is no process-global env race here.

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
    let (dispatch_id, dispatcher, target, expected_kind, corr, elapsed, threshold) = (
        "di-parity",
        "lead",
        "dev",
        "task",
        Some("t-9"),
        900_i64,
        300_i64,
    );

    // Legacy direct delivery (the gate-OFF path).
    let home_legacy = tmp_home("parity-legacy");
    deliver_dispatch_idle(
        &home_legacy,
        dispatch_id,
        dispatcher,
        target,
        expected_kind,
        corr,
        elapsed,
        threshold,
        false,
        false,
    );

    // Bus emit→subscriber delivery (the gate-ON path) — real fan-out.
    let home_bus = tmp_home("parity-bus");
    let bus = crate::daemon::event_bus::EventBus::new();
    bus.subscribe(handle_event);
    bus.emit(
        &home_bus,
        crate::daemon::event_bus::EventKind::DispatchIdleExceeded {
            dispatcher: dispatcher.to_string(),
            target: target.to_string(),
            elapsed_secs: elapsed,
            dispatch_id: dispatch_id.to_string(),
            expected_kind: expected_kind.to_string(),
            threshold_secs: threshold,
            correlation_id: corr.map(String::from),
            long_running: false,
            quota_wedged: false,
        },
    );

    let legacy = drained_payloads(&home_legacy, dispatcher);
    let viabus = drained_payloads(&home_bus, dispatcher);
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

/// #event-bus Step 2 (legacy-zero): `emit_exceeded_event` emits to the global
/// bus; the registered subscriber delivers via `deliver_dispatch_idle` to the
/// event's home (this test's home).
#[test]
fn emit_exceeded_event_delivers_via_bus() {
    let home = tmp_home("via-bus");
    let d = PendingDispatch {
        dispatch_id: "di-gateoff".into(),
        dispatcher: "lead".into(),
        target: "dev".into(),
        expected_kind: "task".into(),
        correlation_id: Some("t-1".into()),
        threshold_secs: 300,
        ..Default::default()
    };
    emit_exceeded_event(&home, &d, 900);
    assert!(
        !drained_payloads(&home, "lead").is_empty(),
        "#event-bus Option A: gate-off must deliver via the legacy path (no regression)"
    );
    std::fs::remove_dir_all(&home).ok();
}
