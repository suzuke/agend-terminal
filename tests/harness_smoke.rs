//! Sprint 42 Phase 2 — AgendHarness + TuiClient MVP integration tests.
//!
//! SCOPE: these are HARNESS smoke tests. They verify the test *infrastructure*
//! itself — that `TuiClient::feed`/`screen_text`/`feed_and_extract`/`wait_for`
//! round-trip correctly through the in-process `TestVTerm`, and that
//! `AgendHarness` boots a real daemon. They deliberately do NOT assert on
//! production `src/vterm.rs` behavior; that is covered by `tests/vte_gotchas.rs`
//! and the `src/vterm.rs` unit tests. A harness that silently breaks would make
//! every test that depends on it (vte_gotchas, behavioral, etc.) unreliable, so
//! smoke-testing the harness is the point — not a substitute for production
//! coverage.

mod common;

use common::harness::AgendHarness;
use common::harness::TuiClient;
use serde_json::json;

fn tmp_home(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("agend-harness-{}-{}", tag, std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// MVP: spawn daemon, connect via API, list agents (empty fleet).
#[test]
fn harness_spawn_and_connect() {
    let home = tmp_home("spawn-connect");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("harness spawn");

    assert!(harness.api_port > 0, "api_port must be assigned");

    let client = TuiClient::new(&harness, 80, 24);
    let result = client.call("list", &json!({}));
    assert!(
        result.is_ok(),
        "API list call must succeed: {:?}",
        result.err()
    );
    let resp = result.expect("list response");
    assert_eq!(resp["ok"], true, "list must return ok: {resp}");

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}

/// TuiClient can call status endpoint.
#[test]
fn tuiclient_status_call() {
    let home = tmp_home("status-call");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("harness spawn");

    let client = TuiClient::new(&harness, 80, 24);
    let result = client.call("status", &json!({}));
    assert!(
        result.is_ok(),
        "status call must succeed: {:?}",
        result.err()
    );

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}

/// BLOCKING 3.2: deliberately bad fleet.yaml → daemon exits → harness returns
/// informative error (not timeout).
#[test]
fn harness_spawn_reports_early_exit_clearly() {
    let home = tmp_home("early-exit");
    // F1 fix: measure elapsed time INCLUDING spawn() to detect hangs
    let start = std::time::Instant::now();
    let result = AgendHarness::spawn(home.clone(), "{{{{invalid yaml that breaks parsing}}}}\n");

    match result {
        Ok(harness) => {
            drop(harness);
        }
        Err(e) => {
            assert!(
                !e.contains("timeout"),
                "early exit must be detected before timeout: {e}"
            );
        }
    }
    assert!(
        start.elapsed() < std::time::Duration::from_secs(10),
        "harness must not hang on bad fleet.yaml"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// BLOCKING 3.1: drop harness kills child processes (no orphans).
///
/// We assert the daemon's PROCESS GROUP is gone, not that the api *port
/// number* is unconnectable. The port-connectability check was a weak proxy:
/// the api port is OS-assigned (ephemeral), and under the full-suite coverage
/// run a *different* concurrent daemon's `bind(0)` can recycle the just-freed
/// port — making `connect()` succeed against an unrelated daemon and the old
/// `port must be closed` assert FALSE-fail. That is the #2159 Coverage-job
/// flake: it failed ONLY on the llvm-cov job (instrumentation slows teardown,
/// widening the reuse window) while passing on all three platforms' Check; a
/// 110-run macOS repro (normal / cov-isolated / cov-under-CPU-saturation)
/// could not reproduce it, and source analysis ruled out any prod port-close
/// race — the api listener is a std `TcpListener` (CLOEXEC, not inheritable by
/// the exec'd shell agent) served by an in-process thread that dies with the
/// daemon, so a dead daemon's port is freed synchronously. By elimination, a
/// post-reap `connect()` success can only mean a *different* listener owns the
/// recycled port.
///
/// `kill(-pgid, 0)` → `ESRCH` tests the test's ACTUAL contract — "drop kills
/// child processes / no orphans" — and is immune to port reuse.
#[cfg(unix)]
#[test]
fn harness_drop_kills_child_processes() {
    let home = tmp_home("drop-kills");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("harness spawn");

    let port = harness.api_port;
    let pgid = harness.pgid();
    // Verify daemon is alive before drop
    assert!(
        std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok(),
        "daemon must be reachable before drop"
    );

    drop(harness);

    // Poll up to 15x with 200ms sleep (3s) — the whole process group must die.
    // Drop SIGTERMs the group (3s grace) then SIGKILLs + waits the leader;
    // orphaned group members (the shell agent) are reparented to init and
    // reaped, so `kill(-pgid, 0)` converges to ESRCH. Wider than the leader's
    // own death to tolerate the slower group-reap under coverage.
    assert!(
        poll_group_gone(pgid, 15, std::time::Duration::from_millis(200)),
        "daemon process group {pgid} must be gone after harness drop (BLOCKING 3.1)"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// Windows variant: the job object (JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE) kills
/// the whole job when the handle closes on drop, so the port-connectability
/// check is retained here — the ephemeral-port-reuse flake was observed only on
/// the unix llvm-cov job, and the job-object guarantee is the platform contract.
#[cfg(not(unix))]
#[test]
fn harness_drop_kills_child_processes() {
    let home = tmp_home("drop-kills");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("harness spawn");

    let port = harness.api_port;
    assert!(
        std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok(),
        "daemon must be reachable before drop"
    );

    drop(harness);

    let mut port_closed = false;
    for _ in 0..5 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_err() {
            port_closed = true;
            break;
        }
    }
    assert!(
        port_closed,
        "daemon port {port} must be closed after harness drop (BLOCKING 3.1)"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// `kill(-pgid, 0)` existence probe: returns `true` once NO member of the
/// process group survives (ESRCH). Polls up to `tries` times with `interval`
/// between attempts; returns early on the first ESRCH.
#[cfg(unix)]
fn poll_group_gone(pgid: i32, tries: u32, interval: std::time::Duration) -> bool {
    for _ in 0..tries {
        std::thread::sleep(interval);
        // signal 0 = existence check; ESRCH => the group has no members left.
        // EPERM (group exists, can't signal) counts as ALIVE — but for our own
        // group as the same user that does not occur.
        let r = unsafe { libc::kill(-pgid, 0) };
        if r != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return true;
        }
    }
    false
}

/// Regression for the #2159 Coverage-job flake: prove the process-liveness
/// contract is IMMUNE to ephemeral-port reuse, where the old port-connect proxy
/// false-failed.
///
/// After the daemon's group is gone, we bind a fresh listener on the just-freed
/// port (simulating a concurrent daemon's `bind(0)` recycling it). The port is
/// now connectable again — exactly the condition that fooled the old
/// `connect(port).is_err()` proxy (red) — yet `kill(-pgid, 0)` still reports
/// ESRCH, so the new contract passes (green).
#[cfg(unix)]
#[test]
fn harness_drop_liveness_immune_to_port_reuse() {
    let home = tmp_home("reuse-immune");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("harness spawn");

    let port = harness.api_port;
    let pgid = harness.pgid();
    drop(harness);

    // The group must actually be gone first (this is the real contract).
    assert!(
        poll_group_gone(pgid, 25, std::time::Duration::from_millis(200)),
        "daemon process group {pgid} must be gone after drop"
    );

    // Simulate a concurrent daemon grabbing the freed ephemeral port.
    let _squatter = std::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .expect("rebind freed port (simulating concurrent-daemon ephemeral-port reuse)");

    // The OLD proxy would now FALSE-fail: the port IS connectable again.
    assert!(
        std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok(),
        "port {port} is connectable via the squatter — this is what fooled the old proxy"
    );

    // The NEW contract is unaffected: our daemon's group is still gone.
    let r = unsafe { libc::kill(-pgid, 0) };
    assert!(
        r != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
        "process-liveness contract must hold despite port reuse (group {pgid} must be ESRCH)"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// Control / non-vacuity: `kill(-pgid, 0)` must correctly DETECT a surviving
/// group member, so the contract check in `harness_drop_kills_child_processes`
/// is not vacuously always-ESRCH. Spawn a `sleep` in its own session
/// (`setsid` ⇒ pid == pgid), assert the group reads ALIVE, then kill + reap and
/// assert it converges to ESRCH. Proves the new assertion would FAIL on a real
/// orphaned child.
#[cfg(unix)]
#[test]
fn liveness_check_detects_surviving_group_member() {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let mut cmd = Command::new("sleep");
    cmd.arg("30");
    // setsid ⇒ the child is its own session/group leader: pid == pgid.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawn sleep");
    let pgid = child.id() as i32;

    // ALIVE: a surviving member ⇒ kill(-pgid, 0) succeeds (NOT ESRCH).
    assert_eq!(
        unsafe { libc::kill(-pgid, 0) },
        0,
        "process group {pgid} must be detected alive while the child runs"
    );

    // Kill + reap, then the group must converge to ESRCH.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
    let _ = child.wait();
    assert!(
        poll_group_gone(pgid, 25, std::time::Duration::from_millis(40)),
        "process group {pgid} must read ESRCH after kill + reap"
    );
}

/// TuiClient vterm: feed bytes → extract screen text.
#[test]
fn tuiclient_vterm_grid_extract() {
    let home = tmp_home("vterm-grid");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("harness spawn");

    let mut client = TuiClient::new(&harness, 80, 24);
    client.feed(b"Hello from vterm\r\n");
    let screen = client.screen_text(5);
    assert!(
        screen.contains("Hello from vterm"),
        "vterm must capture fed text, got: '{screen}'"
    );

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}

/// TuiClient drain_for: feed + drain returns screen content.
#[test]
fn tuiclient_feed_and_extract_returns_content() {
    let home = tmp_home("drain-for");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("harness spawn");

    let mut client = TuiClient::new(&harness, 80, 24);
    let screen = client.feed_and_extract(b"Line1\r\nLine2\r\n");
    assert!(
        screen.contains("Line1"),
        "drain must capture Line1: '{screen}'"
    );
    assert!(
        screen.contains("Line2"),
        "drain must capture Line2: '{screen}'"
    );

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}

/// TuiClient wait_for: predicate matches after feed.
#[test]
fn tuiclient_wait_for_predicate_match() {
    let home = tmp_home("wait-for");
    let harness = AgendHarness::spawn(home.clone(), "instances: {}\n").expect("harness spawn");

    let mut client = TuiClient::new(&harness, 80, 24);
    let found = client.wait_for(
        b"Expected output\r\n",
        |s| s.contains("Expected output"),
        std::time::Duration::from_secs(1),
    );
    assert!(found, "wait_for must find 'Expected output' in vterm");

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}
