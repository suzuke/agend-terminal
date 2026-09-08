//! Sprint 42 Phase 1 — CLI smoke tests using assert_cmd.
//!
//! Exercises the compiled `agend-terminal` binary end-to-end for
//! version, help, bugreport, and completions subcommands.

mod common;

// Only the two real-daemon tests below are `#[cfg(unix)]`; this file also
// builds on Windows, where `daemon_reaper` does not exist.
#[cfg(unix)]
use common::daemon_reaper::FixtureHome;

use assert_cmd::Command;
use predicates::prelude::*;

fn cmd() -> Command {
    Command::cargo_bin("agend-terminal").expect("binary must exist")
}

/// `agend --version` must output the Cargo.toml package version.
#[test]
fn version_outputs_cargo_toml_version() {
    let version = env!("CARGO_PKG_VERSION");
    cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(version));
}

/// `agend --help` must mention key subcommands.
#[test]
fn help_renders_known_subcommands() {
    let output = cmd().arg("--help").assert().success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout);

    // Pin known subcommands (logic-outcome: subcommand presence, not exact wording).
    // Wave 1 CLI consolidation: `daemon` removed in favor of `start --agents`;
    // Sprint 56 Track I-Phase2c (#531): `mcp` removed in favor of the
    // separate `agend-mcp-bridge` binary.
    for sub in &["start", "app", "bugreport", "completions", "stop"] {
        assert!(
            stdout.contains(sub),
            "help must mention subcommand '{sub}', got:\n{stdout}"
        );
    }
    // Sprint 56 Track I-Phase2c regression-proof: `mcp` MUST NOT reappear
    // in the top-level help (the canonical entry is the standalone
    // `agend-mcp-bridge` binary, which clap does not list).
    assert!(
        !stdout.lines().any(|l| {
            let trimmed = l.trim_start();
            trimmed.starts_with("mcp ") || trimmed == "mcp"
        }),
        "Phase2c invariant: top-level help must not list `mcp` subcommand, got:\n{stdout}"
    );
}

/// The one-release compatibility window for `--legacy-json` has expired;
/// the removed flag must fail at CLI parsing rather than silently selecting
/// the pre-#938 payload.
#[test]
fn legacy_json_flag_is_rejected_after_sunset() {
    cmd()
        .args(["list", "--json", "--legacy-json"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "unexpected argument '--legacy-json'",
        ));
}

