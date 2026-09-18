//! `app` unit tests, re-homed out of `app/mod.rs` (which sits at the
//! 2500-LOC anti-monolith ceiling — `tests/src_file_size_invariant.rs`) into
//! this sibling `tests.rs` file, exempt by filename. Included from `app/mod.rs`
//! via `#[cfg(test)] mod tests;`, so `use super::*` reaches `app`'s private
//! items exactly as the former inline `mod tests {}` did.
//!
//! `include_str!("mod.rs")` below still resolves (same directory), and the
//! `"\n#[cfg(test)]\nmod tests"` production-cutoff probe keeps working: the
//! `mod.rs` declaration line `mod tests;` still carries that prefix.

use super::*;
use crate::backend::Backend;
use crate::layout::PaneSource;
use crate::vterm::VTerm;

fn daemon_prod_source() -> String {
    let source = std::fs::read_to_string("src/daemon/mod.rs")
        .or_else(|_| std::fs::read_to_string("agend-terminal/src/daemon/mod.rs"))
        .expect("daemon source file must be readable from test cwd");
    let cutoff = source.rfind("\nmod tests {").unwrap_or(source.len());
    source[..cutoff].to_string()
}

#[test]
fn ready_daemon_accepts_live_control_plane_before_agent_spawn_finishes() {
    let home = std::env::temp_dir().join(format!(
        "agend-thin-client-ready-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    let run_dir = home.join("run").join(std::process::id().to_string());
    std::fs::create_dir_all(&run_dir).expect("create run dir");
    crate::daemon::write_daemon_id(&run_dir);
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind probe listener");
    crate::ipc::write_port(
        &run_dir,
        crate::ipc::API_NAME,
        listener.local_addr().expect("listener address").port(),
    )
    .expect("publish api port");
    assert!(
        !run_dir.join(".ready").exists(),
        "agent spawn completion is intentionally still pending"
    );

    assert_eq!(
        wait_for_ready_daemon(&home, std::time::Duration::from_millis(20)).as_deref(),
        Some(run_dir.as_path())
    );
    std::fs::remove_dir_all(&home).expect("remove temp home");
}

#[test]
fn session_restore_waits_for_agent_spawn_completion() {
    let run_dir = std::env::temp_dir().join(format!(
        "agend-thin-client-session-ready-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&run_dir).expect("create run dir");

    assert!(!wait_for_agent_spawn_completion(
        &run_dir,
        std::time::Duration::from_millis(20)
    ));
    std::fs::write(run_dir.join(".ready"), b"ready").expect("write ready marker");
    assert!(wait_for_agent_spawn_completion(
        &run_dir,
        std::time::Duration::from_millis(20)
    ));

    std::fs::remove_dir_all(&run_dir).expect("remove temp run dir");
}

/// #t-84833-10 redraw-storm frame cap — the `should_draw` rate-limit decision.
#[test]
fn should_draw_caps_at_frame_rate() {
    use std::time::{Duration, Instant};
    let fi = Duration::from_millis(33);
    let t0 = Instant::now();
    assert!(should_draw(None, t0, fi), "first frame always draws");
    assert!(
        !should_draw(Some(t0), t0, fi),
        "no draw immediately after a draw"
    );
    assert!(
        !should_draw(Some(t0), t0 + Duration::from_millis(10), fi),
        "10ms < interval → throttled"
    );
    assert!(
        should_draw(Some(t0), t0 + Duration::from_millis(33), fi),
        "interval elapsed → draw"
    );
    assert!(
        should_draw(Some(t0), t0 + Duration::from_millis(500), fi),
        "well past interval → draw"
    );
}

#[test]
fn task_rpc_refresh_clamps_stale_board_row() {
    let mut state = app_state::AppState::new();
    state.ui.overlay = Overlay::Tasks {
        items: Vec::new(),
        col: 0,
        row: 4,
        mode: TaskBoardMode::Board,
        view: BoardView::Tasks,
        pending: true,
        notice: None,
    };
    let task = crate::tasks::Task {
        id: "t-1".into(),
        title: "remaining".into(),
        description: String::new(),
        status: crate::task_events::TaskStatus::Open,
        priority: crate::task_events::TaskPriority::Normal,
        assignee: None,
        routed_to: None,
        depends_on: Vec::new(),
        result: None,
        created_at: "2026-01-01T00:00:00Z".into(),
        created_by: "test".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        due_at: None,
        branch: None,
        started_at: None,
        eta_secs: None,
        auto_release_on_verdict: None,
        tags: Vec::new(),
        parent_id: None,
        metadata: std::collections::BTreeMap::new(),
    };

    state.handle_task_rpc_outcome(Ok(rpc::TaskOutcome::Snapshot(vec![task])));

    let Overlay::Tasks { row, items, .. } = &state.ui.overlay else {
        panic!("task overlay remains open");
    };
    assert_eq!(*row, 0);
    assert_eq!(items.len(), 1);

    state.handle_task_rpc_outcome(Ok(rpc::TaskOutcome::MutationAppliedRefreshFailed {
        error: "refresh offline".into(),
    }));
    let Overlay::Tasks {
        items,
        pending,
        notice,
        ..
    } = &state.ui.overlay
    else {
        panic!("task overlay remains open");
    };
    assert!(!pending);
    assert_eq!(items.len(), 1);
    assert!(notice
        .as_deref()
        .is_some_and(|notice| notice.contains("mutation applied")));
}

/// #t-84833-10: the redraw count under a wakeup flood is bounded by the FRAME
/// RATE, not by the number of wakeups (the storm fix). 1000 wakeups packed into
/// ~100ms must yield only ~3-4 draws (100ms / 33ms), not 1000.
#[test]
fn frame_cap_bounds_draws_under_wakeup_flood() {
    use std::time::{Duration, Instant};
    let fi = Duration::from_millis(33);
    let t0 = Instant::now();
    let mut last_draw: Option<Instant> = None;
    let mut draws = 0u32;
    for i in 0..1000u64 {
        let now = t0 + Duration::from_micros(i * 100); // 1000 wakeups over ~100ms
        if should_draw(last_draw, now, fi) {
            draws += 1;
            last_draw = Some(now);
        }
    }
    assert!(
        draws <= 5,
        "draws must be bounded by frame-rate, got {draws} for 1000 wakeups in 100ms"
    );
    assert!(
        draws >= 3,
        "but should still draw a few times across 100ms, got {draws}"
    );
}

/// #84833-15 R2 perf: the notification-queue disk-scan count under a wakeup flood
/// is bounded by `NOTIF_SYNC_INTERVAL` (≥1s), not by the number of wakeups. A burst
/// of M wakeups inside one <1s window must yield exactly ONE scan (pre-fix = M);
/// crossing the ≥1s boundary admits exactly one more. Deterministic via constructed
/// `Instant`s (no wall-clock timing), threading `last_sync` like the render loop.
#[test]
fn notif_sync_throttle_bounds_disk_scans_per_window() {
    use std::time::{Duration, Instant};
    let interval = Duration::from_secs(1);
    let t0 = Instant::now();

    // First-frame semantics: never-scanned ⇒ scan now (badge correct at startup).
    assert!(
        should_sync_notifications(None, t0, interval),
        "first frame always scans so the startup badge is correct"
    );

    // 200 wakeups packed into ~900ms (one <1s window) → exactly ONE scan.
    let mut last_sync: Option<Instant> = None;
    let mut scans = 0u32;
    for i in 0..200u64 {
        let now = t0 + Duration::from_micros(i * 4500); // ~900ms total, all < 1s
        if should_sync_notifications(last_sync, now, interval) {
            scans += 1;
            last_sync = Some(now);
        }
    }
    assert_eq!(
        scans, 1,
        "a wakeup burst within one <1s window must scan exactly once, got {scans}"
    );

    // Crossing the ≥1s boundary admits exactly one more scan...
    let after = t0 + Duration::from_millis(1000);
    assert!(
        should_sync_notifications(last_sync, after, interval),
        "≥1s since last scan → scan again"
    );
    last_sync = Some(after);
    // ...and the next sub-1s wakeup is throttled again.
    assert!(
        !should_sync_notifications(last_sync, after + Duration::from_millis(500), interval),
        "0.5s after the last scan → throttled"
    );
}

/// #render-first phase-(b) F2 (r4): `bounded_join_attach_workers` must DETACH a
/// worker wedged past the deadline (so quit can't hang) while still joining the
/// finished ones. Deterministic via a parked thread held by a channel.
#[test]
fn bounded_join_detaches_wedged_worker_without_hanging() {
    let (keep_tx, keep_rx) = std::sync::mpsc::channel::<()>();
    // Wedged: blocks until keep_tx drops (held past the join below).
    let wedged = std::thread::spawn(move || {
        let _ = keep_rx.recv();
    });
    let quick = std::thread::spawn(|| {}); // finishes immediately
    let start = std::time::Instant::now();
    let detached = bounded_join_attach_workers(
        vec![quick, wedged],
        start + std::time::Duration::from_millis(150),
    );
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "bounded join must not hang on a wedged worker (took {:?})",
        start.elapsed()
    );
    assert_eq!(
        detached, 1,
        "the wedged worker is detached; the quick one is joined"
    );
    drop(keep_tx); // release the parked thread (cleanup)
}

/// restart-freeze 真嫌#1 (t-…55279) source-scan invariant: `app_teardown`'s
/// Owned-mode cleanup must (1) flip the shutdown flag so PTY-close handlers
/// fast-return, then (2) tear agents down through the shared parallel core
/// `terminate_agents_parallel` — NOT the old SEQUENTIAL per-tab `kill_agent`
/// loop (each blocking ≤5 s on `wait_for_child_exit`, ~6 s of the restart
/// freeze). Regression-proof: revert app_teardown to a `kill_agent` loop and
/// this fails.
#[test]
fn app_teardown_does_not_touch_daemon_owned_agents() {
    let src = include_str!("mod.rs");
    let start = src.find("fn app_teardown(").expect("app_teardown present");
    let after = &src[start..];
    let end = after
        .find("fn is_text_composing_input(")
        .unwrap_or(after.len());
    let body = &after[..end];

    assert!(
        !body.contains("terminate_agents_parallel(")
            && !body.contains("kill_agent(")
            && !body.contains("sync_fleet_yaml("),
        "thin-client teardown must not mutate daemon-owned agents or fleet state"
    );
}

/// #1457 regression guard: submit detection must fire for ALL backends, not
/// just claude. If this regresses to claude-only, non-claude panes never
/// record a submit timestamp → `draft_state` sees `submit=0` → every
/// keystroke looks like a permanent unsent draft → notifications NEVER
/// deliver to them (strictly worse than the bug #1457 fixes).
#[test]
fn submit_detection_fires_for_all_backends() {
    use crate::backend::Backend;
    for b in [
        Backend::ClaudeCode,
        Backend::Codex,
        Backend::KiroCli,
        Backend::OpenCode,
        Backend::Agy,
    ] {
        assert!(
            pane_input_contains_submit(Some(&b), b"hello\r"),
            "submit key must be detected for {b:?}"
        );
        assert!(
            !pane_input_contains_submit(Some(&b), b"hello"),
            "no submit key in plain text for {b:?}"
        );
    }
    // No backend → never a submit (anonymous/unknown pane).
    assert!(!pane_input_contains_submit(None, b"hello\r"));
}

fn tmp_home(suffix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agend-app-phase2-{}-{}",
        suffix,
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// #3389: pin the complete handler set executed by `run_core`. Exact
/// equality catches additions, removals, renames, duplicates, and reorders.
#[test]
fn run_core_pipeline_matches_canonical_handler_set_3389() {
    let names: Vec<&str> =
        crate::daemon::build_default_handlers(Arc::new(std::sync::atomic::AtomicBool::new(false)))
            .iter()
            .map(|h| h.name())
            .collect();

    assert_eq!(
            names,
            [
                "hang_detection",
                "backend_exit_detection",
                "recovery_dispatcher",
                "respawn_watchdog",
                "watchdog",
                "external_liveness",
                "tool_call_provenance",
                "canonical_heartbeat",
                "shadow_observe",
                "snapshot_rotation",
                "check_schedules",
                "schedule_jobs",
                "ci_watch_poll",
                "pr_state_scan",
                "assignment_reconcile",
                "inbox_maintenance",
                "offline_unread_alert",
                "notification_watchdogs",
                "notification_flush",
                "log_rotation",
                "thread_dump",
                "hourly_gc",
                "worktree_registry_sweep",
                "ephemeral_reap",
                "checkout_txn_recover",
                "context_thresholds",
                "codex_mcp_refusal",
                "inject_delivery",
                "claude_self_kick",
                "anti_stall",
                "idle_watchdog",
                "decision_timeout",
                "helper_staleness",
                "mcp_registry",
                "waiting_on_stale",
                "conflict_notify",
                "canonical_drift",
                "auto_release",
                "dispatch_idle",
                "retention",
                // #3666: busy-park redrive scan (registered between Retention
                // and Reclaim — same watch-don't-strand family as
                // DispatchIdle; lifecycle impact reviewed: read-only scan plus
                // idle-gated redelivery, panic-isolated by the outer loop).
                "busy_park_redrive",
                "reclaim_usage_limit",
            ],
            "run_core production handler set changed; update this invariant only after reviewing the lifecycle impact"
        );
}

/// #3389: exercise snapshot rotation through the canonical `run_core` handler
/// pipeline, then verify its operated-state consumers.
#[cfg(unix)]
#[test]
#[serial_test::serial(shadow_observer)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn run_core_snapshot_preserves_operated_state_1720() {
    use crate::daemon::dispatch_idle;
    use crate::daemon::shadow::evidence::{Authority, Confidence};
    use crate::daemon::shadow::reducer::{ObservedState, ObservedStatus};
    use crate::snapshot::{agent_is_busy, agent_state_of, AgentSnapshot};

    struct G(&'static str, Option<String>);
    impl Drop for G {
        fn drop(&mut self) {
            match &self.1 {
                Some(v) => std::env::set_var(self.0, v),
                None => std::env::remove_var(self.0),
            }
        }
    }
    let _g = (
        G(
            "AGEND_SHADOW_OBSERVER",
            std::env::var("AGEND_SHADOW_OBSERVER").ok(),
        ),
        G(
            "AGEND_OBSERVED_DISPATCH",
            std::env::var("AGEND_OBSERVED_DISPATCH").ok(),
        ),
    );
    std::env::set_var("AGEND_SHADOW_OBSERVER", "1");
    std::env::remove_var("AGEND_OBSERVED_DISPATCH"); // default-ON

    let home = std::env::temp_dir().join(format!("agend-pr-b-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();

    // A false-idle agent: raw screen Idle (mk_test_handle default) + a high-confidence
    // Active observed_status (the mid-API false-idle the reducer produces live).
    let id = crate::types::InstanceId::default();
    let handle = crate::agent::mk_test_handle("victim", id);
    handle.core.lock().observed_status = Some(ObservedStatus {
        state: ObservedState::Active,
        authority: Authority::Hook,
        confidence: Confidence::Strong,
        evidence: vec![],
        since_ms: 0,
    });
    let registry: crate::agent::AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::from([
            (id, handle),
        ])));
    let externals: crate::agent::ExternalRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let configs = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let stale = || Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Run snapshot_rotation from the exact pipeline owned by `run_core`.
    let run_core_snapshot = || {
        let ctx = crate::daemon::per_tick::TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        let mut ran = false;
        for h in crate::daemon::build_default_handlers(stale()) {
            if h.name() == "snapshot_rotation" {
                h.run(&ctx);
                ran = true;
            }
        }
        ran
    };

    // The daemon writes snapshot.json with the promoted operated state.
    assert!(
        run_core_snapshot(),
        "run_core pipeline must run snapshot_rotation"
    );
    assert_eq!(
        agent_state_of(&home, "victim").as_deref(),
        Some("active"),
        "run_core wrote snapshot.json with the promoted operated state (false-idle → active)"
    );
    assert!(
        agent_is_busy(&home, "victim"),
        "(b) the shared busy-gate (inbox/handoff/reply) reads the false-idle agent as BUSY"
    );

    // (b) dispatch_idle: drive the real scan_and_emit on a past-threshold pending dispatch.
    // Isolate the agent_state gate from the silence gate by holding silence > threshold, so
    // ONLY agent_state decides suppress-vs-fire.
    let save_snap = |state: &str| {
        crate::snapshot::save(
            &home,
            &[AgentSnapshot {
                name: "victim".to_string(),
                backend_command: "claude".to_string(),
                args: vec![],
                working_dir: None,
                submit_key: "\r".to_string(),
                health_state: "healthy".to_string(),
                agent_state: state.to_string(),
                silent_secs: 9_999,
                output_silent_secs: 9_999,
            }],
        );
    };
    let make_overdue = |corr: &str| -> String {
        let did = dispatch_idle::record_dispatch(&home, "lead", "victim", Some(corr), "task", 60)
            .expect("recorded pending dispatch");
        let p = dispatch_idle::pending_path(&home, &did);
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        v["issued_at"] =
            serde_json::json!((chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339());
        v["status"] = serde_json::json!("pending");
        v["not_working_streak"] = serde_json::json!(0);
        crate::store::atomic_write(&p, serde_json::to_string(&v).unwrap().as_bytes()).unwrap();
        did
    };
    let status_of = |did: &str| -> String {
        dispatch_idle::list_pending(&home)
            .into_iter()
            .find(|d| d.dispatch_id == *did)
            .map(|d| format!("{:?}", d.status))
            .unwrap_or_else(|| "DELETED".into())
    };

    // Promoted "active" → SUPPRESSED across DEBOUNCE+margin scans (never mis-fires).
    save_snap("active");
    let did_ok = make_overdue("corr-suppress");
    for _ in 0..5 {
        dispatch_idle::scan_and_emit(&home);
    }
    assert_eq!(
        status_of(&did_ok),
        "Pending",
        "(b) dispatch_idle SUPPRESSED on the promoted false-idle — did NOT mis-fire"
    );

    // Contrast: a stale/raw "idle" snapshot (the pre-#1720-fix behaviour) → FIRES (Exceeded).
    save_snap("idle");
    let did_fire = make_overdue("corr-fire");
    for _ in 0..5 {
        dispatch_idle::scan_and_emit(&home);
    }
    assert_eq!(
        status_of(&did_fire),
        "Exceeded",
        "(b-baseline) a stale 'idle' snapshot MIS-FIRES (the bug PR-B fixes for the live daemon)"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// The daemon is the sole TaskSweep owner after app becomes a thin client.
#[test]
fn run_app_wires_task_sweep_owner_only() {
    let source = daemon_prod_source();
    assert_eq!(
        source
            .matches("crate::daemon::task_sweep::TaskSweep::spawn(")
            .count(),
        1,
        "daemon production code must own exactly one TaskSweep"
    );
}

#[test]
fn production_closed_daemon_event_receiver_is_disabled() {
    let (closed_tx, mut daemon_event_rx) = crossbeam_channel::bounded::<rpc::EventStreamOutcome>(1);
    drop(closed_tx);
    assert!(daemon_event_rx.recv().is_err());

    let disabled_event_rx = crossbeam_channel::never::<rpc::EventStreamOutcome>();
    disable_closed_daemon_event_receiver(&mut daemon_event_rx, &disabled_event_rx);
    assert!(matches!(
        daemon_event_rx.try_recv(),
        Err(crossbeam_channel::TryRecvError::Empty)
    ));
}

/// The daemon owns the shadow socket after app becomes a thin client.
#[test]
fn run_app_wires_shadow_socket_server_2413() {
    let source = daemon_prod_source();
    assert!(
        source.contains("crate::daemon::shadow::start("),
        "daemon run_core must own the shadow socket after app becomes a thin client"
    );
}

/// The canonical daemon handlers must degrade cleanly with empty registries.
#[test]
fn run_core_handlers_no_panic_on_empty_context() {
    let home = tmp_home("tick-empty-ctx");
    let registry: crate::agent::AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let externals: crate::agent::ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
    let configs: crate::api::ConfigRegistry = Arc::new(Mutex::new(HashMap::new()));
    let ctx = crate::daemon::per_tick::TickContext {
        home: &home,
        registry: &registry,
        externals: &externals,
        configs: &configs,
    };
    for h in
        crate::daemon::build_default_handlers(Arc::new(std::sync::atomic::AtomicBool::new(false)))
    {
        h.run(&ctx); // panic here = test failure
    }
    std::fs::remove_dir_all(&home).ok();
}

fn pane(name: &str) -> Pane {
    Pane {
        agent_name: name.into(),
        instance_id: crate::types::InstanceId::default(),
        instance_ref: None,
        vterm: VTerm::new(10, 10),
        rx: crossbeam_channel::bounded(1).1,
        id: 1,
        backend: None,
        working_dir: None,
        display_name: None,
        restart_error: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: None,
        last_input_at: None,
        pending_notification_count: 0,
        pending_decision_count: 0,
        selection: None,
        source: PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    }
}

/// #2967 RED: `sync_notification_state` must perform exactly ONE
/// queue-directory `read_dir` per pass, across ALL panes/tabs — not one
/// per pane (the shape the #84833-15 comment above `NOTIF_SYNC_INTERVAL`
/// already documents). Pre-fix (`pending_count` called per pane, each
/// doing its own `list_draining_files` `read_dir`) this fails with N
/// (here 5, spread across 3 tabs); post-fix
/// (`QueueDirSnapshot::scan` once, `pending_count` reading from the
/// snapshot) it is exactly 1. Every agent is idle (nothing queued) so no
/// content read is exercised here — this test is purely about the
/// `read_dir` count.
#[test]
fn sync_notification_state_performs_exactly_one_dir_scan_2967() {
    let home = tmp_home("sync-notif-dirscan");
    let mut layout = Layout::new();
    // 5 panes across 3 tabs, each a distinct agent — mirrors a real
    // multi-tab fleet session.
    layout
        .tabs
        .push(crate::layout::Tab::new("tab0".into(), pane("agent0")));
    layout.tabs[0].split_focused(crate::layout::SplitDir::Horizontal, pane("agent1"));
    layout
        .tabs
        .push(crate::layout::Tab::new("tab1".into(), pane("agent2")));
    layout.tabs[1].split_focused(crate::layout::SplitDir::Horizontal, pane("agent3"));
    layout
        .tabs
        .push(crate::layout::Tab::new("tab2".into(), pane("agent4")));

    notification_queue::reset_scan_counters();
    sync_notification_state(&home, &mut layout);

    assert_eq!(
        notification_queue::dir_scan_count(),
        1,
        "a sync_notification_state pass over 5 panes must perform exactly ONE \
             queue-directory read_dir, not one per pane"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #982 RC wiring-pin: assert `flush_idle_notifications` invokes
/// the submit-aware injector (`inject_notification_with_submit`)
/// so queued hints get the backend `submit_key` applied on flush.
///
/// Implemented as a file-level source pin — the raw
/// `inject_notification` was deleted in this PR, so the negative
/// half of the invariant is compile-time enforced. The positive
/// half (this assertion) is platform-agnostic and survives
/// rustfmt re-wrapping. Companion test:
/// `inbox::tests::t15_composing_flush_uses_submit_aware_inject`
/// pins the JSON payload contract end-to-end.
#[test]
fn flush_idle_notifications_wired_to_submit_aware_inject() {
    let source = std::fs::read_to_string("src/daemon/per_tick/notification_flush.rs")
        .or_else(|_| {
            std::fs::read_to_string("agend-terminal/src/daemon/per_tick/notification_flush.rs")
        })
        .expect("source file must be readable from test cwd");
    assert!(
        source.contains("inject_notification_with_submit("),
        "daemon notification flush must use the submit-aware injector"
    );
}

// ── #1944: buffer-aware draft gate (the operator-facing fix) ──

/// Build a pane whose live `vterm` renders `screen` with `backend`. The term
/// is `VTerm::new(cols, rows)` — wide enough that input lines don't wrap, and
/// few enough rows that the content stays within `DRAFT_INPUT_TAIL_ROWS`.
fn pane_with_screen(name: &str, backend: Option<Backend>, screen: &str) -> Pane {
    let mut p = pane(name);
    p.backend = backend;
    p.vterm = crate::vterm::VTerm::new(80, 6);
    p.vterm.process(screen.as_bytes());
    p
}

/// Set up a recent unsent draft (typed_ms > submit_ms → `Drafting`) and one
/// queued notification for `agent` under `home`.
fn seed_drafting_with_queued(home: &Path, agent: &str) {
    seed_stale_draft(home, agent);
    notification_queue::enqueue(home, agent, "[AGEND-MSG-PENDING] peer report")
        .expect("enqueue test notification");
}

/// Set up a recent unsent draft WITHOUT any queued notification (the
/// restart-gate shape: `:restart` consults metadata, not the queue).
fn seed_stale_draft(home: &Path, agent: &str) {
    let now = chrono::Utc::now().timestamp_millis();
    crate::agent_ops::save_metadata(
        home,
        agent,
        "last_input_epoch_ms",
        serde_json::json!(now - 30_000),
    );
    crate::agent_ops::save_metadata(
        home,
        agent,
        "last_submit_epoch_ms",
        serde_json::json!(now - 60_000),
    );
}

/// #1944 §3.9: a stale type-then-clear draft (typed_ms > submit_ms but the
/// input box is EMPTY) must DELIVER — the old timestamp-only gate held it.
#[test]
fn draft_gate_delivers_when_input_box_empty() {
    let home = tmp_home("draftgate-empty");
    seed_drafting_with_queued(&home, "lead");
    // claude pane, input box empty (`❯ ` with nothing typed).
    let mut p = pane_with_screen("lead", Some(Backend::ClaudeCode), "❯ ");
    p.pending_notification_count = notification_queue::pending_count(&home, "lead");

    let mut injected: Vec<String> = Vec::new();
    flush_notifications_for_pane(&home, &mut p, |t, _channel_origin| {
        injected.push(t.to_string());
        Ok(())
    });
    assert_eq!(
        injected.len(),
        1,
        "empty input box → the stale-draft message must be delivered, not held"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1944 §3.9: a REAL live draft (text in the input box) must still DEFER —
/// the draft-protection invariant is unchanged.
#[test]
fn draft_gate_defers_when_input_box_has_text() {
    let home = tmp_home("draftgate-typed");
    seed_drafting_with_queued(&home, "lead");
    let mut p = pane_with_screen("lead", Some(Backend::ClaudeCode), "❯ half-typed reply");
    p.pending_notification_count = notification_queue::pending_count(&home, "lead");

    let mut injected: Vec<String> = Vec::new();
    flush_notifications_for_pane(&home, &mut p, |t, _channel_origin| {
        injected.push(t.to_string());
        Ok(())
    });
    assert!(
        injected.is_empty(),
        "a real draft (text in the box) must keep deferring (protection unchanged)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1944 §3.9: a backend with no prompt marker (Shell) → buffer-emptiness is
/// undeterminable → fall back to the timestamp behavior (defer), fail toward
/// draft-protection. Same outcome for a claude pane mid-output (no prompt in
/// the tail) — covered by `input_box_none_when_marker_absent`.
#[test]
fn draft_gate_falls_back_to_timestamp_for_markerless_backend() {
    let home = tmp_home("draftgate-shell");
    seed_drafting_with_queued(&home, "lead");
    // Shell has no input_prompt_marker → None → keep the raw Drafting defer.
    let mut p = pane_with_screen("lead", Some(Backend::Shell), "$ ");
    p.pending_notification_count = notification_queue::pending_count(&home, "lead");

    let mut injected: Vec<String> = Vec::new();
    flush_notifications_for_pane(&home, &mut p, |t, _channel_origin| {
        injected.push(t.to_string());
        Ok(())
    });
    assert!(
        injected.is_empty(),
        "markerless backend → fail toward protection (timestamp-only defer)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1948 v2 §3.9: kiro has no prompt marker but its empty box shows a
/// placeholder — a cleared kiro pane (placeholder visible) must DELIVER.
#[test]
fn draft_gate_delivers_for_kiro_when_placeholder_visible() {
    let home = tmp_home("draftgate-kiro-empty");
    seed_drafting_with_queued(&home, "lead");
    // cleared kiro box: the real placeholder is visible (no typed content).
    let mut p = pane_with_screen(
        "lead",
        Some(Backend::KiroCli),
        "Kiro auto\n\n ask a question or describe a task ↵\n /copy",
    );
    p.pending_notification_count = notification_queue::pending_count(&home, "lead");

    let mut injected: Vec<String> = Vec::new();
    flush_notifications_for_pane(&home, &mut p, |t, _channel_origin| {
        injected.push(t.to_string());
        Ok(())
    });
    assert_eq!(
        injected.len(),
        1,
        "kiro cleared (placeholder visible) → stale draft delivered, not held"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1948 v2 §3.9: a kiro pane with a real draft (placeholder replaced by typed
/// text) must still DEFER — protection unchanged.
#[test]
fn draft_gate_defers_for_kiro_when_typed() {
    let home = tmp_home("draftgate-kiro-typed");
    seed_drafting_with_queued(&home, "lead");
    let mut p = pane_with_screen("lead", Some(Backend::KiroCli), "Kiro auto\n\n half typed\n");
    p.pending_notification_count = notification_queue::pending_count(&home, "lead");

    let mut injected: Vec<String> = Vec::new();
    flush_notifications_for_pane(&home, &mut p, |t, _channel_origin| {
        injected.push(t.to_string());
        Ok(())
    });
    assert!(
        injected.is_empty(),
        "kiro with text (placeholder gone) → keep deferring (protection unchanged)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1948(b) §3.9: codex's empty box shows DIM ghost text after `›` (SGR 2) —
/// the dim-aware path must DELIVER (the v1 plain-marker path mis-read the ghost
/// as typed content and held). The vterm processes the real SGR so the dim
/// flag is set exactly as codex emits it.
#[test]
fn draft_gate_delivers_for_codex_when_ghost_is_dim() {
    let home = tmp_home("draftgate-codex-ghost");
    seed_drafting_with_queued(&home, "lead");
    // `ESC[1m›` (bold prompt) + `ESC[2m…` (dim ghost) — codex's real encoding.
    let screen = "\u{1b}[1m›\u{1b}[22m\u{1b}[2m Use /skills to list available skills\u{1b}[0m";
    let mut p = pane_with_screen("lead", Some(Backend::Codex), screen);
    p.pending_notification_count = notification_queue::pending_count(&home, "lead");

    let mut injected: Vec<String> = Vec::new();
    flush_notifications_for_pane(&home, &mut p, |t, _channel_origin| {
        injected.push(t.to_string());
        Ok(())
    });
    assert_eq!(
        injected.len(),
        1,
        "codex empty box (dim ghost after ›) → stale draft delivered, not held"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #1948(b) §3.9: a real codex draft (normal-intensity text after `›`) must
/// still DEFER — the dim signal must not false-deliver on a real draft.
#[test]
fn draft_gate_defers_for_codex_when_input_normal_intensity() {
    let home = tmp_home("draftgate-codex-typed");
    seed_drafting_with_queued(&home, "lead");
    // `ESC[1m›` then NORMAL intensity input (no SGR 2).
    let screen = "\u{1b}[1m›\u{1b}[22m my actual draft reply\u{1b}[0m";
    let mut p = pane_with_screen("lead", Some(Backend::Codex), screen);
    p.pending_notification_count = notification_queue::pending_count(&home, "lead");

    let mut injected: Vec<String> = Vec::new();
    flush_notifications_for_pane(&home, &mut p, |t, _channel_origin| {
        injected.push(t.to_string());
        Ok(())
    });
    assert!(
        injected.is_empty(),
        "codex with normal-intensity input → keep deferring (protection unchanged)"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ── #3663: TUI publishes the empty-box observation for daemon-side gates ──

/// A cleared box with ZERO queued notifications (the `:restart` shape —
/// the gate reads metadata, not the queue) must still publish the cleared
/// observation, so the daemon-side `draft_state` flips to `None`.
#[test]
fn cleared_observation_published_with_empty_queue_3663() {
    let home = tmp_home("cleared-publish-empty");
    seed_stale_draft(&home, "lead");
    let mut p = pane_with_screen("lead", Some(Backend::ClaudeCode), "❯ ");
    p.pending_notification_count = 0;

    flush_notifications_for_pane(&home, &mut p, |_t, _channel_origin| {
        panic!("nothing queued — injector must not run");
    });
    notification_queue::flush_pending_input_activity(&home);
    assert_eq!(
        notification_queue::draft_state(&home, "lead"),
        notification_queue::DraftState::None,
        "#3663: daemon-side draft_state must read None after the TUI publish"
    );
    assert!(
        !crate::inbox::notify::operator_has_live_draft(&home, "lead"),
        "#3663: operator_has_live_draft must clear (no 1.5s typing here)"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// A REAL draft (text in the box) must NOT publish a cleared observation —
/// the daemon-side gate must keep deferring.
#[test]
fn cleared_observation_not_published_for_live_draft_3663() {
    let home = tmp_home("cleared-publish-typed");
    seed_stale_draft(&home, "lead");
    let mut p = pane_with_screen("lead", Some(Backend::ClaudeCode), "❯ half-typed reply");
    p.pending_notification_count = 0;

    flush_notifications_for_pane(&home, &mut p, |_t, _channel_origin| Ok(()));
    notification_queue::flush_pending_input_activity(&home);
    assert_eq!(
        notification_queue::draft_state(&home, "lead"),
        notification_queue::DraftState::Drafting,
        "#3663: a live draft must keep Drafting daemon-side (#1457 preserved)"
    );
    assert!(
        crate::inbox::notify::operator_has_live_draft(&home, "lead"),
        "a live draft must keep operator_has_live_draft true"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn input_prompt_marker_only_for_verified_backends() {
    assert_eq!(Backend::ClaudeCode.input_prompt_marker(), Some("❯"));
    assert_eq!(Backend::Agy.input_prompt_marker(), Some(">"));
    // #1948 codex follow-up: codex is NOT marker-covered — its empty box shows
    // a rotating ghost phrase after `›`, which the PLAIN marker probe mis-reads
    // as typed content. #1948(b): codex is instead covered via the DIM-aware
    // path (`input_dim_ghost_marker`) — the ghost is dim, real input is normal.
    assert_eq!(Backend::Codex.input_prompt_marker(), None);
    assert_eq!(Backend::Codex.input_empty_placeholder(), None);
    assert_eq!(Backend::Codex.input_dim_ghost_marker(), Some("›"));
    assert_eq!(Backend::Shell.input_prompt_marker(), None);
    assert_eq!(Backend::OpenCode.input_prompt_marker(), None);
    // #1948 v2: kiro covered via placeholder, NOT a marker; opencode stays
    // fully fallback (no marker, no placeholder).
    assert_eq!(Backend::KiroCli.input_prompt_marker(), None);
    assert_eq!(
        Backend::KiroCli.input_empty_placeholder(),
        Some("ask a question or describe a task")
    );
    assert_eq!(Backend::OpenCode.input_empty_placeholder(), None);
    assert_eq!(Backend::ClaudeCode.input_empty_placeholder(), None);
    // dim-ghost is codex-only: the marker-backends and kiro are NOT dim-aware.
    assert_eq!(Backend::ClaudeCode.input_dim_ghost_marker(), None);
    assert_eq!(Backend::Agy.input_dim_ghost_marker(), None);
    assert_eq!(Backend::KiroCli.input_dim_ghost_marker(), None);
}

/// The daemon owns event-bus subscriber registration after app becomes a
/// thin client.
#[test]
fn run_app_registers_event_bus_subscribers() {
    let source = daemon_prod_source();
    assert!(
        source.contains("register_event_subscribers(&ctx.registry)"),
        "daemon run_core must register event subscribers after app becomes a thin client"
    );
}

#[test]
fn activity_flush_is_wired_to_both_producers_and_render_error_teardown_3321() {
    let source = std::fs::read_to_string("src/app/mod.rs")
        .or_else(|_| std::fs::read_to_string("agend-terminal/src/app/mod.rs"))
        .expect("source file must be readable from test cwd");
    let prod = &source[..source
        .find("\n#[cfg(test)]\nmod tests")
        .unwrap_or(source.len())];
    for function in ["fn write_to_focused", "fn write_to_pane("] {
        let start = prod.find(function).expect("activity producer must exist");
        let tail = &prod[start..];
        let end = tail.find("\nfn ").unwrap_or(tail.len());
        let body = &tail[..end];
        assert!(
            body.contains("record_input_activity("),
            "{function} must record composing input"
        );
        assert!(
            body.contains("record_submit_activity("),
            "{function} must record submit activity"
        );
    }
    let render = prod
        .find("if let Err(error) = state.render_frame(terminal, &deps)")
        .expect("render errors must be captured before teardown");
    let teardown_flush = prod
        .find("notification_queue::flush_pending_activity_at_teardown(home);")
        .expect("teardown must flush the remaining activity pair");
    assert!(
        teardown_flush > render,
        "render errors must reach teardown before they are returned"
    );
    assert!(prod.contains("let loop_result: Result<()> = loop"));
    assert!(prod.contains("loop_result?;"));
}

/// #3321 behavioral call-site pin: the real app teardown must persist the
/// pending pair, not merely leave a source-text reference to a helper that
/// some alternate teardown path could call.
#[test]
fn app_teardown_invokes_activity_flush_3321() {
    let home = tmp_home("activity-app-teardown-callsite-3321");
    let layout = Layout::new();
    notification_queue::record_input_activity(&home, "agent1");
    notification_queue::record_submit_activity(&home, "agent1");

    app_teardown(&home, &layout, Vec::new(), Vec::new());

    let (input_ms, submit_ms) = notification_queue::read_input_submit_timestamps(&home, "agent1");
    assert!(
        input_ms > 0,
        "app teardown must persist pending input activity"
    );
    assert!(
        submit_ms > 0,
        "app teardown must persist pending submit activity"
    );
    assert_eq!(
        notification_queue::pending_input_count_for(&home),
        0,
        "app teardown must drain the pending pair"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn flush_drains_queue_on_idle() {
    let home = tmp_home("flush");
    let mut pane = pane("agent1");
    notification_queue::enqueue(&home, "agent1", "queued").expect("queue notification");
    pane.pending_notification_count = notification_queue::pending_count(&home, "agent1");
    let mut flushed = Vec::new();
    flush_notifications_for_pane(&home, &mut pane, |text, _channel_origin| {
        flushed.push(text.to_string());
        Ok(())
    });
    assert_eq!(flushed, vec!["queued".to_string()]);
    assert_eq!(pane.pending_notification_count, 0);
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn flush_respects_disk_compose_state_for_fresh_pane() {
    let home = tmp_home("flush-compose-disk");
    let mut pane = pane("agent1");
    notification_queue::record_input_activity(&home, "agent1");
    notification_queue::flush_pending_input_activity(&home);
    notification_queue::enqueue(&home, "agent1", "queued").expect("queue notification");
    pane.pending_notification_count = notification_queue::pending_count(&home, "agent1");

    let mut flushed = Vec::new();
    flush_notifications_for_pane(&home, &mut pane, |text, _channel_origin| {
        flushed.push(text.to_string());
        Ok(())
    });

    assert!(
        flushed.is_empty(),
        "fresh pane must respect disk compose state"
    );
    assert_eq!(pane.pending_notification_count, 1);
    std::fs::remove_dir_all(home).ok();
}

// -----------------------------------------------------------------------
// #1762: draft detection — only text-composing input marks a draft
// -----------------------------------------------------------------------

/// #1762: navigation / control keys + lone whitespace are NOT text-composing
/// (they must not defer actionable injects), while real character input,
/// UTF-8, and bracketed paste ARE (so #1675 still protects a live draft).
/// Byte forms mirror `tui::key_to_bytes`.
#[test]
fn is_text_composing_input_excludes_nav_control_whitespace_1762() {
    // Navigation / control (ESC-prefixed) → NOT composing.
    for seq in [
        &b"\x1b[A"[..], // Up
        b"\x1b[B",      // Down
        b"\x1b[C",      // Right
        b"\x1b[D",      // Left
        b"\x1b[H",      // Home
        b"\x1b[F",      // End
        b"\x1b[5~",     // PageUp
        b"\x1b[6~",     // PageDown
        b"\x1b[3~",     // Delete
        b"\x1b[Z",      // Shift+Tab (BackTab)
        b"\x1bOP",      // F1
        b"\x1b",        // Esc
        b"\x1ba",       // Alt+a
    ] {
        assert!(
            !is_text_composing_input(seq),
            "ESC-seq {seq:?} must NOT be text-composing"
        );
    }
    // Bare control bytes → NOT composing.
    assert!(!is_text_composing_input(&[0x01])); // Ctrl+A
    assert!(!is_text_composing_input(b"\t")); // Tab
    assert!(!is_text_composing_input(&[0x7f])); // Backspace (DEL)
    assert!(!is_text_composing_input(b"\r")); // Enter (submit — counted separately)
    assert!(!is_text_composing_input(b"\n")); // Shift+Enter
                                              // Lone whitespace → NOT composing (#1762 fat-fingered space).
    assert!(!is_text_composing_input(b" "));
    assert!(!is_text_composing_input(b"   "));
    assert!(!is_text_composing_input(&[])); // empty

    // Real character input → IS composing.
    assert!(is_text_composing_input(b"a"));
    assert!(is_text_composing_input(b"hello"));
    assert!(is_text_composing_input(b"hi there")); // space among text still composing
    assert!(is_text_composing_input("café".as_bytes())); // UTF-8
    assert!(is_text_composing_input("日本語".as_bytes())); // multibyte
                                                           // Bracketed paste wraps PASTED TEXT → composing.
    assert!(is_text_composing_input(b"\x1b[200~pasted\x1b[201~"));
}

/// #1762 behavioral contract: exercising the exact gate `write_to_focused`
/// applies (`if is_text_composing_input(bytes) { record_input_activity }`),
/// a navigation key leaves the pane Clean (actionable injects NOT deferred),
/// while real typing marks it Drafting (#1675 still protects a live draft).
/// (`write_to_focused` itself needs a PTY-backed Layout; the wiring is the
/// 3-line gate, exercised here against the real predicate + draft_state.)
#[test]
fn nav_key_does_not_defer_but_typing_does_1762() {
    let home = tmp_home("1762-behavior");
    let agent = "agent1";

    // (a) operator browses history with Up while idle → gate skips → no draft.
    let up = b"\x1b[A";
    if is_text_composing_input(up) {
        notification_queue::record_input_activity(&home, agent);
        notification_queue::flush_pending_input_activity(&home);
    }
    assert_eq!(
        notification_queue::draft_state(&home, agent),
        notification_queue::DraftState::None,
        "#1762: a nav key must NOT mark a draft → actionable notif not deferred"
    );

    // (b) operator types real text → gate records → draft present (deferred).
    if is_text_composing_input(b"hello") {
        notification_queue::record_input_activity(&home, agent);
        notification_queue::flush_pending_input_activity(&home);
    }
    assert_eq!(
        notification_queue::draft_state(&home, agent),
        notification_queue::DraftState::Drafting,
        "#1762: real typing still marks a draft (#1675 preserved)"
    );
    std::fs::remove_dir_all(home).ok();
}

// -----------------------------------------------------------------------
// Regression pins: app mode tick consumers (t-20260423022134)
// -----------------------------------------------------------------------

#[test]
fn app_mode_fires_one_shot_schedule() {
    // Write a one-shot schedule with past run_at directly to disk,
    // call check_schedules, verify it fires (auto-disabled).
    let home = tmp_home("sched-fire");
    let past = (chrono::Utc::now() - chrono::Duration::seconds(2)).to_rfc3339();
    let store_json = serde_json::json!({
        "schema_version": 2,
        "schedules": [{
            "id": "s-test-oneshot",
            "message": "ping",
            "target": "nonexistent-agent",
            "trigger": {"kind": "once", "at": past},
            "enabled": true,
            "timezone": "UTC",
            "label": "test-oneshot",
            "created_at": chrono::Utc::now().to_rfc3339(),
            "updated_at": chrono::Utc::now().to_rfc3339(),
            "run_history": []
        }]
    });
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::write(
        home.join("schedules.json"),
        serde_json::to_string_pretty(&store_json).expect("serialize"),
    )
    .expect("write schedule file");

    // Fire the tick — schedule is past due, should trigger.
    crate::daemon::cron_tick::check_schedules(&home);

    // Verify: schedule should now be disabled (one-shot auto-disable).
    let store = crate::schedules::load(&home);
    let sched = store.schedules.iter().find(|s| s.id == "s-test-oneshot");
    assert!(
        sched.is_some_and(|s| !s.enabled),
        "one-shot schedule must be auto-disabled after firing"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn app_mode_health_decay_runs() {
    // Verify health.maybe_decay() is callable on an agent handle —
    // binding test that the tick consumer code path compiles and
    // exercises the health decay method.
    use crate::health::HealthTracker;
    let mut health = HealthTracker::new();
    // maybe_decay on a fresh tracker should not panic or change state.
    health.maybe_decay(true);
    assert_eq!(
        health.state.display_name(),
        "healthy",
        "fresh tracker should remain healthy after decay tick"
    );
}
