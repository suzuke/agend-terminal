//! Integration tests — spawn daemon as subprocess, test via TCP API port.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

/// Shell binary used as the dummy long-running process for each agent.
/// Both `/bin/bash` (Unix) and `cmd.exe` (Windows) sit in the default PATH
/// and block on stdin when spawned under a PTY, which is all these tests
/// need from the agent — they exercise daemon lifecycle, not shell syntax.
#[cfg(windows)]
const SHELL_BIN: &str = "cmd.exe";
#[cfg(not(windows))]
const SHELL_BIN: &str = "/bin/bash";

fn binary() -> PathBuf {
    // Use debug build
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop(); // remove test binary name
    path.pop(); // remove deps/
    path.push("agend-terminal");
    path
}

struct TestDaemon {
    child: Child,
    home: PathBuf,
}

impl TestDaemon {
    fn start(name: &str) -> Self {
        let agent = format!("shell:{SHELL_BIN}");
        Self::start_with_agents(name, vec![agent.as_str()])
    }

    fn start_with_agents(name: &str, agents: Vec<&str>) -> Self {
        let home =
            std::env::temp_dir().join(format!("agend-integ-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create home");

        let mut args: Vec<&str> = vec!["daemon"];
        args.extend(agents);

        let child = Command::new(binary())
            .args(&args)
            .env("AGEND_TEST_ISOLATION", "1")
            .env("AGEND_HOME", &home)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn daemon");

        // Wait for API port file
        let mut found = false;
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(200));
            if Self::find_api_port(&home).is_some() {
                found = true;
                break;
            }
        }
        assert!(found, "daemon API port not published after 6s");

        TestDaemon { child, home }
    }

    fn find_api_port(home: &Path) -> Option<u16> {
        let run = home.join("run");
        if !run.exists() {
            return None;
        }
        for entry in std::fs::read_dir(&run).ok()?.flatten() {
            let port_path = entry.path().join("api.port");
            if let Ok(contents) = std::fs::read_to_string(&port_path) {
                if let Ok(port) = contents.trim().parse::<u16>() {
                    return Some(port);
                }
            }
        }
        None
    }

    /// Read the 32-byte cookie the daemon published in its run dir. Tests
    /// speak raw TCP so they must present it manually (unlike production
    /// clients which go through `ipc::connect_api`/`auth_cookie`).
    fn find_api_cookie(home: &Path) -> Option<Vec<u8>> {
        let run = home.join("run");
        for entry in std::fs::read_dir(&run).ok()?.flatten() {
            let p = entry.path().join("api.cookie");
            if let Ok(bytes) = std::fs::read(&p) {
                if bytes.len() == 32 {
                    return Some(bytes);
                }
            }
        }
        None
    }

    /// Open a TCP connection to the API port and complete the NDJSON cookie
    /// handshake. Returns the reader/writer pair primed for the first real
    /// request.
    fn connect_authed(&self) -> (BufReader<TcpStream>, TcpStream) {
        let port = Self::find_api_port(&self.home).expect("api port");
        let cookie = Self::find_api_cookie(&self.home).expect("api.cookie");
        let stream =
            TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).expect("connect");
        stream.set_nodelay(true).ok();
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);
        let hex: String = cookie.iter().map(|b| format!("{b:02x}")).collect();
        writeln!(writer, "{{\"auth\":\"{}\"}}", hex).expect("write auth");
        writer.flush().expect("flush auth");
        let mut line = String::new();
        reader.read_line(&mut line).expect("read auth reply");
        let resp: serde_json::Value = serde_json::from_str(line.trim()).expect("parse auth reply");
        assert_eq!(resp["ok"], true, "auth handshake failed: {resp}");
        (reader, writer)
    }

    fn api_call(&self, request: &serde_json::Value) -> serde_json::Value {
        let (mut reader, mut writer) = self.connect_authed();
        writeln!(writer, "{}", request).expect("write");
        writer.flush().expect("flush");
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        serde_json::from_str(line.trim()).expect("parse response")
    }

    /// Send a raw line (not pre-serialised JSON) and read one NDJSON response.
    /// Used to probe the parse-error path — `api_call` can only send valid JSON.
    fn api_call_raw(&self, raw_line: &str) -> serde_json::Value {
        let (mut reader, mut writer) = self.connect_authed();
        writeln!(writer, "{}", raw_line).expect("write");
        writer.flush().expect("flush");
        let mut line = String::new();
        reader.read_line(&mut line).expect("read");
        serde_json::from_str(line.trim()).expect("parse response")
    }

    fn stop(&mut self) {
        let _ = self.api_call(&serde_json::json!({"method": "shutdown"}));
        // Wait for daemon to exit
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(200));
            if self.child.try_wait().ok().flatten().is_some() {
                break;
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[test]
fn test_daemon_list_and_status() {
    let mut daemon = TestDaemon::start("list");
    let resp = daemon.api_call(&serde_json::json!({"method": "list"}));
    assert_eq!(resp["ok"], true);
    let agents = resp["result"]["agents"].as_array().expect("agents");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["name"], "shell");
    daemon.stop();
}

