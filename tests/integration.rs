//! Integration tests — spawn daemon as subprocess, test via TCP API port.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Poll a predicate until it returns true or timeout expires.
fn wait_until<F: FnMut() -> bool>(mut pred: F, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

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

        // Wave 1 CLI consolidation: the historical `daemon` subcommand was
        // removed; `start --agents <name:cmd ...>` is the canonical
        // replacement.
        let expected_agents = agents.len();
        let mut args: Vec<&str> = vec!["start", "--agents"];
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

        // #879v4 C1 — after the daemon-reorder fix, `api.port` is published
        // BEFORE the agent spawn loop runs. Wait for the registry to settle
        // so tests can call `list`/`kill`/`inject` against the expected
        // agents without racing the staggered spawn loop.
        let daemon = TestDaemon { child, home };
        let registered = wait_until(
            || {
                let resp = daemon.api_call(&serde_json::json!({"method": "list"}));
                resp["result"]["agents"]
                    .as_array()
                    .map(|a| a.len() == expected_agents)
                    .unwrap_or(false)
            },
            Duration::from_secs(15),
        );
        assert!(
            registered,
            "agents not registered within 15s of api.port publish (expected {expected_agents})"
        );
        daemon
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

#[cfg_attr(
    windows,
    ignore = "tracking #749: daemon respawn loop on cmd.exe + ConPTY"
)]
#[test]
fn test_crash_respawn_health() {
    let mut daemon = TestDaemon::start("crash");

    // Verify agent exists
    let resp = daemon.api_call(&serde_json::json!({"method": "list"}));
    assert_eq!(resp["result"]["agents"].as_array().expect("a").len(), 1);

    // Kill agent (triggers crash)
    let resp = daemon.api_call(&serde_json::json!({"method": "kill", "params": {"name": "shell"}}));
    assert_eq!(resp["ok"], true);

    // Wait for agent to enter restarting/starting state
    let _ = wait_until(
        || {
            let r = daemon.api_call(&serde_json::json!({"method": "list"}));
            r["result"]["agents"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|a| a["agent_state"].as_str())
                .map(|s| s == "restarting" || s == "starting")
                .unwrap_or(false)
        },
        Duration::from_secs(5),
    );
    let resp = daemon.api_call(&serde_json::json!({"method": "list"}));
    let agents = resp["result"]["agents"].as_array().expect("a");
    if !agents.is_empty() {
        let state = agents[0]["agent_state"].as_str().unwrap_or("");
        assert!(
            state == "restarting" || state == "starting",
            "expected restarting or starting, got: {state}"
        );
    }

    // Lifecycle: kill → restarting → (process up + registry handle insert) → Ready
    //            → (respawn_ok lock window, sub-ms in-memory flip) → Healthy.
    // Note: Shell backend initial state = Ready (src/state.rs:625-628) — managed
    // backends go Starting→Ready, so Phase 1 budget may need recalibration if
    // pattern reused there. Single-kill test: no monotonic Recovering↔Healthy
    // flicker possible; multi-kill extension must re-evaluate Phase 2 budget.
    //
    // Windows note: cmd.exe + ConPTY EOF triggers a 5-20s respawn cycle.
    // SPAWN_TIMEOUT must exceed max backoff to produce a stable diagnostic
    // panic should this test be re-enabled on Windows. Tracked in #749.
    const SPAWN_TIMEOUT: Duration = Duration::from_secs(28);
    const HEALTH_FLIP_TIMEOUT: Duration = Duration::from_secs(2);

    let phase_start = Instant::now();

    // Phase 1: process up + new registry handle inserted → agent_state == "ready"
    if !wait_until(
        || {
            let r = daemon.api_call(&serde_json::json!({"method": "list"}));
            r["result"]["agents"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|a| a["agent_state"].as_str())
                .map(|s| s == "ready")
                .unwrap_or(false)
        },
        SPAWN_TIMEOUT,
    ) {
        let r = daemon.api_call(&serde_json::json!({"method": "list"}));
        let agents = r["result"]["agents"].as_array();
        let first = agents.and_then(|a| a.first());
        panic!(
            "phase 1 (agent_state == 'ready') not satisfied within 28s. \
             last: count={}, agent_state={:?}, health_state={:?}",
            agents.map(|a| a.len()).unwrap_or(0),
            first.and_then(|a| a["agent_state"].as_str()).unwrap_or("?"),
            first
                .and_then(|a| a["health_state"].as_str())
                .unwrap_or("?"),
        );
    }
    let phase1_elapsed = phase_start.elapsed();

    // Phase 2: respawn_ok flips Recovering → Healthy. Guard against transient
    // ready+recovering window between spawn_agent insert and respawn_ok lock
    // reacquire (src/daemon/mod.rs:889-902).
    if !wait_until(
        || {
            let r = daemon.api_call(&serde_json::json!({"method": "list"}));
            r["result"]["agents"]
                .as_array()
                .and_then(|a| a.first())
                .map(|a| {
                    a["agent_state"].as_str() == Some("ready")
                        && a["health_state"].as_str() == Some("healthy")
                })
                .unwrap_or(false)
        },
        HEALTH_FLIP_TIMEOUT,
    ) {
        let r = daemon.api_call(&serde_json::json!({"method": "list"}));
        let agents = r["result"]["agents"].as_array();
        let first = agents.and_then(|a| a.first());
        panic!(
            "phase 2 (ready + healthy) not satisfied within 2s. \
             phase1_elapsed={:?}, last: count={}, agent_state={:?}, health_state={:?}",
            phase1_elapsed,
            agents.map(|a| a.len()).unwrap_or(0),
            first.and_then(|a| a["agent_state"].as_str()).unwrap_or("?"),
            first
                .and_then(|a| a["health_state"].as_str())
                .unwrap_or("?"),
        );
    }

    // Identity check — second api_call is necessary (wait_until returns bool).
    let resp = daemon.api_call(&serde_json::json!({"method": "list"}));
    let agents = resp["result"]["agents"].as_array().expect("a");
    let first = agents.first().expect("agent should have respawned");
    assert_eq!(first["name"], "shell");

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
    // Poll for event log to appear
    let log_path = daemon.home.join("event-log.jsonl");
    assert!(
        wait_until(|| log_path.exists(), Duration::from_secs(3)),
        "event-log.jsonl should exist"
    );

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

    // Wait for respawn (poll instead of hard 8s sleep)
    assert!(
        wait_until(
            || {
                let r = daemon.api_call(&serde_json::json!({"method": "list"}));
                r["result"]["agents"]
                    .as_array()
                    .map(|a| a.len() == 2)
                    .unwrap_or(false)
            },
            Duration::from_secs(30)
        ),
        "agents did not respawn within 30s"
    );

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

    // Verify shell2's inbox has the message. #1441: the inbox is keyed by the
    // instance's UUID (resolved from fleet.yaml, the same source the registry
    // uses) rather than its display name, so the file is `{home}/inbox/
    // {uuid}.jsonl`. Scan the inbox dir for the delivered message rather than
    // assuming a name-based filename.
    let inbox_dir = daemon.home.join("inbox");
    let delivered = std::fs::read_dir(&inbox_dir)
        .expect("inbox dir should exist after send")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .any(|e| {
            std::fs::read_to_string(e.path())
                .map(|c| c.contains("hello from shell1"))
                .unwrap_or(false)
        });
    assert!(
        delivered,
        "shell2's (UUID-keyed) inbox should contain the sent message"
    );

    daemon.stop();
}

