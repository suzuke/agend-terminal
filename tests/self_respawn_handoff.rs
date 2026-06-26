//! #1814 Stage 1 — self-respawn (flag-gated successor handoff) §3.9 real-entry
//! integration tests.
//!
//! These drive the REAL flow end-to-end, not a mocked handoff: a real daemon
//! binary boots with `AGEND_RESTART_HANDOFF=1` and NO external supervisor env, the
//! real `restart_daemon` MCP tool is invoked over the real api socket, which
//! spawns a REAL successor binary (`start --foreground` + `AGEND_SUCCESSOR_
//! HANDOFF`), runs the real Phase-1 health gate, and either commits (predecessor
//! exits 0, successor promotes) or aborts (predecessor stays alive).
//!
//! Coverage:
//! - happy / no external supervisor → a NEW pid serves the api, the OLD pid is
//!   dead, agents are re-spawned, exactly one active run dir remains.
//! - successor-fails (injected via `AGEND_FORCE_SUCCESSOR_FAIL=1`) → the OLD
//!   daemon stays alive (SAME pid still serving), restart reports ok:false.
//!
//! These real-daemon-spawn integration tests are Unix-only (the harness mirrors
//! `restart_smoke.rs`, kept unix-only by #1481: a windows daemon-spawn variant
//! was deemed flaky for the coverage). NOTE (#1814 Stage 4, #2094): the earlier
//! "Windows keeps `exit(42)` + Task Scheduler" claim here is STALE — flipping the
//! self-respawn default ON was platform-agnostic, so Windows ALSO takes the
//! in-process self-respawn path by default now (the `#[cfg(windows)]`
//! `spawn_successor_handoff` branch). Windows handoff coverage is tracked
//! separately (#1814 close-out) rather than by widening this fragile real-spawn
//! harness to windows-latest.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> PathBuf {
    assert_cmd::cargo::cargo_bin("agend-terminal")
}

/// Injected successor failure mode for the abort-path tests.
#[derive(Clone, Copy, PartialEq)]
enum SuccessorFault {
    /// Successor comes up healthy and promotes (happy path).
    None,
    /// Successor crashes on launch (fails the Phase-1 gate → handler aborts).
    OnLaunch,
    /// Successor passes Phase-1 (answers STATUS) then dies before the flock —
    /// exercises the predecessor's commit→exit liveness recheck (FIX2).
    AfterControlReady,
    /// Successor survives Phase-1 AND the predecessor's loop-break recheck, then
    /// dies DURING the predecessor's teardown window — exercises the round-2
    /// final recover-as-primary gate (predecessor re-spawns agents + resumes).
    DuringTeardown,
}