#[test]
fn test_api_error_paths() {
    // Consolidated happy-path + error-path coverage for the JSON API so we
    // don't pay one daemon-startup-cost per assertion.
    let mut daemon = TestDaemon::start("api_errors");

    // Malformed JSON → parse-error response, socket stays live.
    let resp = daemon.api_call_raw("{this-is-not-json");
    assert_eq!(resp["ok"], false, "parse error should set ok=false");
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains("parse"),
        "expected parse error, got: {resp}"
    );

    // Unknown method → the dispatch falls through to the default branch.
    // The test doesn't pin the exact error text (that can evolve); it only
    // asserts ok=false.
    let resp = daemon.api_call(&serde_json::json!({"method": "definitely_not_a_method"}));
    assert_eq!(resp["ok"], false, "unknown method should set ok=false");

    // INJECT with an invalid name (contains "/") — validate_name rejects.
    let resp = daemon.api_call(&serde_json::json!({
        "method": "inject",
        "params": {"name": "bad/name", "data": "x"}
    }));
    assert_eq!(resp["ok"], false);
    assert!(
        resp["error"].as_str().is_some(),
        "expected validation error text, got: {resp}"
    );

    // DELETE an unknown agent — the delete dispatcher removes from external
    // first (no-op), then from managed (no-op). The current implementation
    // returns ok:true even when nothing was there, which matches the
    // intentionally-idempotent semantics documented at the call site. Lock
    // that behaviour in as a test so a future change to strict-mode delete
    // is forced to revisit callers.
    let resp =
        daemon.api_call(&serde_json::json!({"method": "delete", "params": {"name": "ghost"}}));
    assert_eq!(
        resp["ok"], true,
        "delete of unknown name is idempotent; got: {resp}"
    );

    // SEND with from == target — self-send is rejected.
    let resp = daemon.api_call(&serde_json::json!({
        "method": "send",
        "params": {"from": "shell", "target": "shell", "text": "hi"}
    }));
    assert_eq!(resp["ok"], false, "self-send must be rejected");
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains("self"),
        "expected self-send error, got: {resp}"
    );

    // SPAWN for an already-registered name → dedup rejection.
    let resp = daemon.api_call(&serde_json::json!({
        "method": "spawn",
        "params": {"name": "shell", "backend": "shell:/bin/sh"}
    }));
    assert_eq!(resp["ok"], false, "duplicate spawn must be rejected");
    assert!(
        resp["error"].as_str().unwrap_or("").contains("exists"),
        "expected 'already exists' error, got: {resp}"
    );

    daemon.stop();
}

