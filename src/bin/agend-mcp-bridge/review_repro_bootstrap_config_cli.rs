//! Repro for: "Bridge picks the first run dir with api.port without any
//! liveness/identity check" (src/bin/agend-mcp-bridge.rs `find_run_dir`).
//!
//! `find_run_dir` returns the first `run/<pid>/` subdir that merely contains an
//! `api.port` file, with NO check that the named PID is alive or that the port
//! has a listener. If a stale run dir (dead daemon, not yet swept) sorts before
//! the live one in `read_dir` order, the bridge reads its stale port + cookie,
//! attempts to connect, and only recovers via the connect/retry error path —
//! burning the retry budget and, if the stale port is reused by an unrelated
//! local listener, attempting a cookie handshake against the wrong process. The
//! daemon-side `find_active_run_dir` deliberately validates PID liveness +
//! identity; the bridge's copy does not.
//!
//! METHOD: behavioral_unit. `find_run_dir` is a private sibling reachable from
//! this in-module submodule via `super::find_run_dir`. We build a home whose
//! ONLY run dir is named with a DEAD PID and contains `api.port`. The fixed code
//! must skip run dirs whose PID is not alive; the current code returns it.
//!
//! RED now: `find_run_dir` returns `Ok(dead_pid_dir)` for a dead-PID run dir.
//! GREEN after fix: dead-PID run dirs are skipped, so with no live candidate
//! present `find_run_dir` returns `Err`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Spawn `true`, reap it, and return its now-dead PID — unless the kernel
/// recycled it in the wait()→check gap (then `None`, and the caller skips).
#[cfg(unix)]
fn dead_pid() -> Option<u32> {
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn `true`");
    let pid = child.id();
    let _ = child.wait();
    std::thread::sleep(std::time::Duration::from_millis(50));
    if pid_alive(pid) {
        // Recycled in the gap — can't use it as a guaranteed-dead PID.
        return None;
    }
    Some(pid)
}

#[cfg(unix)]
#[test]
#[ignore = "bridge-find-run-dir-no-liveness: red until fix; remove #[ignore] after fix to confirm"]
fn find_run_dir_skips_dead_pid_run_dir_bootstrap_config_cli() {
    let Some(dead) = dead_pid() else {
        eprintln!("test fixture: PID recycled in wait()->check gap — skipping");
        return;
    };

    let home = std::env::temp_dir().join(format!(
        "agend-bridge-findrun-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let stale = home.join("run").join(dead.to_string());
    std::fs::create_dir_all(&stale).expect("mkdir stale run dir");
    // A complete-looking but STALE run dir: api.port present, dead PID name.
    std::fs::write(stale.join("api.port"), "54321\n").expect("write api.port");
    // A realistic stale dir also carries a `.daemon` identity recording the
    // (now dead) pid — present so a fix that validates identity still sees it.
    std::fs::write(stale.join(".daemon"), format!("{dead}:0")).expect("write .daemon");

    let result = super::find_run_dir(&home);

    // The fix must NOT hand back a dead-PID run dir. With no live candidate, the
    // only correct outcome is Err ("no active daemon run dir").
    let returned_stale = matches!(&result, Ok(p) if *p == stale);
    assert!(
        !returned_stale,
        "find_run_dir returned the STALE dead-PID run dir {:?} — it performs no \
         liveness check, so the bridge will read a dead daemon's port + cookie and \
         burn its connect-retry budget (or handshake against an unrelated process if \
         the port was reused). Skip run dirs whose PID is not alive, matching the \
         daemon-side find_active_run_dir contract. Got: {:?}",
        stale,
        result.as_ref().map(|p| p.display().to_string())
    );

    std::fs::remove_dir_all(&home).ok();
}

#[cfg(not(unix))]
#[test]
#[ignore = "bridge-find-run-dir-no-liveness: red until fix; remove #[ignore] after fix to confirm"]
fn find_run_dir_skips_dead_pid_run_dir_bootstrap_config_cli() {
    // The dead-PID fixture relies on POSIX spawn/reap + kill(0) liveness; the
    // bug and its fix are platform-agnostic but this repro is unix-only.
    eprintln!("find_run_dir liveness repro is unix-only; skipping on this platform");
}
