//! Regression test: bridge must complete an MCP handshake with Claude Code's
//! real-world `initialize` request frame and respond fast enough that the
//! client doesn't hit its 30s connection timeout.
//!
//! Background: PR #250 shipped the bridge with Content-Length response framing,
//! which Claude Code 2.x ignores — every reconnect timed out at 30s and the
//! `agend-terminal` MCP server stayed permanently disconnected. PR #253's
//! behavioral parity suite framed both request and response with Content-Length
//! and so passed despite the bug. This test fixes the gap by exercising the
//! exact payload Claude Code sends across a freshly spawned bridge.
//!
//! Failure here means MCP tool/list never returns in real Claude Code sessions.

#![allow(clippy::unwrap_used)]

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Real `initialize` payload captured from Claude Code 2.1.119 in
/// `~/.agend-terminal/bridge-trace-<pid>.log` during the PR debugging session.
/// Note: `protocolVersion` "2025-11-25" intentionally does NOT match the
/// bridge's hard-coded reply — the bridge must still respond, the client
/// reconciles versions itself.
const CLAUDE_CODE_INIT: &str = r#"{"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"roots":{},"elicitation":{}},"clientInfo":{"name":"claude-code","title":"Claude Code","version":"2.1.119","description":"Anthropic's agentic coding tool","websiteUrl":"https://claude.com/claude-code"}},"jsonrpc":"2.0","id":0}"#;

fn bridge_binary() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_agend-mcp-bridge"));
    assert!(
        path.exists(),
        "bridge binary missing at {} — run `cargo build --bin agend-mcp-bridge` first",
        path.display()
    );
    path
}

/// Spawn the bridge, send Claude Code's real init frame, expect an NDJSON
/// response within a few seconds (real client times out at 30s).
#[test]
fn bridge_replies_to_claude_code_initialize_within_5s() {
    let bridge = bridge_binary();
    // Empty AGEND_HOME so the bridge cannot reach a real daemon — initialize
    // is handled locally and must succeed regardless.
    let tmp_home =
        std::env::temp_dir().join(format!("agend-bridge-handshake-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_home).unwrap();

    let mut child = Command::new(&bridge)
        .env("AGEND_HOME", &tmp_home)
        .env("AGEND_INSTANCE_NAME", "test-handshake")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bridge");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    writeln!(stdin, "{CLAUDE_CODE_INIT}").expect("write init");
    stdin.flush().expect("flush");

    // Read response line on a background thread so the assertion can time out
    // independently of pipe blocking.
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        if reader.read_line(&mut line).is_ok() {
            let _ = tx.send(line);
        }
    });

    let line = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("bridge must reply to initialize within 5s (real client times out at 30s)");

    let resp: Value = serde_json::from_str(line.trim())
        .unwrap_or_else(|e| panic!("response must be NDJSON (raw JSON + \\n), got {line:?}: {e}"));

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 0);
    assert!(
        resp["result"]["capabilities"].is_object(),
        "initialize response must include capabilities, got: {resp}"
    );
    assert!(
        resp["result"]["serverInfo"]["name"].is_string(),
        "initialize response must include serverInfo.name, got: {resp}"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp_home);
}

/// Defense-in-depth: the response line must be raw JSON, not Content-Length
/// framed. A regression that re-introduces Content-Length would still parse
/// as JSON above (after `read_line` strips the header line) only by accident,
/// so check explicitly.
#[test]
fn bridge_response_is_raw_ndjson_not_content_length_framed() {
    let bridge = bridge_binary();
    let tmp_home =
        std::env::temp_dir().join(format!("agend-bridge-framing-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_home).unwrap();

    let mut child = Command::new(&bridge)
        .env("AGEND_HOME", &tmp_home)
        .env("AGEND_INSTANCE_NAME", "test-framing")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bridge");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    writeln!(stdin, "{CLAUDE_CODE_INIT}").expect("write init");
    stdin.flush().expect("flush");

    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        if reader.read_line(&mut line).is_ok() {
            let _ = tx.send(line);
        }
    });

    let line = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("bridge must reply within 5s");

    let trimmed = line.trim_start();
    assert!(
        trimmed.starts_with('{'),
        "response must start with '{{' (NDJSON), not a header. Got: {line:?}"
    );
    assert!(
        !line.contains("Content-Length"),
        "response must not be Content-Length framed (Claude Code 2.x doesn't read it). Got: {line:?}"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&tmp_home);
}
