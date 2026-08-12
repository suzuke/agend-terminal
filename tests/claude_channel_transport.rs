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
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const BRIDGE_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Eq)]
struct TestLocator {
    endpoint: String,
    token: String,
    session_id: String,
}

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

fn spawn_bridge(
    home: &Path,
) -> (
    Child,
    ChildStdin,
    BufReader<std::process::ChildStdout>,
    Arc<Mutex<Vec<u8>>>,
) {
    let mut child = Command::new(binary())
        .arg("channel-bridge")
        .arg("--instance")
        .arg("claude-agent")
        .env("AGEND_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Claude ChannelBridge");
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let stderr = child.stderr.take().unwrap();
    let stderr_capture = Arc::new(Mutex::new(Vec::new()));
    let stderr_capture_thread = Arc::clone(&stderr_capture);
    // Test-only diagnostics reader; the child is stopped and joined by the
    // fixture, so this short-lived reader has no production lifecycle.
    thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut captured = Vec::new();
        let _ = reader.read_to_end(&mut captured);
        *stderr_capture_thread.lock().unwrap() = captured;
    });
    (child, stdin, stdout, stderr_capture)
}

fn read_locator(home: &Path) -> Option<TestLocator> {
    let path = home
        .join("transport")
        .join("sessions")
        .join("claude-agent.json");
    let value: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    Some(TestLocator {
        endpoint: value["endpoint_url"].as_str()?.to_string(),
        token: value["password"].as_str()?.to_string(),
        session_id: value["session_id"].as_str()?.to_string(),
    })
}

fn wait_for_locator(
    home: &Path,
    previous: Option<&TestLocator>,
    child: &mut Child,
    stderr_capture: &Arc<Mutex<Vec<u8>>>,
) -> TestLocator {
    // The fixture can start while the CI nextest profile is compiling or
    // running many binaries; allow process startup headroom while retaining a
    // bounded failure with child status and stderr below.
    let deadline = Instant::now() + BRIDGE_STARTUP_TIMEOUT;
    loop {
        if let Some(locator) = read_locator(home) {
            if previous != Some(&locator) {
                return locator;
            }
        }
        if Instant::now() >= deadline {
            let status = child
                .try_wait()
                .map(|status| format!("{status:?}"))
                .unwrap_or_else(|error| format!("try_wait error: {error}"));
            let stderr_bytes = stderr_capture.lock().unwrap().clone();
            let stderr = String::from_utf8_lossy(&stderr_bytes);
            panic!("Claude locator was not persisted; child status={status}; stderr={stderr:?}");
        }
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
    let deadline = Instant::now() + BRIDGE_STARTUP_TIMEOUT;
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

fn open_sse(endpoint: &str, token: &str, last_event_id: Option<&str>) -> BufReader<TcpStream> {
    let address = endpoint.strip_prefix("http://").unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let cursor = last_event_id
        .map(|value| format!("Last-Event-ID: {value}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "GET /events HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nAccept: text/event-stream\r\nConnection: keep-alive\r\n{cursor}\r\n"
    )
    .unwrap();
    stream.flush().unwrap();
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    assert!(status.contains(" 200 "), "SSE must return 200: {status:?}");
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        if header == "\r\n" || header == "\n" {
            break;
        }
    }
    reader
}

fn read_sse_event(reader: &mut BufReader<TcpStream>) -> Value {
    let mut data = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(!line.is_empty(), "SSE stream closed before an event");
        if line == "\n" || line == "\r\n" {
            if !data.is_empty() {
                return serde_json::from_str(data.trim()).unwrap();
            }
            continue;
        }
        if let Some(value) = line.strip_prefix("data:") {
            data.push_str(value.trim_start());
        }
    }
}

fn stop(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn seed_self_kick_receipt(home: &Path, delivery_id: Uuid, body: &str) {
    let path = home
        .join("transport")
        .join("deliveries")
        .join("claude-agent.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let record = json!({
        "envelope": {
            "delivery_id": delivery_id,
            "instance": "claude-agent",
            "session": {
                "backend": "claude",
                "endpoint": null,
                "thread_id": null,
                "session_id": "seed-session",
                "endpoint_url": "http://127.0.0.1:1",
                "username": "bearer",
                "password": "seed-token",
                "model": null,
                "event_cursor": 0,
                "managed": false,
                "server_pid": null,
                "server_start_token": null
            },
            "kind": "Notification",
            "body": body,
            "correlation_id": null,
            "self_kick": true,
            "created_at": chrono::Utc::now().to_rfc3339(),
            "payload_digest": "seed-digest"
        },
        "receipt": {
            "delivery_id": delivery_id,
            "state": "Queued",
            "payload_digest": "seed-digest",
            "protocol_request_id": null,
            "backend_session_id": null,
            "backend_event": null,
            "tui_visibility": null,
            "detail": null,
            "recorded_at": chrono::Utc::now().to_rfc3339()
        }
    });
    std::fs::write(
        path,
        format!("{}\n", serde_json::to_string(&record).unwrap()),
    )
    .unwrap();
}

#[test]
fn channel_bridge_production_entry_preserves_wire_and_receipts_across_restart() {
    let home = temporary_home();
    let (mut child, mut stdin, mut stdout, stderr_capture) = spawn_bridge(&home);
    let locator = wait_for_locator(&home, None, &mut child, &stderr_capture);
    let endpoint = locator.endpoint.as_str();
    let token = locator.token.as_str();

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
    let health = wait_for_health(endpoint, token);
    assert_eq!(health["ready"], true);
    assert_eq!(health["capabilities"]["claude/channel"], true);

    let delivery_id = Uuid::new_v4();
    let (status, accepted) = http_request(
        endpoint,
        token,
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
        notification["params"]["meta"]["sender_id"],
        "telegram:user-7"
    );
    assert_eq!(
        notification["params"]["meta"]["delivery_id"],
        delivery_id.to_string()
    );

    let mut events = open_sse(endpoint, token, None);

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
    let first_event = read_sse_event(&mut events);
    assert_eq!(first_event["delivery_id"], delivery_id.to_string());
    let first_reply_id = first_event["reply_id"].as_str().unwrap().to_string();
    drop(events);

    let second_delivery_id = Uuid::new_v4();
    let (status, accepted) = http_request(
        endpoint,
        token,
        "POST",
        "/webhook",
        &serde_json::to_vec(&json!({
            "delivery_id": second_delivery_id,
            "chat_id": "chat-43",
            "sender_id": "telegram:user-8",
            "text": "second channel delivery"
        }))
        .unwrap(),
    );
    assert_eq!(status, 202, "second webhook must be accepted: {accepted}");
    let second_notification = read_mcp(&mut stdout);
    assert_eq!(
        second_notification["params"]["meta"]["delivery_id"],
        second_delivery_id.to_string()
    );

    let mut resumed_events = open_sse(endpoint, token, Some(&first_reply_id));
    write_mcp(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0",
            "id":4,
            "method":"tools/call",
            "params": {
                "name":"reply",
                "arguments": {
                    "chat_id":"chat-43",
                    "delivery_id":second_delivery_id,
                    "text":"second reply from Claude"
                }
            }
        }),
    );
    let second_reply = read_mcp(&mut stdout);
    assert_eq!(second_reply["result"]["content"][0]["text"], "sent");
    let second_event = read_sse_event(&mut resumed_events);
    assert_eq!(second_event["delivery_id"], second_delivery_id.to_string());
    assert_ne!(second_event["delivery_id"], first_event["delivery_id"]);
    drop(resumed_events);

    let (status, receipt) = http_request(
        endpoint,
        token,
        "GET",
        &format!("/receipts/{delivery_id}"),
        &[],
    );
    assert_eq!(status, 200);
    assert_eq!(receipt["reply"]["text"], "reply from Claude");

    stop(child);
    drop(stdin);
    let persisted_locator = read_locator(&home).unwrap();

    let (mut child, mut stdin, mut stdout, stderr_capture) = spawn_bridge(&home);
    let restarted_locator =
        wait_for_locator(&home, Some(&persisted_locator), &mut child, &stderr_capture);
    assert_ne!(restarted_locator, persisted_locator);
    assert_ne!(restarted_locator.token, persisted_locator.token);
    assert_ne!(restarted_locator.session_id, persisted_locator.session_id);
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
        &restarted_locator.endpoint,
        &restarted_locator.token,
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

