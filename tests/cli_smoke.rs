//! Sprint 42 Phase 1 — CLI smoke tests using assert_cmd.
//!
//! Exercises the compiled `agend-terminal` binary end-to-end for
//! version, help, bugreport, and completions subcommands.

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
#[test]
fn connect_failed_spawn_deregisters_external_agent() {
    let stamp = std::process::id();
    let home = std::env::temp_dir().join(format!("agend-cli-smoke-connect-home-{stamp}"));
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

    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = Command::cargo_bin("agend-terminal")
                .expect("binary must exist")
                .env("AGEND_HOME", &self.0)
                .arg("stop")
                .output();
            let _ = std::fs::remove_dir_all(&self.0);
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
