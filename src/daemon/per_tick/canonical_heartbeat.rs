//! #t-…83936-4 incident follow-up: canonical `source_repo` existence heartbeat.
//!
//! On 2026-07-06 the operator's canonical repo was deleted and went undetected
//! for ~40 minutes: the daemon held the deleted dir as its cwd (an orphaned
//! inode still answers cwd-relative lookups), so `binding_state` reported a stale
//! `valid=true` and nothing surfaced the disappearance until an agent's commit
//! hit the dangling gitdir. The root problem was "deleted with nobody noticing".
//!
//! This watchdog closes that gap. Every N ticks it enumerates the registered
//! source_repos (`binding::bound_source_repos` — distinct ABSOLUTE paths from
//! each `binding.json`) and checks, BY ABSOLUTE PATH, that each still (a) exists
//! and (b) is a git repo (`git -C <abs> rev-parse --git-dir`). A vanished or
//! corrupt canonical pages ALL operator escalation channels + writes an
//! event-log `canonical_repo_missing` row.
//!
//! ⚠ The cwd trap (the soul of this fix): the daemon's own cwd may itself BE the
//! deleted canonical, and an orphaned inode still resolves cwd-relative lookups —
//! exactly the incident. Every check here MUST resolve by ABSOLUTE PATH, never
//! `.`/cwd-relative. `bound_source_repos` yields absolute paths, and
//! `std::fs::metadata(abs)` / `git -C abs` do fresh path lookups. This invariant
//! is pinned by a test that runs the check with the process cwd set to a
//! since-deleted directory (see `flags_missing_repo_even_when_cwd_is_deleted`).
//!
//! Complements protection ① (`binding_state`'s `worktree_resolves`): any agent
//! calling `binding_state` (bind / release / introspect) detects a dead canonical
//! INSTANTLY, so during normal activity detection is second-level; this periodic
//! heartbeat is the backstop for a fully-idle fleet.
//!
//! Dedup: a per-repo latch pages ONCE on the present→missing transition; recovery
//! (missing→present) clears the latch so a later re-deletion pages again.
//!
//! # Offload (architecture-fix PR3 / F3)
//!
//! `git_ok` is bounded by `LOCAL_GIT_TIMEOUT` (60s). Running that inline on the
//! daemon/app tick host freezes later handlers (ThreadDump) and the app TUI
//! maintenance arm for up to 60s × N repos — same liveness class as #P1-2607.
//! The round therefore runs on a fire-and-forget background thread with an
//! `in_flight` re-entrancy guard + `ClearOnDrop`, matching `worktree_registry_sweep`
//! / `hourly_gc`. Cadence and missing/recovery/dedup semantics are unchanged.

use super::{PerTickHandler, TickContext};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Per-tick canonical-existence watchdog. Default cadence 60 ticks (~10 min at
/// the 10 s tick) — a 4× improvement over the incident's 40-min silence; the
/// real-time path is protection ①.
pub(crate) struct CanonicalHeartbeatHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
    /// Repos currently in the alerted-missing state (dedup: one page per outage).
    /// `Arc` so the background round can own a clone without borrowing `self`.
    alerted: Arc<Mutex<HashSet<String>>>,
    /// #P1-2607-class: at most one background round at a time.
    in_flight: Arc<AtomicBool>,
}

impl CanonicalHeartbeatHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
            alerted: Arc::new(Mutex::new(HashSet::new())),
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }

    #[cfg(test)]
    fn is_in_flight(&self) -> bool {
        self.in_flight.load(Ordering::Acquire)
    }
}

impl PerTickHandler for CanonicalHeartbeatHandler {
    fn name(&self) -> &'static str {
        "canonical_heartbeat"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        // Previous round still running — skip rather than stacking git storms.
        if self.in_flight.swap(true, Ordering::AcqRel) {
            tracing::warn!(
                "canonical_heartbeat: previous round still in flight, \
                 skipping this tick's fire (will retry next cadence)"
            );
            return;
        }
        let home = ctx.home.to_path_buf();
        let alerted = Arc::clone(&self.alerted);
        let in_flight = Arc::clone(&self.in_flight);
        // fire-and-forget: #P1-2607-class / F3 — `git_ok` (LOCAL_GIT_TIMEOUT 60s)
        // must not block the daemon/app tick host (TUI, ThreadDump, later handlers).
        // No JoinHandle. `ClearOnDrop` clears `in_flight`; tests join via a
        // worker-exit completion signal that fires *after* that clear (RAII).
        std::thread::spawn(move || {
            // Drop order is reverse of declaration: ClearOnDrop runs first
            // (in_flight=false), then WorkerExitOnDrop signals COMPLETIONS so
            // HookCleanup cannot race a stale completion into the next test.
            #[cfg(test)]
            let _worker_exit = test_hooks::WorkerExitOnDrop::new();
            let _guard = super::ClearOnDrop::new(in_flight);
            #[cfg(test)]
            test_hooks::body_entry_gate();
            run_round(&home, &alerted);
        });
    }
}

