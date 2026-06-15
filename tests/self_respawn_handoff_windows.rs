//! #1814 Windows handoff coverage (close-out audit follow-up).
//!
//! The #1814 self-respawn became the platform-agnostic DEFAULT at Stage 4
//! (#2094), so a Windows `restart_daemon` now takes the in-process self-respawn
//! path too — but the §3.9 real-spawn acceptance tests in
//! `self_respawn_handoff.rs` are `#![cfg(unix)]` (their harness uses `libc` and
//! mirrors the unix-only `restart_smoke.rs`, kept unix-only by #1481 as a flaky
//! real-spawn variant). That left the ONE platform-specific piece untested on
//! Windows: `spawn_successor_handoff`'s `#[cfg(windows)]` branch
//! (`DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`, no fork). The rest of the
//! handoff (singleton flock, Phase-1 health gate, recover-as-primary) is the
//! SAME platform-agnostic Rust already covered by the unix tests.
//!
//! This test exercises the Windows spawn branch through the REAL restart flow,
//! SCOPED to the brick-crux to stay robust (option B, lead-decided):
//!   - fleet-0 (NO agents) → no conpty agent-spawn dependency (that e2e path
//!     stays deliberately untested on Windows — deferred, #1814);
//!   - a real daemon restarts; the successor spawned via the Windows branch must
//!     boot, bind the api, and promote (predecessor exits) → exactly one active
//!     daemon remains = NO brick.
//!
//! Cross-platform primitives only (fs run-dir scan + loopback socket + the `ls`
//! CLI), so the body is identical-shaped to the unix harness without `libc`.
//! Windows-only.
#![cfg(windows)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("agend-terminal")
}

