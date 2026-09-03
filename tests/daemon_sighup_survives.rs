//! RED/GREEN behavioural test for #3499.
//!
//! Closing the terminal that launched `agend-terminal app`/`start` delivers
//! SIGHUP to the detached daemon. Before the fix the daemon's `ctrlc`-based
//! handler (`src/bootstrap/signals.rs::install`, `termination` feature)
//! bundles SIGINT/SIGTERM/SIGHUP into one shutdown trip, so SIGHUP kills it —
//! the run dir gets cleaned, `stop` then reports "not running", and the
//! backend agents are orphaned.
//!
//! This test boots a REAL detached daemon (the realistic route: the CLI
//! `start` path, same as `cli_smoke.rs`) and signals it directly:
//!
//!   - Assertion A (the RED): SIGHUP must NOT kill it.
//!   - Assertion B (negative control): SIGTERM must still kill it — without
//!     this, "fixed" could mean "the daemon ignores every signal", not "the
//!     daemon correctly distinguishes SIGHUP from SIGTERM".
//!
//! Uses `FixtureHome` (see `tests/common/daemon_reaper.rs`) because `start`
//! spawns the daemon with its own process group / session, so it is never
//! this test binary's child and a `Child` handle cannot reap it —
//! `tests/daemon_boot_gate_invariant.rs` requires any direct-boot test to use
//! this guard.

#![cfg(unix)]

mod common;
use common::daemon_reaper::{pid_is_alive, FixtureHome};

use assert_cmd::Command;
use std::path::Path;
use std::time::{Duration, Instant};

fn cmd() -> Command {
    Command::cargo_bin("agend-terminal").expect("binary must exist")
}

/// Read the pid published under `<home>/run/<pid>` — the directory NAME is
/// the daemon's pid (`src/daemon/mod.rs::run_dir_for_pid`).
fn published_pid(home: &Path) -> u32 {
    let run_dir = home.join("run");
    let entries: Vec<u32> = std::fs::read_dir(&run_dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", run_dir.display()))
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<u32>().ok())
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one published run dir under {}, got {entries:?}",
        run_dir.display()
    );
    entries[0]
}

/// Bounded poll — never an unbounded wait. Returns whether `pred` became true
/// before `budget` elapsed.
fn poll_until(budget: Duration, pred: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn sighup_does_not_kill_detached_daemon_but_sigterm_does() {
    let stamp = std::process::id();
    let home_guard = FixtureHome::new(&format!("agend-sighup-survives-home-{stamp}"));
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

    let pid = published_pid(&home);
    assert!(
        pid_is_alive(pid),
        "daemon pid {pid} must be alive right after `start` publishes its run dir"
    );

    // `.daemon` is published (`src/daemon/mod.rs::run_core`) well before
    // `bootstrap::signals::install` runs (it happens after `spawn_fleet_agents`),
    // so a signal sent immediately after `start` succeeds can race the signal
    // handler installation. Give the daemon a bounded grace window to finish
    // booting before we start signalling it — same shape as `cli_smoke.rs`'s
    // `connect_failed_spawn_deregisters_external_agent`, which sleeps for the
    // identical reason.
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        pid_is_alive(pid),
        "daemon pid {pid} must still be alive after the startup grace window"
    );

    // Assertion A (the RED): a detached daemon must survive SIGHUP.
    unsafe { libc::kill(pid as i32, libc::SIGHUP) };
    let died_from_sighup = poll_until(Duration::from_secs(3), || !pid_is_alive(pid));
    assert!(
        !died_from_sighup,
        "daemon pid {pid} died within 3s of SIGHUP — a detached daemon must survive \
         SIGHUP (#3499): closing the launching terminal must not kill it"
    );

    // Assertion B (negative control): SIGTERM must still shut it down. Without
    // this, "fixed" could mean "the daemon now ignores every signal".
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    let died_from_sigterm = poll_until(Duration::from_secs(5), || !pid_is_alive(pid));
    assert!(
        died_from_sigterm,
        "daemon pid {pid} did not exit within 5s of SIGTERM — negative control failed \
         (fix must not make the daemon immortal, only SIGHUP-tolerant)"
    );
}
