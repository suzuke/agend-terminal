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
    );

    let error = ready_rx.recv().unwrap().unwrap_err();
    assert!(
        error.contains("api.cookie missing"),
        "unexpected error: {error}"
    );
}
