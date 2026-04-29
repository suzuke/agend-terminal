//! §3.5.10 wire-format external fixture for `pane_snapshot` MCP tool.
//!
//! Exercises the full MCP subprocess wire path (`agend-terminal mcp`)
//! to pin the JSON response shape that crosses process boundaries.
//! Error-path tests go through the real MCP wire; success-path shape
//! is pinned in `src/api/handlers/instance.rs::tests` where a real
//! agent with deterministic VTerm content is available.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

const INIT_REQUEST: &str = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;

fn binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop();
    path.pop();
    path.push("agend-terminal");
    path
}

fn temp_home(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-pane-snap-{}-{}-{}",
        std::process::id(),
        label,
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn mcp_call(home: &std::path::Path, tool: &str, args: &Value) -> Value {
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": args }
    });
    let mut child = Command::new(binary())
        .args(["mcp"])
        .env("AGEND_HOME", home)
        .env("AGEND_TEST_ISOLATION", "1")
        .env("AGEND_INSTANCE_NAME", "test-snap")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mcp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    writeln!(stdin, "{INIT_REQUEST}").expect("write init");
    writeln!(stdin, "{}", req).expect("write req");
    drop(stdin);

    let reader = BufReader::new(stdout);
    let mut responses = Vec::new();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            responses.push(v);
        }
    }
    child.wait().ok();

    if responses.len() < 2 {
        return json!({"error": "no response"});
    }
    let text = responses[1]["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    serde_json::from_str(text).unwrap_or_else(|_| json!({"raw": text}))
}

/// §3.5.10 wire-format: lines > 10000 returns operator-actionable error.
#[test]
fn pane_snapshot_lines_exceeds_max_returns_error() {
    let home = temp_home("lines-max");
    let result = mcp_call(
        &home,
        "pane_snapshot",
        &json!({"target": "any", "lines": 10001}),
    );
    let err = result["error"].as_str().unwrap_or("");
    assert!(
        err.contains("10000"),
        "lines > 10000 must return error mentioning limit, got: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// §3.5.10 wire-format: missing target returns error.
#[test]
fn pane_snapshot_missing_target_returns_error() {
    let home = temp_home("missing-target");
    let result = mcp_call(&home, "pane_snapshot", &json!({}));
    assert!(
        result.get("error").is_some(),
        "missing target must return error, got: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// §3.5.10 wire-format: nonexistent target returns error.
#[test]
fn pane_snapshot_nonexistent_target_returns_error() {
    let home = temp_home("no-target");
    let result = mcp_call(&home, "pane_snapshot", &json!({"target": "ghost-agent"}));
    assert!(
        result.get("error").is_some(),
        "nonexistent target must return error, got: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}
