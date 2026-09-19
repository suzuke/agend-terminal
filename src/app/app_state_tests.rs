#![cfg(test)]

use super::*;
use crate::layout::{PaneSource, Tab};
use std::collections::HashSet;

#[path = "app_state/tests/event_stream_tests.rs"]
mod event_stream_tests;
#[path = "app_state/tests/retirement_tests.rs"]
mod retirement_tests;
#[path = "app_state/tests/appstate_team_order_tests.rs"]
mod team_order_tests;

fn closed_before_attach_registry_survives(unmanaged: bool) -> bool {
    let home = std::env::temp_dir().join(format!(
        "app_state_late_attach_{}_{}_{}",
        std::process::id(),
        unmanaged,
        crate::types::InstanceId::new()
    ));
    std::fs::create_dir_all(&home).expect("create temp home");
    let fleet_path = home.join("fleet.yaml");
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let instance_id = crate::types::InstanceId::new();
    registry.lock().insert(
        instance_id,
        crate::agent::mk_test_handle("late-shell", instance_id),
    );

    let mut state = AppState::new();
    let pane_id = 42;
    let (fwd_tx, _fwd_rx) = crossbeam_channel::unbounded();
    state.pending_fwd.insert(pane_id, fwd_tx);

    let (wakeup_tx, _wakeup_rx) = crossbeam_channel::unbounded();
    let app_restart_gate = crate::api::app_restart::AppRestartGate::new();
    let daemon_binary_stale = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let attached_run_dir = None;
    let (task_rpc_tx, _task_rpc_rx) = crossbeam_channel::unbounded::<rpc::TaskRequest>();
    let (remote_state_rpc_tx, _remote_state_rpc_rx) =
        crossbeam_channel::unbounded::<rpc::AgentStateRequest>();
    let (remote_restart_request_tx, _remote_restart_request_rx) =
        crossbeam_channel::unbounded::<commands::RemoteRestartRequest>();
    let (remote_restart_worker_tx, _remote_restart_worker_rx) =
        crossbeam_channel::unbounded::<commands::RemoteRestartRequest>();
    let deps = AppDeps {
        home: &home,
        fleet_path: &fleet_path,
        registry: &registry,
        wakeup_tx: &wakeup_tx,
        app_restart_gate: &app_restart_gate,
        daemon_binary_stale: &daemon_binary_stale,
        telegram_status: TelegramStatus::NotConfigured,
        attached_run_dir: &attached_run_dir,
        attached_mode: false,
        size_debug: false,
        task_rpc_tx: &task_rpc_tx,
        remote_state_rpc_tx: &remote_state_rpc_tx,
        remote_restart_request_tx: &remote_restart_request_tx,
        remote_restart_worker_tx: &remote_restart_worker_tx,
    };
    let (_sub_tx, sub_rx) = crossbeam_channel::unbounded();
    let mut reap_workers = Vec::new();
    state.handle_attach_outcome(
        Ok(pane_factory::AttachOutcome::Ready {
            pane_id,
            instance_id,
            instance_ref: None,
            unmanaged,
            rx: sub_rx,
            dump: Vec::new(),
            work_dir: home.clone(),
        }),
        &deps,
        &mut reap_workers,
    );

    let survives = registry.lock().contains_key(&instance_id);
    if let Some(handle) = crate::agent::remove_and_unregister(&registry, &instance_id) {
        crate::daemon::terminate_agents_parallel(vec![("late-shell".to_string(), handle.child)]);
    }
    std::fs::remove_dir_all(home).ok();
    survives
}

fn test_remote_pane(layout: &mut Layout, agent: &str) -> anyhow::Result<Pane> {
    let (_tx, rx) = crossbeam_channel::unbounded();
    Ok(Pane {
        agent_name: agent.into(),
        instance_id: crate::types::InstanceId::default(),
        instance_ref: None,
        vterm: crate::vterm::VTerm::new(10, 10),
        rx,
        id: layout.next_pane_id(),
        backend: None,
        working_dir: None,
        display_name: None,
        restart_error: None,
        scroll_offset: 0,
        has_notification: false,
        fleet_instance_name: Some(agent.into()),
        last_input_at: None,
        pending_notification_count: 0,
        pending_decision_count: 0,
        selection: None,
        source: PaneSource::Local,
        offthread: None,
        _fwd_cancel: None,
    })
}

#[test]
fn retained_disconnected_agent_is_reconnect_candidate() {
    let current = HashSet::from(["returning".to_string(), "steady".to_string()]);
    let known = current.clone();

    assert_eq!(
        remote_attach_candidates(&current, &known, |name| name == "returning"),
        vec!["returning"]
    );
}

