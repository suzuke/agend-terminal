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
    /// CR-2026-06-14: the live PID's OS start-time token did not match the
    /// token recorded in `.daemon` (or no token was recorded / it couldn't be
    /// read). The PID was recycled onto a different process — signalling it
    /// would TOCTOU-kill an innocent. NO signal was sent; the stale run dir is
    /// left for the next-boot sweep (fail-closed-skip, DP3).
    IdentityMismatch,
}

/// Describes one zombie candidate discovered by [`find_zombie_candidates`].
#[derive(Debug, Clone)]
pub struct ZombieInfo {
    pub pid: u32,
    pub run_dir: PathBuf,
    /// Age computed from `<run_dir>/.daemon` mtime relative to the
    /// caller's `now` parameter (typically `SystemTime::now()`).
    pub age: Duration,
    /// OS process start-time token recorded in `.daemon` (third field), or
    /// `None` for a legacy file written before CR-2026-06-14. Passed to
    /// [`cleanup_zombie_daemon`] so it can verify the live PID is still the
    /// same process before signalling. `None` → fail-closed-skip.
    pub start_token: Option<u64>,
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

/// Pure decision core (CR-2026-06-14): may we send a kill signal to a PID?
///
/// Returns `true` ONLY when BOTH the start-token recorded in `.daemon`
/// (`recorded`) AND the live process's current start-token (`current`) are
/// known AND equal. Every other case fails closed (returns `false`):
/// - `recorded == None` — legacy `.daemon` with no token / unreadable → can't
///   prove identity → skip (DP3 fail-closed-skip, next-boot sweep handles it).
/// - `current == None` — couldn't read the live process's token → skip.
/// - `recorded != current` — the PID was recycled onto a different process →
///   signalling it would kill an innocent → skip.
///
/// This is the load-bearing core; the kill primitive and the grace poll both
/// gate on it. Kept pure (no syscalls) so it is exhaustively unit-testable.
pub fn should_signal(recorded: Option<u64>, current: Option<u64>) -> bool {
    matches!((recorded, current), (Some(r), Some(c)) if r == c)
}

/// Core kill primitive. Sends SIGTERM, polls liveness up to `term_grace`,
/// escalates to SIGKILL on timeout and polls up to `kill_grace`.
///
/// CR-2026-06-14 identity-compare: before EVERY signal the live PID's OS
/// start-token is compared against `recorded_token` (read from `.daemon`) via
/// [`should_signal`]. A mismatch — the original daemon exited and the OS
/// recycled its PID onto an unrelated process — short-circuits to
/// [`KillOutcome::IdentityMismatch`] with NO signal sent. This closes the
/// TOCTOU where a stale run-dir PID could be SIGKILLed after the kernel had
/// reassigned it to an innocent process.
///
/// Returns [`KillOutcome::AlreadyExited`] if the PID is already gone at
/// entry. Returns [`KillOutcome::IdentityMismatch`] if the token check fails
/// (no/wrong identity). Returns [`KillOutcome::Graceful(elapsed)`] if SIGTERM
/// landed. Returns [`KillOutcome::ForceKilled`] if SIGKILL was needed and
/// succeeded. Returns [`KillOutcome::RefusedToDie`] if both stages timed out.
///
/// Windows: short-circuits to [`KillOutcome::WindowsTerminated`] via
/// `TerminateProcess` (no SIGTERM equivalent). The two-stage grace is
/// Unix-only.
pub fn cleanup_zombie_daemon(
    pid: u32,
    recorded_token: Option<u64>,
    term_grace: Duration,
    kill_grace: Duration,
) -> KillOutcome {
    if !is_alive(pid) {
        return KillOutcome::AlreadyExited;
    }

    // Identity gate: verify the live PID is still the process we recorded
    // BEFORE sending any signal. Fail-closed on no/wrong identity.
    if !should_signal(recorded_token, crate::process::process_start_token(pid)) {
        tracing::warn!(
            pid,
            recorded_token = ?recorded_token,
            "#927/CR-2026-06-14 cleanup-zombies: start-token mismatch — skipping kill \
             (PID recycled or no recorded identity); next-boot sweep will reclaim the dir"
        );
        return KillOutcome::IdentityMismatch;
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
        if !poll_until_dead_or_recycled(pid, recorded_token, term_grace) {
            // Re-verify before escalating: the original may have exited during
            // the SIGTERM grace and the PID been recycled. Never SIGKILL a
            // freshly-arrived innocent process.
            if !should_signal(recorded_token, crate::process::process_start_token(pid)) {
                tracing::warn!(
                    pid,
                    "#927/CR-2026-06-14 cleanup-zombies: start-token changed during SIGTERM \
                     grace — skipping SIGKILL (PID recycled)"
                );
                return KillOutcome::IdentityMismatch;
            }
            // Stage 2: SIGKILL.
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
            }
            if poll_until_dead_or_recycled(pid, recorded_token, kill_grace) {
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
        if poll_until_dead_or_recycled(pid, recorded_token, Duration::from_secs(2)) {
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
/// CR-2026-06-14: the production kill path moved to the identity-aware
/// [`poll_until_dead_or_recycled`]; the only remaining callers of this plain
/// variant are `#[cfg(unix)]` tests, so it is gated `#[cfg(all(test, unix))]`
/// — keeping it out of the non-test bin build (where it would be dead) and
/// out of the Windows test build (where none of its unix-only callers exist).
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
#[cfg(all(test, unix))]
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

/// Identity-aware variant of [`poll_until_dead`] (CR-2026-06-14): returns true
/// if the PID dies OR its OS start-token stops matching `recorded_token`
/// mid-poll. The latter means the original process exited and the kernel
/// recycled its PID onto a different process — the daemon we were reaping IS
/// gone, so we treat it as dead AND, crucially, the caller will not escalate
/// to SIGKILL against the innocent newcomer (the kill primitive re-checks
/// identity before each signal). Reached only after the entry-level identity
/// gate passed, so `recorded_token` is always `Some` here; a transient
/// unreadable live token (`current == None`) also returns true (safe
/// direction — under-kill, never over-kill).
pub(crate) fn poll_until_dead_or_recycled(
    pid: u32,
    recorded_token: Option<u64>,
    timeout: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !is_alive(pid) {
            return true;
        }
        if !should_signal(recorded_token, crate::process::process_start_token(pid)) {
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
            let start_token = crate::daemon::read_daemon_start_token(&entry.path());
            zombies.push(ZombieInfo {
                pid,
                run_dir: entry.path(),
                age,
                start_token,
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

        // Dead PID short-circuits to AlreadyExited before the identity gate,
        // so the recorded token is irrelevant here (pass None).
        let outcome =
            cleanup_zombie_daemon(pid, None, Duration::from_secs(1), Duration::from_secs(1));
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

        // Pass the live child's REAL start-token so the identity gate passes.
        let token = crate::process::process_start_token(pid);
        let outcome =
            cleanup_zombie_daemon(pid, token, Duration::from_secs(3), Duration::from_secs(2));
        let _ = reaper.join();

        assert!(
            matches!(outcome, KillOutcome::Graceful(_)),
            "SIGTERM-respecting process must return Graceful, got {outcome:?}"
        );
    }

    /// Spawn python3 with SIG_IGN installed, using a sentinel file to
    /// synchronize — eliminates the 300ms sleep race that caused flaky
    /// failures on macOS CI (#1303).
    #[cfg(unix)]
    fn spawn_sigign_with_sentinel(
        sleep_secs: u32,
    ) -> (u32, std::thread::JoinHandle<()>, std::path::PathBuf) {
        let sentinel = std::env::temp_dir().join(format!(
            "agend-sigign-sentinel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let script = format!(
            "import signal, time, sys; signal.signal(signal.SIGTERM, signal.SIG_IGN); \
             open(sys.argv[1], 'w').close(); time.sleep({sleep_secs})"
        );
        let (pid, reaper) =
            spawn_with_reaper("python3", &["-c", &script, sentinel.to_str().unwrap()]);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !sentinel.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "python3 did not write sentinel within 5s — interpreter startup too slow"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        (pid, reaper, sentinel)
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_zombie_daemon_sigterm_ignored_returns_force_killed() {
        let (pid, reaper, sentinel) = spawn_sigign_with_sentinel(60);

        // Pass the live child's REAL start-token so the identity gate passes.
        let token = crate::process::process_start_token(pid);
        // 500ms SIGTERM grace (short — SIG_IGN won't release), 3s SIGKILL grace.
        let outcome = cleanup_zombie_daemon(
            pid,
            token,
            Duration::from_millis(500),
            Duration::from_secs(3),
        );
        let _ = reaper.join();
        let _ = std::fs::remove_file(&sentinel);

        assert!(
            matches!(outcome, KillOutcome::ForceKilled),
            "SIGTERM-ignored process must escalate to SIGKILL, got {outcome:?}"
        );
    }

    // ── CR-2026-06-14 zombie-kill identity-compare repro ──────────────────
    //
    // The TOCTOU finding: `cleanup_zombie_daemon` selected a target purely by
    // run-dir name + `.daemon` mtime and signalled it after one entry
    // `is_alive` — so a PID recycled onto an innocent process between selection
    // and the signal would be killed. The fix gates every signal on
    // `should_signal(recorded_token, live_token)`.
    //
    // These tests simulate PID recycling DETERMINISTICALLY without waiting for
    // a real recycle: a live cooperative child is the "innocent" process; we
    // vary only the RECORDED token to model the three cases.

    /// Pure decision core — exhaustive table. This is the load-bearing gate;
    /// every kill path funnels through it.
    #[test]
    fn should_signal_only_when_both_known_and_equal() {
        assert!(should_signal(Some(7), Some(7)), "match → signal");
        assert!(
            !should_signal(Some(7), Some(8)),
            "recycled (mismatch) → skip"
        );
        assert!(!should_signal(None, Some(7)), "legacy no-token → skip");
        assert!(
            !should_signal(Some(7), None),
            "live token unreadable → skip"
        );
        assert!(!should_signal(None, None), "nothing known → skip");
    }

    /// RED→GREEN: a recycled PID (live token ≠ recorded token) must NOT be
    /// signalled. The pre-fix primitive took only a bare pid and unconditionally
    /// SIGTERM/SIGKILLed → it would kill this innocent live child. With the fix
    /// the token mismatch short-circuits to `IdentityMismatch` and the child
    /// survives untouched (zero signal sent).
    #[cfg(unix)]
    #[test]
    fn cleanup_skips_when_live_token_differs_from_recorded_pid_recycled() {
        let (pid, reaper) = spawn_with_reaper("sh", &["-c", "sleep 60"]);
        let live = crate::process::process_start_token(pid).expect("live token");
        // Recorded token differs from the live process's token → models a
        // recycled PID: the `.daemon` recorded daemon D's token, but `pid` now
        // belongs to an unrelated process.
        let recorded = Some(live.wrapping_add(1));

        let outcome = cleanup_zombie_daemon(
            pid,
            recorded,
            Duration::from_secs(3),
            Duration::from_secs(2),
        );

        assert_eq!(
            outcome,
            KillOutcome::IdentityMismatch,
            "recycled PID (token mismatch) must yield IdentityMismatch, got {outcome:?}"
        );
        assert!(
            is_alive(pid),
            "innocent live process must NOT be signalled when the token mismatches"
        );

        // Cleanup the still-alive child.
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        let _ = reaper.join();
    }

    /// CONTROL: a genuine zombie (live token == recorded token) is still
    /// killed. This proves the identity gate didn't simply break killing — a
    /// regression that dropped the gate would pass `cleanup_skips_…` AND this
    /// one, but a regression that over-tightened (never signals) would fail
    /// HERE. The pair pins the gate as load-bearing.
    #[cfg(unix)]
    #[test]
    fn cleanup_kills_when_live_token_matches_recorded_real_zombie() {
        let (pid, reaper) = spawn_with_reaper("sh", &["-c", "sleep 60"]);
        let recorded = crate::process::process_start_token(pid); // == live token

        let outcome = cleanup_zombie_daemon(
            pid,
            recorded,
            Duration::from_secs(3),
            Duration::from_secs(2),
        );
        let _ = reaper.join();

        assert!(
            matches!(outcome, KillOutcome::Graceful(_) | KillOutcome::ForceKilled),
            "matching-identity zombie must be killed, got {outcome:?}"
        );
        assert!(
            poll_until_dead(pid, Duration::from_secs(5)),
            "matching-identity zombie must be dead after cleanup"
        );
    }

    /// BACK-COMPAT: a legacy `.daemon` with no recorded token (`None`) →
    /// fail-closed-skip (DP3). We do NOT signal when identity can't be proven;
    /// the next-boot stale-pid sweep reclaims the dir instead.
    #[cfg(unix)]
    #[test]
    fn cleanup_skips_legacy_no_token_fail_closed() {
        let (pid, reaper) = spawn_with_reaper("sh", &["-c", "sleep 60"]);

        let outcome =
            cleanup_zombie_daemon(pid, None, Duration::from_secs(3), Duration::from_secs(2));

        assert_eq!(
            outcome,
            KillOutcome::IdentityMismatch,
            "legacy no-token .daemon must fail-closed-skip, got {outcome:?}"
        );
        assert!(
            is_alive(pid),
            "fail-closed-skip must not signal the process"
        );

        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        let _ = reaper.join();
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
        let (pid, reaper, sentinel) = spawn_sigign_with_sentinel(30);

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
        let _ = std::fs::remove_file(&sentinel);

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
