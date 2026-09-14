#![allow(clippy::unwrap_used)]

use super::*;

fn home(label: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let home = std::env::temp_dir().join(format!(
        "agend-api-readiness-test-{}-{}-{}",
        std::process::id(),
        label,
        id
    ));
    std::fs::create_dir_all(&home).unwrap();
    home
}

#[test]
fn daemon_ready_signal_follows_port_publication() {
    let home = home("daemon-ready-signal");
    let run_dir = crate::daemon::run_dir(&home);
    std::fs::create_dir_all(&run_dir).unwrap();
    crate::auth_cookie::issue(&run_dir).unwrap();

    let registry = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let configs = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let externals = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

    let api_home = home.clone();
    std::thread::Builder::new()
        .name("test_daemon_ready_signal".into())
        .spawn(move || {
            serve_with_ready(
                &api_home,
                registry,
                shutdown,
                configs,
                externals,
                None,
                RestartCapability::Daemon,
                None,
                ready_tx,
                None,
            );
        })
        .unwrap();

    assert_eq!(ready_rx.recv().unwrap(), Ok(()));
    let port = std::fs::read_to_string(run_dir.join("api.port")).unwrap();
    assert!(port.trim().parse::<u16>().unwrap() > 0);
}

#[test]
fn daemon_ready_signal_reports_auth_startup_failure() {
    let home = home("daemon-ready-auth-failure");
    let run_dir = crate::daemon::run_dir(&home);
    std::fs::create_dir_all(&run_dir).unwrap();

    let registry = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let configs = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let externals = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let shutdown = Arc::new(AtomicBool::new(true));
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

    serve_with_ready(
        &home,
        registry,
        shutdown,
        configs,
        externals,
        None,
        RestartCapability::Daemon,
        None,
        ready_tx,
        None,
    );

    let error = ready_rx.recv().unwrap().unwrap_err();
    assert!(
        error.contains("api.cookie missing"),
        "unexpected error: {error}"
    );
}

#[test]
fn authenticated_event_subscription_receives_typed_envelope() {
    use std::io::{BufRead, BufReader, Write};

    let home = home("event-subscription");
    let run_dir = crate::daemon::run_dir(&home);
    std::fs::create_dir_all(&run_dir).unwrap();
    crate::daemon::write_daemon_id(&run_dir);
    crate::auth_cookie::issue(&run_dir).unwrap();
    let registry = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let configs = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let externals = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let hub = crate::daemon::event_hub::EventHub::new("test-daemon".to_string(), 8);
    let publish_hub = hub.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let api_home = home.clone();
    std::thread::spawn(move || {
        serve_with_ready_events(
            &api_home,
            registry,
            shutdown,
            configs,
            externals,
            hub,
            RestartCapability::Daemon,
            None,
            ready_tx,
            None,
        );
    });
    ready_rx.recv().unwrap().unwrap();

    let stream = crate::ipc::connect_run_dir_api(&run_dir).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let token = crate::auth_cookie::read_operator_token(&run_dir).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    crate::auth_cookie::client_handshake_ndjson(&mut reader, &mut writer, &token).unwrap();
    writeln!(
        writer,
        "{}",
        serde_json::json!({
            "method": crate::api::method::SUBSCRIBE_EVENTS,
            "params": {},
        })
    )
    .unwrap();
    writer.flush().unwrap();
    let mut ack = String::new();
    reader.read_line(&mut ack).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&ack).unwrap()["ok"], true);

    publish_hub.notify(ApiEvent::TeamCreated {
        name: "team".to_string(),
        members: vec!["worker".to_string()],
    });
    let mut event = String::new();
    reader.read_line(&mut event).unwrap();
    let event: crate::daemon::event_hub::DaemonEvent = serde_json::from_str(&event).unwrap();
    assert_eq!(event.source, "test-daemon");
    assert_eq!(event.sequence, 1);
    assert!(matches!(event.event, ApiEvent::TeamCreated { .. }));
}

#[test]
fn agent_cookie_event_subscription_is_denied() {
    use std::io::{BufRead, BufReader, Write};

    let home = home("agent-event-subscription-denied");
    let run_dir = crate::daemon::run_dir(&home);
    std::fs::create_dir_all(&run_dir).unwrap();
    crate::daemon::write_daemon_id(&run_dir);
    crate::auth_cookie::issue(&run_dir).unwrap();
    let registry = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let configs = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let externals = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let server_shutdown = Arc::clone(&shutdown);
    let hub = crate::daemon::event_hub::EventHub::new("test-daemon".to_string(), 8);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let api_home = home.clone();
    std::thread::spawn(move || {
        serve_with_ready_events(
            &api_home,
            registry,
            server_shutdown,
            configs,
            externals,
            hub,
            RestartCapability::Daemon,
            None,
            ready_tx,
            None,
        );
    });
    ready_rx.recv().unwrap().unwrap();

    let stream = crate::ipc::connect_run_dir_api(&run_dir).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let token = crate::auth_cookie::read_cookie(&run_dir).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    crate::auth_cookie::client_handshake_ndjson(&mut reader, &mut writer, &token).unwrap();
    writeln!(
        writer,
        "{}",
        serde_json::json!({
            "method": crate::api::method::SUBSCRIBE_EVENTS,
            "params": {},
        })
    )
    .unwrap();
    writer.flush().unwrap();

    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["ok"], false);
    assert_eq!(response["denied_by"], "capability");
    shutdown.store(true, std::sync::atomic::Ordering::Release);
}