#[test]
fn only_live_snapshot_reconciles_both_add_and_gone_sets() {
    let current = HashSet::from(["steady".to_string(), "new".to_string()]);
    let known = HashSet::from(["steady".to_string(), "gone".to_string()]);

    let live =
        super::reconcile_remote_roster_names(&current, &known, crate::runtime::AgentListMode::Live);
    assert_eq!(live.to_add, HashSet::from(["new".to_string()]));
    assert_eq!(live.gone, HashSet::from(["gone".to_string()]));

    for mode in [
        crate::runtime::AgentListMode::FallbackDaemonStuck,
        crate::runtime::AgentListMode::FallbackDaemonAbsent,
    ] {
        let fallback = super::reconcile_remote_roster_names(&current, &known, mode);
        assert!(fallback.to_add.is_empty());
        assert!(fallback.gone.is_empty());
    }
}

#[test]
fn agent_state_outcome_updates_mode_and_queues_only_live_names() {
    let mut state = AppState::new();
    state.handle_agent_state_rpc_outcome(Ok(Ok(rpc::AgentStateSnapshotResult {
        snapshot: HashMap::new(),
        names: HashSet::from(["live-agent".to_string()]),
        instance_refs: HashMap::new(),
        mode: crate::runtime::AgentListMode::Live,
    })));
    assert_eq!(state.daemon_list_mode, crate::runtime::AgentListMode::Live);
    assert_eq!(
        state.pending_remote_roster_names,
        Some(HashSet::from(["live-agent".to_string()]))
    );

    state.handle_agent_state_rpc_outcome(Ok(Err(rpc::AgentStateError {
        error: "daemon stuck".into(),
        mode: crate::runtime::AgentListMode::FallbackDaemonStuck,
    })));
    assert_eq!(
        state.daemon_list_mode,
        crate::runtime::AgentListMode::FallbackDaemonStuck
    );
    assert!(state.pending_remote_roster_names.is_none());
}

#[test]
fn fallback_agent_state_snapshot_updates_mode_without_queueing_roster() {
    let mut state = AppState::new();
    state.handle_agent_state_rpc_outcome(Ok(Ok(rpc::AgentStateSnapshotResult {
        snapshot: HashMap::new(),
        names: HashSet::from(["stale-agent".to_string()]),
        instance_refs: HashMap::new(),
        mode: crate::runtime::AgentListMode::FallbackDaemonStuck,
    })));

    assert_eq!(
        state.daemon_list_mode,
        crate::runtime::AgentListMode::FallbackDaemonStuck
    );
    assert!(state.pending_remote_roster_names.is_none());
}

#[test]
fn restart_state_has_bounded_active_correlation_without_broad_delete_buffer_3649() {
    let source = include_str!("app_state.rs");
    let source = &source[..source.rfind("#[cfg(test)]").unwrap_or(source.len())];
    assert!(
        source.contains("REMOTE_RESTART_PENDING_TTL"),
        "pending restart state must have an explicit bounded lifetime"
    );
    assert!(
        source.contains("REMOTE_RESTART_CAPACITY"),
        "active restart correlation must have an explicit capacity"
    );
    let removed_name = ["REMOTE_RESTART_DELETE_BUFFER", "_TTL"].concat();
    assert!(
        !source.contains(&removed_name),
        "uncorrelated deletes must not be retained in a broad TTL buffer"
    );
}

#[test]
fn expired_remote_restart_correlation_is_reaped_3649() {
    let mut state = AppState::new();
    let old_instance_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 1);
    let request = commands::RemoteRestartRequest {
        restart_id: "expired-restart".into(),
        old_instance_ref: Some(old_instance_ref),
        tab_index: 0,
        pane_id: 0,
        name: "agent".into(),
    };
    state.remote_restarts.insert(
        request.restart_id.clone(),
        RemoteRestartPending {
            request,
            successor_instance_ref: None,
            conflicted: false,
            provisional: false,
            created_at: std::time::Instant::now()
                .checked_sub(REMOTE_RESTART_PENDING_TTL + std::time::Duration::from_secs(1))
                .expect("test instant remains representable"),
        },
    );

    state.reap_remote_restart_state();

    assert!(state.remote_restarts.is_empty());
}

