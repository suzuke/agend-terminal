//! Repro (daemon-retention batch): the boot_sweep PID-reuse identity guard is
//! BYPASSED when the `.daemon` PID is unreadable. The guard only runs inside
//! `if let Some(recorded) = read_daemon_pid(&z.run_dir)`. When read_daemon_pid
//! returns None (the `.daemon` file is present-but-truncated/unparseable — so a
//! candidate still surfaces via its mtime), the guard is SKIPPED and the
//! candidate proceeds to log_zombie_state + (destructive mode)
//! cleanup_zombie_daemon against the directory-name PID. A recycled PID whose
//! run dir lost/corrupted its `.daemon` body is then killed on the basis of an
//! unverified directory name — exactly the PID-reuse mis-kill the guard exists
//! to prevent.
//!
//! Behavioral fs/process test on `super::boot_sweep_impl`. Plants
//! `<home>/run/<live_pid>/.daemon` with an EMPTY (unparseable) body so
//! read_daemon_pid returns None, spawns a real SIGTERM-respecting child under
//! that PID, runs destructive boot_sweep_impl, and asserts the live child
//! SURVIVES. RED now (the guard is skipped → child killed); GREEN once an
//! unreadable recorded PID is treated as a guard FAILURE (skip, don't kill).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(unix)]
use super::boot_sweep_impl;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::time::{Duration, SystemTime};

#[cfg(unix)]
fn tmp_home(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-daemon-retention-bootsweep-{}-{}-{}",
        tag,
        std::process::id(),
        id
    ));
    std::fs::create_dir_all(&dir).expect("mkdir tmp_home");
    dir
}

/// Plant `<home>/run/<pid>/.daemon` with an EMPTY body — the file EXISTS (so a
/// zombie candidate surfaces via its mtime) but `read_daemon_pid` returns None
/// (no `pid:boot` content to parse). Returns the run dir path.
#[cfg(unix)]
fn plant_run_dir_unreadable_pid(home: &Path, pid: u32) -> PathBuf {
    let run = home.join("run").join(pid.to_string());
    std::fs::create_dir_all(&run).expect("mkdir run dir");
    // Empty body → read_daemon_pid: split_once(':') is None → None.
    std::fs::write(run.join(".daemon"), "").expect("write empty .daemon");
    run
}

/// Spawn a child that respects SIGTERM. Returns (pid, reaper-join-handle).
#[cfg(unix)]
fn spawn_sigterm_respecter() -> (u32, std::thread::JoinHandle<()>) {
    let mut child = Command::new("sh")
        .args(["-c", "sleep 60"])
        .spawn()
        .expect("spawn sh");
    let pid = child.id();
    // fire-and-forget: test-local reaper thread; its JoinHandle is returned and
    // joined by the caller's bounded try_wait loop — never escapes the test.
    let handle = std::thread::spawn(move || {
        for _ in 0..200 {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    });
    (pid, handle)
}

#[cfg(unix)]
#[test]
#[ignore = "daemon-retention boot_sweep-unreadable-pid-guard: harm already closed by #2170's cleanup start-token guard (this repro is GREEN on current main — the empty .daemon → start_token None → IdentityMismatch → not killed); boot-sweep guard-level hardening is an optional follow-up, re-confirm before un-ignoring"]
fn unreadable_daemon_pid_must_not_be_killed_in_destructive_sweep_daemon_retention() {
    let home = tmp_home("unreadable-pid");
    let (live_pid, reaper) = spawn_sigterm_respecter();
    // Run dir named after the LIVE pid, but `.daemon` body is empty/unreadable
    // → read_daemon_pid returns None → identity guard currently skipped.
    let run_dir = plant_run_dir_unreadable_pid(&home, live_pid);
    // mtime ~ now; synth now +10s, threshold 1s → candidate surfaces.
    let now = SystemTime::now() + Duration::from_secs(10);

    let killed = boot_sweep_impl(
        &home,
        Duration::from_secs(1),
        /* destructive */ true,
        /* dry_run */ false,
        Duration::from_secs(3),
        Duration::from_secs(2),
        now,
    );

    let still_alive = crate::process::is_pid_alive(live_pid);

    // Clean up the child regardless of outcome before asserting.
    let _ = reaper.join();
    std::fs::remove_dir_all(&home).ok();

    assert_eq!(
        killed, 0,
        "daemon-retention: a candidate whose `.daemon` PID is UNREADABLE must NOT be \
         killed in destructive mode — an unreadable recorded PID is a guard FAILURE, \
         not an implicit pass. The dir name alone is not a verified identity."
    );
    assert!(
        still_alive,
        "daemon-retention: the live process occupying the recycled PID was killed on \
         the basis of an unverified directory name (the identity guard was bypassed \
         because read_daemon_pid returned None) — the exact PID-reuse mis-kill the \
         guard exists to prevent. {} expected to survive.",
        live_pid
    );
    let _ = run_dir; // run_dir presence is incidental; the kill is the load-bearing fact.
}
