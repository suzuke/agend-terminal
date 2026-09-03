//! Mutation-kill regression: proves `reap_daemons_under`'s descendant walk
//! (`descendants_from_snapshot`) is load-bearing, by exercising the reaper
//! DIRECTLY against a synthetic process tree instead of a real daemon.
//!
//! ## Why a real daemon can't prove this
//! A `SIGTERM`'d real daemon tears its own agents down as part of normal
//! shutdown, so a test built on a real daemon proves nothing about the
//! descendant walk: root-only signalling and full-tree signalling look
//! identical from outside. The walk only matters on the escalation path — a
//! descendant that does NOT die from `SIGTERM` and therefore needs `SIGKILL`
//! delivered to IT SPECIFICALLY, because nothing else will ever ask the OS
//! to kill it.
//!
//! ## The synthetic tree
//! `root` (published as the fixture's "daemon" pid) backgrounds `descendant`
//! (a real child process, real ppid) and blocks in `wait`. `root` has no
//! trap, so a bare `SIGTERM` kills it immediately — same as the real world,
//! where the top-level daemon usually does die from `SIGTERM`. `descendant`
//! traps `SIGTERM` (`trap '' TERM`) and therefore ONLY dies if the reaper's
//! escalation-to-`SIGKILL` step reaches it by its OWN pid — which requires
//! the snapshot walk to have found it under `root` in the first place.
//!
//! `descendants_from_snapshot(daemon, &snapshot) -> vec![daemon]` (the
//! mutation this test kills) makes `descendant` invisible to the reaper: only
//! `root` is ever signalled, `descendant` is never touched, and the
//! `FixtureHome::Drop` assertion (or this test's own polling) sees it still
//! alive.

#![cfg(unix)]

mod common;

use common::daemon_reaper::{pid_is_alive, FixtureHome};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

/// Direct children of `ppid`, from a fresh `ps` snapshot. Used only to widen
/// the assertion to the descendant's OWN child (a third generation) when one
/// happens to be running yet; not required for the mutation-kill proof
/// itself, which only needs `root` -> `descendant`.
fn children_of(ppid: u32) -> Vec<u32> {
    let Ok(out) = Command::new("ps").args(["-eo", "pid=,ppid="]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid: u32 = it.next()?.parse().ok()?;
            let parent: u32 = it.next()?.parse().ok()?;
            (parent == ppid).then_some(pid)
        })
        .collect()
}

