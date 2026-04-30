//! Sprint 42 Phase 2 — AgendHarness + TuiClient MVP integration tests.

mod common;

use common::harness::AgendHarness;
use common::harness::TuiClient;
use serde_json::json;

fn tmp_home(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("agend-harness-{}-{}", tag, std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// MVP: spawn daemon, connect via API, list agents (empty fleet).
#[test]
fn harness_spawn_and_connect() {
    let home = tmp_home("spawn-connect");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("harness spawn");

    assert!(harness.api_port > 0, "api_port must be assigned");

    let client = TuiClient::new(&harness, 80, 24);
    let result = client.call("list", &json!({}));
    assert!(
        result.is_ok(),
        "API list call must succeed: {:?}",
        result.err()
    );
    let resp = result.expect("list response");
    assert_eq!(resp["ok"], true, "list must return ok: {resp}");

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}

/// TuiClient can call status endpoint.
#[test]
fn tuiclient_status_call() {
    let home = tmp_home("status-call");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("harness spawn");

    let client = TuiClient::new(&harness, 80, 24);
    let result = client.call("status", &json!({}));
    assert!(
        result.is_ok(),
        "status call must succeed: {:?}",
        result.err()
    );

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}

/// BLOCKING 3.2: deliberately bad fleet.yaml → daemon exits → harness returns
/// informative error (not timeout).
#[test]
fn harness_spawn_reports_early_exit_clearly() {
    let home = tmp_home("early-exit");
    // F1 fix: measure elapsed time INCLUDING spawn() to detect hangs
    let start = std::time::Instant::now();
    let result = AgendHarness::spawn(home.clone(), "{{{{invalid yaml that breaks parsing}}}}\n");

    match result {
        Ok(harness) => {
            drop(harness);
        }
        Err(e) => {
            assert!(
                !e.contains("timeout"),
                "early exit must be detected before timeout: {e}"
            );
        }
    }
    assert!(
        start.elapsed() < std::time::Duration::from_secs(10),
        "harness must not hang on bad fleet.yaml"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// BLOCKING 3.1: drop harness kills child processes (no orphans).
#[test]
fn harness_drop_kills_child_processes() {
    let home = tmp_home("drop-kills");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("harness spawn");

    let port = harness.api_port;
    // Verify daemon is alive before drop
    assert!(
        std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok(),
        "daemon must be reachable before drop"
    );

    drop(harness);

    // Poll up to 5x with 200ms sleep — daemon should be dead
    let mut port_closed = false;
    for _ in 0..5 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_err() {
            port_closed = true;
            break;
        }
    }
    assert!(
        port_closed,
        "daemon port {port} must be closed after harness drop (BLOCKING 3.1)"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// TuiClient vterm: feed bytes → extract screen text.
#[test]
fn tuiclient_vterm_grid_extract() {
    let home = tmp_home("vterm-grid");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("harness spawn");

    let mut client = TuiClient::new(&harness, 80, 24);
    client.feed(b"Hello from vterm\r\n");
    let screen = client.screen_text(5);
    assert!(
        screen.contains("Hello from vterm"),
        "vterm must capture fed text, got: '{screen}'"
    );

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}

/// TuiClient drain_for: feed + drain returns screen content.
#[test]
fn tuiclient_feed_and_extract_returns_content() {
    let home = tmp_home("drain-for");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("harness spawn");

    let mut client = TuiClient::new(&harness, 80, 24);
    let screen = client.feed_and_extract(b"Line1\r\nLine2\r\n");
    assert!(
        screen.contains("Line1"),
        "drain must capture Line1: '{screen}'"
    );
    assert!(
        screen.contains("Line2"),
        "drain must capture Line2: '{screen}'"
    );

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}

/// TuiClient wait_for: predicate matches after feed.
#[test]
fn tuiclient_wait_for_predicate_match() {
    let home = tmp_home("wait-for");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("harness spawn");

    let mut client = TuiClient::new(&harness, 80, 24);
    let found = client.wait_for(
        b"Expected output\r\n",
        |s| s.contains("Expected output"),
        std::time::Duration::from_secs(1),
    );
    assert!(found, "wait_for must find 'Expected output' in vterm");

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}