/// One offloaded check+page round. Latch lock is held only for remove/insert/retain;
/// git and notifications run unlocked.
fn run_round(home: &Path, alerted: &Mutex<HashSet<String>>) {
    let repos = crate::binding::bound_source_repos(home);
    let registered: HashSet<String> = repos
        .iter()
        .map(|r| r.to_string_lossy().into_owned())
        .collect();

    for repo in &repos {
        let key = repo.to_string_lossy().into_owned();
        // 1) path/git OUTSIDE the latch lock (may take up to LOCAL_GIT_TIMEOUT).
        let reason = canonical_missing_reason(repo);
        // 2) lock ONLY for remove / insert decision.
        let should_page = {
            let mut guard = alerted.lock();
            match reason {
                None => {
                    guard.remove(&key);
                    false
                }
                Some(_) => guard.insert(key.clone()), // true iff first page this outage
            }
        }; // lock released
           // 3) notifications + event_log UNLOCKED.
        if let (true, Some(reason)) = (should_page, reason) {
            let msg = format!(
                "[canonical-missing] registered source_repo '{}' is GONE ({reason}). \
                 Every worktree bound to it is now unusable (dangling gitdir) and \
                 agents may silently commit into dead worktrees until it is \
                 restored — re-clone/restore the canonical, then repair worktrees.",
                repo.display(),
            );
            let dispatched = crate::channel::notify_all_escalation_channels(
                &key,
                crate::channel::NotifySeverity::Error,
                &msg,
                false,
            );
            crate::event_log::log(home, "canonical_repo_missing", &key, &msg);
            tracing::error!(
                repo = %repo.display(),
                reason,
                channels = dispatched,
                "canonical_repo_missing: registered source_repo vanished"
            );
        }
    }
    // 4) retain under lock only (short critical section).
    {
        let mut guard = alerted.lock();
        guard.retain(|k| registered.contains(k));
    }
}

/// `None` = the canonical is healthy; `Some(reason)` = it's gone or corrupt.
/// Resolves STRICTLY by absolute path (the caller passes absolute source_repo
/// paths) so the daemon's own — possibly orphaned — cwd can never mask a
/// deletion. `git_ok` runs only after the path exists, so it never touches a
/// missing dir.
fn canonical_missing_reason(repo: &Path) -> Option<&'static str> {
    if std::fs::metadata(repo).is_err() {
        return Some("path missing");
    }
    if !crate::git_helpers::git_ok(repo, &["rev-parse", "--git-dir"]) {
        return Some("not a git repo (corrupt/removed .git)");
    }
    None
}

/// Offload-determinism seams (PR3 / #P1-2607 pattern): body-entry GATE (signals
/// entered, then blocks), COMPLETIONS after ClearOnDrop, optional panic-once.
/// `recv_timeout` / wait timeouts are harness watchdogs only.
#[cfg(test)]
mod test_hooks {
    use parking_lot::{Condvar, Mutex};
    use std::time::{Duration, Instant};

    static GATE_ARMED: Mutex<bool> = Mutex::new(false);
    static GATE_CV: Condvar = Condvar::new();
    static BODY_ENTERED: Mutex<u64> = Mutex::new(0);
    static BODY_ENTERED_CV: Condvar = Condvar::new();
    static COMPLETIONS: Mutex<u64> = Mutex::new(0);
    static COMPLETIONS_CV: Condvar = Condvar::new();
    static PANIC_ONCE: Mutex<bool> = Mutex::new(false);

    pub(super) fn reset() {
        *GATE_ARMED.lock() = false;
        GATE_CV.notify_all();
        *BODY_ENTERED.lock() = 0;
        BODY_ENTERED_CV.notify_all();
        *COMPLETIONS.lock() = 0;
        *PANIC_ONCE.lock() = false;
    }

