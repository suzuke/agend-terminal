//! #927 PR-B: zombie-daemon cleanup.
//!
//! Operator pain (empirical 2026-05-18): 3 daemon processes 16-20 days
//! old, holding `~/.agend/run/<pid>/{api.port,*.port}` files + ~200MB
//! RSS each, SIGTERM ignored, required SIGKILL. This module provides:
//!
//! - [`cleanup_zombie_daemon`] — core kill primitive (SIGTERM → grace →
//!   SIGKILL on Unix; TerminateProcess single-stage on Windows).
//! - [`find_zombie_candidates`] — scan `<home>/run/<pid>/` for entries
//!   older than the operator-supplied age threshold.
//! - [`log_zombie_state`] — pre-kill instrumentation (best-effort `ps`
//!   capture for the operator audit trail).
//!
//! CLI: `agend-terminal admin cleanup-zombies [--age <DURATION>] [--yes]`.
//!
//! Cross-platform note: Unix uses the two-stage SIGTERM→SIGKILL with a
//! 5s grace (justification: daemon's own `SHUTDOWN_GRACE = 2s` for
//! agent subprocess teardown + 3s safety buffer for cleanup hooks
//! and worker-thread flush). Windows has no SIGTERM equivalent;
//! `TerminateProcess` is single-stage (forced kill). The asymmetry is
//! documented in the CLI help so operators on each platform know what
//! to expect.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Outcome of a single cleanup attempt against a daemon PID.
///
/// Platform-conditional variants carry `#[allow(dead_code)]` so the
/// cross-platform CI lint passes regardless of which target compiles:
/// `Graceful` + `ForceKilled` only fire on Unix (two-stage SIGTERM/
/// SIGKILL path); `WindowsTerminated` only fires on Windows
/// (`TerminateProcess` single-stage). `AlreadyExited` + `RefusedToDie`
/// can be returned from either platform path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillOutcome {
    /// PID was already gone before any kill signal was sent.
    AlreadyExited,
    /// SIGTERM caused graceful exit within the grace window. Carries the
    /// observed grace duration so operators can tune `--age` thresholds.
    #[allow(dead_code)] // Unix-only constructed; Windows path goes through WindowsTerminated
    Graceful(Duration),
    /// SIGTERM was ignored; SIGKILL was sent and the PID died within
    /// the secondary grace window.
    #[allow(dead_code)] // Unix-only constructed; Windows path goes through WindowsTerminated
    ForceKilled,
    /// Both SIGTERM and SIGKILL failed to reap the PID. Operator must
    /// investigate (kernel-stuck process, defunct, uninterruptible
    /// sleep). Non-zero CLI exit code.
    RefusedToDie,
    /// Windows: `TerminateProcess` returned success. No grace stage
    /// because there's no SIGTERM equivalent. The variant is distinct
    /// from `ForceKilled` so operator logs reflect the platform.
    #[allow(dead_code)] // Windows-only constructed; CI lints Linux/macOS test profile
    WindowsTerminated,
}

/// Describes one zombie candidate discovered by [`find_zombie_candidates`].
#[derive(Debug, Clone)]
pub struct ZombieInfo {
    pub pid: u32,
    pub run_dir: PathBuf,
    /// Age computed from `<run_dir>/.daemon` mtime relative to the
    /// caller's `now` parameter (typically `SystemTime::now()`).
    pub age: Duration,
}

/// Cross-platform check: is the process with `pid` still alive?
/// Delegates to the existing `crate::process::is_pid_alive` helper.
fn is_alive(pid: u32) -> bool {
    crate::process::is_pid_alive(pid)
}

/// Best-effort pre-kill audit log. Captures `ps eww` output for the
/// target PID so operators can see the command line + working
/// directory + env that the zombie was running with. Failure logs at
/// `warn` and continues — the cleanup itself MUST NOT block on
/// diagnostic capture.
///
/// Unix: `ps -p <pid> -o pid,ppid,etime,rss,comm,args`.
/// Windows: no equivalent without an extra dependency; logs a stub line.
pub fn log_zombie_state(pid: u32) {
    #[cfg(unix)]
    {
        let output = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "pid,ppid,etime,rss,comm,args"])
            .output();
        match output {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                tracing::info!(
                    pid,
                    ps_output = %stdout.trim(),
                    "#927 cleanup-zombies: pre-kill state captured"
                );
            }
            Ok(out) => {
                tracing::warn!(
                    pid,
                    stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                    "#927 cleanup-zombies: ps capture exited non-zero"
                );
            }
            Err(e) => {
                tracing::warn!(
                    pid,
                    error = %e,
                    "#927 cleanup-zombies: ps capture failed to spawn"
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        tracing::info!(
            pid,
            "#927 cleanup-zombies: pre-kill state capture skipped on Windows (no ps equivalent)"
        );
    }
}

