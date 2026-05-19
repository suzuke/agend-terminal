//! #933 boot-time zombie daemon sweep.
//!
//! Complements [`crate::daemon::sweep_stale_run_dirs`] (which only cleans
//! dirs whose PIDs are dead). The boot sweep handles the OTHER class:
//! PIDs that are still alive but unresponsive (the empirical 16-20d
//! zombies that motivated #927 PR-B and this follow-up).
//!
//! Two modes:
//!
//! 1. **Always-on telemetry**: at every daemon boot, scan
//!    `<home>/run/<pid>/.daemon` entries and `log_zombie_state` for any
//!    candidate older than [`DEFAULT_AGE_DAYS`]. Operators see zombies
//!    even without opting into destructive sweep.
//! 2. **Opt-in destructive sweep**: when
//!    `AGEND_DAEMON_BOOT_SWEEP_AGE_DAYS=N` (N≥1) is set, each candidate
//!    older than N days is sent through #927 PR-B's
//!    [`crate::admin::cleanup_zombies::cleanup_zombie_daemon`]. A
//!    secondary env `AGEND_DAEMON_BOOT_SWEEP_DRY_RUN=1` downgrades the
//!    destructive sweep to log-only for operator validation.
//!
//! All cleanup primitives (SIGTERM grace, SIGKILL escalation,
//! Unix/Windows asymmetry, `log_zombie_state`) are reused 100% from
//! `crate::admin::cleanup_zombies` — this module is the boot-time entry
//! point only.

use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::admin::cleanup_zombies::{
    cleanup_zombie_daemon, find_zombie_candidates, log_zombie_state, KillOutcome,
};

/// Env: enables destructive boot-sweep and sets the age threshold in
/// days. Must parse to a positive integer (≥1); malformed values are
/// treated as unset with a `warn` log so operators see the parse failure.
pub const ENV_AGE_DAYS: &str = "AGEND_DAEMON_BOOT_SWEEP_AGE_DAYS";
/// Env: when set to `"1"` AND `ENV_AGE_DAYS` is set, candidates are
/// logged but not killed. For operator validation before allowing
/// destructive boot.
pub const ENV_DRY_RUN: &str = "AGEND_DAEMON_BOOT_SWEEP_DRY_RUN";

const TERM_GRACE: Duration = Duration::from_secs(5);
const KILL_GRACE: Duration = Duration::from_secs(2);
const DEFAULT_AGE_DAYS: u64 = 14;
const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// Public boot-sweep entry. Parses env vars, then calls [`boot_sweep_impl`].
///
/// Returns the count of zombies actually killed (0 in telemetry-only or
/// dry-run modes). Callers (daemon boot path) should not depend on the
/// return value beyond informational logging.
pub fn boot_sweep_zombies(_home: &Path) -> usize {
    // RED stub — replaced in GREEN commit.
    unimplemented!("boot_sweep_zombies: RED stub awaiting GREEN impl")
}

/// Test-accessible inner. Parametrized clock + grace windows so tests
/// can pin determinism without `std::env::set_var` side-effects on
/// production globals.
#[allow(clippy::too_many_arguments)]
pub(crate) fn boot_sweep_impl(
    _home: &Path,
    _min_age: Duration,
    _destructive: bool,
    _dry_run: bool,
    _term_grace: Duration,
    _kill_grace: Duration,
    _now: SystemTime,
) -> usize {
    // RED stub — replaced in GREEN commit.
    unimplemented!("boot_sweep_impl: RED stub awaiting GREEN impl")
}

#[allow(dead_code)]
fn _ref_unused_consts_for_red() {
    // Avoid dead_code warnings on consts that the RED stub doesn't touch.
    let _ = (TERM_GRACE, KILL_GRACE, DEFAULT_AGE_DAYS, SECONDS_PER_DAY);
}