/// Boot a real daemon with self-respawn ON, NO external-supervisor env, and a
/// single no-auth shell probe agent (fleet size 1). `fault` injects a successor
/// failure seam for the abort-path tests.
fn boot(home: &Path, fault: SuccessorFault) -> Child {
    // Define `probe` via fleet.yaml (NOT `--agents`) so BOTH the first boot AND
    // the handoff successor resolve it IDENTICALLY to a shell agent. The
    // `--agents probe:/bin/sh` path registers a DEFAULT fleet entry (no
    // backend), so the successor's re-resolve from fleet.yaml falls back to the
    // default `claude` backend — absent on CI runners → agent spawn fails →
    // the post-restart "agents re-spawned" assertion would fail on ubuntu. A
    // `backend: shell` + `command: /bin/sh` entry resolves to /bin/sh on every
    // platform.
    std::fs::create_dir_all(home).ok();
    std::fs::write(
        home.join("fleet.yaml"),
        "instances:\n  probe:\n    backend: shell\n    command: /bin/sh\n",
    )
    .expect("write fleet.yaml");
    let mut cmd = Command::new(bin());
    cmd.env("AGEND_HOME", home)
        .env("AGEND_RESTART_HANDOFF", "1")
        // Strip any ambient supervisor signal so this is a genuine
        // "no external supervisor" environment (e.g. macOS GUI sets
        // XPC_SERVICE_NAME; CI/systemd may set INVOCATION_ID).
        .env_remove("AGEND_WRAPPED")
        .env_remove("XPC_SERVICE_NAME")
        .env_remove("INVOCATION_ID")
        .env_remove("AGEND_SUCCESSOR_HANDOFF")
        .env_remove("AGEND_FORCE_SUCCESSOR_FAIL")
        .env_remove("AGEND_FORCE_SUCCESSOR_FAIL_AFTER_CONTROL_READY")
        .env_remove("AGEND_FORCE_SUCCESSOR_FAIL_DURING_TEARDOWN")
        .env_remove("AGEND_SELF_RESPAWN_SETTLE_SECS")
        .args(["start", "--foreground"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match fault {
        SuccessorFault::None => {}
        SuccessorFault::OnLaunch => {
            cmd.env("AGEND_FORCE_SUCCESSOR_FAIL", "1");
        }
        SuccessorFault::AfterControlReady => {
            cmd.env("AGEND_FORCE_SUCCESSOR_FAIL_AFTER_CONTROL_READY", "1");
        }
        SuccessorFault::DuringTeardown => {
            // Successor stays alive 15s past control-ready (surviving the
            // predecessor's ~10s loop-break tick), then dies. Widen the
            // predecessor's pre-exit settle so its final recover-gate recheck
            // lands AFTER the successor's death — deterministic across CI slop.
            cmd.env("AGEND_FORCE_SUCCESSOR_FAIL_DURING_TEARDOWN", "1")
                .env("AGEND_SELF_RESPAWN_SETTLE_SECS", "12");
        }
    }
    cmd.spawn().expect("daemon must spawn")
}

fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks existence/permission, never delivers.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Active daemon pids: a `run/<pid>` dir whose pid is alive AND has an
/// `api.port` file. After a settled handoff there is exactly one.
fn active_pids(home: &Path) -> Vec<u32> {
    let run = home.join("run");
    let Ok(entries) = std::fs::read_dir(&run) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        if let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() {
            if pid_alive(pid) && e.path().join("api.port").exists() {
                out.push(pid);
            }
        }
    }
    out
}

/// Poll until exactly one daemon is active and `pred(pid)` holds, returning that
/// pid. `None` on timeout.
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
        std::thread::sleep(Duration::from_millis(200));
    }
    None
}

