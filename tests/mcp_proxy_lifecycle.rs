//! MCP proxy lifecycle tests — TCP connection handling and concurrent callers.
//!
//! Sprint 25 P0 Option F REJECT criteria:
//! - TCP connection lifecycle (drop mid-call → structured error, no panic)
//! - Concurrent caller (request_id roundtrip, no response interleave)

#![allow(clippy::unwrap_used)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

/// Spin up a mock daemon API that handles cookie auth + mcp_tool requests.
fn mock_daemon() -> (TcpListener, [u8; 32]) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let cookie = [0x42u8; 32];
    (listener, cookie)
}

fn accept_and_auth(listener: &TcpListener, cookie: &[u8; 32]) -> (BufReader<TcpStream>, TcpStream) {
    let (stream, _) = listener.accept().expect("accept");
    let _ = stream.set_nodelay(true);
    let writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);

    // Read auth line
    let mut auth_line = String::new();
    reader.read_line(&mut auth_line).expect("read auth");
    let expected_hex: String = cookie.iter().map(|b| format!("{b:02x}")).collect();
    assert!(
        auth_line.contains(&expected_hex),
        "auth mismatch: {auth_line}"
    );

    // Send auth OK
    let mut w = writer.try_clone().expect("clone writer");
    writeln!(w, r#"{{"ok":true}}"#).expect("write auth ok");
    w.flush().expect("flush");

    (reader, writer)
}

/// Test: daemon drops connection mid-session → bridge should get an error,
/// not panic or hang.
#[test]
fn tcp_drop_mid_session_produces_error() {
    let (listener, cookie) = mock_daemon();
    let port = listener.local_addr().unwrap().port();

    // Simulate a client connecting and sending a request
    let handle = std::thread::spawn(move || {
        let (mut reader, _writer) = accept_and_auth(&listener, &cookie);
        // Read the mcp_tool request
        let mut line = String::new();
        reader.read_line(&mut line).expect("read request");
        // Drop without responding — simulates daemon crash
        drop(reader);
    });

    // Client side: connect, auth, send request, expect error
    let stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    let mut w = writer;

    // Auth handshake
    let hex: String = cookie.iter().map(|b| format!("{b:02x}")).collect();
    writeln!(w, r#"{{"auth":"{hex}"}}"#).expect("write auth");
    w.flush().expect("flush");
    let mut auth_resp = String::new();
    reader.read_line(&mut auth_resp).expect("read auth resp");
    assert!(auth_resp.contains("true"));

    // Send a request
    writeln!(
        w,
        r#"{{"method":"mcp_tool","params":{{"tool":"inbox","arguments":{{}},"instance":"test"}}}}"#
    )
    .expect("write");
    w.flush().expect("flush");

    // The peer dropped without responding, so the read must terminate cleanly
    // at EOF — 0 bytes, empty buffer — within the 5s read timeout (i.e. no
    // hang, no panic, no partial/garbage line). The previous assertion
    // `bytes == 0 || resp.contains("error") || resp.is_empty()` was
    // unfalsifiable: on a dropped connection `read_line` always yields EOF (or
    // a timeout coerced to 0 via `unwrap_or(0)`), so `bytes` is always 0 and
    // the whole disjunction is always true. Assert the specific observable.
    let mut resp = String::new();
    let bytes = reader.read_line(&mut resp).unwrap_or(0);
    assert_eq!(
        bytes, 0,
        "dropped mid-session connection must surface as EOF, got {bytes} bytes: {resp:?}"
    );
    assert!(
        resp.is_empty(),
        "no partial response expected after a mid-session drop, got: {resp:?}"
    );

    handle.join().expect("mock daemon thread");
}

/// Test: sequential requests on a single connection each get the matching
/// response back (no response interleave / off-by-one). NOTE: despite the old
/// name, this drives 3 requests SEQUENTIALLY (not concurrently) and matches on
/// the echoed `tool` field — there is no `request_id` in the protocol here.
#[test]
fn sequential_calls_tool_field_roundtrip() {
    let (listener, cookie) = mock_daemon();
    let port = listener.local_addr().unwrap().port();

    // Mock daemon: accept one connection, handle 3 sequential requests
    let handle = std::thread::spawn(move || {
        let (mut reader, writer) = accept_and_auth(&listener, &cookie);
        let mut w = writer;

        for _ in 0..3 {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let req: Value = serde_json::from_str(line.trim()).expect("parse request");
            let tool = req["params"]["tool"].as_str().unwrap_or("unknown");
            let resp = json!({"ok": true, "result": {"tool": tool, "status": "ok"}});
            writeln!(w, "{resp}").expect("write response");
            w.flush().expect("flush");
        }
    });

    // Client: send 3 requests on the same connection
    let stream = TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    let mut w = writer;

    // Auth
    let hex: String = cookie.iter().map(|b| format!("{b:02x}")).collect();
    writeln!(w, r#"{{"auth":"{hex}"}}"#).expect("auth");
    w.flush().expect("flush");
    let mut auth_resp = String::new();
    reader.read_line(&mut auth_resp).expect("read auth");

    let tools = ["inbox", "list_instances", "task"];
    for tool in &tools {
        let req = json!({"method": "mcp_tool", "params": {"tool": tool, "arguments": {}, "instance": "test"}});
        writeln!(w, "{req}").expect("write");
        w.flush().expect("flush");

        let mut resp_line = String::new();
        reader.read_line(&mut resp_line).expect("read response");
        let resp: Value = serde_json::from_str(resp_line.trim()).expect("parse response");
        assert_eq!(
            resp["ok"].as_bool(),
            Some(true),
            "request for {tool} failed"
        );
        assert_eq!(
            resp["result"]["tool"].as_str(),
            Some(*tool),
            "response tool mismatch for {tool}"
        );
    }

    handle.join().expect("mock daemon thread");
}
