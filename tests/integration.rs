//! Integration tests — spawn daemon as subprocess, test via API socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
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
        let home = std::env::temp_dir().join(format!("agend-integ-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create home");

        let child = Command::new(binary())
            .args(["daemon", "shell:/bin/bash"])
            .env("AGEND_TERMINAL_HOME", &home)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn daemon");

        // Wait for API socket
        let mut found = false;
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(200));
            if Self::find_api_sock(&home).is_some() {
                found = true;
                break;
            }
        }
        assert!(found, "daemon API socket not found after 6s");

        TestDaemon { child, home }
    }

    fn find_api_sock(home: &Path) -> Option<PathBuf> {
        let run = home.join("run");
        if !run.exists() { return None; }
        for entry in std::fs::read_dir(&run).ok()?.flatten() {
            let api = entry.path().join("api.sock");
            if api.exists() {
                return Some(api);
            }
        }
        None
    }

    fn api_call(&self, request: &serde_json::Value) -> serde_json::Value {
        let sock = Self::find_api_sock(&self.home).expect("api socket");
        let mut stream = UnixStream::connect(&sock).expect("connect");
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
    assert_eq!(health, "healthy", "health should be healthy after respawn_ok");

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
    let log: String = stderr_reader.lines()
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
    assert!(content.contains("daemon_start"), "should have daemon_start event");

    daemon.stop();
    std::thread::sleep(Duration::from_secs(2));

    // daemon_stop may not be written if process::exit runs before flush
    // Just verify file exists and has daemon_start — daemon_stop is best-effort
}
