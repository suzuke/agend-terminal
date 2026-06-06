//! MCP proxy behavioral parity test — end-to-end verification that the
//! bridge subprocess produces correct MCP responses for tool calls.
//!
//! Sprint 25 P2: closes M2 NIT from PR #250 (dev-reviewer m-74).
//! Structural parity tests (mcp_proxy_parity.rs) verify code shape;
//! these tests verify runtime behavior by spawning the actual bridge
//! binary against a mock daemon and comparing MCP responses.
//!
//! Architecture:
//! - Mock daemon: TCP listener returning known tool results
//! - Bridge subprocess: `agend-mcp-bridge` binary connected to mock
//! - Test: sends MCP `tools/call` via bridge stdin, verifies response

#![allow(clippy::unwrap_used)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Known tool results the mock daemon returns. Each entry is (tool_name,
/// daemon_result) — the mock returns `{"ok": true, "result": daemon_result}`
/// for the matching tool.
fn known_tool_results() -> Vec<(&'static str, Value)> {
    vec![
        ("inbox", json!({"messages": []})),
        (
            "list_instances",
            json!({"instances": [{"name": "general", "status": "running"}]}),
        ),
        ("task", json!({"tasks": []})),
        ("reply", json!({"message_id": "42"})),
        (
            "create_instance",
            json!({"error": "missing 'name' parameter"}),
        ),
    ]
}

/// Set up a mock daemon that handles cookie auth + mcp_tool requests
/// with known responses.
fn spawn_mock_daemon() -> (std::thread::JoinHandle<()>, u16, PathBuf) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();

    // Create temp run dir with port + cookie files
    let run_dir = std::env::temp_dir().join(format!(
        "agend-behavioral-parity-{}-{}",
        std::process::id(),
        port
    ));
    let pid_dir = run_dir.join("run").join(format!("{}", std::process::id()));
    std::fs::create_dir_all(&pid_dir).expect("create run dir");
    std::fs::write(pid_dir.join("api.port"), port.to_string()).expect("write port");
    let cookie = [0x42u8; 32];
    std::fs::write(pid_dir.join("api.cookie"), cookie).expect("write cookie");

    let known = known_tool_results();
    let handle = std::thread::spawn(move || {
        // Accept one connection
        let (stream, _) = listener.accept().expect("accept");
        let _ = stream.set_nodelay(true);
        let writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);
        let mut w = writer;

        // Cookie auth
        let mut auth_line = String::new();
        reader.read_line(&mut auth_line).expect("read auth");
        writeln!(w, r#"{{"ok":true}}"#).expect("auth ok");
        w.flush().expect("flush");

        // Handle requests until EOF
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                _ => {}
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let req: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => break,
            };
            let method = req["method"].as_str().unwrap_or("");
            if method == "mcp_tools_list" {
                writeln!(w, r#"{{"ok":true,"result":{{"tools":[]}}}}"#).expect("write");
                w.flush().expect("flush");
                continue;
            }
            if method == "mcp_tool" {
                let tool = req["params"]["tool"].as_str().unwrap_or("");
                let result = known
                    .iter()
                    .find(|(t, _)| *t == tool)
                    .map(|(_, r)| r.clone())
                    .unwrap_or(json!({"error": format!("unknown tool: {tool}")}));
                let resp = json!({"ok": true, "result": result});
                writeln!(w, "{resp}").expect("write");
                w.flush().expect("flush");
                continue;
            }
            writeln!(w, r#"{{"ok":false,"error":"unknown method"}}"#).expect("write");
            w.flush().expect("flush");
        }
    });

    (handle, port, run_dir)
}

/// Find the bridge binary in the cargo target directory.
fn bridge_binary() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_BIN_EXE_agend-mcp-bridge"));
    if !path.exists() {
        // Fallback: look in target/debug
        path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("debug")
            .join("agend-mcp-bridge");
    }
    path
}

