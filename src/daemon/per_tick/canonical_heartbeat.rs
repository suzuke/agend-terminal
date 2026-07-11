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

use super::{PerTickHandler, TickContext};
use parking_lot::Mutex;
use std::collections::HashSet;
use std::path::Path;

/// Per-tick canonical-existence watchdog. Default cadence 60 ticks (~10 min at
/// the 10 s tick) — a 4× improvement over the incident's 40-min silence; the
/// real-time path is protection ①.
pub(crate) struct CanonicalHeartbeatHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
    /// Repos currently in the alerted-missing state (dedup: one page per outage).
    alerted: Mutex<HashSet<String>>,
}

impl CanonicalHeartbeatHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
            alerted: Mutex::new(HashSet::new()),
        }
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
        // #[cfg(test)] only: deterministic body-entry seam for the RED liveness
        // pin. Production builds omit this; production control flow is otherwise
        // byte-identical to pre-PR3. No production in_flight/spawn here (RED).
        #[cfg(test)]
        test_hooks::body_entry_gate();
        let repos = crate::binding::bound_source_repos(ctx.home);
        let registered: HashSet<String> = repos
            .iter()
            .map(|r| r.to_string_lossy().into_owned())
            .collect();
        let mut alerted = self.alerted.lock();
        for repo in &repos {
            let key = repo.to_string_lossy().into_owned();
            let Some(reason) = canonical_missing_reason(repo) else {
                alerted.remove(&key); // healthy → reset latch so a future loss re-pages
                continue;
            };
            if !alerted.insert(key.clone()) {
                continue; // already paged this outage — don't re-page every cadence
            }
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
            crate::event_log::log(ctx.home, "canonical_repo_missing", &key, &msg);
            tracing::error!(
                repo = %repo.display(),
                reason,
                channels = dispatched,
                "canonical_repo_missing: registered source_repo vanished"
            );
        }
        // De-registered repos (no live binding references them) drop out of the
        // latch so it can't grow unbounded.
        alerted.retain(|k| registered.contains(k));
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

/// Test-only seams for the RED liveness pin. No production offload state.
#[cfg(test)]
mod test_hooks {
    use parking_lot::{Condvar, Mutex};
    use std::time::{Duration, Instant};

    static GATE_ARMED: Mutex<bool> = Mutex::new(false);
    static GATE_CV: Condvar = Condvar::new();
    /// Monotone count of times `body_entry_gate` was entered (before wait).
    static BODY_ENTERED: Mutex<u64> = Mutex::new(0);
    static BODY_ENTERED_CV: Condvar = Condvar::new();