#[test]
fn ordinary_instance_delete_retires_immediately_3649() {
    let home = std::env::temp_dir().join(format!(
        "remote-restart-ordering-{}",
        crate::types::InstanceId::new()
    ));
    let fleet_path = home.join("fleet.yaml");
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (wakeup_tx, _wakeup_rx) = crossbeam_channel::unbounded();
    let app_restart_gate = crate::api::app_restart::AppRestartGate::new();
    let daemon_binary_stale = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let attached_run_dir = None;
    let (task_rpc_tx, _task_rpc_rx) = crossbeam_channel::unbounded::<rpc::TaskRequest>();
    let (remote_state_rpc_tx, _remote_state_rpc_rx) =
        crossbeam_channel::unbounded::<rpc::AgentStateRequest>();
    let (remote_restart_request_tx, _remote_restart_request_rx) =
        crossbeam_channel::unbounded::<commands::RemoteRestartRequest>();
    let (remote_restart_worker_tx, remote_restart_worker_rx) =
        crossbeam_channel::unbounded::<commands::RemoteRestartRequest>();
    let deps = AppDeps {
        home: &home,
        fleet_path: &fleet_path,
        registry: &registry,
        wakeup_tx: &wakeup_tx,
        app_restart_gate: &app_restart_gate,
        daemon_binary_stale: &daemon_binary_stale,
        telegram_status: TelegramStatus::NotConfigured,
        attached_run_dir: &attached_run_dir,
        attached_mode: false,
        size_debug: false,
        task_rpc_tx: &task_rpc_tx,
        remote_state_rpc_tx: &remote_state_rpc_tx,
        remote_restart_request_tx: &remote_restart_request_tx,
        remote_restart_worker_tx: &remote_restart_worker_tx,
    };
    let old_instance_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 2);
    let mut state = AppState::new();
    let mut pane = test_remote_pane(&mut state.ui.layout, "agent").expect("test pane");
    pane.instance_ref = Some(old_instance_ref);
    state.ui.layout.add_tab(Tab::new("agent".into(), pane));
    state.handle_event_stream_outcome(
        Ok(rpc::EventStreamOutcome::Event(
            crate::daemon::event_hub::DaemonEvent {
                source: "daemon".into(),
                sequence: 1,
                event: crate::api::ApiEvent::InstanceDeleted {
                    name: "agent".into(),
                    instance_ref: Some(old_instance_ref),
                    restart_id: None,
                },
            },
        )),
        &deps,
    );
    assert!(
        state.ui.layout.find_agent_pane("agent").is_none(),
        "an uncorrelated delete must not be delayed behind restart TTL"
    );

    state.handle_remote_restart_request(
        commands::RemoteRestartRequest {
            restart_id: "restart-ordering".into(),
            old_instance_ref: Some(old_instance_ref),
            tab_index: 0,
            pane_id: 0,
            name: "agent".into(),
        },
        &deps,
    );

    assert!(state.remote_restarts.contains_key("restart-ordering"));
    assert!(state.ui.layout.find_agent_pane("agent").is_none());
    state.handle_remote_restart_request(
        commands::RemoteRestartRequest {
            restart_id: "restart-ordering".into(),
            old_instance_ref: Some(old_instance_ref),
            tab_index: 0,
            pane_id: 0,
            name: "agent".into(),
        },
        &deps,
    );
    assert_eq!(
        state.remote_restarts.len(),
        1,
        "duplicate must be idempotent"
    );

    let conflicting_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 3);
    state.handle_remote_restart_request(
        commands::RemoteRestartRequest {
            restart_id: "restart-ordering".into(),
            old_instance_ref: Some(conflicting_ref),
            tab_index: 9,
            pane_id: 9,
            name: "different-agent".into(),
        },
        &deps,
    );
    assert_eq!(state.remote_restarts.len(), 1, "conflict must be ignored");
    drop(remote_restart_worker_rx);
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn registered_remote_restart_retains_pane_when_delete_arrives_3649() {
    let home = team_fixture_home("remote-restart-registered");
    let fleet_path = home.join("fleet.yaml");
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (wakeup_tx, _wakeup_rx) = crossbeam_channel::unbounded();
    let app_restart_gate = crate::api::app_restart::AppRestartGate::new();
    let daemon_binary_stale = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let attached_run_dir = None;
    let (task_rpc_tx, _task_rpc_rx) = crossbeam_channel::unbounded::<rpc::TaskRequest>();
    let (remote_state_rpc_tx, _remote_state_rpc_rx) =
        crossbeam_channel::unbounded::<rpc::AgentStateRequest>();
    let (remote_restart_request_tx, _remote_restart_request_rx) =
        crossbeam_channel::unbounded::<commands::RemoteRestartRequest>();
    let (remote_restart_worker_tx, remote_restart_worker_rx) =
        crossbeam_channel::unbounded::<commands::RemoteRestartRequest>();
    let deps = AppDeps {
        home: &home,
        fleet_path: &fleet_path,
        registry: &registry,
        wakeup_tx: &wakeup_tx,
        app_restart_gate: &app_restart_gate,
        daemon_binary_stale: &daemon_binary_stale,
        telegram_status: TelegramStatus::NotConfigured,
        attached_run_dir: &attached_run_dir,
        attached_mode: false,
        size_debug: false,
        task_rpc_tx: &task_rpc_tx,
        remote_state_rpc_tx: &remote_state_rpc_tx,
        remote_restart_request_tx: &remote_restart_request_tx,
        remote_restart_worker_tx: &remote_restart_worker_tx,
    };
    let old_instance_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 4);
    let mut state = AppState::new();
    let mut pane = test_remote_pane(&mut state.ui.layout, "agent").expect("test pane");
    pane.instance_ref = Some(old_instance_ref);
    state.ui.layout.add_tab(Tab::new("agent".into(), pane));

    state.handle_remote_restart_request(
        commands::RemoteRestartRequest {
            restart_id: "registered-restart".into(),
            old_instance_ref: Some(old_instance_ref),
            tab_index: 0,
            pane_id: 0,
            name: "agent".into(),
        },
        &deps,
    );
    state.handle_event_stream_outcome(
        Ok(rpc::EventStreamOutcome::Event(
            crate::daemon::event_hub::DaemonEvent {
                source: "daemon".into(),
                sequence: 1,
                event: crate::api::ApiEvent::InstanceDeleted {
                    name: "agent".into(),
                    instance_ref: Some(old_instance_ref),
                    restart_id: None,
                },
            },
        )),
        &deps,
    );

    assert!(state.remote_restarts.contains_key("registered-restart"));
    assert!(
        state.ui.layout.find_agent_pane("agent").is_some(),
        "registered correlation must retain the pane for exact successor replacement"
    );
    assert!(remote_restart_worker_rx.try_recv().is_ok());
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn daemon_restart_delete_then_create_retains_exact_pane_3670() {
    let home = team_fixture_home("remote-restart-daemon-red");
    let fleet_path = home.join("fleet.yaml");
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (wakeup_tx, _wakeup_rx) = crossbeam_channel::unbounded();
    let app_restart_gate = crate::api::app_restart::AppRestartGate::new();
    let daemon_binary_stale = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let attached_run_dir = None;
    let (task_rpc_tx, _task_rpc_rx) = crossbeam_channel::unbounded::<rpc::TaskRequest>();
    let (remote_state_rpc_tx, _remote_state_rpc_rx) =
        crossbeam_channel::unbounded::<rpc::AgentStateRequest>();
    let (remote_restart_request_tx, _remote_restart_request_rx) =
        crossbeam_channel::unbounded::<commands::RemoteRestartRequest>();
    let (remote_restart_worker_tx, _remote_restart_worker_rx) =
        crossbeam_channel::unbounded::<commands::RemoteRestartRequest>();
    let deps = AppDeps {
        home: &home,
        fleet_path: &fleet_path,
        registry: &registry,
        wakeup_tx: &wakeup_tx,
        app_restart_gate: &app_restart_gate,
        daemon_binary_stale: &daemon_binary_stale,
        telegram_status: TelegramStatus::NotConfigured,
        attached_run_dir: &attached_run_dir,
        attached_mode: false,
        size_debug: false,
        task_rpc_tx: &task_rpc_tx,
        remote_state_rpc_tx: &remote_state_rpc_tx,
        remote_restart_request_tx: &remote_restart_request_tx,
        remote_restart_worker_tx: &remote_restart_worker_tx,
    };
    let old_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 3670);
    let successor_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 3671);
    let mut state = AppState::new();
    let mut pane = test_remote_pane(&mut state.ui.layout, "daemon-agent").expect("test pane");
    pane.instance_ref = Some(old_ref);
    pane.set_restart_error("stale failure");
    state.ui.layout.add_tab(Tab::new("team".into(), pane));

    state.handle_event_stream_outcome(
        Ok(rpc::EventStreamOutcome::Event(
            crate::daemon::event_hub::DaemonEvent {
                source: "daemon".into(),
                sequence: 1,
                event: crate::api::ApiEvent::InstanceDeleted {
                    name: "daemon-agent".into(),
                    instance_ref: Some(old_ref),
                    restart_id: Some("daemon-restart-3670".into()),
                },
            },
        )),
        &deps,
    );

    // Direct MCP/watchdog restart has no TUI pre-registration. The delete
    // leg must therefore retain the exact pane until the successor arrives.
    assert!(
        state.ui.layout.find_agent_pane("daemon-agent").is_some(),
        "daemon restart delete must retain the pane for correlated successor replacement"
    );

    state.handle_event_stream_outcome(
        Ok(rpc::EventStreamOutcome::Event(
            crate::daemon::event_hub::DaemonEvent {
                source: "daemon".into(),
                sequence: 2,
                event: crate::api::ApiEvent::InstanceCreated {
                    name: "daemon-agent".into(),
                    instance_ref: Some(successor_ref),
                    restart_id: Some("daemon-restart-3670".into()),
                    old_instance_ref: Some(old_ref),
                    layout: crate::api::LayoutHint::Tab,
                    spawner: None,
                    target_pane: None,
                },
            },
        )),
        &deps,
    );
    let mut successor =
        test_remote_pane(&mut state.ui.layout, "daemon-agent").expect("successor pane");
    successor.instance_ref = Some(successor_ref);
    assert!(
        state.place_correlated_remote_pane(successor).is_none(),
        "correlated successor must replace the retained pane in place"
    );
    assert_eq!(state.ui.layout.tabs.len(), 1);
    assert_eq!(state.ui.layout.tabs[0].root().pane_count(), 1);
    assert_eq!(state.ui.layout.tabs[0].root().pane_ids(), vec![0]);
    assert_eq!(
        state
            .ui
            .layout
            .find_agent_pane("daemon-agent")
            .and_then(|(_, pane_id)| state.ui.layout.tabs[0].root().find_pane(pane_id))
            .and_then(crate::layout::Pane::restart_error),
        None,
        "successful replacement must clear transient restart failure text"
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn correlated_restart_failure_marks_exact_remote_pane_disconnected_3670() {
    let home = team_fixture_home("remote-restart-failure-3670");
    let fleet_path = home.join("fleet.yaml");
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (wakeup_tx, _wakeup_rx) = crossbeam_channel::unbounded();
    let app_restart_gate = crate::api::app_restart::AppRestartGate::new();
    let daemon_binary_stale = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let attached_run_dir = None;
    let (task_rpc_tx, _task_rpc_rx) = crossbeam_channel::unbounded::<rpc::TaskRequest>();
    let (remote_state_rpc_tx, _remote_state_rpc_rx) =
        crossbeam_channel::unbounded::<rpc::AgentStateRequest>();
    let (remote_restart_request_tx, _remote_restart_request_rx) =
        crossbeam_channel::unbounded::<commands::RemoteRestartRequest>();
    let (remote_restart_worker_tx, _remote_restart_worker_rx) =
        crossbeam_channel::unbounded::<commands::RemoteRestartRequest>();
    let deps = AppDeps {
        home: &home,
        fleet_path: &fleet_path,
        registry: &registry,
        wakeup_tx: &wakeup_tx,
        app_restart_gate: &app_restart_gate,
        daemon_binary_stale: &daemon_binary_stale,
        telegram_status: TelegramStatus::NotConfigured,
        attached_run_dir: &attached_run_dir,
        attached_mode: false,
        size_debug: false,
        task_rpc_tx: &task_rpc_tx,
        remote_state_rpc_tx: &remote_state_rpc_tx,
        remote_restart_request_tx: &remote_restart_request_tx,
        remote_restart_worker_tx: &remote_restart_worker_tx,
    };
    let old_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 3670);
    let (listener, client_stream) = {
        let listener = std::net::TcpListener::bind((crate::ipc::LOOPBACK, 0)).expect("listener");
        let client_stream =
            std::net::TcpStream::connect(listener.local_addr().expect("listener address"))
                .expect("client stream");
        (listener, client_stream)
    };
    let (server_stream, _) = listener.accept().expect("server stream");
    let client = crate::bridge_client::BridgeClient::from_stream_for_test(client_stream);
    let mut state = AppState::new();
    let mut pane = test_remote_pane(&mut state.ui.layout, "failed-agent").expect("test pane");
    pane.instance_ref = Some(old_ref);
    pane.source = PaneSource::Remote(
        Arc::new(Mutex::new(client)),
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
    );
    state
        .ui
        .layout
        .add_tab(Tab::new("failed-agent".into(), pane));

    let mut event = |sequence, event| {
        state.handle_event_stream_outcome(
            Ok(rpc::EventStreamOutcome::Event(
                crate::daemon::event_hub::DaemonEvent {
                    source: "daemon".into(),
                    sequence,
                    event,
                },
            )),
            &deps,
        );
    };
    event(
        1,
        crate::api::ApiEvent::InstanceDeleted {
            name: "failed-agent".into(),
            instance_ref: Some(old_ref),
            restart_id: Some("restart-failure-3670".into()),
        },
    );
    event(
        2,
        crate::api::ApiEvent::InstanceRestartFailed {
            name: "failed-agent".into(),
            restart_id: "restart-failure-3670".into(),
            old_instance_ref: Some(old_ref),
            error: "spawn failed".into(),
        },
    );

    assert!(!state.remote_restarts.contains_key("restart-failure-3670"));
    assert!(state.ui.layout.agent_pane_is_disconnected("failed-agent"));
    let (_, pane_id) = state
        .ui
        .layout
        .find_agent_pane("failed-agent")
        .expect("failed pane");
    assert_eq!(
        state.ui.layout.tabs[0]
            .root()
            .find_pane(pane_id)
            .and_then(crate::layout::Pane::restart_error),
        Some("spawn failed")
    );
    drop(server_stream);
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn provisional_restart_quota_is_bounded_and_expiry_retains_pane_3670() {
    let mut state = AppState::new();
    let mut refs = Vec::new();
    for index in 0..(REMOTE_RESTART_PROVISIONAL_CAPACITY + 1) {
        let name = format!("provisional-{index}");
        let old_ref =
            crate::types::InstanceRef::new(crate::types::InstanceId::new(), (index + 1) as u64);
        let mut pane = test_remote_pane(&mut state.ui.layout, &name).expect("test pane");
        pane.instance_ref = Some(old_ref);
        state.ui.layout.add_tab(Tab::new(name.clone(), pane));
        refs.push((name, old_ref));
    }
    let listener = std::net::TcpListener::bind((crate::ipc::LOOPBACK, 0)).expect("listener");
    let client_stream =
        std::net::TcpStream::connect(listener.local_addr().expect("listener address"))
            .expect("client stream");
    let (server_stream, _) = listener.accept().expect("server stream");
    let client = crate::bridge_client::BridgeClient::from_stream_for_test(client_stream);
    state.ui.layout.tabs[0]
        .root_mut()
        .find_pane_mut(0)
        .expect("first pane")
        .source = PaneSource::Remote(
        Arc::new(Mutex::new(client)),
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
    );
    for (index, (name, old_ref)) in refs.iter().enumerate() {
        assert_eq!(
            state.register_provisional_remote_restart(
                name,
                &format!("provisional-restart-{index}"),
                *old_ref,
            ),
            index < REMOTE_RESTART_PROVISIONAL_CAPACITY
        );
    }
    assert_eq!(
        state
            .remote_restarts
            .values()
            .filter(|pending| pending.provisional)
            .count(),
        REMOTE_RESTART_PROVISIONAL_CAPACITY
    );
    state
        .remote_restarts
        .get_mut("provisional-restart-0")
        .expect("provisional entry")
        .created_at = std::time::Instant::now()
        .checked_sub(REMOTE_RESTART_PENDING_TTL + std::time::Duration::from_secs(1))
        .expect("instant arithmetic");
    state.reap_remote_restart_state();
    assert!(state.remote_restarts.len() < REMOTE_RESTART_PROVISIONAL_CAPACITY);
    assert!(
        state.ui.layout.find_agent_pane("provisional-0").is_some(),
        "provisional expiry must retain the stale pane for disconnected rendering"
    );
    assert!(
        state.ui.layout.agent_pane_is_disconnected("provisional-0"),
        "provisional expiry must mark the retained pane disconnected"
    );
    drop(server_stream);
}

#[test]
fn provisional_restart_duplicate_is_idempotent_and_conflict_fails_closed_3670() {
    let mut state = AppState::new();
    let old_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 1);
    let mut pane = test_remote_pane(&mut state.ui.layout, "provisional-agent").expect("pane");
    pane.instance_ref = Some(old_ref);
    state.ui.layout.add_tab(Tab::new("team".into(), pane));

    assert!(state.register_provisional_remote_restart(
        "provisional-agent",
        "same-restart",
        old_ref
    ));
    assert!(state.register_provisional_remote_restart(
        "provisional-agent",
        "same-restart",
        old_ref
    ));
    assert!(!state.register_provisional_remote_restart(
        "other-agent",
        "same-restart",
        crate::types::InstanceRef::new(crate::types::InstanceId::new(), 2)
    ));
    assert_eq!(state.remote_restarts.len(), 1);
    assert!(
        state.remote_restarts["same-restart"].conflicted,
        "conflicting identity must be marked unusable"
    );
}