/// Core kill primitive. Sends SIGTERM, polls liveness up to `term_grace`,
/// escalates to SIGKILL on timeout and polls up to `kill_grace`.
///
/// Returns [`KillOutcome::AlreadyExited`] if the PID is already gone at
/// entry. Returns [`KillOutcome::Graceful(elapsed)`] if SIGTERM landed.
/// Returns [`KillOutcome::ForceKilled`] if SIGKILL was needed and
/// succeeded. Returns [`KillOutcome::RefusedToDie`] if both stages
/// timed out.
///
/// Windows: short-circuits to [`KillOutcome::WindowsTerminated`] via
/// `TerminateProcess` (no SIGTERM equivalent). The two-stage grace is
/// Unix-only.
pub fn cleanup_zombie_daemon(pid: u32, term_grace: Duration, kill_grace: Duration) -> KillOutcome {
    if !is_alive(pid) {
        return KillOutcome::AlreadyExited;
    }

    #[cfg(unix)]
    {
        let term_start = SystemTime::now();
        // SAFETY: libc::kill with signal 15 (SIGTERM) is a safe POSIX
        // syscall against any process the caller has permission to
        // signal. Failure (EPERM/ESRCH) flows through to the liveness
        // poll which will detect the no-op.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }
        if !poll_until_dead(pid, term_grace) {
            // Stage 2: SIGKILL.
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
            if poll_until_dead(pid, kill_grace) {
                return KillOutcome::ForceKilled;
            }
            return KillOutcome::RefusedToDie;
        }
        let elapsed = term_start
            .elapsed()
            .unwrap_or_else(|_| Duration::from_secs(0));
        KillOutcome::Graceful(elapsed)
    }
    #[cfg(windows)]
    {
        let _ = term_grace; // unused on Windows — TerminateProcess is single-stage
        let _ = kill_grace;
        crate::process::terminate(pid);
        // Best-effort wait for the process to actually exit.
        if poll_until_dead(pid, Duration::from_secs(2)) {
            KillOutcome::WindowsTerminated
        } else {
            KillOutcome::RefusedToDie
        }
    }
}

/// Poll `is_alive(pid)` every 100ms until it returns false or `timeout`
/// elapses. Returns true if the PID died within the window. Used as the
/// grace-loop primitive for both SIGTERM and SIGKILL stages.
///
/// #934: promoted from `fn` to `pub(crate) fn` so consumers OUTSIDE
/// `admin::cleanup_zombies` can reuse the deadline-poll idiom. Current
/// in-crate consumers (added in the same PR):
/// - `src/agent.rs::sweep_child_tree_body` test — replaces bare
///   `assert!(!is_pid_alive(_pid))` post-kill assertions
/// - `src/process.rs::test_kill_process_tree_kills_child_subprocess` —
///   sibling test with identical race shape
///
/// ### Deadline guidance (OS-conditional)
///
/// Callers killing a process whose PID they CAN `waitpid` on (it's their
/// own child) → typically <1s deadline; `wait()` reaps synchronously
/// and `kill(pid, 0)` returns ESRCH immediately after.
///
/// Callers killing a process whose PID is re-parented to init / launchd
/// upon parent death (orphaned grandchild scenario) → MUST use longer
/// deadline because reap is asynchronous in the new parent:
/// - **Linux init / systemd**: reaper runs on scheduler tick, typically
///   reaps within <1s
/// - **macOS launchd**: longer cycle observed; ~3s under nominal load,
///   ~10s under heavily contended CI runners
/// - **Heavily-loaded CI** (e.g. ubuntu-latest with parallel tests on
///   2 vCPUs): 5-10s worst case for either platform
///
/// Recommend 5s for self-reaped children, 10s for orphaned grandchildren.
/// The 100ms poll cadence balances responsiveness vs CPU waste.
pub(crate) fn poll_until_dead(pid: u32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !is_alive(pid) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Scan `<home>/run/<pid>/` entries and surface daemons whose `.daemon`
/// file mtime is older than `min_age` AND whose PID is still alive
/// (i.e., genuine zombies — `sweep_stale_run_dirs` already cleans dead
/// PIDs at boot).
///
/// `now` is supplied by the caller so tests can pin a deterministic
/// clock; production passes `SystemTime::now()`.
pub fn find_zombie_candidates(home: &Path, min_age: Duration, now: SystemTime) -> Vec<ZombieInfo> {
    let run = home.join("run");
    let Ok(entries) = std::fs::read_dir(&run) else {
        return Vec::new();
    };
    let mut zombies = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid_str = name.to_string_lossy();
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if !is_alive(pid) {
            // Dead PID → `sweep_stale_run_dirs` will handle it on next boot.
            // Not our concern.
            continue;
        }
        let daemon_file = entry.path().join(".daemon");
        let Ok(meta) = std::fs::metadata(&daemon_file) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let age = now.duration_since(mtime).unwrap_or(Duration::from_secs(0));
        if age >= min_age {
            zombies.push(ZombieInfo {
                pid,
                run_dir: entry.path(),
                age,
            });
        }
    }
    zombies
}