#[test]
fn channel_bridge_production_entry_self_kick_ack_is_correlated_and_non_replying() {
    let home = temporary_home();
    let delivery_id = Uuid::new_v4();
    let body = "[AGEND-RESUME] recover own state";
    seed_self_kick_receipt(&home, delivery_id, body);
    let (mut child, mut stdin, mut stdout, stderr_capture) = spawn_bridge(&home);
    let locator = wait_for_locator(&home, None, &mut child, &stderr_capture);
    let endpoint = locator.endpoint.as_str();
    let token = locator.token.as_str();

    write_mcp(
        &mut stdin,
        &json!({
            "jsonrpc":"2.0", "id":1, "method":"initialize",
            "params":{"clientInfo":{"name":"claude-code","version":"2.1.89"}}
        }),
    );
    let _ = read_mcp(&mut stdout);
    let _ = wait_for_health(endpoint, token);
    let (status, _) = http_request(
        endpoint,
        token,
        "POST",
        "/webhook",
        &serde_json::to_vec(&json!({
            "delivery_id": delivery_id,
            "chat_id": format!("agend:claude-agent:{delivery_id}"),
            "sender_id": "agend-terminal",
            "text": body
        }))
        .unwrap(),
    );
    assert_eq!(status, 202);
    let notification = read_mcp(&mut stdout);
    assert_eq!(
        notification["params"]["meta"]["delivery_id"],
        delivery_id.to_string()
    );

    write_mcp(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"ack_start","arguments":{"delivery_id":Uuid::new_v4()}
        }}),
    );
    let wrong = read_mcp(&mut stdout);
    assert_eq!(wrong["error"]["code"], -32602);

    write_mcp(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
            "name":"ack_start","arguments":{"delivery_id":delivery_id}
        }}),
    );
    let started = read_mcp(&mut stdout);
    assert_eq!(
        started["result"]["structuredContent"]["state"],
        "TurnStarted"
    );
    let (status, receipt) = http_request(
        endpoint,
        token,
        "GET",
        &format!("/receipts/{delivery_id}"),
        &[],
    );
    assert_eq!(status, 200);
    assert_eq!(receipt["state"], "TurnStarted");
    assert!(
        receipt["reply"].is_null(),
        "start ack must not create reply receipt"
    );

    write_mcp(
        &mut stdin,
        &json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{
            "name":"ack_complete","arguments":{"delivery_id":delivery_id}
        }}),
    );
    let completed = read_mcp(&mut stdout);
    assert_eq!(
        completed["result"]["structuredContent"]["state"],
        "Completed"
    );
    let (status, receipt) = http_request(
        endpoint,
        token,
        "GET",
        &format!("/receipts/{delivery_id}"),
        &[],
    );
    assert_eq!(status, 200);
    assert_eq!(receipt["state"], "Completed");
    assert!(
        receipt["reply"].is_null(),
        "completion ack must not create reply receipt"
    );
    stop(child);
    drop(stdin);
    let _ = std::fs::remove_dir_all(home);
}
