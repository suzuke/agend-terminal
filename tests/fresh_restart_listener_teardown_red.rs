//! #3373 recurrence: a fresh restart must retire the predecessor's TUI bridge.
//!
//! `restart_instance(mode:"fresh")` is delete(no-wait) + spawn
//! (`src/mcp/handlers/instance_state/mod.rs`). The successor binds a new
//! per-agent listener and republishes `run/<pid>/<name>.port`, but the
//! predecessor's listener — bound in `src/daemon/tui_bridge.rs` — is left
//! running with its accepted client connections intact.
//!
//! That is what re-broke #3373. A retained APP pane only becomes a reconnect
//! candidate when `Pane::is_disconnected` is true (`src/app/app_state.rs`
//! `remote_attach_candidates` → `src/layout/mod.rs` `agent_pane_is_disconnected`),
//! and `connected` only flips on a send or forwarder-read failure
//! (`src/layout/pane.rs`, `src/app/pane_factory.rs`). While the predecessor's
//! socket stays healthy the pane never disconnects, so it never reconnects, and
//! it sits on the exited process forever.
//!
//! These tests pin the server-side half of the contract: once the successor is
//! published, the predecessor's bridge must be gone. The client-side reconnect
//! that follows is #3380's existing logic and is covered by its own tests.
//!
//! Determinism: the synchronisation point is the successor's port publication —
//! an observable state transition, not a sleep. The timeouts below are bounded
//! fail-safes, not the mechanism.

#![cfg(unix)]
#![allow(clippy::unwrap_used)]

mod common;

use common::harness::AgendHarness;
use serde_json::{json, Value};
use serial_test::serial;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const AGENT: &str = "shell";
/// Bounded fail-safe for a transition that should already have happened by the
/// time `restart_instance` returns and the successor port is published.
const TEARDOWN_BUDGET: Duration = Duration::from_secs(5);
const PUBLISH_BUDGET: Duration = Duration::from_secs(20);

fn hermetic_home(tag: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let home =
        std::env::temp_dir().join(format!("agend-3373-{tag}-{}-{stamp}", std::process::id()));
    assert!(home.starts_with(std::env::temp_dir()));
    home
}

fn run_dir(home: &Path) -> PathBuf {
    std::fs::read_dir(home.join("run"))
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.join("api.cookie").is_file())
        .expect("daemon run dir with api.cookie")
}

