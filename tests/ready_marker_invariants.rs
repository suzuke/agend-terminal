//! #922 RED tests — `.ready` marker post-condition contract.
//!
//! Pins the load-bearing invariant:
//!
//!   `<run_dir>/.ready` exists ⟹ daemon's agent spawn loop has completed,
//!   so the API registry's agent count is FINAL (no further changes from
//!   this boot sequence; subsequent changes are operator-initiated via
//!   spawn/kill/etc. RPC).
//!
//! Per the lead-synthed weakening: `.ready` does NOT promise `count == N`,
//! because daemon's log-and-continue policy lets individual agent spawn
//! errors NOT abort the loop (see src/daemon/mod.rs ~line 491). Test 2
//! pins "count stable over 100 ms" rather than "count == N".
//!
//! Test 4 (N=10 stress) is non-gating — it documents the high-N case
//! without enforcing it (slow CI runners may legitimately need more
//! time per agent). It runs `#[cfg(unix)]` only and asserts only that
//! `.ready` eventually exists, not the final count.
//!
//! RED on parent (no `.ready` write): tests 1-3 fail because polling
//! `.ready` times out. GREEN after the daemon writes `.ready` post
//! spawn-loop.

#![allow(clippy::unwrap_used)]

// All tests + helpers are `#[cfg(unix)]` because the harness drives
// `/bin/sh` agents. Windows path is covered by the smoke harness
// (`.github/workflows/ci.yml`) which exercises `cmd.exe` end-to-end.
// Gating imports + helpers behind cfg(unix) so Windows clippy
// doesn't flag them as dead.
#[cfg(unix)]
use serde_json::Value;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
fn agend_binary() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_agend-terminal"));
    assert!(
        path.exists(),
        "agend-terminal binary missing — run `cargo build --bin agend-terminal` first"
    );
    path
}

#[cfg(unix)]
fn tmp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agend-922-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Scan `<home>/run/*` for the first run-dir containing `.ready`.
/// Returns `None` until the daemon writes the marker.
#[cfg(unix)]
fn find_ready(home: &Path) -> Option<PathBuf> {
    let run_base = home.join("run");
    let entries = std::fs::read_dir(&run_base).ok()?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() && p.join(".ready").exists() {
            return Some(p.join(".ready"));
        }
    }
    None
}