/// `list --json` always emits the canonical mode envelope after the legacy
/// compatibility flag is removed.
#[test]
fn list_json_emits_canonical_mode_envelope() {
    let home = std::env::temp_dir().join(format!(
        "agend-cli-smoke-list-json-envelope-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).expect("create home dir");

    let output = cmd()
        .env("AGEND_HOME", &home)
        .args(["list", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&output.get_output().stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON output");
    assert_eq!(value["mode"], "fallback_daemon_absent");
    assert!(
        value["agents"].is_array(),
        "agents must remain an array: {value}"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// `agend bugreport` with a valid temp AGEND_HOME produces output under
/// AGEND_HOME/bugreports rather than littering the caller's current directory.
#[test]
fn bugreport_writes_with_valid_home() {
    let stamp = std::process::id();
    let home = std::env::temp_dir().join(format!("agend-cli-smoke-bugreport-home-{stamp}"));
    let cwd = std::env::temp_dir().join(format!("agend-cli-smoke-bugreport-cwd-{stamp}"));
    std::fs::create_dir_all(&home).ok();
    std::fs::create_dir_all(&cwd).ok();

    let output = cmd()
        .env("AGEND_HOME", &home)
        .current_dir(&cwd)
        .arg("bugreport")
        .assert()
        .success();

    let stdout = String::from_utf8_lossy(&output.get_output().stdout);
    let path = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Bug report saved to: "))
        .map(std::path::PathBuf::from)
        .expect("bugreport should print the saved report path");
    assert!(
        path.starts_with(home.join("bugreports")),
        "bugreport should write under AGEND_HOME/bugreports, got {}",
        path.display()
    );
    assert!(
        path.exists(),
        "bugreport output should exist at {}",
        path.display()
    );
    let cwd_reports: Vec<_> = std::fs::read_dir(&cwd)
        .expect("read cwd dir")
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("bugreport-")
        })
        .collect();
    assert!(
        cwd_reports.is_empty(),
        "bugreport should not write into cwd"
    );

    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&cwd).ok();
}

/// `agend bugreport` with nonexistent AGEND_HOME errors clearly.
#[test]
fn bugreport_with_nonexistent_home_errors_clearly() {
    let output = cmd()
        .env("AGEND_HOME", "/nonexistent/agend-smoke-test-path")
        .arg("bugreport")
        .output()
        .expect("run bugreport");

    // On macOS /nonexistent is read-only → error. On Linux it may also fail.
    // The CLI must either succeed (created the dir) or fail with a descriptive
    // error containing filesystem-related keywords — never panic (exit 101).
    let code = output.status.code().unwrap_or(-1);
    assert_ne!(code, 101, "bugreport must not panic");

    if !output.status.success() {
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            combined.contains("Error")
                || combined.contains("error")
                || combined.contains("denied")
                || combined.contains("not found")
                || combined.contains("os error"),
            "failed bugreport must contain descriptive error keyword, got: {combined}"
        );
    }
}

/// `connect` must not leave a stale external-agent registration if backend
/// spawn fails after successful daemon registration.
///
/// `#[cfg(unix)]`: this is a heavy real-daemon integration test (spawns a daemon
/// via `start`, then drives `connect`). The repo gates all such daemon-spawning
/// integration tests to Unix (e.g. `tests/e2e_workflow.rs`, `tests/restart_smoke.rs`,
/// `tests/ready_marker_invariants.rs`) because the daemon-spawn/teardown path hangs
/// under the Windows nextest runner (observed: this test TIMED OUT at 120s on
/// `windows-latest`). The fix it guards (`src/connect.rs` deregister-on-spawn-failure)
/// is platform-independent and is exercised on the Linux/macOS CI legs.
#[cfg(unix)]
#[test]
fn connect_failed_spawn_deregisters_external_agent() {
    let stamp = std::process::id();
    // `FixtureHome` owns teardown: it reaps any daemon this fixture booted (and
    // its setsid'd stub agents) BEFORE removing the directory. `start` spawns
    // the daemon with `process_group(0)`, so it is not this process's child and
    // no `Child` handle can reach it.
    let home_guard = FixtureHome::new(&format!("agend-cli-smoke-connect-home-{stamp}"));
    let home = home_guard.path().to_path_buf();
    let shell_dir = home.join("workspace/shell");
    let ext_dir = home.join("workspace/ext");
    std::fs::create_dir_all(&shell_dir).expect("create shell dir");
    std::fs::create_dir_all(&ext_dir).expect("create ext dir");
    std::fs::write(
        home.join("fleet.yaml"),
        format!(
            "defaults:\n  backend: claude\ninstances:\n  shell:\n    command: /bin/bash\n    working_directory: {}\n",
            shell_dir.display()
        ),
    )
    .expect("write fleet.yaml");

    /// Graceful teardown only. `stop` is asynchronous and its result was
    /// already being ignored; `home_guard` drops after this and does the
    /// verifying + escalation to SIGTERM/SIGKILL, then removes the directory.
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = Command::cargo_bin("agend-terminal")
                .expect("binary must exist")
                .env("AGEND_HOME", &self.0)
                .arg("stop")
                .output();
        }
    }
    let _cleanup = Cleanup(home.clone());

    cmd()
        .env("AGEND_HOME", &home)
        .arg("start")
        .assert()
        .success();
    std::thread::sleep(std::time::Duration::from_secs(4));

    cmd()
        .env("AGEND_HOME", &home)
        .args([
            "connect",
            "badext",
            "--backend",
            "/no/such/backend",
            "--working-dir",
        ])
        .arg(&ext_dir)
        .assert()
        .failure();

    let list = cmd()
        .env("AGEND_HOME", &home)
        .args(["list", "--json"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&list.get_output().stdout);
    assert!(
        !stdout.contains("badext"),
        "failed connect must deregister stale external agent, got: {stdout}"
    );
}

/// A second detached-default `start` must not mistake the first daemon's
/// already-published run dir for evidence that its own child started.
#[cfg(unix)]
#[test]
fn second_detached_start_rejects_existing_daemon() {
    let stamp = std::process::id();
    let home_guard = FixtureHome::new(&format!("agend-cli-smoke-second-start-{stamp}"));
    let home = home_guard.path().to_path_buf();
    std::fs::write(
        home.join("fleet.yaml"),
        "defaults:\n  command: /bin/cat\ninstances:\n  probe: {}\n",
    )
    .expect("write fleet.yaml");

    /// Graceful teardown only. `stop` is asynchronous and its result was
    /// already being ignored; `home_guard` drops after this and does the
    /// verifying + escalation to SIGTERM/SIGKILL, then removes the directory.
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = Command::cargo_bin("agend-terminal")
                .expect("binary must exist")
                .env("AGEND_HOME", &self.0)
                .arg("stop")
                .output();
        }
    }
    let _cleanup = Cleanup(home.clone());

    cmd()
        .env("AGEND_HOME", &home)
        .arg("start")
        .assert()
        .success();
    cmd()
        .env("AGEND_HOME", &home)
        .arg("start")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "another agend-terminal daemon is already running",
        ));
}

