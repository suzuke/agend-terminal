//! Signed RED coverage for #3411 / #3405 Phase 0a.
//!
//! These tests intentionally describe the post-implementation contract while
//! the generic CLI, shared wire classifier, and fence are absent from the
//! current-main baseline. They must fail before production implementation and
//! pass through real binary/bridge entry points afterward.

#![allow(clippy::unwrap_used)]

use assert_cmd::Command;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Output, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const RESPONSES: &str = include_str!("fixtures/tool-cli/daemon-responses.json");

fn response(name: &str) -> Value {
    serde_json::from_str::<Value>(RESPONSES)
        .unwrap()
        .get(name)
        .cloned()
        .unwrap_or_else(|| panic!("fixture response {name:?} must exist"))
}

/// A minimal authenticated daemon endpoint. It records the exact request
/// envelope before returning the supplied response, so the CLI test verifies
/// the real subprocess boundary rather than an injected helper.
struct MockDaemon {
    home: PathBuf,
    requests: Arc<Mutex<Vec<Value>>>,
    join: Option<thread::JoinHandle<()>>,
}

impl MockDaemon {
    fn new(response: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let home =
            std::env::temp_dir().join(format!("agend-tool-cli-red-{}-{port}", std::process::id()));
        let run = home.join("run").join(std::process::id().to_string());
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("api.port"), port.to_string()).unwrap();
        // The bridge reads api.cookie; the operator CLI reads api.operator.
        std::fs::write(run.join("api.cookie"), [0x42u8; 32]).unwrap();
        std::fs::write(run.join("api.operator"), [0x24u8; 32]).unwrap();

        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let join = thread::spawn(move || {
            // The bridge is a separately linked binary; allow bounded startup
            // time without making a missing-client test hang indefinitely.
            let deadline = Instant::now() + Duration::from_secs(5);
            let (stream, _) = loop {
                match listener.accept() {
                    Ok(pair) => {
                        break pair;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return;
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            };
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
            let writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut writer = writer;

            let mut auth = String::new();
            if reader.read_line(&mut auth).unwrap_or(0) == 0 {
                return;
            }
            writeln!(writer, r#"{{"ok":true}}"#).unwrap();
            writer.flush().unwrap();

            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            if let Ok(request) = serde_json::from_str::<Value>(line.trim()) {
                captured.lock().unwrap().push(request);
            }
            writeln!(writer, "{response}").unwrap();
            writer.flush().unwrap();
        });

        Self {
            home,
            requests,
            join: Some(join),
        }
    }

    fn finish(mut self) -> Vec<Value> {
        if let Some(join) = self.join.take() {
            join.join().unwrap();
        }
        let requests = self.requests.lock().unwrap().clone();
        let _ = std::fs::remove_dir_all(&self.home);
        requests
    }
}

impl Drop for MockDaemon {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn run_tool(home: &std::path::Path, args: &[&str]) -> Output {
    let mut command = Command::cargo_bin("agend-terminal").unwrap();
    command
        .env("AGEND_HOME", home)
        .env("AGEND_INSTANCE_NAME", "red-cli")
        .args(args)
        .output()
        .unwrap()
}

fn combined(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_cli_envelope(requests: &[Value], tool: &str) {
    assert_eq!(
        requests.len(),
        1,
        "CLI must make exactly one daemon request"
    );
    let request = &requests[0];
    assert_eq!(request["method"], "mcp_tool");
    assert_eq!(request["params"]["tool"], tool);
    assert_eq!(request["params"]["transport"], "cli");
    assert!(
        request["request_id"].as_str().is_some(),
        "CLI must add a UUID request_id at the daemon envelope boundary: {request}"
    );
}

fn assert_list_envelope(requests: &[Value]) {
    assert_eq!(
        requests.len(),
        1,
        "schema lookup must make exactly one request"
    );
    let request = &requests[0];
    assert_eq!(request["method"], "mcp_tools_list");
    assert_eq!(request["params"]["instance"], "red-cli");
    assert!(
        request["request_id"].as_str().is_some(),
        "schema lookup must add a UUID request_id: {request}"
    );
}

#[derive(Debug)]
enum DisconnectEvent {
    First,
    Second,
    NoFirst,
    NoSecond,
}

struct PostWriteDisconnectDaemon {
    home: PathBuf,
    events: mpsc::Receiver<DisconnectEvent>,
    join: Option<thread::JoinHandle<Vec<Value>>>,
}

impl PostWriteDisconnectDaemon {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let home = std::env::temp_dir().join(format!(
            "agend-tool-cli-post-write-{}-{port}",
            std::process::id()
        ));
        let run = home.join("run").join(std::process::id().to_string());
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join("api.port"), port.to_string()).unwrap();
        std::fs::write(run.join("api.cookie"), [0x42u8; 32]).unwrap();
        std::fs::write(run.join("api.operator"), [0x24u8; 32]).unwrap();

        let (event_sender, events) = mpsc::channel();
        let join = thread::spawn(move || {
            let mut requests = Vec::new();
            let first = accept_until(&listener, Duration::from_secs(5));
            let Some(first) = first else {
                event_sender.send(DisconnectEvent::NoFirst).ok();
                return requests;
            };
            let Some(request) = read_authenticated_request(first, None) else {
                event_sender.send(DisconnectEvent::NoFirst).ok();
                return requests;
            };
            requests.push(request);
            event_sender.send(DisconnectEvent::First).ok();

            let second = accept_until(&listener, Duration::from_secs(1));
            let Some(second) = second else {
                event_sender.send(DisconnectEvent::NoSecond).ok();
                return requests;
            };
            if let Some(request) = read_authenticated_request(
                second,
                Some(r#"{"ok":true,"result":{"status":"completed"}}"#),
            ) {
                requests.push(request);
                event_sender.send(DisconnectEvent::Second).ok();
            } else {
                event_sender.send(DisconnectEvent::NoSecond).ok();
            }
            requests
        });

        Self {
            home,
            events,
            join: Some(join),
        }
    }

    fn finish(mut self) -> (Vec<Value>, Vec<DisconnectEvent>) {
        let requests = self.join.take().unwrap().join().unwrap();
        let events = self.events.try_iter().collect();
        let _ = std::fs::remove_dir_all(&self.home);
        (requests, events)
    }
}

impl Drop for PostWriteDisconnectDaemon {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn accept_until(listener: &TcpListener, timeout: Duration) -> Option<std::net::TcpStream> {
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, _)) => return Some(stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return None;
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

fn read_authenticated_request(
    stream: std::net::TcpStream,
    response: Option<&str>,
) -> Option<Value> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    let mut writer = stream.try_clone().ok()?;
    let mut reader = BufReader::new(stream);
    let mut auth = String::new();
    if reader.read_line(&mut auth).ok()? == 0 {
        return None;
    }
    writeln!(writer, r#"{{"ok":true}}"#).ok()?;
    writer.flush().ok()?;
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let request = serde_json::from_str(line.trim()).ok()?;
    if let Some(response) = response {
        writeln!(writer, "{response}").ok()?;
        writer.flush().ok()?;
    }
    Some(request)
}

#[test]
fn unknown_tool_is_a_tool_error_and_forwards_cli_transport() {
    let daemon = MockDaemon::new(response("unknown_tool"));
    let home = daemon.home.clone();
    let output = run_tool(
        &home,
        &[
            "tool",
            "definitely_missing_tool",
            "--json",
            "{}",
            "--home",
            home.to_str().unwrap(),
        ],
    );
    let requests = daemon.finish();

    assert_eq!(
        output.status.code(),
        Some(1),
        "ToolError must exit 1: {}",
        combined(&output)
    );
    assert!(
        combined(&output).contains("unknown tool"),
        "ToolError must remain visible: {}",
        combined(&output)
    );
    assert_cli_envelope(&requests, "definitely_missing_tool");
}

#[test]
fn accepted_in_progress_is_success_and_preserves_status() {
    let daemon = MockDaemon::new(response("accepted_in_progress"));
    let home = daemon.home.clone();
    let output = run_tool(
        &home,
        &[
            "tool",
            "send",
            "--json",
            r#"{"message":"red"}"#,
            "--home",
            home.to_str().unwrap(),
        ],
    );
    let requests = daemon.finish();

    assert_eq!(
        output.status.code(),
        Some(0),
        "Accepted must exit 0: {}",
        combined(&output)
    );
    assert!(
        combined(&output).contains("accepted_in_progress"),
        "Accepted status must be printed, not discarded: {}",
        combined(&output)
    );
    assert_cli_envelope(&requests, "send");
}

#[test]
fn missing_runtime_config_keeps_cli_fence_closed() {
    let daemon = MockDaemon::new(response("fence_closed"));
    let home = daemon.home.clone();
    assert!(
        !home.join("runtime-config.json").exists(),
        "fixture must exercise config absence, not an explicit false file"
    );
    let output = run_tool(
        &home,
        &[
            "tool",
            "inbox",
            "--json",
            "{}",
            "--home",
            home.to_str().unwrap(),
        ],
    );
    let requests = daemon.finish();

    assert_eq!(
        output.status.code(),
        Some(2),
        "Refused must exit 2: {}",
        combined(&output)
    );
    assert!(
        combined(&output).contains("tool CLI disabled"),
        "fence refusal must remain actionable: {}",
        combined(&output)
    );
    assert_cli_envelope(&requests, "inbox");
}

#[test]
fn usage_error_is_exit_three() {
    let home =
        std::env::temp_dir().join(format!("agend-tool-cli-red-usage-{}", std::process::id()));
    let output = run_tool(&home, &["tool"]);
    let _ = std::fs::remove_dir_all(&home);

    assert_eq!(
        output.status.code(),
        Some(3),
        "usage must exit 3: {}",
        combined(&output)
    );
    assert!(
        combined(&output).contains("tool") && combined(&output).contains("Usage"),
        "usage diagnostics must identify the command: {}",
        combined(&output)
    );
}

#[test]
fn transport_failure_is_indeterminate_exit_four() {
    let home = std::env::temp_dir().join(format!(
        "agend-tool-cli-red-transport-{}",
        std::process::id()
    ));
    let output = run_tool(
        &home,
        &[
            "tool",
            "inbox",
            "--json",
            "{}",
            "--home",
            home.to_str().unwrap(),
        ],
    );
    let _ = std::fs::remove_dir_all(&home);

    assert_eq!(
        output.status.code(),
        Some(4),
        "unknown transport outcome must exit 4: {}",
        combined(&output)
    );
    assert!(
        combined(&output).contains("indeterminate")
            || combined(&output).contains("state check")
            || combined(&output).contains("daemon"),
        "exit 4 must tell the operator to inspect state before resend: {}",
        combined(&output)
    );
}

#[test]
fn post_write_disconnect_is_indeterminate_without_cli_replay() {
    let daemon = PostWriteDisconnectDaemon::new();
    let home = daemon.home.clone();
    let output = run_tool(
        &home,
        &[
            "tool",
            "send",
            "--json",
            r#"{"message":"ambiguous"}"#,
            "--home",
            home.to_str().unwrap(),
        ],
    );
    let (requests, events) = daemon.finish();

    assert!(
        events
            .iter()
            .any(|event| matches!(event, DisconnectEvent::First)),
        "listener must receive the first request: {events:?}"
    );
    if let Some(second) = requests.get(1) {
        assert_eq!(
            requests[0]["request_id"], second["request_id"],
            "an observed retry must preserve request_id for deduplication; requests: {requests:?}"
        );
    }
    assert_eq!(
        requests.len(),
        1,
        "CLI must not replay after a post-write disconnect; captured requests: {requests:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, DisconnectEvent::NoSecond)),
        "listener must complete the bounded no-replay observation: {events:?}"
    );
    assert_eq!(
        output.status.code(),
        Some(4),
        "post-write disconnect must be indeterminate: {}",
        combined(&output)
    );
    assert!(
        combined(&output).contains("indeterminate"),
        "exit 4 must identify the outcome as indeterminate: {}",
        combined(&output)
    );
    let request_id = requests[0]["request_id"]
        .as_str()
        .expect("CLI request must carry a request_id");
    uuid::Uuid::parse_str(request_id).expect("CLI request_id must be a UUID");
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn bridge_binary() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_agend-mcp-bridge"));
    assert!(
        path.exists(),
        "bridge binary must exist at {}",
        path.display()
    );
    path
}

fn read_ndjson(reader: &mut BufReader<std::process::ChildStdout>) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(line.trim()).unwrap()
}

#[test]
fn bridge_accepted_in_progress_is_not_serialized_as_null() {
    let daemon = MockDaemon::new(response("accepted_in_progress"));
    let home = daemon.home.clone();
    let mut child = ChildGuard(
        std::process::Command::new(bridge_binary())
            .env("AGEND_HOME", &home)
            .env("AGEND_INSTANCE_NAME", "red-bridge")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut stdin = child.0.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.0.stdout.take().unwrap());

    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"red","version":"1"}}}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"send","arguments":{{"message":"red"}}}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    let _init = read_ndjson(&mut stdout);
    let call = read_ndjson(&mut stdout);
    drop(stdin);
    let _ = child.0.wait();
    let requests = daemon.finish();

    let text = call["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("bridge response must have MCP text content: {call}"));
    assert_ne!(
        text, "null",
        "accepted_in_progress must not become JSON null"
    );
    let value: Value = serde_json::from_str(text).unwrap();
    assert_eq!(value["status"], "accepted_in_progress");
    assert_eq!(requests.len(), 1, "bridge must forward exactly one call");
}