/// Poll for `.ready` until present or `deadline` elapses. Returns
/// `true` iff observed before the deadline.
#[cfg(unix)]
fn wait_for_ready(home: &Path, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        if find_ready(home).is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

/// Subprocess `agend-terminal list --json` against `home`. Returns the
/// parsed agent count, or `None` if the call failed or returned bad JSON.
#[cfg(unix)]
fn agent_count_via_list(bin: &Path, home: &Path) -> Option<usize> {
    let out = Command::new(bin)
        .arg("list")
        .arg("--json")
        .env("AGEND_HOME", home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    v["agents"].as_array().map(Vec::len)
}

/// Spawn an `agend-terminal start --foreground --agents …` daemon child
/// with `stagger_ms` tight race window. Returns the spawned `Child`
/// handle for the caller to teardown.
#[cfg(unix)]
fn spawn_daemon_with_agents(
    bin: &Path,
    home: &Path,
    agents: &[&str],
    stagger_ms: &str,
) -> std::process::Child {
    let mut cmd = Command::new(bin);
    cmd.arg("start").arg("--foreground");
    for a in agents {
        cmd.args(["--agents", a]);
    }
    cmd.env("AGEND_HOME", home)
        .env("AGEND_SPAWN_STAGGER_MS", stagger_ms)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn daemon")
}

/// #922 contract test (baseline N=3 with stagger=0 for tight race):
/// after `.ready` is observed, `agend-terminal list` returns all 3
/// agents. On parent (no `.ready` write), polling times out → assertion
/// fails with the "didn't observe .ready" branch.
#[cfg(unix)]
#[test]
fn ready_marker_implies_all_agents_registered_n3() {
    let bin = agend_binary();
    let home = tmp_home("ready-n3");

    let mut child = spawn_daemon_with_agents(
        &bin,
        &home,
        &["t1:/bin/sh", "t2:/bin/sh", "t3:/bin/sh"],
        "0",
    );

    let deadline = Instant::now() + Duration::from_secs(8);
    let ready = wait_for_ready(&home, deadline);

    let count_after_ready = if ready {
        agent_count_via_list(&bin, &home)
    } else {
        None
    };

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);

    assert!(
        ready,
        "daemon must write <run_dir>/.ready after the agent spawn loop \
         completes (#922). Polling timed out — no `.ready` file appeared \
         within 8s of daemon start."
    );
    assert_eq!(
        count_after_ready,
        Some(3),
        "after `.ready` is observed, registry must contain all 3 spawned \
         shell agents — got {count_after_ready:?}"
    );
}

/// #922 weakened-invariant test (lead synth refinement): `.ready` does
/// NOT promise `count == N` (log-and-continue), but it DOES promise the
/// count is FINAL for this boot sequence. Pin "count stable over 100 ms"
/// rather than count-equality.
#[cfg(unix)]
#[test]
fn ready_marker_implies_agent_count_is_final() {
    let bin = agend_binary();
    let home = tmp_home("ready-stable");

    let mut child = spawn_daemon_with_agents(&bin, &home, &["a:/bin/sh", "b:/bin/sh"], "0");

    let deadline = Instant::now() + Duration::from_secs(8);
    let ready = wait_for_ready(&home, deadline);

    let (first, second) = if ready {
        let c1 = agent_count_via_list(&bin, &home);
        std::thread::sleep(Duration::from_millis(100));
        let c2 = agent_count_via_list(&bin, &home);
        (c1, c2)
    } else {
        (None, None)
    };

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);

    assert!(ready, "daemon must write `.ready` within 8s of start");
    assert_eq!(
        first, second,
        "after `.ready` is observed, agent count must be FINAL: sample at \
         T+0 = {first:?}, sample at T+100ms = {second:?}. Differing samples \
         mean spawn-loop work was still happening after `.ready` was \
         written (contract violation)."
    );
    assert!(
        first.is_some() && first.unwrap_or(0) > 0,
        "stable count must be a real number (>0 since we spawned agents), \
         got {first:?}"
    );
}

/// #922 degenerate N=1 case (reviewer condition C): single-agent
/// daemon must still write `.ready` and report 1 agent.
#[cfg(unix)]
#[test]
fn ready_marker_n1_degenerate_case() {
    let bin = agend_binary();
    let home = tmp_home("ready-n1");

    let mut child = spawn_daemon_with_agents(&bin, &home, &["solo:/bin/sh"], "0");

    let deadline = Instant::now() + Duration::from_secs(5);
    let ready = wait_for_ready(&home, deadline);
    let count = if ready {
        agent_count_via_list(&bin, &home)
    } else {
        None
    };

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);

    assert!(
        ready,
        "N=1 daemon must still write `.ready` after the single agent's \
         spawn loop iteration"
    );
    assert_eq!(
        count,
        Some(1),
        "after `.ready`, the single shell agent must be in registry, \
         got {count:?}"
    );
}

/// #922 N=10 stress (non-gating per lead synth): does NOT assert
/// count == 10 (slow CI may legitimately miss agents under log-and-continue),
/// only that `.ready` is eventually written. Provides empirical coverage
/// for high-N boot behavior without fragile timing assertions.
#[cfg(unix)]
#[test]
fn ready_marker_n10_stress_non_gating() {
    let bin = agend_binary();
    let home = tmp_home("ready-n10");

    let agents: Vec<String> = (1..=10).map(|i| format!("s{i}:/bin/sh")).collect();
    let agents_ref: Vec<&str> = agents.iter().map(String::as_str).collect();

    let mut child = spawn_daemon_with_agents(&bin, &home, &agents_ref, "0");

    let deadline = Instant::now() + Duration::from_secs(15);
    let ready = wait_for_ready(&home, deadline);

    let count = if ready {
        agent_count_via_list(&bin, &home)
    } else {
        None
    };

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&home);

    assert!(
        ready,
        "high-N daemon (N=10) must still write `.ready` within the budget \
         — `.ready` write is a single fs::write at end of spawn loop, \
         independent of per-agent latency"
    );
    // Non-gating: just log observed count for empirical visibility.
    // Log-and-continue means count could be < 10 if some agents failed.
    eprintln!("ready_marker_n10_stress: observed agent count = {count:?}");
}