/// Send an MCP JSON-RPC request as NDJSON and read the NDJSON response.
///
/// Mirrors how Claude Code (the real client we ship for) frames messages:
/// raw JSON terminated by a single newline. The bridge must respond in
/// the same shape, otherwise the client hits its connection timeout.
fn mcp_roundtrip(
    stdin: &mut std::process::ChildStdin,
    stdout: &mut BufReader<std::process::ChildStdout>,
    request: &Value,
) -> Value {
    writeln!(stdin, "{request}").expect("write request");
    stdin.flush().expect("flush");

    let mut line = String::new();
    stdout.read_line(&mut line).expect("read response line");
    serde_json::from_str(line.trim()).expect("parse NDJSON response")
}

/// Extract the tool result text from an MCP tools/call response.
fn extract_tool_result(resp: &Value) -> Value {
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("response must have content[0].text");
    serde_json::from_str(text).expect("content text must be valid JSON")
}

/// Behavioral parity test: 5 tools through the bridge produce expected results.
#[test]
fn bridge_tools_call_returns_expected_results_for_five_tools() {
    let (daemon_handle, _port, run_dir) = spawn_mock_daemon();
    let bridge = bridge_binary();
    if !bridge.exists() {
        eprintln!("bridge binary not found at {}, skipping", bridge.display());
        return;
    }

    let mut child = Command::new(&bridge)
        .env("AGEND_HOME", &run_dir)
        .env("AGEND_INSTANCE_NAME", "test-agent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bridge");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    // Initialize MCP session
    let init_resp = mcp_roundtrip(
        &mut stdin,
        &mut stdout,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        }}),
    );
    assert!(
        init_resp["result"]["capabilities"].is_object(),
        "initialize must return capabilities"
    );

    // Send initialized notification (no response expected) — NDJSON
    let notif = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    writeln!(stdin, "{notif}").expect("write notif");
    stdin.flush().expect("flush");

    // Test 5 tools
    for (id, (tool, expected_result)) in known_tool_results().into_iter().enumerate() {
        let resp = mcp_roundtrip(
            &mut stdin,
            &mut stdout,
            &json!({
                "jsonrpc": "2.0",
                "id": id + 10,
                "method": "tools/call",
                "params": {"name": tool, "arguments": {}}
            }),
        );

        // Verify no JSON-RPC error
        assert!(
            resp.get("error").is_none(),
            "tool '{tool}' returned JSON-RPC error: {resp}"
        );

        // Extract and compare tool result
        let actual = extract_tool_result(&resp);
        assert_eq!(
            actual, expected_result,
            "behavioral parity failed for tool '{tool}':\n  expected: {expected_result}\n  actual:   {actual}"
        );

        // Verify isError flag matches
        let is_error = resp["result"]["isError"].as_bool().unwrap_or(false);
        let expected_is_error = expected_result.get("error").is_some();
        assert_eq!(
            is_error, expected_is_error,
            "isError flag mismatch for tool '{tool}'"
        );
    }

    // Clean shutdown
    drop(stdin);
    let _ = child.wait();
    let _ = daemon_handle.join();
    std::fs::remove_dir_all(&run_dir).ok();
}

/// Behavioral test: bridge returns JSON-RPC error when daemon is unreachable.
#[test]
fn bridge_returns_error_when_daemon_unreachable() {
    let bridge = bridge_binary();
    if !bridge.exists() {
        eprintln!("bridge binary not found, skipping");
        return;
    }

    // Point at a non-existent run dir
    let fake_home = std::env::temp_dir().join(format!(
        "agend-behavioral-parity-norun-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&fake_home).ok();

    let mut child = Command::new(&bridge)
        .env("AGEND_HOME", &fake_home)
        .env("AGEND_INSTANCE_NAME", "test-agent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bridge");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    // Initialize (handled locally, should succeed)
    let init_resp = mcp_roundtrip(
        &mut stdin,
        &mut stdout,
        &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        }}),
    );
    assert!(init_resp["result"].is_object());

    // tools/call should fail (no daemon)
    let resp = mcp_roundtrip(
        &mut stdin,
        &mut stdout,
        &json!({
            "jsonrpc": "2.0", "id": 2,
            "method": "tools/call",
            "params": {"name": "inbox", "arguments": {}}
        }),
    );
    assert!(
        resp.get("error").is_some(),
        "tools/call must return error when daemon unreachable: {resp}"
    );

    drop(stdin);
    let _ = child.wait();
    std::fs::remove_dir_all(&fake_home).ok();
}
