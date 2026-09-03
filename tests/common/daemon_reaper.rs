//! Test-only reaper for daemons a fixture booted.
//!
//! Integration tests that boot a REAL daemon cannot reap it with a `Child`
//! handle: `src/bootstrap/daemon_spawn.rs` spawns the daemon with
//! `process_group(0)`, so it is its own process-group leader and is never the
//! test binary's child (in `app_singleton_fail_closed` the test's child is
//! `agend-terminal app` and the daemon is a grandchild; in `cli_smoke` the
//! child is the `start` CLI, which exits immediately). `child.kill()` therefore
//! leaves the daemon alive, orphaned to init.
//!
//! Killing the daemon's process group is not enough either: the daemon spawns
//! its stub agents through `libc::setsid()` (`src/process.rs`), so each agent
//! lands in its own session/group. Measured on this machine: daemon pid 85716
//! pgid 85716, its `/bin/sh` stub pid 85887 pgid **85887**.
//!
//! Unix-only by decision — both leaking targets are already `#[cfg(unix)]`.

#![cfg(unix)]

use std::path::Path;
use std::process::Command;

/// A process still referencing a fixture home, as observed in the process table.
#[allow(dead_code)]
#[derive(Debug)]
pub struct HomeReferencingProcess {
    pub pid: u32,
    pub command: String,
}

/// Which processes still reference `home` in their command line?
///
/// This is the ASSERTION side of the harness — a check, never a kill — so a
/// `ps` scan is the right tool: it sees a leaked daemon even after its run dir
/// has been destroyed. Killing uses the authoritative `<home>/run/<pid>` handle
/// instead (see `FixtureHome`), never a command-line match.
///
/// `#[allow(dead_code)]` because `tests/common` is compiled into several test
/// binaries, not all of which use this helper.
#[allow(dead_code)]
pub fn processes_referencing_home(home: &Path) -> Vec<HomeReferencingProcess> {
    let needle = home.display().to_string();
    let out = match Command::new("ps").args(["-eo", "pid=,command="]).output() {
        Ok(out) => out,
        Err(_) => return Vec::new(),
    };
    let me = std::process::id();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, command) = line.split_once(char::is_whitespace)?;
            let pid: u32 = pid.parse().ok()?;
            if pid == me || !command.contains(&needle) {
                return None;
            }
            Some(HomeReferencingProcess {
                pid,
                command: command.trim().to_string(),
            })
        })
        .collect()
}

/// Panic naming every process still holding `home`, for use immediately before
/// the fixture directory is torn down.
///
/// `#[allow(dead_code)]`: see `processes_referencing_home`.
#[allow(dead_code)]
pub fn assert_no_process_references_home(home: &Path, context: &str) {
    let leaked = processes_referencing_home(home);
    assert!(
        leaked.is_empty(),
        "{context}: fixture home {} is still referenced by {} process(es) after \
         the test finished — the harness leaked a daemon (or its stub agents) to \
         init: {leaked:#?}",
        home.display(),
        leaked.len(),
    );
}