#[test]
fn same_name_remote_replacement_cannot_overwrite_retained_pane_3625() {
    let home = team_fixture_home("identity-replacement");
    let mut state = AppState::new();
    let old_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 101);
    let new_ref = crate::types::InstanceRef::new(crate::types::InstanceId::new(), 102);
    state
        .remote_instance_refs
        .insert("solo".to_string(), old_ref);
    let mut pane_builder = |name: &str, layout: &mut Layout| test_remote_pane(layout, name);
    state.place_remote_team_grouped(&["solo".to_string()], &home, &mut pane_builder);
    state
        .remote_instance_refs
        .insert("solo".to_string(), new_ref);
    state.place_remote_team_grouped(&["solo".to_string()], &home, &mut pane_builder);

    assert_eq!(state.ui.layout.tabs.len(), 2);
    assert_eq!(
        state.ui.layout.tabs[0]
            .root()
            .find_pane(0)
            .and_then(Pane::instance_ref),
        Some(old_ref)
    );
    assert_eq!(
        state.ui.layout.tabs[1]
            .root()
            .find_pane(1)
            .and_then(Pane::instance_ref),
        Some(new_ref)
    );
    std::fs::remove_dir_all(home).ok();
}

#[test]
fn closed_before_attach_reaps_unmanaged_shell_by_instance_id() {
    assert!(!closed_before_attach_registry_survives(true));
}

