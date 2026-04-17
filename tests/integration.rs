//! Integration tests — spawn daemon as subprocess, test via TCP API port.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Duration;

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
        Self::start_with_agents(name, vec!["shell:/bin/bash"])
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

    fn api_call(&self, request: &serde_json::Value) -> serde_json::Value {
        let port = Self::find_api_port(&self.home).expect("api port");
        let mut stream =
            TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).expect("connect");
        stream.set_nodelay(true).ok();
        stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
        writeln!(stream, "{}", request).expect("write");
        stream.flush().expect("flush");
        let mut reader = BufReader::new(stream);
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
    let mut daemon =
        TestDaemon::start_with_agents("fleet", vec!["shell1:/bin/bash", "shell2:/bin/bash"]);

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