/// Boot a real daemon with self-respawn ON, NO external-supervisor env, and
/// fleet-0 (no `fleet.yaml` → zero agents) so the test never depends on Windows
/// conpty agent spawn — it isolates the daemon control-plane handoff.
fn boot_fleet0(home: &Path) -> Child {
    std::fs::create_dir_all(home).ok();
    let mut cmd = Command::new(bin());
    cmd.env("AGEND_HOME", home)
        .env("AGEND_RESTART_HANDOFF", "1")
        // Genuine "no external supervisor": strip any ambient signal.
        .env_remove("AGEND_WRAPPED")
        .env_remove("AGEND_SUPERVISED")
        .env_remove("INVOCATION_ID")
        .env_remove("AGEND_SUCCESSOR_HANDOFF")
        .args(["start", "--foreground"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn().expect("daemon must spawn")
}

/// Active daemon pids: a `run/<pid>` dir carrying an `api.port` file. A daemon
/// removes its run dir on graceful exit, so after a settled handoff exactly one
/// remains. Pure-fs (cross-platform) — no process-liveness syscall needed: the
/// `api.port` file is written after the bind and removed on shutdown.
fn active_pids(home: &Path) -> Vec<u32> {
    let run = home.join("run");
    let Ok(entries) = std::fs::read_dir(&run) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        if let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() {
            if e.path().join("api.port").exists() {
                out.push(pid);
            }
        }
    }
    out
}

/// Poll until exactly one daemon is active and `pred(pid)` holds; return it.
fn wait_for_single_active<F: Fn(u32) -> bool>(
    home: &Path,
    budget: Duration,
    pred: F,
) -> Option<u32> {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        let pids = active_pids(home);
        if pids.len() == 1 && pred(pids[0]) {
            return Some(pids[0]);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    None
}

/// The live daemon answers the real `ls` CLI (which round-trips the loopback api
/// socket) within `budget` — proves the socket is up AND serving.
fn serves_within(home: &Path, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if let Ok(out) = Command::new(bin())
            .env("AGEND_HOME", home)
            .arg("ls")
            .output()
        {
            if out.status.success() {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    false
}

/// Set operator mode Active (a fresh daemon locks to "Away", which gates
/// `restart_daemon`). Mirrors the unix harness.
fn set_mode_active(home: &Path) {
    let _ = Command::new(bin())
        .env("AGEND_HOME", home)
        .args(["mode", "active"])
        .output();
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Invoke the real `restart_daemon` MCP tool over the live api socket (NDJSON
/// cookie handshake → `mcp_tool`). Identical wire protocol to the unix harness;
/// loopback `TcpStream` + fs reads are already cross-platform.
fn trigger_restart(home: &Path, active_pid: u32) -> Option<serde_json::Value> {
    let run_dir = home.join("run").join(active_pid.to_string());
    let port: u16 = std::fs::read_to_string(run_dir.join("api.port"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let cookie_bytes = std::fs::read(run_dir.join("api.cookie")).ok()?;
    let stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(45))).ok();
    let mut writer = stream.try_clone().ok()?;
    let mut reader = BufReader::new(stream);

    writeln!(writer, "{{\"auth\":\"{}\"}}", hex(&cookie_bytes)).ok()?;
    writer.flush().ok();
    let mut ack = String::new();
    reader.read_line(&mut ack).ok()?;
    let ack: serde_json::Value = serde_json::from_str(ack.trim()).ok()?;
    if !ack.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) {
        return None;
    }

    let req = serde_json::json!({
        "method": "mcp_tool",
        "params": { "tool": "restart_daemon", "arguments": {} },
    });
    writeln!(writer, "{req}").ok()?;
    writer.flush().ok();
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    serde_json::from_str(line.trim()).ok()
}

/// Kill any daemon still under `home/run` (detached successors aren't this test's
/// children) via `taskkill`, then remove the home.
fn cleanup(home: &Path, predecessor: &mut Child) {
    let _ = predecessor.kill();
    for pid in active_pids(home) {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .output();
    }
    std::thread::sleep(Duration::from_millis(300));
    if std::env::var("AGEND_KEEP_TEST_HOME").is_err() {
        std::fs::remove_dir_all(home).ok();
    }
}

/// #1814 Windows brick-crux: with self-respawn ON and NO external supervisor, a
/// real `restart_daemon` on Windows spawns a successor (via
/// `spawn_successor_handoff`'s `#[cfg(windows)]` DETACHED_PROCESS |
/// CREATE_NEW_PROCESS_GROUP branch) that boots, binds the api, and promotes —
/// the OLD pid exits, exactly one active run dir remains. Restart can't strand
/// the control plane on Windows either (no supervisor needed).
#[test]
fn windows_self_respawn_successor_takes_over_no_brick() {
    let home = std::env::temp_dir().join(format!("agend-selfrespawn-win-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("mkdir AGEND_HOME");

    let mut d1 = boot_fleet0(&home);

    // First boot becomes the single active daemon (generous: cold spawn + bind).
    let old_pid = match wait_for_single_active(&home, Duration::from_secs(45), |_| true) {
        Some(p) => p,
        None => {
            cleanup(&home, &mut d1);
            panic!("first boot never became the single active daemon");
        }
    };
    assert!(
        serves_within(&home, Duration::from_secs(30)),
        "first boot must serve the api"
    );

    set_mode_active(&home);

    // Real restart → predecessor spawns + health-gates a real Windows successor.
    let _ = trigger_restart(&home, old_pid);

    // A DIFFERENT pid must become the single active daemon (successor promoted),
    // and it must serve — proving the Windows spawn branch booted + bound the api.
    let new_pid = wait_for_single_active(&home, Duration::from_secs(90), |p| p != old_pid);
    let served = new_pid.is_some() && serves_within(&home, Duration::from_secs(30));
    let single_after = active_pids(&home).len() == 1;
    let old_gone = !active_pids(&home).contains(&old_pid);

    cleanup(&home, &mut d1);

    let new_pid =
        new_pid.expect("a NEW daemon pid must serve after self-respawn (old must be gone)");
    assert_ne!(new_pid, old_pid, "successor must be a distinct process");
    assert!(
        served,
        "successor must boot + bind the api (Windows spawn branch)"
    );
    assert!(
        old_gone,
        "the predecessor must have exited (no brick, no orphan)"
    );
    assert!(
        single_after,
        "exactly one active run dir after handoff (no double-bind / duplication)"
    );
}
