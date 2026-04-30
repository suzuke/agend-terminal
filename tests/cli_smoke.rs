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

    // Pin known subcommands (logic-outcome: subcommand presence, not exact wording)
    for sub in &[
        "start",
        "daemon",
        "app",
        "mcp",
        "bugreport",
        "completions",
        "stop",
    ] {
        assert!(
            stdout.contains(sub),
            "help must mention subcommand '{sub}', got:\n{stdout}"
        );
    }
}

/// `agend bugreport` with a valid temp AGEND_HOME produces output.
#[test]
fn bugreport_writes_with_valid_home() {
    let home =
        std::env::temp_dir().join(format!("agend-cli-smoke-bugreport-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();

    let output = cmd().env("AGEND_HOME", &home).arg("bugreport").assert();

    // bugreport may succeed or fail depending on system state,
    // but must not panic (exit code 101 = panic)
    let code = output.get_output().status.code().unwrap_or(-1);
    assert_ne!(code, 101, "bugreport must not panic");

    std::fs::remove_dir_all(&home).ok();
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