#[test]
fn packaged_tarball_reaches_shared_wire_and_cli_modules() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for path in ["src/mcp_wire.rs", "src/tool_cli.rs"] {
        assert!(
            root.join(path).is_file(),
            "production module {path} must exist before package reachability can pass"
        );
    }

    let output = std::process::Command::new("cargo")
        .current_dir(&root)
        .args(["package", "--list", "--allow-dirty"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "cargo package --list must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listing = String::from_utf8_lossy(&output.stdout);
    for path in ["src/mcp_wire.rs", "src/tool_cli.rs"] {
        assert!(
            listing.lines().any(|line| line.trim() == path),
            "packaged source must contain {path}; listing was:\n{listing}"
        );
    }
}

#[test]
fn schema_name_filters_live_role_filtered_tool_list() {
    let daemon = MockDaemon::new(response("tool_list"));
    let home = daemon.home.clone();
    let output = run_tool(
        &home,
        &["tool", "schema", "send", "--home", home.to_str().unwrap()],
    );
    let requests = daemon.finish();

    assert_eq!(
        output.status.code(),
        Some(0),
        "schema lookup must succeed: {}",
        combined(&output)
    );
    assert!(
        combined(&output).contains("Send a message to an instance")
            && combined(&output).contains("\"name\": \"send\""),
        "schema must be selected from the live list, not synthesized: {}",
        combined(&output)
    );
    assert!(
        !combined(&output).contains("Read pending messages"),
        "schema must return only the requested live definition: {}",
        combined(&output)
    );
    assert_list_envelope(&requests);
}

#[test]
fn fixture_covers_the_wire_outcome_partition() {
    let fixture: Value = serde_json::from_str(RESPONSES).unwrap();
    for name in ["ok", "unknown_tool", "fence_closed", "accepted_in_progress"] {
        assert!(fixture.get(name).is_some(), "missing RED fixture {name}");
    }
    assert_eq!(fixture["ok"]["ok"], true);
    assert_eq!(fixture["unknown_tool"]["ok"], true);
    assert_eq!(fixture["fence_closed"]["ok"], false);
    assert_eq!(
        fixture["accepted_in_progress"]["status"],
        "accepted_in_progress"
    );
    assert!(fixture["accepted_in_progress"].get("result").is_none());
}