/// P1-10: verify the daemon rejects a TCP peer that cannot present the
/// cookie (no auth / bad auth). Complements the unit tests in
/// `src/auth_cookie.rs` — this exercises the full end-to-end path including
/// the TCP listener and the `handle_session` gate.
#[test]
fn test_api_rejects_connection_without_cookie() {
    let mut daemon = TestDaemon::start("auth_missing");
    let port = TestDaemon::find_api_port(&daemon.home).expect("api port");
    let mut stream =
        TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).expect("connect");
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    // Send a command line directly — no {"auth":...} first. Server's first-line
    // handshake must treat this as malformed/missing auth and close.
    writeln!(stream, r#"{{"method":"list"}}"#).expect("write");
    stream.flush().expect("flush");
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read reply");
    let resp: serde_json::Value = serde_json::from_str(line.trim()).expect("parse");
    assert_eq!(
        resp["ok"], false,
        "server must reject unauthenticated request, got: {resp}"
    );
    // Second read should see EOF — server closed after the auth failure.
    let mut tail = String::new();
    let n = reader.read_line(&mut tail).expect("second read");
    assert_eq!(n, 0, "server should close after auth failure; got: {tail}");
    daemon.stop();
}

#[test]
fn test_api_rejects_connection_with_wrong_cookie() {
    let mut daemon = TestDaemon::start("auth_wrong");
    let port = TestDaemon::find_api_port(&daemon.home).expect("api port");
    let mut stream =
        TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).expect("connect");
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    // 64 hex chars of a cookie the daemon never issued.
    let fake_hex = "a".repeat(64);
    writeln!(stream, r#"{{"auth":"{}"}}"#, fake_hex).expect("write");
    stream.flush().expect("flush");
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read reply");
    let resp: serde_json::Value = serde_json::from_str(line.trim()).expect("parse");
    assert_eq!(resp["ok"], false, "wrong cookie must be rejected");
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains("auth"),
        "expected auth error, got: {resp}"
    );
    daemon.stop();
}

#[test]
fn test_crash_respawn_health() {
    let mut daemon = TestDaemon::start("crash");

    // Verify agent exists
    let resp = daemon.api_call(&serde_json::json!({"method": "list"}));
    assert_eq!(resp["result"]["agents"].as_array().expect("a").len(), 1);

    // Kill agent (triggers crash)
    let resp = daemon.api_call(&serde_json::json!({"method": "kill", "params": {"name": "shell"}}));
    assert_eq!(resp["ok"], true);

    // Immediately after kill — agent should be in Restarting state
    std::thread::sleep(Duration::from_millis(500));
    let resp = daemon.api_call(&serde_json::json!({"method": "list"}));
    let agents = resp["result"]["agents"].as_array().expect("a");
    if !agents.is_empty() {
        let state = agents[0]["agent_state"].as_str().unwrap_or("");
        assert!(
            state == "restarting" || state == "starting",
            "expected restarting or starting, got: {state}"
        );
    }

    // Wait for respawn (backoff 5s + spawn time)
    std::thread::sleep(Duration::from_secs(8));

    // Agent should be back
    let resp = daemon.api_call(&serde_json::json!({"method": "list"}));
    let agents = resp["result"]["agents"].as_array().expect("a");
    assert_eq!(agents.len(), 1, "agent should have respawned");
    assert_eq!(agents[0]["name"], "shell");

    // Health state should be healthy (respawn_ok called)
    let health = agents[0]["health_state"].as_str().unwrap_or("");
    assert_eq!(
        health, "healthy",
        "health should be healthy after respawn_ok"
    );

    daemon.stop();
}

#[test]
fn test_inject_restarting() {
    let mut daemon = TestDaemon::start("inject_restart");

    // Kill then immediately inject
    daemon.api_call(&serde_json::json!({"method": "kill", "params": {"name": "shell"}}));
    std::thread::sleep(Duration::from_millis(300));

    let resp = daemon.api_call(&serde_json::json!({
        "method": "inject",
        "params": {"name": "shell", "data": "hello"}
    }));

    // Should get "restarting" error, not "not found"
    let error = resp["error"].as_str().unwrap_or("");
    assert!(
        error.contains("restarting"),
        "expected 'restarting' error, got: {error}"
    );

    daemon.stop();
}