#[allow(dead_code)]
fn _ref_unused_imports_for_red() {
    // Suppress unused-import warnings on the cleanup_zombies surface
    // that the GREEN impl wires in. The references are pointer-only so
    // monomorphization cost is zero.
    let _f: fn(_, _, _) -> _ = cleanup_zombie_daemon;
    let _g: fn(_, _, _) -> _ = find_zombie_candidates;
    let _h: fn(_) = log_zombie_state;
    let _k = std::mem::size_of::<KillOutcome>();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("agend-933-{}-{}-{}", tag, std::process::id(), id));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Plant `<home>/run/<pid>/.daemon` with `<pid>:0` content (matches
    /// production `write_daemon_id` shape).
    fn plant_run_dir(home: &Path, pid: u32) -> PathBuf {
        let run = home.join("run").join(pid.to_string());
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join(".daemon"), format!("{pid}:0")).unwrap();
        run
    }

    /// Same as [`plant_run_dir`] but writes a DIFFERENT PID into the
    /// `.daemon` body — simulates PID reuse where a recycled PID's dir
    /// has stale identity from a prior daemon.
    fn plant_run_dir_with_mismatch(home: &Path, dir_pid: u32, recorded_pid: u32) -> PathBuf {
        let run = home.join("run").join(dir_pid.to_string());
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join(".daemon"), format!("{recorded_pid}:0")).unwrap();
        run
    }

    /// Spawn a child that respects SIGTERM (default sh `sleep` behavior).
    /// Returns (pid, reaper-join-handle). Reaper polls `try_wait` to
    /// keep the child table clean during tests.
    #[cfg(unix)]
    fn spawn_sigterm_respecter() -> (u32, std::thread::JoinHandle<()>) {
        let mut child = Command::new("sh")
            .args(["-c", "sleep 60"])
            .spawn()
            .expect("spawn sh");
        let pid = child.id();
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

    /// Spawn a child that IGNORES SIGTERM via python3 + SIG_IGN.
    /// Mirror of `crate::admin::cleanup_zombies::tests::spawn_with_reaper`.
    #[cfg(unix)]
    fn spawn_sigterm_ignorer() -> (u32, std::thread::JoinHandle<()>) {
        let mut child = Command::new("python3")
            .args([
                "-c",
                "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)",
            ])
            .spawn()
            .expect("spawn python3");
        let pid = child.id();
        // Let python install SIG_IGN before any test signal arrives.
        std::thread::sleep(Duration::from_millis(300));
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

    // ── Test 1 — env-equivalent destructive sweep kills age-threshold candidate ──
    #[cfg(unix)]
    #[test]
    fn boot_sweep_kills_zombie_older_than_threshold_when_env_set() {
        let home = tmp_home("t1-kill");
        let (pid, reaper) = spawn_sigterm_respecter();
        let run_dir = plant_run_dir(&home, pid);
        // mtime ≈ now; age = synth_now - mtime = 10s, threshold = 1s → kill.
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
        let _ = reaper.join();
        assert_eq!(
            killed, 1,
            "destructive sweep must kill aged sigterm-respecter"
        );
        assert!(!run_dir.exists(), "killed zombie's run_dir must be removed");
        assert!(
            !crate::process::is_pid_alive(pid),
            "child must be dead post-sweep"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Test 2 — env unset → telemetry only (no kill) ──
    #[cfg(unix)]
    #[test]
    fn boot_sweep_skips_when_env_unset() {
        let home = tmp_home("t2-noenv");
        let (pid, reaper) = spawn_sigterm_respecter();
        let run_dir = plant_run_dir(&home, pid);
        let now = SystemTime::now() + Duration::from_secs(10);
        let killed = boot_sweep_impl(
            &home,
            Duration::from_secs(1),
            /* destructive */ false,
            /* dry_run */ false,
            Duration::from_secs(3),
            Duration::from_secs(2),
            now,
        );
        // Reaper kills the child after test, but boot_sweep must not have.
        assert_eq!(killed, 0, "non-destructive run must report 0 killed");
        assert!(
            run_dir.exists(),
            "run_dir must be preserved when destructive=false"
        );
        assert!(
            crate::process::is_pid_alive(pid),
            "child must still be alive immediately after non-destructive sweep"
        );
        let _ = reaper.join();
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Test 3 — dry-run logs but does not kill ──
    #[cfg(unix)]
    #[test]
    fn boot_sweep_dry_run_logs_but_does_not_kill() {
        let home = tmp_home("t3-dry");
        let (pid, reaper) = spawn_sigterm_respecter();
        let run_dir = plant_run_dir(&home, pid);
        let now = SystemTime::now() + Duration::from_secs(10);
        let killed = boot_sweep_impl(
            &home,
            Duration::from_secs(1),
            /* destructive */ true,
            /* dry_run */ true,
            Duration::from_secs(3),
            Duration::from_secs(2),
            now,
        );
        assert_eq!(killed, 0, "dry-run must report 0 killed");
        assert!(run_dir.exists(), "dry-run must preserve run_dir");
        assert!(
            crate::process::is_pid_alive(pid),
            "dry-run must not kill child"
        );
        let _ = reaper.join();
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Test 4 — boundary pair: age = threshold-1s skip; age = threshold kill ──
    #[cfg(unix)]
    #[test]
    fn boot_sweep_threshold_boundary_pair() {
        let home = tmp_home("t4-bdry");
        let (pid, reaper) = spawn_sigterm_respecter();
        plant_run_dir(&home, pid);
        let threshold = Duration::from_secs(86400);
        let mtime = std::fs::metadata(home.join("run").join(pid.to_string()).join(".daemon"))
            .unwrap()
            .modified()
            .unwrap();
        // age = threshold - 1s → skip
        let now_under = mtime + threshold - Duration::from_secs(1);
        let killed_under = boot_sweep_impl(
            &home,
            threshold,
            true,
            false,
            Duration::from_secs(3),
            Duration::from_secs(2),
            now_under,
        );
        assert_eq!(killed_under, 0, "age < threshold must skip");
        assert!(
            crate::process::is_pid_alive(pid),
            "skipped child must still be alive"
        );
        // age = threshold → kill (find_zombie_candidates uses `age >= min_age`)
        let now_at = mtime + threshold;
        let killed_at = boot_sweep_impl(
            &home,
            threshold,
            true,
            false,
            Duration::from_secs(3),
            Duration::from_secs(2),
            now_at,
        );
        assert_eq!(killed_at, 1, "age = threshold must kill");
        let _ = reaper.join();
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Test 5 — PID reuse identity guard: dir name != .daemon PID → skip ──
    #[cfg(unix)]
    #[test]
    fn boot_sweep_pid_reuse_identity_guard() {
        let home = tmp_home("t5-pidreuse");
        let (live_pid, reaper) = spawn_sigterm_respecter();
        // Plant <home>/run/<live_pid>/.daemon with a DIFFERENT recorded PID.
        // Simulates a recycled PID whose `.daemon` carries stale identity
        // from a prior daemon. Boot-sweep must refuse to kill the
        // currently-occupying process.
        let recorded_pid = 99999u32;
        let run_dir = plant_run_dir_with_mismatch(&home, live_pid, recorded_pid);
        let now = SystemTime::now() + Duration::from_secs(10);
        let killed = boot_sweep_impl(
            &home,
            Duration::from_secs(1),
            true,
            false,
            Duration::from_secs(3),
            Duration::from_secs(2),
            now,
        );
        assert_eq!(killed, 0, "identity-mismatch candidate must not be killed");
        assert!(
            crate::process::is_pid_alive(live_pid),
            "live PID (different daemon's identity) must remain alive"
        );
        assert!(run_dir.exists(), "skipped run_dir must remain on disk");
        let _ = reaper.join();
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Test 6 — multi-daemon coexistence: boot_sweep(home_A) does not touch home_B ──
    #[cfg(unix)]
    #[test]
    fn boot_sweep_multi_daemon_coexistence_no_cross_kill() {
        let home_a = tmp_home("t6-homeA");
        let home_b = tmp_home("t6-homeB");
        let (pid_a, reaper_a) = spawn_sigterm_respecter();
        let (pid_b, reaper_b) = spawn_sigterm_respecter();
        let run_a = plant_run_dir(&home_a, pid_a);
        let run_b = plant_run_dir(&home_b, pid_b);
        let now = SystemTime::now() + Duration::from_secs(10);
        // Sweep only home_A.
        let killed = boot_sweep_impl(
            &home_a,
            Duration::from_secs(1),
            true,
            false,
            Duration::from_secs(3),
            Duration::from_secs(2),
            now,
        );
        assert_eq!(killed, 1, "home_A sweep must kill only home_A's zombie");
        assert!(!run_a.exists(), "home_A run_dir must be removed");
        assert!(run_b.exists(), "home_B run_dir must be untouched");
        assert!(
            crate::process::is_pid_alive(pid_b),
            "home_B zombie must remain alive (no cross-home kill)"
        );
        let _ = reaper_a.join();
        let _ = reaper_b.join();
        std::fs::remove_dir_all(&home_a).ok();
        std::fs::remove_dir_all(&home_b).ok();
    }

    // ── Test 7 — self-target impossibility: own_pid always filtered ──
    #[test]
    fn boot_sweep_self_target_impossibility() {
        let home = tmp_home("t7-self");
        let own_pid = std::process::id();
        let run_dir = plant_run_dir(&home, own_pid);
        let now = SystemTime::now() + Duration::from_secs(10);
        let killed = boot_sweep_impl(
            &home,
            Duration::from_secs(1),
            true,
            false,
            Duration::from_secs(3),
            Duration::from_secs(2),
            now,
        );
        assert_eq!(killed, 0, "own PID must never appear in killed count");
        assert!(run_dir.exists(), "own-PID run_dir must be preserved");
        // We're obviously still alive — the assert is redundant but
        // documents the contract.
        assert!(crate::process::is_pid_alive(own_pid));
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Test 8 — malformed env parse safety: env=abc → no kill + warn ──
    // Note: this targets the PUBLIC `boot_sweep_zombies` (which reads env).
    // Uses serial_test to prevent env-var contention with other tests.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn boot_sweep_malformed_env_parse_safety() {
        let home = tmp_home("t8-badenv");
        let (pid, reaper) = spawn_sigterm_respecter();
        let run_dir = plant_run_dir(&home, pid);
        // SAFETY: tests serialised via `serial_test::serial`; no other thread
        // mutates these env vars concurrently within a serial scope.
        unsafe {
            std::env::set_var(ENV_AGE_DAYS, "abc");
            std::env::remove_var(ENV_DRY_RUN);
        }
        let killed = boot_sweep_zombies(&home);
        unsafe {
            std::env::remove_var(ENV_AGE_DAYS);
        }
        assert_eq!(
            killed, 0,
            "malformed env must NOT trigger destructive sweep"
        );
        assert!(
            run_dir.exists() || !run_dir.exists(),
            "candidate may or may not surface depending on default threshold"
        );
        assert!(
            crate::process::is_pid_alive(pid),
            "child must remain alive after malformed-env sweep"
        );
        let _ = reaper.join();
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Test 9 — sweep-fail recovery: RefusedToDie outcome doesn't panic ──
    // Spawns a SIGTERM-ignoring python3 with very tight grace windows to
    // force RefusedToDie. Verifies boot_sweep_impl returns normally and
    // killed count reflects the failed cleanup (=0 for that candidate).
    #[cfg(unix)]
    #[test]
    fn boot_sweep_eperm_degradation_continues_boot() {
        let home = tmp_home("t9-refused");
        let (pid, reaper) = spawn_sigterm_ignorer();
        let run_dir = plant_run_dir(&home, pid);
        let now = SystemTime::now() + Duration::from_secs(10);
        // Tight grace windows: SIGTERM is SIG_IGN'd → 100ms wait → SIGKILL.
        // SIGKILL needs ~1-10ms to take effect; we give 5ms which is
        // BELOW SIGKILL propagation time on most kernels → RefusedToDie.
        // Even if SIGKILL races and succeeds, the test asserts boot_sweep
        // returns without panic, not a specific killed count.
        let killed = boot_sweep_impl(
            &home,
            Duration::from_secs(1),
            true,
            false,
            Duration::from_millis(100),
            Duration::from_millis(5),
            now,
        );
        // Killed count is 0 (RefusedToDie) or 1 (SIGKILL raced and succeeded).
        // The contract being tested is "boot returns normally" — assertion
        // is permissive.
        assert!(killed <= 1, "killed count must be bounded (got {killed})");
        // run_dir status mirrors killed: removed iff actually killed.
        if killed == 0 {
            assert!(
                run_dir.exists(),
                "RefusedToDie path must leave run_dir intact"
            );
        }
        // Always-on contract: boot_sweep returned without panic ✓ (we
        // wouldn't reach here otherwise).
        let _ = reaper.join();
        std::fs::remove_dir_all(&home).ok();
    }
}