/// The probe agent is listed by `ls` (which queries the live socket) within
/// `budget` — proves the socket is up AND serving AND agents re-spawned.
fn ls_lists_probe_within(home: &Path, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if let Ok(out) = Command::new(bin())
            .env("AGEND_HOME", home)
            .arg("ls")
            .output()
        {
            if String::from_utf8_lossy(&out.stdout).contains("probe") {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

/// Set operator mode to Active via the real `mode` CLI (api MODE → signed
/// operator-mode.json + immediate in-memory update). REQUIRED before triggering
/// restart: a fresh daemon with no signed operator-mode.json locks down to
/// "Away" (#1576), and the operator gate blocks `restart_daemon` while Away —
/// so without this the restart is (racily) gated and the handoff never runs.
fn set_mode_active(home: &Path) {
    let _ = Command::new(bin())
        .env("AGEND_HOME", home)
        .args(["mode", "active"])
        .output();
}

/// Hex-encode bytes (lower-case), matching `auth_cookie::to_hex`.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Invoke the real `restart_daemon` MCP tool over the live api socket. Speaks
/// the daemon's real wire protocol: the NDJSON cookie handshake
/// (`{"auth":"<hex>"}` → `{"ok":true}`) FIRST, then the `mcp_tool` request. The
/// cookie file is raw 32 bytes (see `auth_cookie::issue`); hex-encode it for the
/// handshake. Best-effort: on the happy path the predecessor may exit before the
/// reply lands, so commit-expecting callers poll process state, not the reply.
fn trigger_restart(home: &Path, active_pid: u32) -> Option<serde_json::Value> {
    let run_dir = home.join("run").join(active_pid.to_string());
    let port: u16 = std::fs::read_to_string(run_dir.join("api.port"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let cookie_bytes = std::fs::read(run_dir.join("api.cookie")).ok()?; // raw 32 bytes
    let stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(45))).ok();
    let mut writer = stream.try_clone().ok()?;
    let mut reader = BufReader::new(stream);

    // NDJSON cookie handshake (server requires this BEFORE any request).
    writeln!(writer, "{{\"auth\":\"{}\"}}", hex(&cookie_bytes)).ok()?;
    writer.flush().ok();
    let mut ack = String::new();
    reader.read_line(&mut ack).ok()?;
    let ack: serde_json::Value = serde_json::from_str(ack.trim()).ok()?;
    if !ack.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) {
        return None;
    }

    // The real restart_daemon tool call.
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

/// Kill any daemon still alive under `home/run` and remove the dir. Handoff
/// successors run in their own process group (detached), so a harness pgid kill
/// would miss them — clean up by scanning run dirs.
fn cleanup_test_home(home: &Path) {
    for pid in active_pids(home) {
        // SAFETY: deliberate cleanup signal to a known test daemon pid.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    for pid in active_pids(home) {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    if std::env::var("AGEND_KEEP_TEST_HOME").is_err() {
        std::fs::remove_dir_all(home).ok();
    }
}

/// #1814 happy path: with self-respawn ON and NO external supervisor,
/// `restart_daemon` brings up a successor that takes over — a NEW pid serves,
/// the OLD pid dies, agents re-spawn, exactly one active run dir remains. This
/// is the brick-class fix: restart can't strand the control plane even with no
/// supervisor to respawn the process.
#[test]
fn self_respawn_succeeds_with_no_external_supervisor() {
    let home = std::env::temp_dir().join(format!("agend-selfrespawn-ok-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("mkdir AGEND_HOME");

    let mut d1 = boot(&home, SuccessorFault::None);

    // First boot must serve (generous: cold spawn + bind + agent spawn).
    let old_pid = match wait_for_single_active(&home, Duration::from_secs(30), pid_alive) {
        Some(p) => p,
        None => {
            let _ = d1.kill();
            cleanup_test_home(&home);
            panic!("first boot never became the single active daemon");
        }
    };
    assert!(
        ls_lists_probe_within(&home, Duration::from_secs(30)),
        "first boot must serve the probe agent"
    );

    // Operator gate allows restart only when Active (fresh daemon → Away).
    set_mode_active(&home);

    // Real restart over the real api → spawns + health-gates a real successor.
    let _ = trigger_restart(&home, old_pid);

    // A DIFFERENT pid must become the single active daemon (the successor
    // promoted). We assert via `active_pids` (run dir + api.port + live pid),
    // NOT `pid_alive(old_pid)`: the old daemon is a child of THIS test process,
    // so after it exits(0) it stays a zombie (un-`wait`ed) and `kill(pid, 0)`
    // reports a zombie as alive — a false "still alive". `active_pids` instead
    // sees old vanish the moment it removes its own run dir on exit, leaving
    // exactly the successor.
    let new_pid = wait_for_single_active(&home, Duration::from_secs(60), |p| p != old_pid);

    // The successor's agents must be re-spawned (probe served by the new pid).
    let served = new_pid.is_some() && ls_lists_probe_within(&home, Duration::from_secs(30));
    let single_after = active_pids(&home).len() == 1;

    // The original child handle is the OLD daemon (now exited 0); reap it.
    let _ = d1.wait();
    cleanup_test_home(&home);

    let new_pid =
        new_pid.expect("a NEW daemon pid must serve after self-respawn (old must be dead)");
    assert_ne!(new_pid, old_pid, "successor must be a distinct process");
    assert!(
        served,
        "successor must re-spawn agents (probe served by new pid)"
    );
    assert!(
        single_after,
        "exactly one active run dir after handoff (no double-bind / duplication)"
    );
}

/// #1814 abort-stay-alive: when the successor fails its Phase-1 gate (injected
/// crash-on-launch), the predecessor must NOT shut down — the SAME pid keeps
/// serving and `restart_daemon` reports ok:false. No brick, agents intact.
#[test]
fn self_respawn_aborts_and_old_stays_alive_when_successor_fails() {
    let home = std::env::temp_dir().join(format!("agend-selfrespawn-fail-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("mkdir AGEND_HOME");

    let mut d1 = boot(&home, SuccessorFault::OnLaunch); // successor crashes on launch

    let old_pid = match wait_for_single_active(&home, Duration::from_secs(30), pid_alive) {
        Some(p) => p,
        None => {
            let _ = d1.kill();
            cleanup_test_home(&home);
            panic!("first boot never became the single active daemon");
        }
    };

    // Operator gate allows restart only when Active (fresh daemon → Away).
    set_mode_active(&home);

    // Restart must come back ok:false (the predecessor was never signalled, so
    // the reply lands) and the predecessor must still be the SAME live daemon.
    let resp = trigger_restart(&home, old_pid);

    // Give any (wrongly) spawned successor a moment to settle/die, then assert
    // the OLD daemon is unchanged and still serving.
    std::thread::sleep(Duration::from_secs(2));
    let still_old = active_pids(&home) == vec![old_pid] && pid_alive(old_pid);
    let still_serves = ls_lists_probe_within(&home, Duration::from_secs(10));

    let _ = d1.kill();
    let _ = d1.wait();
    cleanup_test_home(&home);

    // The mcp_tool tunnel wraps handler output as {ok:true, result:{...}}; the
    // self-respawn ABORT is signalled by result.ok == false.
    if let Some(resp) = resp {
        let result_ok = resp
            .get("result")
            .and_then(|r| r.get("ok"))
            .and_then(|b| b.as_bool());
        assert_eq!(
            result_ok,
            Some(false),
            "failed-successor restart must report result.ok=false, got {resp}"
        );
    } else {
        panic!("abort path must return a reply (predecessor stays alive to answer)");
    }
    assert!(
        still_old,
        "predecessor must stay the SAME live daemon after abort"
    );
    assert!(
        still_serves,
        "predecessor must keep serving its agents after abort"
    );
}

/// #1814 FIX2 (reviewer race High): the successor passes Phase-1 (answers a
/// STATUS round-trip) so the predecessor COMMITS, but then dies before
/// acquiring the flock — the predecessor's pre-exit liveness recheck must catch
/// this and abort-stay-alive instead of exiting into a brick. Asserts the
/// predecessor (SAME pid) is still the single active daemon, still serving, a
/// while after the commit window.
#[test]
fn self_respawn_aborts_when_successor_dies_after_phase1_commit() {
    let home = std::env::temp_dir().join(format!(
        "agend-selfrespawn-postphase1-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).expect("mkdir AGEND_HOME");

    let mut d1 = boot(&home, SuccessorFault::AfterControlReady);

    let old_pid = match wait_for_single_active(&home, Duration::from_secs(30), pid_alive) {
        Some(p) => p,
        None => {
            let _ = d1.kill();
            cleanup_test_home(&home);
            panic!("first boot never became the single active daemon");
        }
    };
    set_mode_active(&home);

    // Commit happens (successor answers Phase-1), then the successor dies before
    // the flock. The predecessor's loop recheck must abort + stay alive.
    let _ = trigger_restart(&home, old_pid);

    // Wait past the predecessor's tick-latency recheck window (the loop notices
    // the shutdown flag + rechecks the dead successor on its next ~10s tick).
    std::thread::sleep(Duration::from_secs(25));

    let still_old = active_pids(&home) == vec![old_pid] && pid_alive(old_pid);
    let still_serves = ls_lists_probe_within(&home, Duration::from_secs(10));

    let _ = d1.kill();
    let _ = d1.wait();
    cleanup_test_home(&home);

    assert!(
        still_old,
        "predecessor must abort-stay-alive (SAME pid) when the successor dies in the commit→exit window — no brick"
    );
    assert!(
        still_serves,
        "predecessor must keep serving its agents after the FIX2 abort"
    );
}

/// #1814 round-2 (reviewer TOCTOU): the successor survives Phase-1 AND the
/// predecessor's loop-break recheck (so the predecessor begins teardown), then
/// dies DURING teardown — before the predecessor's irreversible exit. The final
/// recover-as-primary gate must catch this: the predecessor does NOT exit, it
/// re-spawns its agents and resumes as primary. Asserts the predecessor (SAME
/// pid) is still the single active daemon, serving its (re-spawned) agent — no
/// brick. (`SELF_RESPAWN_SETTLE_SECS=12` widens the recheck window so the
/// cross-process death lands inside it deterministically.)
#[test]
fn self_respawn_recovers_as_primary_when_successor_dies_during_teardown() {
    let home =
        std::env::temp_dir().join(format!("agend-selfrespawn-teardown-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("mkdir AGEND_HOME");

    let mut d1 = boot(&home, SuccessorFault::DuringTeardown);

    let old_pid = match wait_for_single_active(&home, Duration::from_secs(30), pid_alive) {
        Some(p) => p,
        None => {
            let _ = d1.kill();
            cleanup_test_home(&home);
            panic!("first boot never became the single active daemon");
        }
    };
    set_mode_active(&home);

    let _ = trigger_restart(&home, old_pid);

    // Wait out: loop-break tick (~10s) + shutdown_sequence (~2s) + settle (12s) +
    // re-spawn/resume + margin. Successor dies at control-ready+15s, inside the
    // widened recheck window → predecessor recovers as primary.
    std::thread::sleep(Duration::from_secs(40));

    let still_old = active_pids(&home) == vec![old_pid] && pid_alive(old_pid);
    let still_serves = ls_lists_probe_within(&home, Duration::from_secs(15));

    let _ = d1.kill();
    let _ = d1.wait();
    cleanup_test_home(&home);

    assert!(
        still_old,
        "predecessor must recover-as-primary (SAME pid) when the successor dies during teardown — no brick"
    );
    assert!(
        still_serves,
        "predecessor must re-spawn + keep serving its agent after recover-as-primary"
    );
}

/// Boot a daemon like `boot` but with stderr PIPED (so the loser's
/// already-running / lock error is capturable) and self-respawn ON. Shares the
/// `fleet.yaml` write so concurrent racers resolve the same probe agent.
fn boot_capturing_stderr(home: &Path) -> Child {
    std::fs::create_dir_all(home).ok();
    std::fs::write(
        home.join("fleet.yaml"),
        "instances:\n  probe:\n    backend: shell\n    command: /bin/sh\n",
    )
    .expect("write fleet.yaml");
    Command::new(bin())
        .env("AGEND_HOME", home)
        .env("AGEND_RESTART_HANDOFF", "1")
        .env_remove("AGEND_WRAPPED")
        .env_remove("XPC_SERVICE_NAME")
        .env_remove("INVOCATION_ID")
        .env_remove("AGEND_SUCCESSOR_HANDOFF")
        .args(["start", "--foreground"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("daemon must spawn")
}

/// #1814 Phase 1 — the spike's biggest UNVERIFIED risk, now pinned: a flag-ON
/// self-respawn `exit(0)` can race an external supervisor (launchd `KeepAlive`)
/// respawn. Two daemons must NEVER both bind. This drives the REAL race: two
/// daemon binaries started near-simultaneously on the SAME `AGEND_HOME` — at
/// that instant neither's api socket is up yet, so each one's early
/// `try_attach` finds nothing and BOTH reach `bootstrap::acquire_daemon_lock`,
/// where the `.daemon.lock` `try_lock` fail-fast lets exactly ONE win. The
/// loser exits non-zero with the "already running" lock error — it does NOT
/// become a second daemon. (§3.9: real binaries competing for the real flock,
/// no mock.)
#[test]
fn flag_on_concurrent_respawn_cannot_double_bind_via_flock_1814() {
    let home = std::env::temp_dir().join(format!("agend-1814-dualbind-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("mkdir AGEND_HOME");

    // Two real daemons, same home, fired back-to-back to maximise the flock
    // race (the launchd-KeepAlive-vs-self-respawn collision the spike flagged).
    let mut a = boot_capturing_stderr(&home);
    let mut b = boot_capturing_stderr(&home);

    // Exactly ONE must become the active daemon (the flock winner).
    let winner = wait_for_single_active(&home, Duration::from_secs(30), pid_alive);

    // The other must EXIT (a daemon stays alive; the flock loser does not).
    // Poll both for exit within a generous window.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut a_status = None;
    let mut b_status = None;
    while Instant::now() < deadline && (a_status.is_none() || b_status.is_none()) {
        if a_status.is_none() {
            a_status = a.try_wait().ok().flatten();
        }
        if b_status.is_none() {
            b_status = b.try_wait().ok().flatten();
        }
        if a_status.is_some() || b_status.is_some() {
            // One has exited — the invariant we care about; stop early once the
            // active-daemon count is confirmed single below.
            std::thread::sleep(Duration::from_millis(100));
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let active = active_pids(&home);
    // Capture the loser's stderr for the lock-error assertion before cleanup.
    let loser_stderr = {
        let mut s = String::new();
        for (child, status) in [(&mut a, &a_status), (&mut b, &b_status)] {
            if status.is_some() {
                if let Some(mut err) = child.stderr.take() {
                    use std::io::Read;
                    let _ = err.read_to_string(&mut s);
                }
            }
        }
        s
    };

    // Tear down whichever is still alive (the winner) + reap.
    let _ = a.kill();
    let _ = b.kill();
    let _ = a.wait();
    let _ = b.wait();
    cleanup_test_home(&home);

    // ── Load-bearing invariant: NO double-bind ──
    assert!(
        winner.is_some(),
        "exactly one daemon must win the flock and serve — neither came up"
    );
    assert_eq!(
        active.len(),
        1,
        "NO double-bind: exactly one active daemon (run dir + api.port + live pid), got {}",
        active.len()
    );
    // The loser must EXIT (a real second daemon stays alive); the winner is
    // still running (we killed it above). Exactly one should have exited during
    // the poll window — and with a non-success status (it FAILED to bind, did
    // not clean-exit as a daemon).
    let loser_status = a_status.or(b_status);
    let loser = loser_status.expect("the flock loser must EXIT — neither process exited");
    assert!(
        !loser.success(),
        "the flock loser must exit NON-zero (failed to acquire the lock), got {loser:?}"
    );
    // MECHANISM PIN (codex #2088 — the test's whole point): the loser MUST have
    // died from one of the two legitimate single-daemon fail-fast paths, NOT any
    // unrelated crash. The race decides which path observes first:
    //   - `.daemon.lock` contention in bootstrap::acquire_daemon_lock, or
    //   - attached-daemon detection after the winner has published run/<pid>/.
    // Both prove the same invariant this test exists to pin: no double-bind.
    eprintln!("[1814] flock-loser stderr: {loser_stderr:?}");
    let lock_contended = loser_stderr.contains(
        "another agend-terminal daemon is already running (lock held): \
         try_lock failed because the operation would block",
    );
    let attached_existing = loser_stderr
        .contains("another agend-terminal daemon is already running (pid ")
        && loser_stderr.contains("run_dir ");
    assert!(
        lock_contended || attached_existing,
        "the flock loser MUST exit via a production single-daemon fail-fast path \
         (lock contention or attached-daemon detection), not an unrelated crash; stderr was: \
         {loser_stderr:?}"
    );
}

/// #2098 §3.9: boot a REAL `agend-terminal app` (combined TUI+daemon, `run_app`)
/// under a libc PTY so its `ratatui::init` TUI setup succeeds without an
/// interactive terminal, then its in-process api server comes up (run/<pid>/
/// api.port + api.cookie — IDENTICAL layout to run_core, via the shared
/// `bootstrap::prepare` → OwnedFleet that also writes `.daemon`). Self-respawn is
/// the #2094 DEFAULT (AGEND_RESTART_HANDOFF unset) and every external-supervisor
/// signal is stripped — the exact brick scenario. Returns the child; a DETACHED
/// thread drains the pty master so the TUI never blocks on a full buffer. The
/// drain thread is deliberately NOT joined: a surviving child that inherited the
/// slave fd (e.g. a doomed successor on the pre-fix brick path) would keep the
/// master open forever, so a join could hang. It is a per-test process, so the
/// thread + fd are reclaimed on test-process exit.
fn boot_app_under_pty(home: &Path) -> Child {
    std::fs::create_dir_all(home).ok();
    std::fs::write(
        home.join("fleet.yaml"),
        "instances:\n  probe:\n    backend: shell\n    command: /bin/sh\n",
    )
    .expect("write fleet.yaml");

    // openpty with a non-zero winsize so the TUI has a sane render area.
    // NB: openpty's `termp`/`winp` are `*mut` on macOS but `*const` on Linux;
    // a `*mut` coerces to `*const`, so pass `*mut` to compile on both.
    let mut winsize = libc::winsize {
        ws_row: 40,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    // SAFETY: openpty fills master+slave with fresh valid fds on success (rc==0).
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut::<libc::termios>(),
            &mut winsize as *mut libc::winsize,
        )
    };
    assert_eq!(rc, 0, "openpty must succeed");

    let mut cmd = Command::new(bin());
    cmd.env("AGEND_HOME", home)
        // self-respawn is the #2094 DEFAULT — do NOT set AGEND_RESTART_HANDOFF.
        // Strip ambient supervisor + handoff env so this is the genuine
        // "app mode, self-respawn default ON, no supervisor" brick scenario.
        .env_remove("AGEND_RESTART_HANDOFF")
        .env_remove("AGEND_WRAPPED")
        .env_remove("AGEND_SUPERVISED")
        .env_remove("XPC_SERVICE_NAME")
        .env_remove("INVOCATION_ID")
        .env_remove("AGEND_SUCCESSOR_HANDOFF")
        .arg("app");
    // SAFETY: dup the slave fd for each std stream; std::process owns + closes
    // the dup'd fds. The child's stdio is therefore a real tty (the pty slave),
    // so `ratatui::init` / crossterm raw-mode succeed headlessly.
    unsafe {
        cmd.stdin(Stdio::from_raw_fd(libc::dup(slave)));
        cmd.stdout(Stdio::from_raw_fd(libc::dup(slave)));
        cmd.stderr(Stdio::from_raw_fd(libc::dup(slave)));
    }
    let child = cmd.spawn().expect("app must spawn under pty");
    // Parent no longer needs the slave end (the child holds its own dups).
    // SAFETY: slave is a valid fd we opened above.
    unsafe {
        libc::close(slave);
    }

    // Drain the master so the TUI's writes never block on a full pty buffer.
    // DETACHED (not joined — see fn doc): ends on EOF when the child + any
    // fd-inheriting descendants exit, otherwise on test-process exit.
    std::thread::spawn(move || {
        // SAFETY: master is a valid fd owned here; the File closes it on drop.
        let mut f = unsafe { std::fs::File::from_raw_fd(master) };
        let mut buf = [0u8; 4096];
        loop {
            match f.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });
    child
}

/// Connect to a SPECIFIC daemon's api socket (by pid run dir), do the cookie
/// handshake, and send a STATUS request — true iff a well-formed reply comes
/// back within `budget`. A live control plane answers; a bricked one
/// (RESTART_PENDING latched → the session loop breaks before reading the
/// request, api/mod.rs:499) completes the handshake but never replies.
fn api_status_ok_within(home: &Path, pid: u32, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if api_status_once(home, pid) == Some(true) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

fn api_status_once(home: &Path, pid: u32) -> Option<bool> {
    let run_dir = home.join("run").join(pid.to_string());
    let port: u16 = std::fs::read_to_string(run_dir.join("api.port"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let cookie_bytes = std::fs::read(run_dir.join("api.cookie")).ok()?; // raw 32 bytes
    let stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut writer = stream.try_clone().ok()?;
    let mut reader = BufReader::new(stream);
    writeln!(writer, "{{\"auth\":\"{}\"}}", hex(&cookie_bytes)).ok()?;
    writer.flush().ok();
    let mut ack = String::new();
    reader.read_line(&mut ack).ok()?;
    let ack: serde_json::Value = serde_json::from_str(ack.trim()).ok()?;
    if !ack.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) {
        return Some(false);
    }
    writeln!(writer, "{}", serde_json::json!({"method": "status"})).ok()?;
    writer.flush().ok();
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        // EOF before a reply = the session loop broke before reading our
        // request (the brick: RESTART_PENDING latched).
        return Some(false);
    }
    let reply: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    Some(reply.is_object())
}

/// #2098 §3.9 — `agend-terminal app` (combined TUI+daemon) must NOT brick its
/// control plane on `restart_daemon`. With self-respawn the DEFAULT (#2094) and
/// no external supervisor, the pre-fix handler set RESTART_PENDING in the app
/// process — which has NO run_core consumer — so api/mod.rs broke every session
/// and the held-flock successor 30s-timed-out and died, latching the brick
/// permanently. The fix fails CLOSED in app mode: restart_daemon returns
/// ok:false (actionable) and the app's control plane stays fully alive.
///
/// Plugs the dual-review blind spot: the sibling `self_respawn_*` tests boot
/// `start --foreground` (run_core mode, which DOES consume RESTART_PENDING), so
/// they never exercised the app-mode brick. Here the predecessor is the REAL
/// `app` binary (under a PTY so ratatui::init succeeds headlessly). Post-fix the
/// handler fail-closes BEFORE spawning any successor, so this GREEN run brings up
/// only the one app process (no successor subprocess).
#[test]
fn app_mode_restart_fails_closed_no_brick_2098() {
    let home = std::env::temp_dir().join(format!("agend-2098-appmode-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("mkdir AGEND_HOME");

    let mut child = boot_app_under_pty(&home);

    // The app's in-process api server must come up (run dir + api.port + live pid).
    let pid = match wait_for_single_active(&home, Duration::from_secs(30), pid_alive) {
        Some(p) => p,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            cleanup_test_home(&home);
            panic!("app-mode api server never became the single active daemon");
        }
    };

    // Operator gate allows restart only when Active (fresh daemon → Away).
    set_mode_active(&home);

    // Real restart_daemon over the real app api socket.
    let resp = trigger_restart(&home, pid);

    // ── Load-bearing: NO BRICK ── a NEW session must STILL be served after
    // restart_daemon. Pre-fix the app latched RESTART_PENDING → api/mod.rs broke
    // every session → this STATUS round-trip fails. Post-fix (fail-closed, no
    // flag set) it succeeds.
    let still_alive = api_status_ok_within(&home, pid, Duration::from_secs(15));
    // The SAME app process must remain the single active daemon (no exit, no
    // successor double-bind).
    let same_single = active_pids(&home) == vec![pid] && pid_alive(pid);

    // Cleanup BEFORE asserting so a failure never leaks the child. (The drain
    // thread is detached — see boot_app_under_pty — so there is nothing to join;
    // cleanup_test_home reaps any spawned successor on the pre-fix brick path.)
    let _ = child.kill();
    let _ = child.wait();
    cleanup_test_home(&home);

    // restart_daemon must report fail-closed (mcp_tool wraps handler output as
    // {ok:true, result:{...}}; the app-mode refusal is result.ok == false).
    if let Some(resp) = resp {
        let result_ok = resp
            .get("result")
            .and_then(|r| r.get("ok"))
            .and_then(|b| b.as_bool());
        assert_eq!(
            result_ok,
            Some(false),
            "app-mode restart_daemon must report result.ok=false (fail-closed), got {resp}"
        );
        let err = resp
            .get("result")
            .and_then(|r| r.get("error"))
            .and_then(|e| e.as_str())
            .unwrap_or("");
        assert!(
            err.contains("app"),
            "fail-closed error must name `app` mode (actionable) — got {err:?}"
        );
    } else {
        panic!(
            "app-mode restart must return a reply (the predecessor is never signalled — it stays alive to answer)"
        );
    }
    assert!(
        still_alive,
        "app control plane must stay alive after restart_daemon (no brick) — a new STATUS round-trip failed"
    );
    assert!(
        same_single,
        "the SAME app process must remain the single active daemon (no exit, no double-bind)"
    );
}
