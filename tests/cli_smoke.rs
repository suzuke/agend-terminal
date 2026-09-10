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

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 delivers nothing; it only asks whether the pid exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Owns a unique fixture directory and removes only that exact path.
#[cfg(unix)]
struct UniqueFixtureHome {
    path: std::path::PathBuf,
    cleaned: bool,
}

#[cfg(unix)]
impl UniqueFixtureHome {
    fn new() -> Result<Self, String> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| format!("read clock: {error}"))?
            .as_nanos();
        for attempt in 0..16 {
            let path = std::env::temp_dir().join(format!(
                "agend-cli-smoke-stop-transport-{}-{stamp}-{attempt}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        cleaned: false,
                    })
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(format!("create unique fixture home: {error}")),
            }
        }
        Err("could not allocate a unique fixture home".into())
    }

    fn cleanup(&mut self) -> Result<(), String> {
        if self.cleaned {
            return Ok(());
        }
        match std::fs::remove_dir_all(&self.path) {
            Ok(()) => {
                self.cleaned = true;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.cleaned = true;
                Ok(())
            }
            Err(error) => Err(format!(
                "remove fixture home {}: {error}",
                self.path.display()
            )),
        }
    }
}

#[cfg(unix)]
impl Drop for UniqueFixtureHome {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!("fixture-home cleanup incomplete: {error}");
        }
    }
}

/// Owns one foreground daemon through its `Child` handle. The guard is
/// created immediately after spawn, before readiness polling can fail.
#[cfg(unix)]
struct OwnedForegroundDaemon {
    child: std::process::Child,
    home: std::path::PathBuf,
}