#[test]
fn closed_before_attach_preserves_managed_agent() {
    assert!(closed_before_attach_registry_survives(false));
}

#[test]
fn hot_reload_team_grouped_places_team_in_one_tab() {
    // Simulate a team svc with members svc-a, svc-b and orchestrator svc-a,
    // plus a standalone solo. to_add = [svc-a, svc-b, solo] should yield
    // 2 tabs: svc (with svc-a, svc-b) and solo.
    let home = std::env::temp_dir().join(format!(
        "agend-test-hot-reload-team-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).expect("create temp home");
    // Create fleet entries first so team create can merge into fleet.yaml
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  svc-a:\n    backend: shell\n  svc-b:\n    backend: shell\n  svc-c:\n    backend: shell\n  solo:\n    backend: shell\n",
    )
    .expect("write fleet.yaml");
    // Create a team svc with svc-a, svc-b, orchestrator svc-a
    let res = crate::teams::create(
        &home,
        &serde_json::json!({"name": "svc", "members": ["svc-a", "svc-b", "svc-c"], "orchestrator": "svc-a"}),
    );
    assert_eq!(
        res["status"],
        serde_json::Value::String("created".to_string()),
        "create team failed: {res}"
    );

    // Use the real production placement path with only pane construction
    // replaced, so this test fails if grouping or ordering is disabled.
    let to_add = vec!["svc-a".to_string(), "svc-b".to_string(), "solo".to_string()];
    let mut state = AppState::new();
    let mut pane_builder = |name: &str, layout: &mut Layout| test_remote_pane(layout, name);
    state.place_remote_team_grouped(&to_add, &home, &mut pane_builder);

    let team_tab = state
        .ui
        .layout
        .tabs
        .iter()
        .find(|tab| tab.name == "svc")
        .expect("team tab named svc");
    assert_eq!(team_tab.root().pane_count(), 2);
    assert_eq!(
        team_tab.root().agent_names(),
        vec!["svc-a", "svc-b"],
        "orchestrator must be first in the initial team tab"
    );

    // A later roster tick must find the existing team tab even though the
    // standalone tab is active after the first batch.
    state.ui.layout.next_tab();
    let active_before = state.ui.layout.tabs[state.ui.layout.active].name.clone();
    let team_index = state
        .ui
        .layout
        .tabs
        .iter()
        .position(|tab| tab.name == "svc")
        .expect("team tab exists");
    state.ui.layout.goto_tab(team_index);
    state
        .ui
        .layout
        .active_tab_mut()
        .expect("team tab exists")
        .cycle_focus();
    let focused_before = state
        .ui
        .layout
        .active_tab()
        .and_then(|tab| tab.focused_pane())
        .map(|pane| pane.agent_name.to_string())
        .expect("team focus exists");
    state.ui.layout.goto_tab(
        state
            .ui
            .layout
            .tabs
            .iter()
            .position(|tab| tab.name == active_before)
            .expect("active tab exists"),
    );
    state.place_remote_team_grouped(&["svc-c".to_string()], &home, &mut pane_builder);
    assert_eq!(
        state.ui.layout.tabs[state.ui.layout.active].name, active_before,
        "joining an EXISTING team tab must leave the operator where they were"
    );

    assert_eq!(state.ui.layout.tabs.len(), 2, "team + standalone tab");
    let team_tab = state
        .ui
        .layout
        .tabs
        .iter()
        .find(|tab| tab.name == "svc")
        .expect("team tab named svc");
    assert_eq!(team_tab.root().pane_count(), 3);
    let team_names = team_tab.root().agent_names();
    assert_eq!(team_names.first().map(String::as_str), Some("svc-a"));
    let mut non_orchestrators = team_names[1..].to_vec();
    non_orchestrators.sort();
    assert_eq!(non_orchestrators, vec!["svc-b", "svc-c"]);
    assert_eq!(
        team_tab
            .focused_pane()
            .map(|pane| pane.agent_name.to_string()),
        Some(focused_before),
        "appending a team member must preserve the existing focused pane"
    );
    let solo_tab = state
        .ui
        .layout
        .tabs
        .iter()
        .find(|tab| tab.name == "solo")
        .expect("standalone tab named solo");
    assert_eq!(solo_tab.root().agent_names(), vec!["solo"]);

    // F1 (#3505 backoff for team members): a failing team member must
    // increment the same failure counter as the standalone arm — without
    // it the member never enters backoff and the stale hint never fires.
    // svc-b already has a pane; the builder fails first so the Err arm
    // runs before any retained-pane check.
    let mut failing_builder = |name: &str, layout: &mut Layout| -> anyhow::Result<Pane> {
        if name == "svc-b" {
            Err(anyhow::anyhow!("boom"))
        } else {
            test_remote_pane(layout, name)
        }
    };
    state.place_remote_team_grouped(&["svc-b".to_string()], &home, &mut failing_builder);
    assert_eq!(
        state.remote_attach_failures.get("svc-b"),
        Some(&1),
        "one failed team tick must record fails=1"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Fixture for the #3501 focus/reconnect pins: fleet with m1, m2, solo and
/// a team `svc` (members m1+m2, orchestrator m1). `solo` is in no team.
fn team_fixture_home(tag: &str) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!(
        "agend-test-3501-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).expect("create temp home");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  m1:\n    backend: shell\n  m2:\n    backend: shell\n  solo:\n    backend: shell\n",
    )
    .expect("write fleet.yaml");
    let res = crate::teams::create(
        &home,
        &serde_json::json!({"name": "svc", "members": ["m1", "m2"], "orchestrator": "m1"}),
    );
    assert_eq!(
        res["status"],
        serde_json::Value::String("created".to_string()),
        "create team failed: {res}"
    );
    home
}

/// #3501 B1: opening a team tab happens on a background roster tick, so it
/// must NOT steal the active tab — `push_tab_preserve_focus`, not `add_tab`
/// (whose contract is to focus what it adds). The existing hot-reload test
/// only reaches the split branch, so this is the case that pins the choice.
#[test]
fn hot_reload_team_tab_creation_preserves_active_tab_3501() {
    let home = team_fixture_home("active");
    let mut state = AppState::new();
    let mut pane_builder = |name: &str, layout: &mut Layout| test_remote_pane(layout, name);
    // Standalone `solo` opens the tab the operator is working on.
    state.place_remote_team_grouped(&["solo".to_string()], &home, &mut pane_builder);
    let active_before = state.ui.layout.tabs[state.ui.layout.active].name.clone();
    assert_eq!(active_before, "solo");
    // A team member appears on a later tick: a NEW team tab is created.
    state.place_remote_team_grouped(&["m1".to_string()], &home, &mut pane_builder);
    assert_eq!(state.ui.layout.tabs.len(), 2, "solo + svc");
    assert_eq!(
        state.ui.layout.tabs[state.ui.layout.active].name, active_before,
        "opening a team tab must not steal the active tab"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #3501 B2: a retained team pane (agent gone, pane kept for scrollback)
/// that re-appears reconnects IN PLACE — no duplicate leaf, same team tab,
/// same position. Without the `already_has_pane` check the member is
/// appended a second time and the tab shows one agent twice.
#[test]
fn hot_reload_retained_team_pane_reconnects_in_place_3501() {
    let home = team_fixture_home("retained");
    let mut state = AppState::new();
    // The production bridge supplies this identity during its greeting;
    // keep the fixture's two attaches on the same incarnation so this
    // pre-existing reconnect contract exercises the exact path.
    state.remote_instance_refs.insert(
        "m1".to_string(),
        crate::types::InstanceRef::new(crate::types::InstanceId::new(), 201),
    );
    state.remote_instance_refs.insert(
        "m2".to_string(),
        crate::types::InstanceRef::new(crate::types::InstanceId::new(), 202),
    );
    let mut pane_builder = |name: &str, layout: &mut Layout| test_remote_pane(layout, name);
    state.place_remote_team_grouped(
        &["m1".to_string(), "m2".to_string()],
        &home,
        &mut pane_builder,
    );
    assert_eq!(state.ui.layout.tabs.len(), 1, "one team tab");
    assert_eq!(
        state.ui.layout.tabs[0].root().agent_names(),
        vec!["m1", "m2"]
    );
    // m1 disappeared (pane retained) and comes back on a later tick.
    state.place_remote_team_grouped(&["m1".to_string()], &home, &mut pane_builder);
    assert_eq!(state.ui.layout.tabs.len(), 1, "no extra tab");
    assert_eq!(
        state.ui.layout.tabs[0].root().agent_names(),
        vec!["m1", "m2"],
        "retained team pane must reconnect in place — same tab, same position, no duplicate"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// N1 (tab-name collision, pinned): a team whose name collides with an
/// existing tab does NOT open a second same-named tab — members split
/// into the existing one. Recorded here so a future grouping change
/// must consciously alter this behavior, not drift into it.
#[test]
fn hot_reload_team_name_collision_reuses_existing_tab() {
    let home = std::env::temp_dir().join(format!(
        "agend-test-hot-reload-collide-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).expect("create temp home");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  m1:\n    backend: shell\n  solo:\n    backend: shell\n",
    )
    .expect("write fleet.yaml");
    let res = crate::teams::create(
        &home,
        &serde_json::json!({"name": "solo", "members": ["m1"], "orchestrator": "m1"}),
    );
    assert_eq!(
        res["status"],
        serde_json::Value::String("created".to_string()),
        "create team failed: {res}"
    );

    let mut state = AppState::new();
    let mut pane_builder = |name: &str, layout: &mut Layout| test_remote_pane(layout, name);
    // Standalone first: opens a tab named "solo".
    state.place_remote_team_grouped(&["solo".to_string()], &home, &mut pane_builder);
    assert_eq!(state.ui.layout.tabs.len(), 1);
    // Team "solo" member arrives: must reuse, not duplicate.
    state.place_remote_team_grouped(&["m1".to_string()], &home, &mut pane_builder);
    assert_eq!(
        state.ui.layout.tabs.len(),
        1,
        "colliding team name must not open a second tab"
    );
    let tab = &state.ui.layout.tabs[0];
    assert_eq!(tab.name, "solo");
    assert_eq!(tab.root().pane_count(), 2);
    std::fs::remove_dir_all(&home).ok();
}