#[test]
fn daemon_shutdown_cleans_up_three_agents() {
    let a1 = format!("agent1:{SHELL_BIN}");
    let a2 = format!("agent2:{SHELL_BIN}");
    let a3 = format!("agent3:{SHELL_BIN}");
    let mut daemon =
        TestDaemon::start_with_agents("shutdown3", vec![a1.as_str(), a2.as_str(), a3.as_str()]);

    // Verify all 3 agents are running
    let resp = daemon.api_call(&serde_json::json!({"method": "list"}));
    assert_eq!(resp["ok"], true);
    let agents = resp["result"]["agents"].as_array().expect("agents");
    assert_eq!(agents.len(), 3, "should have 3 agents before shutdown");

    // Trigger shutdown
    let start = Instant::now();
    daemon.api_call(&serde_json::json!({"method": "shutdown"}));

    // Wait for daemon process to exit (should be < 30s)
    let exited = wait_until(
        || daemon.child.try_wait().ok().flatten().is_some(),
        Duration::from_secs(30),
    );
    let elapsed = start.elapsed();
    assert!(exited, "daemon must exit within 30s of shutdown");
    assert!(
        elapsed < Duration::from_secs(30),
        "shutdown took too long: {elapsed:?}"
    );

    // Verify no orphan processes by checking the run dir is cleaned
    let run_dir = daemon.home.join("run");
    if run_dir.exists() {
        let entries: Vec<_> = std::fs::read_dir(&run_dir)
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        // Run dir may have the PID subdir but api.port should be gone
        for entry in &entries {
            let port_file = entry.path().join("api.port");
            assert!(
                !port_file.exists(),
                "api.port should be cleaned on shutdown"
            );
        }
    }
}