    pub(super) fn arm_gate() {
        *GATE_ARMED.lock() = true;
    }

    pub(super) fn release_gate() {
        *GATE_ARMED.lock() = false;
        GATE_CV.notify_all();
    }

    pub(super) fn is_armed() -> bool {
        *GATE_ARMED.lock()
    }

    pub(super) fn body_entered_count() -> u64 {
        *BODY_ENTERED.lock()
    }

    pub(super) fn wait_for_body_entered(timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut n = BODY_ENTERED.lock();
        while *n == 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                drop(n);
                panic!("watchdog: body_entry_gate never entered within {timeout:?}");
            }
            if BODY_ENTERED_CV.wait_for(&mut n, remaining).timed_out() && *n == 0 {
                drop(n);
                panic!("watchdog: body_entry_gate never entered within {timeout:?}");
            }
        }
    }

    /// Signal body-entered **before** blocking on the armed gate.
    pub(super) fn body_entry_gate() {
        {
            *BODY_ENTERED.lock() += 1;
            BODY_ENTERED_CV.notify_all();
        }
        let mut armed = GATE_ARMED.lock();
        while *armed {
            GATE_CV.wait(&mut armed);
        }
        if std::mem::take(&mut *PANIC_ONCE.lock()) {
            panic!("canonical_heartbeat test: intentional body panic");
        }
    }

    pub(super) fn arm_panic_once() {
        *PANIC_ONCE.lock() = true;
    }

    fn signal_round_complete() {
        *COMPLETIONS.lock() += 1;
        COMPLETIONS_CV.notify_all();
    }

    pub(super) fn completions() -> u64 {
        *COMPLETIONS.lock()
    }

    /// Wait until completions > `prev`. `timeout` is a harness watchdog only.
    pub(super) fn wait_for_completion(prev: u64, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut n = COMPLETIONS.lock();
        while *n <= prev {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                drop(n);
                panic!("watchdog: completions never advanced past {prev} within {timeout:?}");
            }
            if COMPLETIONS_CV.wait_for(&mut n, remaining).timed_out() && *n <= prev {
                drop(n);
                panic!("watchdog: completions never advanced past {prev} within {timeout:?}");
            }
        }
    }

    /// Bounded join watchdog for a test-spawned runner thread.
    pub(super) fn join_with_watchdog(handle: std::thread::JoinHandle<()>, timeout: Duration) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = handle.join();
            let _ = tx.send(());
        });
        if rx.recv_timeout(timeout).is_err() {
            panic!("watchdog: worker JoinHandle did not finish within {timeout:?}");
        }
    }

    /// Zero-sized RAII: declare **before** `ClearOnDrop` so Drop order is
    /// ClearOnDrop (`in_flight=false`) → then this (increment COMPLETIONS).
    /// Fires on normal return **and** panic (no explicit post-body signal).
    pub(super) struct WorkerExitOnDrop;

    impl WorkerExitOnDrop {
        pub(super) fn new() -> Self {
            Self
        }
    }

    impl Drop for WorkerExitOnDrop {
        fn drop(&mut self) {
            signal_round_complete();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::agent::{AgentRegistry, ExternalRegistry};
    use crate::daemon::per_tick::{run_handlers_with_panic_guard, PerTickHandler, TickContext};
    use parking_lot::Mutex as ParkingMutex;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    fn tmp(tag: &str) -> PathBuf {
        static C: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "agend-canon-hb-{tag}-{}-{}",
            std::process::id(),
            C.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn git_init(dir: &Path) {
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git init");
    }

    type Configs = Arc<ParkingMutex<HashMap<String, crate::daemon::AgentConfig>>>;

    fn empty_regs() -> (AgentRegistry, ExternalRegistry, Configs) {
        (
            Arc::new(ParkingMutex::new(HashMap::new())),
            Arc::new(ParkingMutex::new(HashMap::new())),
            Arc::new(ParkingMutex::new(HashMap::new())),
        )
    }

    /// RAII cleanup order (required):
    ///
    /// 1. `release_gate` so any blocked body can proceed
    /// 2. join any test-spawned runner (bounded watchdog)
    /// 3. when a heartbeat worker was tracked: `wait_for_completion(before, 30s)`,
    ///    then verify `!in_flight` (completion is join; in_flight is post-check)
    /// 4. only then `reset()` globals
    ///
    /// Never poll `in_flight` as a join surrogate — ClearOnDrop can clear it
    /// before `WorkerExitOnDrop` signals COMPLETIONS.
    struct HookCleanup {
        runner: Option<std::thread::JoinHandle<()>>,
        hb: Option<Arc<CanonicalHeartbeatHandler>>,
        /// Snapshot of COMPLETIONS before the tracked offload spawn.
        completions_before: Option<u64>,
    }

    impl HookCleanup {
        fn new() -> Self {
            Self {
                runner: None,
                hb: None,
                completions_before: None,
            }
        }
        #[allow(dead_code)] // RED path spawns the tick runner off the test thread
        fn track_runner(&mut self, h: std::thread::JoinHandle<()>) {
            self.runner = Some(h);
        }
        /// Snapshot COMPLETIONS *before* `run()` spawns the worker; retain `hb`
        /// so Drop can verify `!in_flight` after the exit signal.
        fn track_hb(&mut self, hb: Arc<CanonicalHeartbeatHandler>) {
            self.completions_before = Some(test_hooks::completions());
            self.hb = Some(hb);
        }
        /// After an explicit bounded wait_for_completion, Drop becomes a no-op drain.
        fn disarm_after_success(&mut self) {
            self.runner = None;
            self.hb = None;
            self.completions_before = None;
        }
    }

    impl Drop for HookCleanup {
        fn drop(&mut self) {
            // (1) always release first
            test_hooks::release_gate();
            // (2) join test-spawned runner if any
            if let Some(h) = self.runner.take() {
                test_hooks::join_with_watchdog(h, Duration::from_secs(30));
            }
            // (3) wait for WorkerExitOnDrop COMPLETIONS, then verify !in_flight
            if let Some(before) = self.completions_before.take() {
                test_hooks::wait_for_completion(before, Duration::from_secs(30));
                if let Some(hb) = self.hb.take() {
                    assert!(
                        !hb.is_in_flight(),
                        "WorkerExitOnDrop fired but in_flight still true — ClearOnDrop order broken"
                    );
                }
            } else {
                self.hb = None;
            }
            // (4) reset globals only after the worker has fully exited
            test_hooks::reset();
        }
    }

    struct ArcHb(Arc<CanonicalHeartbeatHandler>);
    impl PerTickHandler for ArcHb {
        fn name(&self) -> &'static str {
            self.0.name()
        }
        fn run(&self, ctx: &TickContext<'_>) {
            self.0.run(ctx);
        }
    }

    struct ProbeHandler {
        tx: ParkingMutex<Option<mpsc::Sender<()>>>,
    }

    impl PerTickHandler for ProbeHandler {
        fn name(&self) -> &'static str {
            "probe_after_canonical_heartbeat"
        }
        fn run(&self, _ctx: &TickContext<'_>) {
            if let Some(tx) = self.tx.lock().take() {
                let _ = tx.send(());
            }
        }
    }

    #[test]
    fn healthy_repo_is_not_missing() {
        let d = tmp("healthy");
        git_init(&d);
        assert_eq!(canonical_missing_reason(&d), None);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn deleted_dir_is_path_missing() {
        let d = tmp("deleted");
        git_init(&d);
        std::fs::remove_dir_all(&d).unwrap();
        assert_eq!(canonical_missing_reason(&d), Some("path missing"));
    }

    #[test]
    fn existing_non_git_dir_is_corrupt() {
        let d = tmp("nongit");
        assert_eq!(
            canonical_missing_reason(&d),
            Some("not a git repo (corrupt/removed .git)")
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn flags_missing_repo_even_when_cwd_is_deleted() {
        let healthy = tmp("cwd-healthy");
        git_init(&healthy);
        let gone = tmp("cwd-gone");
        git_init(&gone);
        std::fs::remove_dir_all(&gone).unwrap();

        let prev_cwd = std::env::current_dir().ok();
        let orphan_cwd = tmp("cwd-orphan");
        std::env::set_current_dir(&orphan_cwd).unwrap();
        std::fs::remove_dir_all(&orphan_cwd).unwrap();

        let gone_verdict = canonical_missing_reason(&gone);
        let healthy_verdict = canonical_missing_reason(&healthy);

        if let Some(p) = prev_cwd {
            std::env::set_current_dir(p).ok();
        }
        assert_eq!(gone_verdict, Some("path missing"));
        assert_eq!(healthy_verdict, None);
        std::fs::remove_dir_all(&healthy).ok();
    }

    /// GREEN liveness: body-entered + Probe while gate armed via real runner.
    #[test]
    #[serial_test::serial(canonical_heartbeat_gate)]
    fn tick_runner_progresses_while_body_gated_pr3() {
        test_hooks::reset();
        let mut cleanup = HookCleanup::new();

        let home = tmp("offload-probe");
        let (registry, externals, configs) = empty_regs();
        let hb = Arc::new(CanonicalHeartbeatHandler::new(1));
        cleanup.track_hb(Arc::clone(&hb));
        let (probe_tx, probe_rx) = mpsc::channel();
        let handlers: Vec<Box<dyn PerTickHandler>> = vec![
            Box::new(ArcHb(Arc::clone(&hb))),
            Box::new(ProbeHandler {
                tx: ParkingMutex::new(Some(probe_tx)),
            }),
        ];

        test_hooks::arm_gate();
        let tick_ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };
        // Offload: returns while body gated on bg thread.
        run_handlers_with_panic_guard(&handlers, &tick_ctx);

        // Prove worker reached gated body (not merely in_flight).
        test_hooks::wait_for_body_entered(Duration::from_secs(30));
        assert!(test_hooks::body_entered_count() >= 1);
        assert!(hb.is_in_flight());
        assert!(test_hooks::is_armed());

        match probe_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(()) => {
                assert!(
                    test_hooks::is_armed(),
                    "Probe must progress while body gate still armed"
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("watchdog: Probe did not run while body gated (offload broken?)");
            }
            Err(e) => panic!("{e}"),
        }

        assert!(hb.is_in_flight(), "body still in flight after Probe");
        let before = test_hooks::completions();
        test_hooks::release_gate();
        test_hooks::wait_for_completion(before, Duration::from_secs(30));
        assert!(!hb.is_in_flight());
        cleanup.disarm_after_success();
        drop(cleanup);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[serial_test::serial(canonical_heartbeat_gate)]
    fn skip_when_previous_round_in_flight() {
        test_hooks::reset();
        let mut cleanup = HookCleanup::new();

        let home = tmp("skip-inflight");
        let (registry, externals, configs) = empty_regs();
        let h = Arc::new(CanonicalHeartbeatHandler::new(1));
        cleanup.track_hb(Arc::clone(&h));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        test_hooks::arm_gate();
        h.run(&ctx);
        test_hooks::wait_for_body_entered(Duration::from_secs(30));
        assert!(h.is_in_flight());
        h.run(&ctx);
        assert!(h.is_in_flight());

        let before = test_hooks::completions();
        test_hooks::release_gate();
        test_hooks::wait_for_completion(before, Duration::from_secs(30));
        assert_eq!(
            test_hooks::completions(),
            before + 1,
            "exactly one round completes; re-entrant run must skip"
        );
        assert!(!h.is_in_flight());
        cleanup.disarm_after_success();
        drop(cleanup);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    #[serial_test::serial(canonical_heartbeat_gate)]
    fn panic_in_body_clears_in_flight_guard() {
        test_hooks::reset();
        let mut cleanup = HookCleanup::new();

        let home = tmp("panic-clear");
        let (registry, externals, configs) = empty_regs();
        let h = Arc::new(CanonicalHeartbeatHandler::new(1));
        cleanup.track_hb(Arc::clone(&h));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        test_hooks::arm_panic_once();
        let before = test_hooks::completions();
        h.run(&ctx);
        // WorkerExitOnDrop signals after ClearOnDrop even when the body panics —
        // proves actual thread exit, not just in_flight clear.
        test_hooks::wait_for_completion(before, Duration::from_secs(30));
        assert!(
            !h.is_in_flight(),
            "ClearOnDrop must clear in_flight before WorkerExitOnDrop signals"
        );
        cleanup.disarm_after_success();
        drop(cleanup);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn fires_on_first_tick_then_every_n() {
        let h = CanonicalHeartbeatHandler::new(4);
        let fires: Vec<bool> = (0..9).map(|_| h.gate.fire()).collect();
        assert_eq!(
            fires,
            vec![true, false, false, false, true, false, false, false, true]
        );
    }

    #[test]
    fn name_matches_module() {
        assert_eq!(
            CanonicalHeartbeatHandler::new(60).name(),
            "canonical_heartbeat"
        );
    }
}