/// Parse human-friendly duration strings like `"14d"`, `"3h"`, `"30m"`,
/// `"60s"` into [`Duration`]. Returns `None` for unrecognized input so
/// callers can fall back to a default.
///
/// Accepted suffixes: `s` (seconds), `m` (minutes), `h` (hours),
/// `d` (days). Bare integers default to seconds. Case-insensitive.
pub fn parse_age(s: &str) -> Option<Duration> {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    let (num_part, mult): (&str, u64) = if let Some(stripped) = s.strip_suffix('d') {
        (stripped, 24 * 60 * 60)
    } else if let Some(stripped) = s.strip_suffix('h') {
        (stripped, 60 * 60)
    } else if let Some(stripped) = s.strip_suffix('m') {
        (stripped, 60)
    } else if let Some(stripped) = s.strip_suffix('s') {
        (stripped, 1)
    } else {
        (s.as_str(), 1)
    };
    let n: u64 = num_part.trim().parse().ok()?;
    Some(Duration::from_secs(n.checked_mul(mult)?))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-927prb-{}-{}-{}",
            tag,
            std::process::id(),
            id
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parse_age_accepts_common_suffixes() {
        assert_eq!(parse_age("14d"), Some(Duration::from_secs(14 * 86400)));
        assert_eq!(parse_age("3h"), Some(Duration::from_secs(10800)));
        assert_eq!(parse_age("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_age("60s"), Some(Duration::from_secs(60)));
        assert_eq!(parse_age("60"), Some(Duration::from_secs(60)));
        // Case-insensitive.
        assert_eq!(parse_age("14D"), Some(Duration::from_secs(14 * 86400)));
    }

    #[test]
    fn parse_age_rejects_garbage() {
        assert_eq!(parse_age(""), None);
        assert_eq!(parse_age("abc"), None);
        assert_eq!(parse_age("14days"), None); // strict suffix
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_zombie_daemon_already_exited_when_pid_dead() {
        // Spawn a short-lived process and wait for it to die. Then run
        // cleanup against its PID — must return AlreadyExited.
        let mut child = std::process::Command::new("sh")
            .args(["-c", "true"])
            .spawn()
            .expect("spawn sh");
        let pid = child.id();
        // Reap so kill(0) returns ESRCH.
        let _ = child.wait();
        // Tiny sleep to ensure the kernel has cleared the entry.
        std::thread::sleep(Duration::from_millis(50));

        let outcome = cleanup_zombie_daemon(pid, Duration::from_secs(1), Duration::from_secs(1));
        assert!(
            matches!(outcome, KillOutcome::AlreadyExited),
            "dead PID must return AlreadyExited, got {outcome:?}"
        );
    }

    /// Concurrent reap loop: after we fork+exec a child in tests, even
    /// after it dies it stays as a zombie in our process's child table
    /// until we `wait()` for it. `is_pid_alive` (= `kill(pid, 0)`) sees
    /// a zombie as alive, which would make `cleanup_zombie_daemon`'s
    /// poll loop return `RefusedToDie` even when the child actually
    /// terminated. The bg reaper polls `try_wait` and clears the
    /// zombie entry shortly after death, so the cleanup poll's next
    /// iteration observes the PID as dead.
    ///
    /// In production this is a non-issue: zombie daemons are NOT
    /// children of the cleanup-zombies CLI process (they're orphaned
    /// to PID 1 / launchd / init), so the kernel reaper handles them.
    /// Only test fixtures need this dance.
    #[cfg(unix)]
    fn spawn_with_reaper(program: &str, args: &[&str]) -> (u32, std::thread::JoinHandle<()>) {
        let mut child = std::process::Command::new(program)
            .args(args)
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {program} failed: {e}"));
        let pid = child.id();
        let handle = std::thread::spawn(move || {
            for _ in 0..100 {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            // Last-ditch reap on test teardown.
            let _ = child.kill();
            let _ = child.wait();
        });
        (pid, handle)
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_zombie_daemon_sigterm_respected_returns_graceful() {
        // Default `sh -c sleep 60` exits on SIGTERM (no trap).
        let (pid, reaper) = spawn_with_reaper("sh", &["-c", "sleep 60"]);

        let outcome = cleanup_zombie_daemon(pid, Duration::from_secs(3), Duration::from_secs(2));
        let _ = reaper.join();

        assert!(
            matches!(outcome, KillOutcome::Graceful(_)),
            "SIGTERM-respecting process must return Graceful, got {outcome:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_zombie_daemon_sigterm_ignored_returns_force_killed() {
        // Use python3 for clean SIGTERM-ignore semantics:
        // `sh -c "trap '' TERM; ..."` is fragile on macOS where bash-
        // as-sh and process-group SIGTERM propagation can short-
        // circuit the trap. Python's `signal.SIG_IGN` installs a true
        // SIG_IGN disposition that survives all signal delivery paths.
        //
        // python3 is universally available on macOS + Linux CI runners
        // (the test is `#[cfg(unix)]`-gated; Windows-side coverage is
        // separate). If a future CI image drops python3, fall back to
        // a compiled C helper or feature-gate this test.
        let (pid, reaper) = spawn_with_reaper(
            "python3",
            &[
                "-c",
                "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)",
            ],
        );

        // Give python3 ~300ms to actually run `signal.signal(SIGTERM,
        // SIG_IGN)` — without this, the SIGTERM lands during interpreter
        // startup before the handler is installed and python's default
        // SIGTERM disposition (terminate) kills it gracefully, which is
        // the OPPOSITE of what this test pins.
        std::thread::sleep(Duration::from_millis(300));

        // 500ms SIGTERM grace (short — SIG_IGN won't release), 3s SIGKILL grace.
        let outcome =
            cleanup_zombie_daemon(pid, Duration::from_millis(500), Duration::from_secs(3));
        let _ = reaper.join();

        assert!(
            matches!(outcome, KillOutcome::ForceKilled),
            "SIGTERM-ignored process must escalate to SIGKILL, got {outcome:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn find_zombie_candidates_filters_by_age_threshold() {
        let home = tmp_home("find-zombies-age");

        // Synthetic run/<pid>/.daemon entries with mtime injection via filetime
        // is fragile; instead, plant a synthetic .daemon and rely on real
        // mtime + a 0-duration min_age that picks up any live PID.
        // Use our OWN PID as the "alive zombie" — guaranteed alive during
        // the test.
        let our_pid = std::process::id();
        let run = home.join("run").join(our_pid.to_string());
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join(".daemon"), format!("{our_pid}:0")).unwrap();

        // Threshold 0 → our PID surfaces.
        let now = SystemTime::now();
        let zombies = find_zombie_candidates(&home, Duration::from_secs(0), now);
        assert!(
            zombies.iter().any(|z| z.pid == our_pid),
            "min_age=0 must surface live PID; got {zombies:?}"
        );

        // Threshold huge (year) → our PID does NOT surface (file just created).
        let zombies = find_zombie_candidates(&home, Duration::from_secs(365 * 86400), now);
        assert!(
            !zombies.iter().any(|z| z.pid == our_pid),
            "min_age=1year must NOT surface fresh .daemon entry; got {zombies:?}"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    // ── #934 direct poll_until_dead tests ─────────────────────────────────
    //
    // These exercise the primitive directly so a future regression that
    // (e.g.) inverts the deadline check, drops the early-return on dead
    // PID, or changes the poll cadence will fail visibly. The primitive
    // is consumed by both cleanup_zombies (its original site) and #934's
    // sweep_child_tree post-kill assertions; direct tests pin the
    // contract once instead of relying on consumer-side coverage.
    //
    // §3.20 SOP 1 deterministic: each test uses a deadline + observable
    // state. No sleep-based assertions. The "timeout on undying zombie"
    // test uses python3 SIG_IGN (same pattern as
    // `cleanup_zombie_daemon_sigterm_ignored_returns_force_killed`) so
    // the unkillable behavior is forced, not racy.

    #[cfg(unix)]
    #[test]
    fn poll_until_dead_returns_immediately_when_already_dead() {
        // Spawn `true` (exits instantly), `.wait()` to fully reap, then
        // use that PID. Post-wait the kernel has cleared the entry so
        // `kill(pid, 0)` returns ESRCH ("no such process") and
        // `is_alive` returns false.
        //
        // (Naïve `u32::MAX` doesn't work: cast to i32 = -1, and
        // `kill(-1, 0)` is the POSIX "send to every process you can
        // signal" semantic — always succeeds, so `is_alive(u32::MAX)`
        // returns true on Unix.)
        //
        // PID-recycling caveat: on busy systems the kernel can reassign
        // the PID to a new process within microseconds. The test takes
        // ~ms total wall-clock so recycling is statistically unlikely
        // but not impossible. If observed flaky on real CI, gate via
        // the same skip-on-recycle pattern below.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn `true`");
        let dead_pid = child.id();
        let _ = child.wait();

        // If the kernel recycled the PID between wait() and is_alive()
        // (microsecond race on a busy host), skip rather than emit a
        // misleading red. Production code never sees this race because
        // `cleanup_zombie_daemon` calls `poll_until_dead` synchronously
        // after `libc::kill(_, SIGKILL)` — the target PID is reaped by
        // its real parent, not the cleanup process.
        if is_alive(dead_pid) {
            eprintln!("test fixture: PID {dead_pid} recycled in wait()→is_alive() gap — skipping");
            return;
        }

        let start = std::time::Instant::now();
        let result = poll_until_dead(dead_pid, Duration::from_secs(10));
        let elapsed = start.elapsed();

        assert!(
            result,
            "poll_until_dead must return true for already-dead PID"
        );
        // Returns BEFORE the first 100ms sleep tick (early-return path).
        assert!(
            elapsed < Duration::from_millis(50),
            "early-return path must not sleep; got {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn poll_until_dead_returns_after_kill_completes() {
        // Spawn cooperative child (no SIG_IGN). SIGKILL it. Poll should
        // observe death within the window.
        let (pid, reaper) = spawn_with_reaper("sh", &["-c", "sleep 30"]);
        assert!(is_alive(pid), "child must be alive pre-kill");

        // SIGKILL — immediate kernel-side process exit.
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }

        let result = poll_until_dead(pid, Duration::from_secs(5));
        let _ = reaper.join();

        assert!(
            result,
            "poll_until_dead must observe child death within 5s after SIGKILL"
        );
    }

    #[cfg(unix)]
    #[test]
    fn poll_until_dead_returns_timeout_on_undying_zombie() {
        // python3 SIG_IGN — process survives SIGTERM. We send SIGTERM
        // (which is ignored), then poll with a short deadline. Since
        // we DON'T escalate to SIGKILL here, the process stays alive
        // and poll_until_dead must return false on timeout.
        //
        // Pattern matches `cleanup_zombie_daemon_sigterm_ignored_returns_force_killed`
        // (line ~374) for SIG_IGN-disposition reliability across macOS +
        // Linux. python3 is universally available on `#[cfg(unix)]`-gated
        // CI runners.
        let (pid, reaper) = spawn_with_reaper(
            "python3",
            &[
                "-c",
                "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(30)",
            ],
        );

        // Give python3 ~300ms to install the SIG_IGN handler (otherwise
        // SIGTERM lands during interpreter startup before the handler is
        // in place; python's default SIGTERM disposition terminates,
        // which would make poll_until_dead succeed prematurely).
        std::thread::sleep(Duration::from_millis(300));

        // Send SIGTERM — ignored by the child.
        unsafe {
            libc::kill(pid as i32, libc::SIGTERM);
        }

        // Poll for ONLY 500ms. SIG_IGN child stays alive → return false.
        let start = std::time::Instant::now();
        let result = poll_until_dead(pid, Duration::from_millis(500));
        let elapsed = start.elapsed();

        // Cleanup: SIGKILL the surviving child + reap.
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        let _ = reaper.join();

        assert!(
            !result,
            "poll_until_dead must return false (timeout) for SIG_IGN-armed child"
        );
        // Timing: must have polled for AT LEAST the timeout window.
        // We give 50ms tolerance for scheduler jitter.
        assert!(
            elapsed >= Duration::from_millis(450),
            "must wait the full timeout; got {elapsed:?}"
        );
    }
}