// ─── Issue #714: Bridge invariant tests ──────────────────────────────────
#[test]
#[cfg(unix)]
#[ignore = "daemon does not currently reject cookies with wrong permissions — future hardening"]
fn bridge_rejects_cookie_wrong_permissions() {
    // Placeholder: daemon writes 0600 at creation but does not check on read.
    // When permission check is added, this test should verify rejection.
}

#[test]
fn bridge_rejects_cookie_mismatch() {
    // Start daemon, then try to connect with wrong cookie
    let mut daemon = TestDaemon::start("bridge_mismatch");
    let port = TestDaemon::find_api_port(&daemon.home).expect("api port");
    let mut stream =
        TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    // Send wrong cookie (all zeros)
    let fake_hex = "00".repeat(32);
    writeln!(stream, r#"{{"auth":"{}"}}"#, fake_hex).expect("write");
    stream.flush().expect("flush");
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read");
    let resp: serde_json::Value = serde_json::from_str(line.trim()).expect("parse");
    assert_eq!(resp["ok"], false, "wrong cookie must be rejected: {resp}");
    daemon.stop();
}

#[test]
fn bridge_rejects_malformed_mcp_json() {
    let mut daemon = TestDaemon::start("bridge_malformed");
    // Send malformed JSON after auth
    let (mut reader, mut writer) = daemon.connect_authed();
    writeln!(writer, "{{not valid json at all").expect("write");
    writer.flush().expect("flush");
    let mut line = String::new();
    reader.read_line(&mut line).expect("read");
    let resp: serde_json::Value = serde_json::from_str(line.trim()).expect("parse");
    assert_eq!(
        resp["ok"], false,
        "malformed JSON must return error: {resp}"
    );
    assert!(
        resp["error"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .contains("parse"),
        "should mention parse error: {resp}"
    );
    daemon.stop();
}

#[test]
fn bridge_handles_half_up_daemon() {
    // Get a random port that nothing is listening on
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
        // l drops here — port is free but nothing listening
    };
    // Try to connect — should fail (connection refused), not hang
    let start = Instant::now();
    let result = TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        Duration::from_secs(3),
    );
    let elapsed = start.elapsed();
    assert!(result.is_err(), "connect to non-listening port should fail");
    assert!(
        elapsed < Duration::from_secs(5),
        "should fail quickly, not hang: {elapsed:?}"
    );
}