/// `agend app` without a TTY must fail with a clean, actionable error rather
/// than panicking (exit 101) and leaking raw terminal escape sequences.
#[test]
fn app_without_tty_errors_cleanly_not_panic() {
    let stamp = std::process::id();
    let home = std::env::temp_dir().join(format!("agend-cli-smoke-app-tty-{stamp}"));
    std::fs::create_dir_all(&home).expect("create home dir");

    // assert_cmd pipes stdin/stdout (not a TTY), reproducing the headless case.
    let output = cmd()
        .env("AGEND_HOME", &home)
        .arg("app")
        .output()
        .expect("run app");

    let code = output.status.code().unwrap_or(-1);
    assert_ne!(code, 101, "app must not panic when stdout is not a TTY");
    assert!(!output.status.success(), "app should fail without a TTY");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        combined.contains("TTY") || combined.contains("interactive terminal"),
        "app must explain it needs a TTY, got: {combined}"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// `agend completions` for each shell produces non-empty, distinct output.
#[test]
fn completions_emits_for_zsh_fish_bash() {
    let mut outputs = Vec::new();
    for shell in &["zsh", "fish", "bash"] {
        let output = cmd().args(["completions", shell]).assert().success();
        let stdout = output.get_output().stdout.clone();
        assert!(
            !stdout.is_empty(),
            "completions {shell} must produce non-empty output"
        );
        outputs.push((shell.to_string(), stdout));
    }
    // Verify outputs differ across shells
    assert_ne!(
        outputs[0].1, outputs[1].1,
        "zsh and fish completions must differ"
    );
    assert_ne!(
        outputs[0].1, outputs[2].1,
        "zsh and bash completions must differ"
    );
}

/// `agend attach <nonexistent>` without daemon shows daemon hint, not "not found".
#[test]
fn attach_without_daemon_shows_daemon_hint() {
    let output = cmd()
        .env(
            "AGEND_HOME",
            std::env::temp_dir().join("agend-attach-test-nodaemon"),
        )
        .arg("attach")
        .arg("ghost-agent")
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&output.get_output().stderr);
    assert!(
        stderr.contains("not running") || stderr.contains("Start it with"),
        "attach without daemon must hint about daemon, got stderr: {stderr}"
    );
    assert!(
        !stderr.contains("not found"),
        "must not say 'not found' when daemon isn't running"
    );
}

// ---------------------------------------------------------------------------
// #3539: `stop` waits for the daemon to be gone and reports what it left behind
// ---------------------------------------------------------------------------