    pub(super) fn reset() {
        *GATE_ARMED.lock() = false;
        GATE_CV.notify_all();
        *BODY_ENTERED.lock() = 0;
        BODY_ENTERED_CV.notify_all();
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

    /// Block until `body_entry_gate` has been entered at least once after reset.
    /// `timeout` is a harness watchdog only (not a functional elapsed bound).
    pub(super) fn wait_for_body_entered(timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut n = BODY_ENTERED.lock();
        while *n == 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                drop(n);
                panic!(
                    "watchdog: body_entry_gate never entered within {:?}",
                    timeout
                );
            }
            if BODY_ENTERED_CV.wait_for(&mut n, remaining).timed_out() && *n == 0 {
                drop(n);
                panic!(
                    "watchdog: body_entry_gate never entered within {:?}",
                    timeout
                );
            }
        }
    }

    /// Signal body-entered **before** blocking on the armed gate, so tests can
    /// prove the worker reached the gated body (not just that a flag was set).
    pub(super) fn body_entry_gate() {
        {
            *BODY_ENTERED.lock() += 1;
            BODY_ENTERED_CV.notify_all();
        }
        let mut armed = GATE_ARMED.lock();
        while *armed {
            GATE_CV.wait(&mut armed);
        }
    }

    /// Join a worker with a bounded watchdog; panics if join does not complete.
    pub(super) fn join_with_watchdog(
        handle: std::thread::JoinHandle<()>,
        timeout: Duration,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = handle.join();
            let _ = tx.send(());
        });
        if rx.recv_timeout(timeout).is_err() {
            panic!("watchdog: worker JoinHandle did not finish within {timeout:?}");
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
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration;

    fn tmp(tag: &str) -> std::path::PathBuf {
        static C: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "agend-canon-hb-{tag}-{}-{}",
            std::process::id(),
            C.fetch_add(1, Ordering::Relaxed)
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

    /// RAII: release gate, join any runner worker, then reset globals so a failed
    /// assertion cannot leak a blocked worker into the next serial test.
    struct HookCleanup {
        runner: Option<std::thread::JoinHandle<()>>,
    }

    impl HookCleanup {
        fn new() -> Self {
            Self { runner: None }
        }
        fn track_runner(&mut self, h: std::thread::JoinHandle<()>) {
            self.runner = Some(h);
        }
    }

    impl Drop for HookCleanup {
        fn drop(&mut self) {
            test_hooks::release_gate();
            if let Some(h) = self.runner.take() {
                test_hooks::join_with_watchdog(h, Duration::from_secs(30));
            }
            test_hooks::reset();
        }
    }

    /// Sibling probe: sends one token when its `run` executes.
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

    /// THE SOUL of this fix (#t-…83936-4): the check must resolve by ABSOLUTE PATH
    /// and stay correct even when the process cwd is itself a deleted directory —
    /// exactly the incident, where the daemon's cwd was the orphaned canonical.
    /// A future refactor to a `.`/cwd-relative check would silently reintroduce
    /// the 40-min blind spot; this pins against that.
    ///
    /// `#[cfg(unix)]`: the hazard being pinned is a POSIX semantic — an orphaned
    /// inode keeps answering cwd-relative lookups after its dir is unlinked.
    /// Windows LOCKS the process cwd, so it cannot even be deleted (the fixture's
    /// `remove_dir_all(cwd)` errors) and the blind spot cannot occur there. The
    /// production check is cross-platform; only this simulation is Unix-only.
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
        assert_eq!(
            gone_verdict,
            Some("path missing"),
            "a deleted absolute repo must be flagged even from a deleted cwd"
        );
        assert_eq!(
            healthy_verdict, None,
            "a healthy absolute repo must resolve even from a deleted cwd"
        );
        std::fs::remove_dir_all(&healthy).ok();
    }

    /// RED liveness pin (PR3): through real `run_handlers_with_panic_guard`.
    ///
    /// Sequence:
    /// 1. Arm body gate.
    /// 2. Run handlers on a worker thread (so the test thread can observe).
    /// 3. Wait for **body-entered** signal (proves worker reached gated body).
    /// 4. Assert Probe sibling progressed **while gate still armed**.
    ///
    /// Pre-offload (this commit): body enters and blocks → body-entered fires,
    /// but Probe never runs → step 4 fails (RED).
    /// Post-offload: body enters on bg thread, `run()` returns, Probe runs while
    /// gate armed (GREEN).
    ///
    /// Production struct/run unchanged except `#[cfg(test)] body_entry_gate`.
    /// No `in_flight` / spawn / latch restructure in this RED commit.
    #[test]
    #[serial_test::serial(canonical_heartbeat_gate)]
    fn tick_runner_progresses_while_body_gated_pr3() {
        let mut cleanup = HookCleanup::new();
        test_hooks::reset();

        let home = tmp("offload-probe");
        let (registry, externals, configs) = empty_regs();
        let (probe_tx, probe_rx) = mpsc::channel();
        let handlers: Vec<Box<dyn PerTickHandler>> = vec![
            Box::new(CanonicalHeartbeatHandler::new(1)),
            Box::new(ProbeHandler {
                tx: ParkingMutex::new(Some(probe_tx)),
            }),
        ];

        test_hooks::arm_gate();
        let home2 = home.clone();
        let reg = Arc::clone(&registry);
        let ext = Arc::clone(&externals);
        let cfg = Arc::clone(&configs);
        let join = std::thread::spawn(move || {
            let tick_ctx = TickContext {
                home: &home2,
                registry: &reg,
                externals: &ext,
                configs: &cfg,
            };
            run_handlers_with_panic_guard(&handlers, &tick_ctx);
        });
        cleanup.track_runner(join);

        // Prove the worker reached the gated body (not just that a flag exists).
        test_hooks::wait_for_body_entered(Duration::from_secs(30));
        assert!(
            test_hooks::body_entered_count() >= 1,
            "body_entry_gate must have been entered"
        );
        assert!(
            test_hooks::is_armed(),
            "gate must still be armed after body entered"
        );

        // GREEN contract: Probe fires while body gate is still armed.
        // RED (sync): Timeout → fail. recv_timeout is watchdog-only.
        match probe_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(()) => {
                assert!(
                    test_hooks::is_armed(),
                    "Probe ran but body gate already released"
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Drop cleanup joins/releases; then fail the RED pin.
                drop(cleanup);
                std::fs::remove_dir_all(&home).ok();
                panic!(
                    "RED: body entered and blocked on sync gate — Probe did not run \
                     while gated (pre-offload liveness failure)"
                );
            }
            Err(e) => panic!("{e}"),
        }
        // GREEN path: release and join via Drop
        drop(cleanup);
        std::fs::remove_dir_all(&home).ok();
    }
}
