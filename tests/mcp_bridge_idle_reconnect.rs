//! Regression test: bridge transparently reconnects after the daemon idle-closes
//! the persistent TCP session.
//!
//! Background: the daemon's pre-auth slow-loris defense (PR #267) installed
//! a 5s read timeout on the API socket. After auth, the same timeout applied
//! to subsequent requests, so any MCP tool call >5s after the previous one
//! found the daemon's session loop already broken and the socket closed.
//! Each bridge request then hit `Broken pipe` on its first write and the
//! caller (Claude Code) had to retry by hand. This test fixes the gap by
//! spawning the bridge against a mock daemon that closes after the first
//! request, then issues a second request and asserts the bridge succeeds
//! without surfacing the transport error.
//!
//! r4 drops the post-auth read timeout entirely when the PID watcher is
//! active (single-operator threat model rejects the slow-loris-against-
//! authenticated-peer concern), so daemon-initiated idle close becomes
//! rare. The bridge retry remains as defense in depth for daemon restart
//! and genuine network blips.

// Windows TCP loopback close timing differs enough to make the
// child-process / mock-daemon shape hang in CI (same pattern that forced
// PowerShell-only fixes in PR #263). The bridge code itself ships on
// Windows; the unit-level retry logic is covered by the `is_retriable_io`
// classifier and the fix's behavior is exercised by macOS + Linux runners.
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn bridge_binary() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_agend-mcp-bridge"));
    assert!(
        path.exists(),
        "bridge binary missing at {} — run `cargo build --bin agend-mcp-bridge` first",
        path.display()
    );
    path
}

/// Mock daemon that:
///   1. Accepts the first cookie-authenticated session, replies to one
///      `mcp_tools_list` request, then drops the connection (simulating
///      idle-close from the post-auth read timeout).
///   2. Accepts a second session and replies to one more request before
///      shutting the listener down.
///
/// Returns (run_dir for AGEND_HOME, join handle for the listener thread).
fn spawn_idle_close_daemon() -> (PathBuf, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock daemon");
    let port = listener.local_addr().unwrap().port();

    let run_dir = std::env::temp_dir().join(format!(
        "agend-bridge-reconnect-{}-{}",
        std::process::id(),
        port
    ));
    let pid_dir = run_dir.join("run").join(format!("{}", std::process::id()));
    std::fs::create_dir_all(&pid_dir).expect("create run dir");
    std::fs::write(pid_dir.join("api.port"), port.to_string()).expect("write port");
    let cookie = [0x42u8; 32];
    std::fs::write(pid_dir.join("api.cookie"), cookie).expect("write cookie");

    let handle = thread::spawn(move || {
        for session_index in 0..2 {
            let (stream, _) = match listener.accept() {
                Ok(s) => s,
                Err(_) => return,
            };
            let _ = stream.set_nodelay(true);
            let writer_clone = stream.try_clone().expect("clone");
            let mut reader = BufReader::new(stream);
            let mut writer = writer_clone;

            // Cookie auth handshake — accept whatever cookie the bridge sends.
            let mut auth_line = String::new();
            if reader.read_line(&mut auth_line).is_err() {
                return;
            }
            let _ = writeln!(writer, r#"{{"ok":true}}"#);
            let _ = writer.flush();

            // One request → one response. Then drop the connection to
            // mimic the post-auth idle close.
            let mut req_line = String::new();
            if reader.read_line(&mut req_line).is_err() {
                return;
            }
            let resp = json!({
                "ok": true,
                "result": {"session": session_index, "tools": []}
            });
            let _ = writeln!(writer, "{resp}");
            let _ = writer.flush();

            // Explicitly drop to close the FD before next accept.
            drop(reader);
            drop(writer);
        }
    });

    (run_dir, handle)
}

fn send_ndjson(stdin: &mut std::process::ChildStdin, value: &Value) {
    writeln!(stdin, "{value}").expect("write request");
    stdin.flush().expect("flush");
}

fn read_ndjson_line_with_timeout(
    stdout: &mut Option<std::process::ChildStdout>,
    timeout: Duration,
) -> String {
    let mut taken = stdout.take().expect("stdout already consumed");
    let (tx, rx) = mpsc::channel::<(String, std::process::ChildStdout)>();
    thread::spawn(move || {
        let mut reader = BufReader::new(&mut taken);
        let mut line = String::new();
        if reader.read_line(&mut line).is_ok() {
            let _ = tx.send((line, taken));
        }
    });
    let (line, returned) = rx.recv_timeout(timeout).expect("bridge reply timeout");
    *stdout = Some(returned);
    line
}

#[test]
fn bridge_retries_after_idle_close_so_caller_sees_no_failure() {
    let (run_dir, daemon_handle) = spawn_idle_close_daemon();

    let mut child = Command::new(bridge_binary())
        .env("AGEND_HOME", &run_dir)
        .env("AGEND_INSTANCE_NAME", "test-reconnect")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bridge");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = Some(child.stdout.take().unwrap());

    // MCP initialize handshake (handled locally by the bridge, no daemon hop)
    send_ndjson(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "test", "version": "1"}}
        }),
    );
    let init_line = read_ndjson_line_with_timeout(&mut stdout, Duration::from_secs(5));
    let init: Value = serde_json::from_str(init_line.trim()).expect("init json");
    assert_eq!(init["id"], 0);

    // First tools/list — opens daemon session 0, daemon replies then closes.
    send_ndjson(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    );
    let first = read_ndjson_line_with_timeout(&mut stdout, Duration::from_secs(5));
    let first_val: Value = serde_json::from_str(first.trim()).expect("first json");
    assert_eq!(first_val["id"], 1);
    assert!(
        first_val["error"].is_null(),
        "first call must succeed cleanly, got {first_val}"
    );

    // Allow the daemon thread to finish closing session 0 before the next
    // request; the read of the next request still fails the cached conn.
    thread::sleep(Duration::from_millis(150));

    // Second tools/list — bridge's cached connection is now dead. Without
    // the retry fix this would surface a "Broken pipe" / "daemon closed
    // connection" error to the caller. With the fix it transparently
    // reconnects and the caller sees a clean response.
    send_ndjson(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    );
    let second = read_ndjson_line_with_timeout(&mut stdout, Duration::from_secs(5));
    let second_val: Value = serde_json::from_str(second.trim()).expect("second json");
    assert_eq!(second_val["id"], 2);
    assert!(
        second_val["error"].is_null(),
        "second call must succeed transparently after idle close, got {second_val}"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = daemon_handle.join();
    let _ = std::fs::remove_dir_all(&run_dir);
}

