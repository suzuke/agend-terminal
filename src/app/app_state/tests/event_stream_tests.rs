//! #3661 event-stream fencing and terminal-receiver regression witnesses.
//!
//! Kept outside `app_state.rs` so the production owner remains below the
//! repository's anti-monolith source-size invariant.

use super::*;

#[test]
fn lifecycle_stream_fences_source_order_and_non_live_snapshots() {
    let home =
        std::env::temp_dir().join(format!("event_stream_{}", crate::types::InstanceId::new()));
    let fleet_path = home.join("fleet.yaml");
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (wakeup_tx, _wakeup_rx) = crossbeam_channel::unbounded();
    let app_restart_gate = crate::api::app_restart::AppRestartGate::new();
    let daemon_binary_stale = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let attached_run_dir = Some(home.clone());
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
    let mut state = AppState::new();
    let event = |source: &str, sequence: u64| {
        rpc::EventStreamOutcome::Event(crate::daemon::event_hub::DaemonEvent {
            source: source.to_string(),
            sequence,
            event: crate::api::ApiEvent::TeamCreated {
                name: "team".to_string(),
                members: Vec::new(),
            },
        })
    };

    state.handle_event_stream_outcome(
        Ok(rpc::EventStreamOutcome::Connected {
            source: "daemon-a".to_string(),
        }),
        &deps,
    );
    assert_eq!(state.event_source.as_deref(), Some("daemon-a"));
    assert_eq!(state.event_sequence, 0);
    assert!(state.event_resync_required);

    state.handle_event_stream_outcome(Ok(event("unknown", 1)), &deps);
    assert!(state.event_source.is_none());
    assert!(state.event_resync_required);
    state.handle_agent_state_rpc_outcome(Ok(Ok(rpc::AgentStateSnapshotResult {
        snapshot: HashMap::new(),
        names: HashSet::new(),
        instance_refs: HashMap::new(),
        mode: crate::runtime::AgentListMode::Live,
    })));
    assert!(!state.event_resync_required);

    state.handle_event_stream_outcome(Ok(event("daemon-a", 1)), &deps);
    assert_eq!(state.event_source.as_deref(), Some("daemon-a"));
    assert_eq!(state.event_sequence, 1);
    assert!(!state.event_resync_required);

    // Duplicate events are harmless and do not move the fence backward.
    state.handle_event_stream_outcome(Ok(event("daemon-a", 1)), &deps);
    assert_eq!(state.event_sequence, 1);
    assert!(!state.event_resync_required);

    // A gap forces a Live snapshot before any destructive event is trusted.
    state.handle_event_stream_outcome(Ok(event("daemon-a", 3)), &deps);
    assert_eq!(state.event_sequence, 3);
    assert!(state.event_resync_required);

    // A successor source is never accepted into the old stream epoch.
    state.handle_event_stream_outcome(Ok(event("daemon-b", 4)), &deps);
    assert!(state.event_source.is_none());
    assert_eq!(state.event_sequence, 0);
    assert!(state.event_resync_required);

    // Fallback snapshots cannot clear the resync fence.
    state.handle_agent_state_rpc_outcome(Ok(Ok(rpc::AgentStateSnapshotResult {
        snapshot: HashMap::new(),
        names: HashSet::new(),
        instance_refs: HashMap::new(),
        mode: crate::runtime::AgentListMode::FallbackDaemonStuck,
    })));
    assert!(state.event_resync_required);
}

#[test]
fn terminal_event_receiver_error_requests_refresh_only_once() {
    let home = std::path::PathBuf::from("/tmp/event-stream-terminal-test");
    let fleet_path = home.join("fleet.yaml");
    let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
    let (wakeup_tx, _wakeup_rx) = crossbeam_channel::unbounded();
    let app_restart_gate = crate::api::app_restart::AppRestartGate::new();
    let daemon_binary_stale = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let attached_run_dir = Some(home.clone());
    let (task_rpc_tx, _task_rpc_rx) = crossbeam_channel::unbounded::<rpc::TaskRequest>();
    let (remote_state_rpc_tx, remote_state_rpc_rx) =
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
    let mut state = AppState::new();
    let (closed_tx, closed_rx) = crossbeam_channel::bounded::<rpc::EventStreamOutcome>(1);
    drop(closed_tx);
    let closed_outcome = closed_rx.recv();

    state.handle_event_stream_outcome(closed_outcome, &deps);
    state.handle_event_stream_outcome(Err(crossbeam_channel::RecvError), &deps);

    assert!(state.event_stream_receiver_closed);
    assert_eq!(
        remote_state_rpc_rx.try_iter().count(),
        1,
        "terminal receiver must not requeue refreshes on every closed-channel tick"
    );
}