/// Poll `<home>/run/<pid>/` for a daemon that has published both its identity
/// and its API port. Returns the daemon pid.
#[cfg(unix)]
fn wait_for_daemon_identity(home: &std::path::Path, budget: std::time::Duration) -> u32 {
    let started = std::time::Instant::now();
    while started.elapsed() < budget {
        if let Ok(entries) = std::fs::read_dir(home.join("run")) {
            for entry in entries.flatten() {
                let dir = entry.path();
                if dir.join("api.port").exists() {
                    if let Ok(content) = std::fs::read_to_string(dir.join(".daemon")) {
                        if let Some(pid) = content.split(':').next().and_then(|p| p.parse().ok()) {
                            return pid;
                        }
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!(
        "daemon under {} did not publish .daemon + api.port within {budget:?}",
        home.display()
    );
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 delivers nothing; it only asks whether the pid exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// The session id the V1 orphan scope reads (`getsid`), or -1 when unreadable.
#[cfg(unix)]
fn session_of(pid: u32) -> i32 {
    // SAFETY: getsid on any pid returns -1/ESRCH rather than misbehaving.
    unsafe { libc::getsid(pid as libc::pid_t) }
}

/// What a reader needs when the residual scan does not list a planted pid:
/// the kernel's view of each pid (`ps` + `getsid`, i.e. exactly the inputs of
/// `scope_reparented`) and the full V1 report — every candidate with or without
/// a hint, plus the scope counts — which bare `doctor` prints without a daemon.
#[cfg(unix)]
fn residue_diagnostics(home: &std::path::Path, pids: &[u32]) -> String {
    let list: Vec<String> = pids.iter().map(|p| p.to_string()).collect();
    let ps = std::process::Command::new("ps")
        .args(["-o", "pid,ppid,uid,sess,command", "-p", &list.join(",")])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_else(|e| format!("ps failed: {e}"));
    let sids: Vec<String> = pids
        .iter()
        .map(|p| format!("getsid({p})={}", session_of(*p)))
        .collect();
    let doctor = cmd()
        .env("AGEND_HOME", home)
        .arg("doctor")
        .output()
        .map(|o| {
            let out = String::from_utf8_lossy(&o.stdout).to_string();
            out.split("Reparented processes in scope")
                .nth(1)
                .map(|tail| format!("Reparented processes in scope{tail}"))
                .unwrap_or(out)
        })
        .unwrap_or_else(|e| format!("doctor failed: {e}"));
    format!(
        "--- diagnostics ---\nself pid={} getsid={}\n{ps}{}\n--- doctor V1 report ---\n{doctor}",
        std::process::id(),
        session_of(std::process::id()),
        sids.join(" ")
    )
}

/// Processes planted as if a test runner had died and left them behind:
/// `sleep` invoked through a symlink at `…/target/debug/deps/agend_terminal-<hash>`
/// (the #3539 residue shape — `ps` reports argv[0] as invoked, and a symlink
/// keeps the real `/bin/sleep`, which macOS would SIGKILL if copied out of its
/// signed location) and a plain one elsewhere (the control).
///
/// The V1 scope (`scope_reparented`) is `ppid == 1` AND same uid AND `sid != 1`
/// AND not a session leader. A child backgrounded from a plain `sh` only
/// guarantees the first: it INHERITS the session, and on the GitHub macOS
/// runner that inherited session put it outside the scope (deterministic red
/// at cli_smoke.rs:551 on two heads, green on ubuntu and on a developer
/// shell). So the throw-away `sh` is started with `setsid()`: it becomes the
/// leader of a fresh session, the `sleep` inherits that session, and when the
/// `sh` exits the `sleep` holds a session it did not create whose leader is
/// dead — the exact #3273 shape — with `ppid == 1`, whatever session the test
/// process itself runs in. `Drop` kills them — test-only authority.
#[cfg(unix)]
struct PlantedResidue {
    hinted_pid: u32,
    control_pid: u32,
}

#[cfg(unix)]
impl PlantedResidue {
    fn plant(home: &std::path::Path) -> Self {
        let deps = home.join("target").join("debug").join("deps");
        let plain = home.join("ctl");
        std::fs::create_dir_all(&deps).expect("mkdir deps");
        std::fs::create_dir_all(&plain).expect("mkdir ctl");
        let hinted = deps.join("agend_terminal-deadbeef3539");
        let control = plain.join("plain-sleeper");
        std::os::unix::fs::symlink("/bin/sleep", &hinted).expect("link sleep as test-binary shape");
        std::os::unix::fs::symlink("/bin/sleep", &control).expect("link sleep as control");
        Self {
            hinted_pid: Self::spawn_reparented(&hinted, home),
            control_pid: Self::spawn_reparented(&control, home),
        }
    }

    fn spawn_reparented(exe: &std::path::Path, cwd: &std::path::Path) -> u32 {
        use std::os::unix::process::CommandExt;
        let mut sh = std::process::Command::new("sh");
        sh.arg("-c")
            .arg("\"$0\" 300 >/dev/null 2>&1 & echo $!")
            .arg(exe)
            .current_dir(cwd);
        // SAFETY: `setsid` is async-signal-safe and touches only the child's
        // own session/group; the same shape as tests/harness_smoke.rs.
        unsafe {
            sh.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let out = sh.output().expect("spawn via sh");
        let pid: u32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("sh must echo the background pid");
        // `sh` has exited, so the child now belongs to init and holds the
        // session that `sh` created and no longer leads.
        let started = std::time::Instant::now();
        while pid_alive(pid) && started.elapsed() < std::time::Duration::from_secs(5) {
            let ppid = std::process::Command::new("ps")
                .args(["-o", "ppid=", "-p", &pid.to_string()])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            let sid = session_of(pid);
            if ppid == "1" && sid > 1 && sid != pid as i32 {
                return pid;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!(
            "planted pid {pid} never reached the V1 scope shape (ppid 1, sid > 1, not its own \
             session leader): alive={} getsid={}",
            pid_alive(pid),
            session_of(pid)
        );
    }
}

#[cfg(unix)]
impl Drop for PlantedResidue {
    fn drop(&mut self) {
        for pid in [self.hinted_pid, self.control_pid] {
            // SAFETY: exact positive pid this fixture spawned; test-only.
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
            let started = std::time::Instant::now();
            while pid_alive(pid) && started.elapsed() < std::time::Duration::from_secs(5) {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
}

/// #3539 (1): `stop` returns only once the daemon process is gone and says so.
/// #3539 (2): what a dead test runner left behind is LISTED with a hint and
/// left alone — the control process without a hint is not listed, and neither
/// is signalled (both are still alive after `stop` returns).
#[cfg(unix)]
#[test]
fn stop_waits_for_daemon_exit_and_lists_hinted_residue_untouched_3539() {
    let stamp = std::process::id();
    let home_guard = FixtureHome::new(&format!("agend-cli-smoke-stop-wait-{stamp}"));
    let home = home_guard.path().to_path_buf();
    std::fs::write(
        home.join("fleet.yaml"),
        "defaults:\n  command: /bin/cat\ninstances:\n  probe: {}\n",
    )
    .expect("write fleet.yaml");

    cmd()
        .env("AGEND_HOME", &home)
        .arg("start")
        .assert()
        .success();
    let daemon_pid = wait_for_daemon_identity(&home, std::time::Duration::from_secs(30));
    assert!(pid_alive(daemon_pid), "daemon must be alive before stop");

    // Declared AFTER `home_guard` so it drops FIRST: the planted binaries live
    // under the home, and the guard's teardown asserts nothing references it.
    let residue = PlantedResidue::plant(&home);

    let output = cmd()
        .env("AGEND_HOME", &home)
        .arg("stop")
        .output()
        .expect("run stop");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stop must exit 0 once the daemon is gone: {stdout}"
    );

    // (1) synchronous: the daemon is gone by the time `stop` has returned.
    assert!(
        !pid_alive(daemon_pid),
        "daemon pid {daemon_pid} must have exited before stop returned: {stdout}"
    );
    assert!(
        stdout.contains(&format!("Daemon shutdown initiated (pid {daemon_pid}).")),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("Daemon (pid {daemon_pid}) exited after")),
        "stop must report the exit, not just the accepted request: {stdout}"
    );

    // (2) residue: listed with its hint, not acted on; control not listed.
    // The diagnostics are built only when an assertion fails.
    let planted = [residue.hinted_pid, residue.control_pid];
    assert!(
        stdout.contains(&format!(
            "pid={} hint=test-runner-binary",
            residue.hinted_pid
        )),
        "the test-runner-shaped residue must be listed with its hint: {stdout}\n{}",
        residue_diagnostics(&home, &planted)
    );
    assert!(
        !stdout.contains(&format!("pid={} ", residue.control_pid)),
        "a reparented process without a hint is not stop's business: {stdout}\n{}",
        residue_diagnostics(&home, &planted)
    );
    assert!(
        pid_alive(residue.hinted_pid) && pid_alive(residue.control_pid),
        "stop reports residue and never signals it (#3273 consensus)"
    );
    assert!(
        !stdout.contains("SIGTERM") && !stdout.contains("SIGKILL"),
        "the residual report must not read as an action: {stdout}"
    );
    drop(residue);
}

/// `--no-wait` is the pre-#3539 receipt: accepted request, no exit claim.
#[cfg(unix)]
#[test]
fn stop_no_wait_returns_on_the_accepted_request_only_3539() {
    let stamp = std::process::id();
    let home_guard = FixtureHome::new(&format!("agend-cli-smoke-stop-nowait-{stamp}"));
    let home = home_guard.path().to_path_buf();
    std::fs::write(
        home.join("fleet.yaml"),
        "defaults:\n  command: /bin/cat\ninstances:\n  probe: {}\n",
    )
    .expect("write fleet.yaml");

    cmd()
        .env("AGEND_HOME", &home)
        .arg("start")
        .assert()
        .success();
    let daemon_pid = wait_for_daemon_identity(&home, std::time::Duration::from_secs(30));

    let output = cmd()
        .env("AGEND_HOME", &home)
        .args(["stop", "--no-wait"])
        .output()
        .expect("run stop --no-wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(
        stdout.contains(&format!("Daemon shutdown initiated (pid {daemon_pid}).")),
        "{stdout}"
    );
    assert!(
        !stdout.contains("exited after") && !stdout.contains("Residual scan"),
        "--no-wait must not claim anything about the exit: {stdout}"
    );
    // `home_guard` reaps whatever is still shutting down.
}