#[cfg(unix)]
impl OwnedForegroundDaemon {
    fn spawn(home: &std::path::Path) -> Result<Self, String> {
        let binary = cmd().get_program().to_owned();
        let child = std::process::Command::new(binary)
            .args(["start", "--foreground"])
            .env("AGEND_HOME", home)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|error| format!("spawn foreground daemon: {error}"))?;
        let mut daemon = Self {
            child,
            home: home.to_path_buf(),
        };
        daemon.wait_ready()?;
        Ok(daemon)
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn reap(&mut self) -> Result<(), String> {
        if self
            .child
            .try_wait()
            .map_err(|error| format!("poll foreground daemon before cleanup: {error}"))?
            .is_some()
        {
            return Ok(());
        }
        self.child
            .kill()
            .map_err(|error| format!("kill owned foreground daemon {}: {error}", self.pid()))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            match self
                .child
                .try_wait()
                .map_err(|error| format!("poll owned foreground daemon {}: {error}", self.pid()))?
            {
                Some(_) => return Ok(()),
                None if std::time::Instant::now() >= deadline => {
                    return Err(format!(
                        "owned foreground daemon {} remained alive after 3s",
                        self.pid()
                    ));
                }
                None => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
    }

    fn wait_ready(&mut self) -> Result<(), String> {
        let run_dir = self.home.join("run").join(self.pid().to_string());
        let started = std::time::Instant::now();
        let budget = std::time::Duration::from_secs(30);
        while started.elapsed() < budget {
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|error| format!("poll foreground daemon: {error}"))?
            {
                return Err(format!("foreground daemon exited early: {status}"));
            }
            if run_dir.join(".daemon").exists()
                && run_dir.join("api.port").exists()
                && run_dir.join(".ready").exists()
            {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Err(format!(
            "foreground daemon did not become ready within {budget:?}"
        ))
    }
}

#[cfg(unix)]
impl Drop for OwnedForegroundDaemon {
    fn drop(&mut self) {
        // The Child handle is the ownership proof; no process-table lookup or
        // PID/argv/PPID matching is used for teardown. Panic unwinding is
        // best-effort, while the normal test path asserts `reap()` directly.
        if let Err(error) = self.reap() {
            eprintln!("owned foreground daemon cleanup incomplete: {error}");
        }
    }
}

/// Owns a stop CLI child and captures output without leaving a joined thread
/// behind if the test itself times out. The output files are fixture-local;
/// the child handle is the only cleanup authority.
#[cfg(unix)]
struct OwnedStop {
    child: std::process::Child,
    stdout_path: std::path::PathBuf,
    stderr_path: std::path::PathBuf,
}

#[cfg(unix)]
impl OwnedStop {
    fn spawn(home: &std::path::Path) -> Result<Self, String> {
        let stdout_path = home.join("stop.stdout");
        let stderr_path = home.join("stop.stderr");
        let stdout = std::fs::File::create(&stdout_path)
            .map_err(|error| format!("create stop stdout capture: {error}"))?;
        let stderr = std::fs::File::create(&stderr_path)
            .map_err(|error| format!("create stop stderr capture: {error}"))?;
        let binary = cmd().get_program().to_owned();
        let child = std::process::Command::new(binary)
            .env("AGEND_HOME", home)
            .arg("stop")
            .stdout(std::process::Stdio::from(stdout))
            .stderr(std::process::Stdio::from(stderr))
            .spawn()
            .map_err(|error| format!("spawn stop: {error}"))?;
        Ok(Self {
            child,
            stdout_path,
            stderr_path,
        })
    }

    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, String> {
        self.child
            .try_wait()
            .map_err(|error| format!("poll stop child: {error}"))
    }

    fn output(&self) -> Result<(String, String), String> {
        let stdout = std::fs::read_to_string(&self.stdout_path)
            .map_err(|error| format!("read stop stdout capture: {error}"))?;
        let stderr = std::fs::read_to_string(&self.stderr_path)
            .map_err(|error| format!("read stop stderr capture: {error}"))?;
        Ok((stdout, stderr))
    }

    fn reap(&mut self) -> Result<(), String> {
        if self.try_wait()?.is_some() {
            return Ok(());
        }
        let _ = self.child.kill();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if self.try_wait()?.is_some() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err("stop child remained alive after 3s cleanup bound".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

#[cfg(unix)]
impl Drop for OwnedStop {
    fn drop(&mut self) {
        if let Err(error) = self.reap() {
            eprintln!("owned stop cleanup incomplete: {error}");
        }
    }
}

/// #3539 (1): `stop` returns only once the daemon process is gone and says so.
#[cfg(unix)]
#[test]
fn stop_waits_for_daemon_exit_and_reports_residuals_untouched_3539() {
    let mut home_guard = UniqueFixtureHome::new().expect("create unique fixture home");
    let home = home_guard.path.clone();
    std::fs::write(
        home.join("fleet.yaml"),
        "defaults:\n  command: /bin/cat\ninstances: {}\n",
    )
    .expect("write fleet.yaml");

    let mut daemon = OwnedForegroundDaemon::spawn(&home).expect("foreground daemon must start");
    let daemon_pid = daemon.pid();
    assert!(pid_alive(daemon_pid), "daemon must be alive before stop");
    // The foreground fixture owns the exact Child, while this test exercises
    // the legacy PID-only identity path; start-token matching is covered by
    // the cli_stop unit tests without making this lifecycle test platform-
    // dependent on a readable process token.
    std::fs::write(
        home.join("run")
            .join(daemon_pid.to_string())
            .join(".daemon"),
        daemon_pid.to_string(),
    )
    .expect("write legacy daemon identity");

    let mut stop = OwnedStop::spawn(&home).expect("spawn owned stop child");
    let reap_deadline = std::time::Instant::now() + std::time::Duration::from_secs(35);
    let mut daemon_reaped = false;
    let mut stop_status = None;
    while !daemon_reaped || stop_status.is_none() {
        if !daemon_reaped {
            daemon_reaped = daemon
                .child
                .try_wait()
                .expect("poll owned foreground daemon during stop")
                .is_some();
        }
        if stop_status.is_none() {
            stop_status = stop.try_wait().expect("poll owned stop child");
        }
        assert!(
            std::time::Instant::now() < reap_deadline,
            "owned foreground daemon and stop child did not settle within the test bound"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let status = stop_status.expect("stop child must have exited");
    let (stdout, stderr) = stop.output().expect("read owned stop output");
    assert!(
        status.success(),
        "stop must exit 0 once the daemon is gone: stdout={stdout} stderr={stderr}"
    );

    // (1) synchronous: the daemon is gone by the time `stop` has returned.
    assert!(
        !pid_alive(daemon_pid),
        "daemon pid {daemon_pid} must have exited before stop returned: {stdout}"
    );
    assert!(
        stdout.contains(&format!("Daemon shutdown initiated (pid {daemon_pid}).")),
        "stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains(&format!("Daemon (pid {daemon_pid}) exited after")),
        "stop must report the exit, not just the accepted request: stdout={stdout} stderr={stderr}"
    );

    assert!(
        stdout.contains("Residual scan: no reparented process in scope looks agend-related"),
        "stop must run the report-only residual scan: {stdout}"
    );
    assert!(
        !stdout.contains("SIGTERM") && !stdout.contains("SIGKILL"),
        "the residual report must not read as an action: {stdout}"
    );
    daemon
        .reap()
        .expect("owned foreground daemon must be reaped");
    home_guard
        .cleanup()
        .expect("unique fixture home must be removed after child reap");
}

/// `--no-wait` is the pre-#3539 receipt: accepted request, no exit claim.
#[cfg(unix)]
#[test]
fn stop_no_wait_returns_on_the_accepted_request_only_3539() {
    let mut home_guard = UniqueFixtureHome::new().expect("create unique fixture home");
    let home = home_guard.path.clone();
    std::fs::write(
        home.join("fleet.yaml"),
        "defaults:\n  command: /bin/cat\ninstances: {}\n",
    )
    .expect("write fleet.yaml");

    let mut daemon = OwnedForegroundDaemon::spawn(&home).expect("foreground daemon must start");
    let daemon_pid = daemon.pid();

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
    daemon
        .reap()
        .expect("owned foreground daemon must be reaped");
    home_guard
        .cleanup()
        .expect("unique fixture home must be removed after child reap");
}

/// A live, owned daemon with an unreachable API is not the same as an absent
/// daemon: stop must not report success or invite a restart.
#[cfg(unix)]
#[test]
fn stop_transport_failure_is_not_reported_as_absent_3559() {
    let mut home_guard = UniqueFixtureHome::new().expect("create unique fixture home");
    let home = home_guard.path.clone();
    std::fs::write(
        home.join("fleet.yaml"),
        "defaults:\n  command: /bin/cat\ninstances: {}\n",
    )
    .expect("write fleet.yaml");

    let mut daemon = OwnedForegroundDaemon::spawn(&home).expect("foreground daemon must start");
    let daemon_pid = daemon.pid();
    assert!(pid_alive(daemon_pid), "daemon must be alive before stop");

    // Keep the daemon alive but make the published API endpoint unreachable.
    // `daemon` owns the exact foreground child and tears it down after this
    // assertion; `home_guard` removes the unique home after `daemon` drops.
    std::fs::write(
        home.join("run")
            .join(daemon_pid.to_string())
            .join("api.port"),
        "1\n",
    )
    .expect("replace api port with a refused endpoint");

    let output = cmd()
        .env("AGEND_HOME", &home)
        .arg("stop")
        .output()
        .expect("run stop");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "transport failure must not be reported as success: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stderr.contains("Unable to contact daemon"),
        "transport failure must be explicit: stdout={stdout} stderr={stderr}"
    );
    assert!(
        pid_alive(daemon_pid),
        "the daemon remains live after the refused request: stdout={stdout} stderr={stderr}"
    );
    daemon
        .reap()
        .expect("owned foreground daemon must be reaped");
    assert!(
        !pid_alive(daemon_pid),
        "owned foreground daemon must be gone after explicit cleanup"
    );
    home_guard
        .cleanup()
        .expect("unique fixture home must be removed after child reap");
}