/// Poll `cond` until true or `budget` elapses; returns `cond()`'s final value.
/// Poll interval, not sleep-as-synchronisation, per CONTRIBUTING.md. Bounded
/// unconditionally: on timeout this returns `false` rather than looping
/// forever, so a broken escalation path is a fast red, never a CI hang.
fn wait_until(budget: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return cond();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `pid: command line` for each of `pids`, from one `ps` snapshot — same
/// liveness primitive (`kill(pid, 0)` via `is_alive`) drives every wait in
/// this test; `ps` here is ONLY for the failure message, mirroring how
/// `daemon_reaper::processes_referencing_home` describes what it found,
/// never a second liveness check.
fn describe(pids: &[u32]) -> Vec<String> {
    let Ok(out) = Command::new("ps").args(["-eo", "pid=,command="]).output() else {
        return pids
            .iter()
            .map(|p| format!("pid {p} (ps unavailable)"))
            .collect();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    pids.iter()
        .map(|&pid| {
            text.lines()
                .find_map(|line| {
                    let line = line.trim_start();
                    let (p, cmd) = line.split_once(char::is_whitespace)?;
                    (p.parse::<u32>().ok()? == pid).then(|| format!("pid {pid}: {}", cmd.trim()))
                })
                .unwrap_or_else(|| format!("pid {pid} (gone between the check and this describe)"))
        })
        .collect()
}

/// Backstop cleanup INDEPENDENT of the reaper under test: SIGKILLs the whole
/// synthetic process group. `root` is spawned with `process_group(0)`, so
/// every process it (or its descendants) fork without calling `setsid`/
/// `setpgid` themselves shares that one pgid — one signal reaches the whole
/// tree regardless of whether the code under test worked.
///
/// This exists so the test cleans up its own spawned processes even when it
/// fails — including failing exactly because the reaper leaked one of them.
struct HardKillGroup(i32);
impl Drop for HardKillGroup {
    fn drop(&mut self) {
        // SAFETY: worst case the group is already empty (ESRCH, ignored).
        unsafe {
            libc::kill(-self.0, libc::SIGKILL);
        }
    }
}

#[test]
fn reaper_kills_descendants_a_root_only_signal_would_orphan() {
    let home_guard = FixtureHome::new(&format!(
        "agend-reaper-descendant-kill-{}",
        std::process::id()
    ));
    let home = home_guard.path().to_path_buf();

    // A real separate `sh` process (not a backgrounded shell function —
    // that would make `$$` ambiguous under bash's subshell semantics), so
    // its own `$$` unambiguously names itself, and its parentage under
    // `root` is real OS parentage the process-table snapshot will show.
    let descendant_script = home.join("descendant.sh");
    std::fs::write(
        &descendant_script,
        "#!/bin/sh\ntrap '' TERM\necho $$ > \"$1\"\nsleep 100000\n",
    )
    .expect("write descendant script");
    let marker = home.join("descendant.pid");

    let mut root = Command::new("sh")
        .process_group(0)
        .args([
            "-c",
            "sh \"$1\" \"$2\" &\nwait",
            "root",
            descendant_script.to_str().expect("utf8 tmp path"),
            marker.to_str().expect("utf8 tmp path"),
        ])
        .spawn()
        .expect("spawn synthetic root");
    let root_pid = root.id();
    // Constructed BEFORE anything below can panic, so unwinding always runs it.
    let _hard_kill = HardKillGroup(root_pid as i32);

    assert!(
        wait_until(Duration::from_secs(5), || marker.exists()),
        "synthetic descendant never started (its pid marker never appeared)"
    );
    let descendant_pid: u32 = std::fs::read_to_string(&marker)
        .expect("read descendant pid marker")
        .trim()
        .parse()
        .expect("marker holds a pid");
    assert_ne!(
        descendant_pid, root_pid,
        "descendant must be a distinct process from root — real parentage, not a self-match"
    );
    assert!(
        pid_is_alive(descendant_pid),
        "synthetic descendant must be alive before the reap runs"
    );
    // A third generation, if it has started by now — best-effort widening,
    // not required for the root-vs-descendant proof above.
    let grandchildren = children_of(descendant_pid);

    // The authoritative handle `live_daemon_pids` reads. The reaper does not
    // care what the process actually is — only that this directory names it.
    std::fs::create_dir_all(home.join("run").join(root_pid.to_string())).expect("publish run dir");

    // Trigger the reap: `FixtureHome::Drop` runs `reap_daemons_under` (SIGTERM
    // deepest-first, poll, escalate to SIGKILL) before removing the directory.
    // That call already blocks for up to its own 3s SIGTERM + 3s SIGKILL
    // budget internally (and, if a leak is left, panics itself out of its
    // post-reap `assert_no_process_references_home` scan — a separate,
    // already-bounded check). What follows is a SECOND, independent
    // confirmation: not a hang risk, since every wait below carries its own
    // bound.
    drop(home_guard);
    let _ = root.try_wait(); // reap the `Child`'s own zombie slot.

    let mut must_be_dead = vec![root_pid, descendant_pid];
    must_be_dead.extend(grandchildren.iter().copied());

    // Generous relative to the reap that already ran synchronously above
    // (3s SIGTERM + 3s SIGKILL): this only needs to cover OS bookkeeping
    // after a signal that already landed, not a second escalation cycle.
    // Bounded — never polls past this — so a broken escalation path is a
    // fast, loud red here, never a CI hang (the 20x flake-gate job would
    // otherwise multiply the cost of a silent hang by 20).
    let confirm_budget = Duration::from_secs(10);
    let all_dead = wait_until(confirm_budget, || {
        must_be_dead.iter().all(|&pid| !pid_is_alive(pid))
    });

    if !all_dead {
        let survivors: Vec<u32> = must_be_dead
            .iter()
            .copied()
            .filter(|&pid| pid_is_alive(pid))
            .collect();
        // `_hard_kill` (still in scope) SIGKILLs the whole synthetic process
        // group when this panic unwinds past it below — nothing is left
        // running just because this assertion failed.
        panic!(
            "reaper left {} synthetic process(es) alive {confirm_budget:?} after the reap ran \
             — the descendant walk did not find them (exactly what \
             `descendants_from_snapshot(daemon, &snapshot) -> vec![daemon]` produces), or \
             SIGKILL escalation did not run:\n  {}",
            survivors.len(),
            describe(&survivors).join("\n  ")
        );
    }
}