#[test]
fn test_shutdown_no_crash_log() {
    let mut daemon = TestDaemon::start("shutdown");

    // Get stderr handle
    let stderr = daemon.child.stderr.take().expect("stderr");
    let stderr_reader = BufReader::new(stderr);

    // Stop daemon
    daemon.api_call(&serde_json::json!({"method": "shutdown"}));
    std::thread::sleep(Duration::from_secs(3));

    // Read stderr — should NOT contain "crash"
    let log: String = stderr_reader
        .lines()
        .take(50)
        .filter_map(|l| l.ok())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !log.contains("exit code") || log.contains("stopped (daemon shutdown)"),
        "shutdown should not show crash messages. log:\n{log}"
    );
}

#[test]
fn test_event_log_written() {
    let mut daemon = TestDaemon::start("event_log");
    std::thread::sleep(Duration::from_secs(1));

    // Check event log has daemon_start
    let log_path = daemon.home.join("event-log.jsonl");
    assert!(log_path.exists(), "event-log.jsonl should exist");

    let content = std::fs::read_to_string(&log_path).expect("read log");
    assert!(
        content.contains("daemon_start"),
        "should have daemon_start event"
    );

    daemon.stop();
    std::thread::sleep(Duration::from_secs(2));

    // daemon_stop may not be written if process::exit runs before flush
    // Just verify file exists and has daemon_start — daemon_stop is best-effort
}

#[test]
fn test_fleet_multi_agent_lifecycle() {
    // Start daemon with two agents
    let a1 = format!("shell1:{SHELL_BIN}");
    let a2 = format!("shell2:{SHELL_BIN}");
    let mut daemon = TestDaemon::start_with_agents("fleet", vec![a1.as_str(), a2.as_str()]);

    // Verify both appear in API list
    let resp = daemon.api_call(&serde_json::json!({"method": "list"}));
    assert_eq!(resp["ok"], true);
    let agents = resp["result"]["agents"].as_array().expect("agents array");
    assert_eq!(agents.len(), 2, "should have 2 agents");
    let names: Vec<&str> = agents.iter().filter_map(|a| a["name"].as_str()).collect();
    assert!(names.contains(&"shell1"), "should contain shell1");
    assert!(names.contains(&"shell2"), "should contain shell2");

    // Kill shell1 → verify it respawns after backoff
    let resp =
        daemon.api_call(&serde_json::json!({"method": "kill", "params": {"name": "shell1"}}));
    assert_eq!(resp["ok"], true);

    // Wait for respawn (5s backoff + spawn time)
    std::thread::sleep(Duration::from_secs(8));

    let resp = daemon.api_call(&serde_json::json!({"method": "list"}));
    let agents = resp["result"]["agents"].as_array().expect("agents");
    assert_eq!(
        agents.len(),
        2,
        "both agents should be present after respawn"
    );
    let shell1 = agents
        .iter()
        .find(|a| a["name"] == "shell1")
        .expect("shell1 should exist");
    assert_eq!(
        shell1["health_state"].as_str().unwrap_or(""),
        "healthy",
        "shell1 should be healthy after respawn"
    );

    // Send a cross-instance message via API: from shell1 to shell2
    let resp = daemon.api_call(&serde_json::json!({
        "method": "send",
        "params": {
            "from": "shell1",
            "target": "shell2",
            "text": "hello from shell1",
            "kind": "message"
        }
    }));
    assert_eq!(resp["ok"], true, "send should succeed");

    // Verify shell2's inbox has the message by draining inbox file directly
    // The inbox is stored at {home}/inbox/shell2.jsonl
    let inbox_path = daemon.home.join("inbox").join("shell2.jsonl");
    assert!(
        inbox_path.exists(),
        "shell2 inbox file should exist after send"
    );
    let content = std::fs::read_to_string(&inbox_path).expect("read inbox");
    assert!(
        content.contains("hello from shell1"),
        "inbox should contain the sent message"
    );

    daemon.stop();
}
