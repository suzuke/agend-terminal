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
// `tests/common` is compiled fresh into EVERY integration-test binary that
// declares `mod common;` (each `tests/*.rs` is its own crate). Only
// `app_singleton_fail_closed.rs` and `cli_smoke.rs` actually construct
// `FixtureHome` today; every other binary that pulls this module in sees its
// entire contents as unreachable, and `-D warnings` (dead_code) turns that
// into a hard build failure for binaries that never asked to use it. One
// module-level allow, rather than five scattered per-item ones, because the
// reasoning is file-wide, not item-by-item.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

/// A process still referencing a fixture home, as observed in the process table.
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

/// Is `pid` still alive? Mirrors `crate::process::is_pid_alive`, which an
/// integration test binary cannot import (it is not part of the lib's public
/// surface here). `kill(pid, 0)` is the only option available: these processes
/// are not our children, so `waitpid` cannot be used to observe their death.
pub fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: signal 0 performs the permission/existence check only.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Daemons published under `<home>/run/<pid>` that are still alive.
///
/// `run_dir_for_pid` names the run dir after the daemon's pid
/// (`src/daemon/mod.rs`), and `find_active_run_dir` reads it back exactly this
/// way. It is the AUTHORITATIVE handle, and the only thing this module ever
/// kills: a command-line scan could match a daemon belonging to another
/// fixture, and killing someone else's daemon is worse than leaking our own.
fn live_daemon_pids(home: &Path) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir(home.join("run")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<u32>().ok())
        .filter(|pid| pid_is_alive(*pid))
        .collect()
}

/// `root` plus every transitive descendant, shallowest-first, from ONE process
/// table snapshot.
///
/// The snapshot must be taken BEFORE anything is killed: the daemon spawns its
/// stub agents through `libc::setsid()`, so they are neither in its process
/// group nor reachable once it dies and they reparent to init.
fn descendants_from_snapshot(root: u32, snapshot: &[(u32, u32)]) -> Vec<u32> {
    let mut order = vec![root];
    let mut idx = 0;
    while idx < order.len() {
        let parent = order[idx];
        idx += 1;
        for (pid, ppid) in snapshot {
            if *ppid == parent && *pid != parent && !order.contains(pid) {
                order.push(*pid);
            }
        }
    }
    order
}

fn process_snapshot() -> Vec<(u32, u32)> {
    let Ok(out) = Command::new("ps").args(["-eo", "pid=,ppid="]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect()
}

/// Poll `kill(pid, 0)` until every pid is gone or `budget` elapses.
/// Returns the pids still alive.
fn wait_for_death(pids: &[u32], budget: std::time::Duration) -> Vec<u32> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let alive: Vec<u32> = pids.iter().copied().filter(|p| pid_is_alive(*p)).collect();
        if alive.is_empty() || std::time::Instant::now() >= deadline {
            return alive;
        }
        // Poll interval, not sleep-as-synchronisation: death is observed the
        // moment it happens; the interval only bounds the spin.
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn signal(pid: u32, sig: libc::c_int) {
    if pid != 0 {
        // SAFETY: worst case the pid is gone and this is a no-op ESRCH.
        unsafe {
            libc::kill(pid as i32, sig);
        }
    }
}

/// Reap every daemon this fixture home published, and its stub agents.
///
/// Deepest-first so a child is never orphaned by its parent's death before we
/// have signalled it.
fn reap_daemons_under(home: &Path) {
    let daemons = live_daemon_pids(home);
    if daemons.is_empty() {
        return;
    }
    let snapshot = process_snapshot();
    let mut targets: Vec<u32> = Vec::new();
    for daemon in daemons {
        for pid in descendants_from_snapshot(daemon, &snapshot) {
            if !targets.contains(&pid) {
                targets.push(pid);
            }
        }
    }
    // Shallowest-first was built above; signal in reverse = deepest-first.
    targets.reverse();

    for pid in &targets {
        signal(*pid, libc::SIGTERM);
    }
    let stubborn = wait_for_death(&targets, std::time::Duration::from_secs(3));
    if stubborn.is_empty() {
        return;
    }
    for pid in &stubborn {
        signal(*pid, libc::SIGKILL);
    }
    wait_for_death(&stubborn, std::time::Duration::from_secs(3));
}

/// Owns a fixture `AGEND_HOME` and guarantees nothing it booted outlives it.
///
/// On `Drop`, in this order (the order is load-bearing):
///   1. read the live daemon pids out of `<home>/run/`;
///   2. take ONE process-table snapshot and expand each daemon to its
///      transitive descendants;
///   3. SIGTERM deepest-first, poll, escalate to SIGKILL;
///   4. only THEN remove the directory — the run dir lives INSIDE the home, so
///      removing it first would destroy the only authoritative handle.
pub struct FixtureHome {
    path: std::path::PathBuf,
    context: String,
}

impl FixtureHome {
    /// Create `std::env::temp_dir()/<name>`. `name` is the directory basename
    /// the test already uses, kept verbatim so fixture paths do not change.
    pub fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(name);
        std::fs::create_dir_all(&path).expect("mkdir fixture home");
        Self {
            path,
            context: name.to_string(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for FixtureHome {
    fn drop(&mut self) {
        reap_daemons_under(&self.path);
        // The commit-1 RED assertion, now owned by the guard: it can only be
        // meaningful AFTER the reap and BEFORE the directory disappears.
        // Skipped while unwinding — a panic here would abort the process and
        // hide the test's real failure.
        if !std::thread::panicking() {
            assert_no_process_references_home(&self.path, &self.context);
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
