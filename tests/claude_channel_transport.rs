//! Production-entry fixtures for the Claude ChannelBridge transport.
//!
//! These tests deliberately speak the two wire protocols directly: MCP
//! newline-delimited JSON frames on the child stdin/stdout and authenticated HTTP on
//! the loopback bridge. This catches integration regressions that unit tests
//! over the JSON helpers cannot see.

#![allow(clippy::unwrap_used)]

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_agend-terminal"))
}

fn temporary_home() -> PathBuf {
    let home = std::env::temp_dir().join(format!(
        "agend-claude-channel-e2e-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    std::fs::create_dir_all(&home).unwrap();
    home
}

fn spawn_bridge(home: &Path) -> (Child, ChildStdin, BufReader<std::process::ChildStdout>) {
    let mut child = Command::new(binary())
        .arg("channel-bridge")
        .arg("--instance")
        .arg("claude-agent")
        .env("AGEND_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn Claude ChannelBridge");
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    (child, stdin, stdout)
}

fn read_locator(home: &Path) -> Option<(String, String)> {
    let path = home
        .join("transport")
        .join("sessions")
        .join("claude-agent.json");
    let value: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    Some((
        value["endpoint_url"].as_str()?.to_string(),
        value["password"].as_str()?.to_string(),
    ))
}

fn wait_for_locator(home: &Path) -> (String, String) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(locator) = read_locator(home) {
            return locator;
        }
        assert!(
            Instant::now() < deadline,
            "Claude locator was not persisted"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn write_mcp(stdin: &mut ChildStdin, value: &Value) {
    serde_json::to_writer(&mut *stdin, value).unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
}

fn read_mcp(reader: &mut BufReader<std::process::ChildStdout>) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(
        line.trim_start().starts_with('{'),
        "expected NDJSON, got {line:?}"
    );
    serde_json::from_str(line.trim()).unwrap()
}

fn http_request(
    endpoint: &str,
    token: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> (u16, Value) {
    let address = endpoint.strip_prefix("http://").unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let header = std::str::from_utf8(&response[..header_end]).unwrap();
    let status = header
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let value = serde_json::from_slice(&response[header_end + 4..]).unwrap_or(Value::Null);
    (status, value)
}

fn wait_for_health(endpoint: &str, token: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(mut stream) = TcpStream::connect(endpoint.strip_prefix("http://").unwrap()) {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
            let request = format!(
                "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(request.as_bytes());
            let mut response = Vec::new();
            let _ = stream.read_to_end(&mut response);
            if let Some(header_end) = response.windows(4).position(|window| window == b"\r\n\r\n") {
                if response.starts_with(b"HTTP/1.1 200") {
                    if let Ok(value) = serde_json::from_slice(&response[header_end + 4..]) {
                        return value;
                    }
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "Claude ChannelBridge never became healthy"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn stop(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn channel_bridge_production_entry_preserves_wire_and_receipts_across_restart() {
    let home = temporary_home();
    let (child, mut stdin, mut stdout) = spawn_bridge(&home);
    let (endpoint, token) = wait_for_locator(&home);

    write_mcp(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "clientInfo": {"name": "claude-code", "version": "2.1.89"}
            }
        }),
    );
    let initialize = read_mcp(&mut stdout);
    assert_eq!(initialize["id"], 1);
    assert_eq!(initialize["result"]["capabilities"]["tools"], json!({}));
    assert_eq!(
        initialize["result"]["capabilities"]["experimental"]["claude/channel"],
        json!({})
    );
    write_mcp(
        &mut stdin,
        &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    );
    let health = wait_for_health(&endpoint, &token);
    assert_eq!(health["ready"], true);
    assert_eq!(health["capabilities"]["claude/channel"], true);

    let delivery_id = Uuid::new_v4();
    let (status, accepted) = http_request(
        &endpoint,
        &token,
        "POST",
        "/webhook",
        &serde_json::to_vec(&json!({
            "delivery_id": delivery_id,
            "chat_id": "chat-42",
            "sender_id": "telegram:user-7",
            "text": "hello from the channel"
        }))
        .unwrap(),
    );
    assert_eq!(status, 202, "webhook must be accepted: {accepted}");
    assert_eq!(accepted["delivery_id"], delivery_id.to_string());

    let notification = read_mcp(&mut stdout);
    assert_eq!(notification["method"], "notifications/claude/channel");
    assert_eq!(notification["params"]["content"], "hello from the channel");
    assert_eq!(notification["params"]["meta"]["chat_id"], "chat-42");
    assert_eq!(
        notification["params"]["meta"]["delivery_id"],
        delivery_id.to_string()
    );

    write_mcp(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"tools/call",
            "params": {
                "name":"reply",
                "arguments": {
                    "chat_id":"chat-42",
                    "delivery_id":delivery_id,
                    "text":"reply from Claude"
                }
            }
        }),
    );
    let reply = read_mcp(&mut stdout);
    assert_eq!(reply["result"]["content"][0]["text"], "sent");
    assert_eq!(
        reply["result"]["structuredContent"]["delivery_id"],
        delivery_id.to_string()
    );
    let (status, receipt) = http_request(
        &endpoint,
        &token,
        "GET",
        &format!("/receipts/{delivery_id}"),
        &[],
    );
    assert_eq!(status, 200);
    assert_eq!(receipt["reply"]["text"], "reply from Claude");

    stop(child);
    drop(stdin);
    let persisted_locator = read_locator(&home).unwrap();

    let (child, mut stdin, mut stdout) = spawn_bridge(&home);
    let restarted_locator = wait_for_locator(&home);
    assert_eq!(restarted_locator, persisted_locator);
    write_mcp(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":3,
            "method":"initialize",
            "params":{"clientInfo":{"name":"claude-code","version":"2.1.89"}}
        }),
    );
    let _ = read_mcp(&mut stdout);
    let (status, receipt) = http_request(
        &restarted_locator.0,
        &restarted_locator.1,
        "GET",
        &format!("/receipts/{delivery_id}"),
        &[],
    );
    assert_eq!(status, 200, "reply receipt must survive bridge restart");
    assert_eq!(receipt["reply"]["text"], "reply from Claude");
    stop(child);
    drop(stdin);
    let _ = std::fs::remove_dir_all(home);
}
