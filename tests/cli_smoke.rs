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

#[test]
#[cfg(unix)]
// This fixture intentionally treats daemon/bootstrap corruption as a test
// failure; keep the setup assertions local instead of obscuring them behind
// fallible plumbing in this end-to-end smoke test.
#[allow(clippy::unwrap_used)]
fn operator_settlement_3553_cli_positive_roundtrip() {
    use std::time::{Duration, Instant};
    const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(45);
    const CLI_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
    struct Fixture {
        home: std::path::PathBuf,
        child: Option<std::process::Child>,
    }
    impl Fixture {
        fn start(&mut self) {
            self.child = Some(
                std::process::Command::new(assert_cmd::cargo::cargo_bin("agend-terminal"))
                    .env("AGEND_HOME", &self.home)
                    .current_dir(&self.home)
                    .args(["start", "--foreground"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::fs::File::create(self.home.join("daemon.stderr")).unwrap())
                    .spawn()
                    .unwrap(),
            );
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
                if child.wait().is_err() {
                    return;
                }
            }
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }
    let mut fixture = Fixture {
        home: std::env::temp_dir().join(format!("settlement-positive-{}", uuid::Uuid::new_v4())),
        child: None,
    };
    std::fs::create_dir(&fixture.home).unwrap();
    // A minimal shell probe keeps the fixture on the normal (non-successor)
    // startup path: `start_with_fleet` rejects a truly empty fleet, while the
    // settlement contract only needs one owned daemon transport.
    std::fs::write(
        fixture.home.join("fleet.yaml"),
        "schema_version: 1\ndefaults:\n  backend: shell\n  command: /bin/cat\ninstances:\n  probe: {}\n",
    )
    .unwrap();
    let log = fixture.home.join("task_events.jsonl");
    let seed = serde_json::json!({"schema_version":3,"seq":1,
        "timestamp":"2026-01-01T00:00:00Z","instance":"fixture",
        "event":{"kind":"Created","task_id":"cli-exact","title":"CLI fixture",
            "description":"","priority":"normal","owner":null}});
    std::fs::write(&log, format!("{seed}\n")).unwrap();
    fixture.start();
    let diagnostics = |fixture: &mut Fixture| {
        let child_status = fixture
            .child
            .as_mut()
            .and_then(|child| child.try_wait().ok())
            .flatten();
        let read = |name: &str| {
            std::fs::read_to_string(fixture.home.join(name))
                .unwrap_or_else(|error| format!("<unavailable: {error}>"))
        };
        format!(
            "child={child_status:?}; daemon.stderr={}; daemon.log={}",
            read("daemon.stderr"),
            read("daemon.log")
        )
    };
    let deadline = Instant::now() + DAEMON_READY_TIMEOUT;
    let preview = loop {
        let output = cmd()
            .env("AGEND_HOME", &fixture.home)
            .timeout(CLI_REQUEST_TIMEOUT)
            .args([
                "admin",
                "task-settlement-preview",
                "--task-id",
                "cli-exact",
                "--target",
                "done",
                "--result",
                "operator inspected CLI fixture",
            ])
            .output()
            .unwrap();
        if output.status.success() {
            break serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "daemon/preview did not become ready: {} / {}",
            String::from_utf8_lossy(&output.stderr),
            diagnostics(&mut fixture)
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(std::fs::read_to_string(&log).unwrap(), format!("{seed}\n"));
    let token = preview["result"]["confirmation"].as_str().unwrap();
    for already in [false, true] {
        if already {
            fixture.child.as_mut().unwrap().kill().unwrap();
            fixture.child.as_mut().unwrap().wait().unwrap();
            fixture.child = None;
            fixture.start();
        }
        let deadline = Instant::now() + DAEMON_READY_TIMEOUT;
        let response: serde_json::Value = loop {
            let output = cmd()
                .env("AGEND_HOME", &fixture.home)
                .timeout(CLI_REQUEST_TIMEOUT)
                .args(["admin", "task-settlement-apply", "--confirmation", token])
                .output()
                .unwrap();
            if output.status.success() {
                break serde_json::from_slice(&output.stdout).unwrap();
            }
            assert!(
                Instant::now() < deadline,
                "apply/restart failed: {} / {} / {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
                diagnostics(&mut fixture)
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        assert_eq!(response["result"]["already_applied"], already);
        assert_eq!(response["result"]["current_status"], "done");
        assert_eq!(response["result"]["cleanup_status"], "complete");
    }
    let events: Vec<serde_json::Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"]["kind"] == "OperatorSettled")
            .count(),
        1
    );
}

#[test]
// This fixture intentionally treats daemon/bootstrap corruption as a test
// failure; keep the setup assertion local in this end-to-end smoke test.
#[allow(clippy::unwrap_used)]
fn operator_settlement_3553_cli_transport_failure_is_nonzero() {
    struct Home(std::path::PathBuf);
    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let home = Home(std::env::temp_dir().join(format!("settlement-cli-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir(&home.0).unwrap();
    for args in [
        vec![
            "admin",
            "task-settlement-preview",
            "--task-id",
            "missing",
            "--target",
            "done",
            "--result",
            "inspected",
        ],
        vec!["admin", "task-settlement-apply", "--confirmation", "unused"],
    ] {
        cmd()
            .env("AGEND_HOME", &home.0)
            .timeout(std::time::Duration::from_secs(10))
            .args(args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("no active daemon"));
    }
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