/// Defense-in-depth: an application-level error (daemon returns ok=false)
/// must not be retried — the same error would just repeat and the caller
/// needs the actual diagnostic.
#[test]
fn bridge_does_not_retry_application_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let run_dir = std::env::temp_dir().join(format!(
        "agend-bridge-noretry-{}-{}",
        std::process::id(),
        port
    ));
    let pid_dir = run_dir.join("run").join(format!("{}", std::process::id()));
    std::fs::create_dir_all(&pid_dir).unwrap();
    std::fs::write(pid_dir.join("api.port"), port.to_string()).unwrap();
    let cookie = [0x42u8; 32];
    std::fs::write(pid_dir.join("api.cookie"), cookie).unwrap();

    // Track how many sessions the daemon accepts — should be exactly 1.
    // Listener runs in non-blocking mode and the thread exits via the
    // shared `stop` flag so the test never hangs in `daemon.join()`.
    listener
        .set_nonblocking(true)
        .expect("listener set_nonblocking");
    let session_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let session_count_clone = session_count.clone();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_clone = stop.clone();
    let daemon = thread::spawn(move || {
        let mut last_session_thread: Option<thread::JoinHandle<()>> = None;
        while !stop_clone.load(std::sync::atomic::Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    session_count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let stop_inner = stop_clone.clone();
                    let h = thread::spawn(move || {
                        let _ = stream.set_nodelay(true);
                        let _ = stream.set_nonblocking(false);
                        let writer_clone = stream.try_clone().unwrap();
                        let mut reader = BufReader::new(stream);
                        let mut writer = writer_clone;
                        let mut auth = String::new();
                        let _ = reader.read_line(&mut auth);
                        let _ = writeln!(writer, r#"{{"ok":true}}"#);
                        let _ = writer.flush();
                        let mut req = String::new();
                        if reader.read_line(&mut req).is_err() {
                            return;
                        }
                        let _ =
                            writeln!(writer, r#"{{"ok":false,"error":"deliberate app error"}}"#);
                        let _ = writer.flush();
                        // Keep the FD alive (blocking the bridge from seeing
                        // EOF, which would *legitimately* trigger a retry)
                        // until the test signals stop.
                        while !stop_inner.load(std::sync::atomic::Ordering::SeqCst) {
                            thread::sleep(Duration::from_millis(50));
                        }
                    });
                    last_session_thread = Some(h);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
        if let Some(h) = last_session_thread {
            let _ = h.join();
        }
    });

    let mut child = Command::new(bridge_binary())
        .env("AGEND_HOME", &run_dir)
        .env("AGEND_INSTANCE_NAME", "test-no-retry")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bridge");

    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = Some(child.stdout.take().unwrap());

    send_ndjson(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "test", "version": "1"}}
        }),
    );
    let _ = read_ndjson_line_with_timeout(&mut stdout, Duration::from_secs(5));

    send_ndjson(
        &mut stdin,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    );
    let resp_line = read_ndjson_line_with_timeout(&mut stdout, Duration::from_secs(5));
    let _resp: Value = serde_json::from_str(resp_line.trim()).expect("json");

    // The bridge must NOT have opened a second session to retry an
    // application-level error.
    thread::sleep(Duration::from_millis(150));
    assert_eq!(
        session_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "bridge must not retry application-level errors (ok=false from daemon)"
    );

    let _ = child.kill();
    let _ = child.wait();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = daemon.join();
    let _ = std::fs::remove_dir_all(&run_dir);
}