fn agent_port(home: &Path) -> Option<u16> {
    std::fs::read_to_string(run_dir(home).join(format!("{AGENT}.port")))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn wait_for_port(home: &Path, differing_from: Option<u16>) -> u16 {
    let deadline = Instant::now() + PUBLISH_BUDGET;
    loop {
        if let Some(port) = agent_port(home) {
            if differing_from != Some(port) {
                return port;
            }
        }
        assert!(
            Instant::now() < deadline,
            "agent '{AGENT}' never published a port distinct from {differing_from:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Attach exactly as a retained APP pane does — see `BridgeClient::connect`
/// in `src/bridge_client.rs`: cookie, protocol version byte, initial resize.
fn attach_pane(home: &Path, port: u16) -> TcpStream {
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(5),
    )
    .expect("attach to agent bridge");
    stream.set_nodelay(true).unwrap();
    let cookie = std::fs::read(run_dir(home).join("api.cookie")).unwrap();
    stream.write_all(&cookie).unwrap();
    stream.flush().unwrap();
    let mut version = [0u8; 1];
    stream.read_exact(&mut version).unwrap();
    assert_eq!(version[0], 1, "TUI bridge protocol version");
    let mut resize = vec![1u8];
    resize.extend_from_slice(&4u32.to_be_bytes());
    resize.extend_from_slice(&80u16.to_be_bytes());
    resize.extend_from_slice(&24u16.to_be_bytes());
    stream.write_all(&resize).unwrap();
    stream.flush().unwrap();
    stream
}

fn operator_token(home: &Path) -> String {
    std::fs::read(run_dir(home).join("api.operator"))
        .unwrap()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn api_call(port: u16, auth_hex: &str, request: &Value) -> Value {
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    writeln!(writer, r#"{{"auth":"{auth_hex}"}}"#).unwrap();
    writer.flush().unwrap();
    let mut auth = String::new();
    reader.read_line(&mut auth).unwrap();
    writeln!(writer, "{request}").unwrap();
    writer.flush().unwrap();
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    serde_json::from_str(response.trim()).unwrap()
}

fn restart_fresh(harness: &AgendHarness) -> Value {
    api_call(
        harness.api_port,
        &operator_token(&harness.home),
        &json!({
            "method": "mcp_tool",
            "request_id": uuid::Uuid::new_v4().to_string(),
            "params": {
                "tool": "restart_instance",
                "instance": AGENT,
                "arguments": {"instance": AGENT, "mode": "fresh", "force": true}
            }
        }),
    )
}

fn boot() -> (AgendHarness, u16) {
    let home = hermetic_home("teardown");
    let fleet = format!("instances:\n  {AGENT}:\n    command: /bin/sh\n");
    let harness = AgendHarness::spawn(home.clone(), &fleet).expect("daemon");
    let port = wait_for_port(&home, None);
    (harness, port)
}

/// A pane attached to the predecessor must observe its bridge close once the
/// successor is published — that close is what makes the pane a reconnect
/// candidate at all.
#[test]
#[serial]
fn fresh_restart_closes_the_predecessor_pane_stream() {
    let (harness, first_port) = boot();
    let mut pane = attach_pane(&harness.home, first_port);

    let response = restart_fresh(&harness);
    assert_eq!(
        response["result"]["spawned"], true,
        "restart_instance must spawn the successor: {response}"
    );
    let second_port = wait_for_port(&harness.home, Some(first_port));
    assert_ne!(
        first_port, second_port,
        "successor must publish a distinct bridge port"
    );

    pane.set_read_timeout(Some(TEARDOWN_BUDGET)).unwrap();
    let mut buf = [0u8; 4096];
    let observed_close = loop {
        match pane.read(&mut buf) {
            Ok(0) => break true,
            Ok(_) => continue,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                break false
            }
            Err(_) => break true,
        }
    };
    assert!(
        observed_close,
        "predecessor pane stream must close once the successor is published \
         (port {first_port} -> {second_port}); a still-healthy stream leaves the pane \
         permanently attached to the exited generation"
    );

    drop(harness);
}

/// The predecessor's listener itself must be retired, not merely orphaned:
/// a still-accepting listener leaks one thread and one port per fresh restart.
#[test]
#[serial]
fn fresh_restart_retires_the_predecessor_listener() {
    let (harness, first_port) = boot();
    let _pane = attach_pane(&harness.home, first_port);

    let response = restart_fresh(&harness);
    assert_eq!(
        response["result"]["spawned"], true,
        "restart_instance must spawn the successor: {response}"
    );
    let second_port = wait_for_port(&harness.home, Some(first_port));

    let deadline = Instant::now() + TEARDOWN_BUDGET;
    let mut still_accepting = true;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], first_port)),
            Duration::from_millis(200),
        )
        .is_err()
        {
            still_accepting = false;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !still_accepting,
        "predecessor listener on port {first_port} must stop accepting once the successor \
         is published on {second_port}; an orphaned listener leaks a thread and a port per \
         fresh restart"
    );

    drop(harness);
}

/// A client that disconnects must not keep the bridge from retiring a LATER,
/// still-live client. The retirement path owns no shared client list — each
/// output thread retires the socket it owns — so a dead peer leaves nothing
/// behind for a live one to trip over.
#[test]
#[serial]
fn retirement_reaches_a_live_client_after_an_earlier_one_disconnected() {
    let (harness, first_port) = boot();

    // A connects and goes away before the restart.
    let departed = attach_pane(&harness.home, first_port);
    departed
        .shutdown(std::net::Shutdown::Both)
        .expect("close the departing client");
    drop(departed);

    // B is the retained pane that must be retired.
    let mut retained = attach_pane(&harness.home, first_port);

    let response = restart_fresh(&harness);
    assert_eq!(
        response["result"]["spawned"], true,
        "restart_instance must spawn the successor: {response}"
    );
    let second_port = wait_for_port(&harness.home, Some(first_port));

    retained.set_read_timeout(Some(TEARDOWN_BUDGET)).unwrap();
    let mut buf = [0u8; 4096];
    let observed_close = loop {
        match retained.read(&mut buf) {
            Ok(0) => break true,
            Ok(_) => continue,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                break false
            }
            Err(_) => break true,
        }
    };
    assert!(
        observed_close,
        "a live client must still be retired after an earlier client disconnected \
         (port {first_port} -> {second_port})"
    );

    drop(harness);
}
