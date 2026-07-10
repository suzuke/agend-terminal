//! #2550 W5 PR-3: worktree-registry auto-cleanup — drops local branches
//! whose PRs have merged into main, via the runtime config registry (not the
//! GC-candidate marker walk `worktree_pool::gc.rs` uses). Extracted verbatim
//! from `inbox_maintenance.rs` (was one of its sub-ops, gated on the SAME
//! every-60-tick cadence) — this logic has nothing to do with inbox
//! maintenance and is semantically GC, but per decision Q4
//! (d-20260704035059093740-0) it keeps its OWN independent 60-tick
//! `PerTickHandler` registration rather than folding into `HourlyGcHandler`'s
//! 360-tick cadence: doing so would regress this cleanup's latency from
//! ~10min to ~1h, a real-world-visible regression the operator did not
//! approve.

use super::{PerTickHandler, TickContext};
use crate::api::ConfigRegistry;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// #P1-2607 incident: `worktree_auto_cleanup` (fetch --prune + a squash-merge
/// check per candidate branch) ran INLINE on the daemon's main tick loop —
/// its first production run against the canonical repo took 83s wall time
/// (172 accumulated candidates, five months of #2605's dead repo-discovery
/// finally fixed) and froze the ENTIRE daemon (TUI, inbox, every other
/// handler) for that whole window. Because dry-run mode never consumes
/// candidates, this repeated every ~10 minutes indefinitely.
///
/// Fix: the real work now runs in its own background thread; `run()` itself
/// only checks the cadence gate, checks/sets the re-entrancy guard, and
/// spawns — it never blocks the tick loop. `in_flight` prevents a second
/// round from stacking on top of one still running (which would compound the
/// cost, not fix it) — if the previous round hasn't finished by the next
/// scheduled fire, that fire is skipped and logged; the round after tries
/// again. See `worktree_cleanup::is_squash_gc_eligible`'s tip-SHA cache for
/// the complementary fix (bounds each round's cost to just the NEW/moved
/// candidates instead of re-deriving the whole accumulated set every time).
pub(crate) struct WorktreeRegistrySweepHandler {
    gate: crate::daemon::cadence_gate::CadenceGate,
    in_flight: Arc<AtomicBool>,
    /// #P4 (branch-residue): episode-dedup for the >15 non-default-branch
    /// residue alarm — flips false→true on the first over-threshold sweep so
    /// the warn+operator-notify fires ONCE, resets when the count drops back to
    /// ≤15. Process-lifetime state on this long-lived singleton handler (mirrors
    /// `in_flight`); mutated through the Arc's interior mutability from the
    /// spawned round.
    count_warned: Arc<AtomicBool>,
}

impl WorktreeRegistrySweepHandler {
    pub(crate) fn new(every_n_ticks: u64) -> Self {
        Self {
            gate: crate::daemon::cadence_gate::CadenceGate::new(every_n_ticks),
            in_flight: Arc::new(AtomicBool::new(false)),
            count_warned: Arc::new(AtomicBool::new(false)),
        }
    }

    #[cfg(test)]
    fn is_in_flight(&self) -> bool {
        self.in_flight.load(Ordering::Acquire)
    }
}

impl PerTickHandler for WorktreeRegistrySweepHandler {
    fn name(&self) -> &'static str {
        "worktree_registry_sweep"
    }

    fn run(&self, ctx: &TickContext<'_>) {
        if !self.gate.fire() {
            return;
        }
        // #P1-2607: a previous round is still running in its background
        // thread — skip this tick's fire instead of stacking a second sweep
        // on top of it.
        if self.in_flight.swap(true, Ordering::AcqRel) {
            tracing::warn!(
                "worktree_registry_sweep: previous round still in flight, \
                 skipping this tick's fire (will retry next cadence)"
            );
            return;
        }
        let home = ctx.home.to_path_buf();
        let configs = Arc::clone(ctx.configs);
        let in_flight = Arc::clone(&self.in_flight);
        let count_warned = Arc::clone(&self.count_warned);
        // fire-and-forget: #P1-2607 — moves the potentially-slow sweep
        // (git subprocess per candidate branch) off the daemon's main tick
        // loop. No JoinHandle is kept; completion is signaled via `in_flight`,
        // released by `ClearOnDrop` on the sweep's return OR an unwinding panic
        // (so a panicking round can't wedge the guard true forever). Results
        // remain fully observable via tracing + event_log per candidate, same
        // as before this offload.
        std::thread::spawn(move || {
            // The guard is SCOPED so `in_flight` is cleared (its `Drop`) BEFORE
            // the test-only completion signal below — a test waking on that
            // signal then observes `!is_in_flight()` deterministically, with no
            // wall-clock poll. In non-test builds both `#[cfg(test)]` lines
            // vanish and the extra scope is a no-op (the guard still drops at
            // closure end), so production behaviour is byte-identical.
            {
                let _guard = super::ClearOnDrop::new(in_flight);
                #[cfg(test)]
                test_hooks::round_gate();
                worktree_auto_cleanup(&home, &configs, &count_warned);
            }
            #[cfg(test)]
            test_hooks::signal_round_complete();
        });
    }
}

/// #P1-2607 offload-determinism seams (t-20260706052959557862-96214-2, #2661
/// follow-up): a `GATE` the spawned round blocks on (so a test can hold a round
/// provably in-flight) and a monotone `COMPLETIONS` counter bumped AFTER the
/// round's `ClearOnDrop` clears `in_flight`. These replace the old wall-clock
/// `DELAY_MS` sleep, whose `elapsed() < 100ms` / 2s-poll assertions are the
/// same flake pattern #2661 fixed for `hourly_gc`. Process-global (the round
/// observes them from its background thread); the two tests that use them are
/// `serial(worktree_registry_sweep_gate)`, so they never race each other. No-op
/// in production — every call site is `#[cfg(test)]`.
#[cfg(test)]
mod test_hooks {
    use parking_lot::{Condvar, Mutex};

    /// `true` while the gate is armed — a spawned round blocks in `round_gate`.
    static GATE_ARMED: Mutex<bool> = Mutex::new(false);
    static GATE_CV: Condvar = Condvar::new();
    /// Monotone count of rounds that have finished (and cleared `in_flight`).
    static COMPLETIONS: Mutex<u64> = Mutex::new(0);
    static COMPLETIONS_CV: Condvar = Condvar::new();

    /// Reset both seams to idle. Called at the start of each test that uses
    /// them so a prior test — or an early return/panic before `release_gate` —
    /// can't leave the gate armed or the counter dirty.
    pub(super) fn reset() {
        *GATE_ARMED.lock() = false;
        GATE_CV.notify_all();
        *COMPLETIONS.lock() = 0;
    }

    /// Arm the gate so the NEXT spawned round blocks in `round_gate` until
    /// `release_gate`, letting a test hold a round provably in-flight.
    pub(super) fn arm_gate() {
        *GATE_ARMED.lock() = true;
    }

    /// Release a gated round (and any future ones) so it runs to completion.
    pub(super) fn release_gate() {
        *GATE_ARMED.lock() = false;
        GATE_CV.notify_all();
    }

    /// Called by a spawned round (replaces the old `maybe_delay`): blocks while
    /// the gate is armed, else falls straight through.
    pub(super) fn round_gate() {
        let mut armed = GATE_ARMED.lock();
        while *armed {
            GATE_CV.wait(&mut armed);
        }
    }

    /// Bumped by a spawned round AFTER its `ClearOnDrop` cleared `in_flight`.
    pub(super) fn signal_round_complete() {
        *COMPLETIONS.lock() += 1;
        COMPLETIONS_CV.notify_all();
    }

    /// Snapshot of the completed-round count (baseline before an action).
    pub(super) fn completions() -> u64 {
        *COMPLETIONS.lock()
    }

    /// Block until the completed-round count exceeds `prev`. Deterministic — no
    /// wall-clock ceiling; a genuine hang is caught by nextest's slow-timeout.
    pub(super) fn wait_for_completion(prev: u64) {
        let mut n = COMPLETIONS.lock();
        while *n <= prev {
            COMPLETIONS_CV.wait(&mut n);
        }
    }
}

/// Worktree auto-cleanup (runtime registry based): drop branches whose
/// PRs have merged into main. Logged via `event_log` + tracing on every
/// removal so operators can audit. Verbatim from `inbox_maintenance.rs`
/// (was verbatim from the pre-extraction block at mod.rs:678-717).
///
/// #2605: repo discovery moved to live `binding.json` state
/// (`sweep_from_registry` reads it via `home`) instead of the removed
/// `AgentConfig.worktree_source` cache. PR-D6: real deletion is gated by
/// `AGEND_WORKTREE_AUTO_CLEANUP` ONLY — when the sweep returns candidates they
/// were actually removed (the retired `AGEND_WORKTREE_PRUNE_LIVE` dry-run toggle
/// is gone), so every returned entry is logged as a live removal.
fn worktree_auto_cleanup(home: &Path, configs: &ConfigRegistry, count_warned: &AtomicBool) {
    let cfgs = configs.lock();
    let config_data: std::collections::HashMap<String, Option<std::path::PathBuf>> = cfgs
        .iter()
        .map(|(name, cfg)| (name.clone(), cfg.working_dir.clone()))
        .collect();
    drop(cfgs);
    let fleet_dirs: Vec<std::path::PathBuf> =
        crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
            .ok()
            .map(|c| {
                c.instance_names()
                    .iter()
                    .filter_map(|n| c.resolve_instance(n).and_then(|r| r.working_directory))
                    .collect()
            })
            .unwrap_or_default();
    let cleaned = crate::worktree_cleanup::sweep_from_registry(home, &config_data, &fleet_dirs);
    // PR-D6: sweep is always live (gated by AUTO_CLEANUP only) — every returned
    // entry was actually removed.
    for (branch, path, reason) in &cleaned {
        let detail = format!("path={path}, reason={reason}");
        crate::event_log::log(home, "worktree_auto_removed", branch, &detail);
        tracing::info!(branch, path, reason, "worktree auto-removed");
    }

    // #P4 (branch-residue): end-of-sweep residue alarm. If any bound repo has
    // accumulated more than 15 non-default local branches, worktree/branch GC is
    // not keeping up (GC stalled, or AUTO_CLEANUP disabled). Warn + notify the
    // operator ONCE per episode (de-duped via `count_warned`, reset when back
    // under). The count is observational, independent of whether the sweep ran.
    let max_count = max_nondefault_branch_count(home);
    if branch_count_alert(max_count, count_warned) {
        tracing::warn!(
            count = max_count,
            "non-default local branch count exceeds 15 — residue may be accumulating \
             (is AGEND_WORKTREE_AUTO_CLEANUP=0 / GC stalled?)"
        );
        let text = format!(
            "Local non-default branch count is {max_count} (>15) in a bound repo; \
             worktree/branch GC is not keeping up. Investigate residue."
        );
        crate::inbox::notify_agent(
            home,
            "general",
            &crate::inbox::NotifySource::System("branch-residue"),
            &text,
        );
    }
}

/// #P4: max non-default local branch count across all repos bound under `home`
/// (`git branch --format=%(refname:short)`, filtering out each repo's default
/// branch). Fail-open on a git error — a transient enumeration failure counts as
/// 0 for that repo rather than raising a false residue alarm.
fn max_nondefault_branch_count(home: &Path) -> usize {
    crate::binding::bound_source_repos(home)
        .iter()
        .map(|repo| {
            let default = crate::git_helpers::default_branch(repo);
            match crate::git_helpers::git_cmd(repo, &["branch", "--format=%(refname:short)"]) {
                Ok(stdout) => stdout.lines().filter(|b| *b != default.as_str()).count(),
                Err(_) => 0,
            }
        })
        .max()
        .unwrap_or(0)
}

/// #P4: pure episode-dedup decision for the residue alarm. Returns `true` iff
/// the caller should fire the warn+operator-notify NOW: the count exceeds the
/// threshold AND we haven't already warned this episode (`already_warned` flips
/// false→true on the first fire, mirroring the `in_flight` swap idiom). Once the
/// count drops back to ≤threshold the flag resets, so a later re-accumulation
/// re-fires exactly once. Extracted so the fire/skip/reset transitions are
/// unit-testable without asserting on tracing/inbox side effects (the RED8 seam).
fn branch_count_alert(max_count: usize, already_warned: &AtomicBool) -> bool {
    const BRANCH_COUNT_WARN_THRESHOLD: usize = 15;
    if max_count > BRANCH_COUNT_WARN_THRESHOLD {
        !already_warned.swap(true, Ordering::AcqRel)
    } else {
        already_warned.store(false, Ordering::Release);
        false
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Cadence predicate test (same pattern as InboxMaintenanceHandler's,
    /// which this handler's cadence was extracted out of unchanged).
    #[test]
    fn fires_on_first_tick_then_every_n() {
        let h = WorktreeRegistrySweepHandler::new(4);
        let fires: Vec<bool> = (0..9).map(|_| h.gate.fire()).collect();
        assert_eq!(
            fires,
            vec![true, false, false, false, true, false, false, false, true]
        );
    }

    /// Smoke, made deterministic (t-20260706052959557862-96214-2): `run()`
    /// against an empty registry + temp home spawns a round that finishes and
    /// clears `in_flight`, with no panic. Waits on the completion signal
    /// (bumped after the guard clears the flag) instead of the old 50ms sleep,
    /// which was a latent teardown race (the sweep reads `home` while
    /// `remove_dir_all` tears it down). A single `run()` keeps the completed
    /// count unambiguous — re-entrancy is pinned deterministically by the
    /// freeze test below.
    #[test]
    #[serial_test::serial(worktree_registry_sweep_gate)]
    fn run_is_no_op_on_empty_fixtures() {
        use crate::agent::{AgentRegistry, ExternalRegistry};
        use parking_lot::Mutex;
        use std::collections::HashMap;
        use std::sync::Arc;

        test_hooks::reset();
        let tag = std::process::id();
        let home = std::env::temp_dir().join(format!("agend-worktree-reg-sweep-handler-{tag}"));
        std::fs::create_dir_all(&home).unwrap();

        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        // N=1 forces every call to fire.
        let h = WorktreeRegistrySweepHandler::new(1);
        let before = test_hooks::completions();
        h.run(&ctx);
        test_hooks::wait_for_completion(before);
        assert!(
            !h.is_in_flight(),
            "background round should finish on empty fixtures"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #P1-2607 freeze-regression pin, made DETERMINISTIC
    /// (t-20260706052959557862-96214-2, #2661 follow-up): `run()` must offload
    /// the sweep to a background thread and return without waiting for it. The
    /// old version proved this with wall-clock `elapsed() < 100ms` assertions —
    /// the same pattern that flaked for `hourly_gc` under full-parallel-nextest
    /// CPU contention (#2661). Here a test GATE holds the spawned round provably
    /// in-flight, so the property is proven STRUCTURALLY at any machine speed:
    /// had `run()` done the sweep inline it would block on the armed gate and
    /// never return (a hang nextest's slow-timeout attributes), so reaching the
    /// post-`run()` assertions — with the round still `in_flight` — is itself
    /// the proof of offload. Re-entrancy is pinned deterministically: exactly
    /// ONE round completes even though `run()` fired twice while the first was
    /// in flight. Serial: the gate/counter seams are process-global.
    #[test]
    #[serial_test::serial(worktree_registry_sweep_gate)]
    fn run_does_not_block_tick_loop_during_slow_sweep_p1_2607() {
        use crate::agent::{AgentRegistry, ExternalRegistry};
        use parking_lot::Mutex;
        use std::collections::HashMap;
        use std::sync::Arc;

        test_hooks::reset();
        let tag = std::process::id();
        let home = std::env::temp_dir().join(format!("agend-worktree-reg-sweep-slow-{tag}"));
        std::fs::create_dir_all(&home).unwrap();

        let registry: AgentRegistry = Arc::new(Mutex::new(HashMap::new()));
        let externals: ExternalRegistry = Arc::new(Mutex::new(HashMap::new()));
        let configs = Arc::new(Mutex::new(HashMap::new()));
        let ctx = TickContext {
            home: &home,
            registry: &registry,
            externals: &externals,
            configs: &configs,
        };

        // Arm the gate so the spawned round blocks mid-flight until we release.
        test_hooks::arm_gate();
        let h = WorktreeRegistrySweepHandler::new(1); // fires on every call

        // If `run()` ran the sweep inline it would block on the armed gate and
        // never return — reaching the next line proves it offloaded.
        h.run(&ctx);
        assert!(
            h.is_in_flight(),
            "the offloaded round must still be in flight (blocked on the gate)"
        );

        // A second tick while the first round is still in flight: the
        // re-entrancy guard must skip spawning a second overlapping round — and
        // this call must also return (not block on the gate).
        h.run(&ctx);
        assert!(
            h.is_in_flight(),
            "re-entrant tick must neither block nor clear the in-flight round"
        );

        // Release the round and wait — deterministically — for it to finish.
        let before = test_hooks::completions();
        test_hooks::release_gate();
        test_hooks::wait_for_completion(before);

        assert_eq!(
            test_hooks::completions(),
            before + 1,
            "exactly ONE round may complete — the re-entrant second run() must \
             have skipped, not spawned an overlapping round"
        );
        assert!(
            !h.is_in_flight(),
            "in_flight must clear once the background sweep completes"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// #P4 (branch-residue) RED8 — pure episode-dedup transitions: >15 fires
    /// once, a second >15 sweep in the same episode is suppressed, and dropping
    /// back to ≤15 resets the flag so a later re-accumulation re-fires. Pins the
    /// dedup contract without asserting on tracing/inbox side effects.
    #[test]
    fn branch_count_alert_fires_once_per_episode_red8() {
        let flag = AtomicBool::new(false);
        assert!(
            branch_count_alert(16, &flag),
            "first over-threshold sweep must fire"
        );
        assert!(
            !branch_count_alert(16, &flag),
            "a second >15 sweep in the same episode must NOT re-fire (deduped)"
        );
        assert!(
            !branch_count_alert(15, &flag),
            "exactly 15 is NOT over the >15 threshold and resets the episode"
        );
        assert!(
            !branch_count_alert(10, &flag),
            "further under-threshold sweeps stay quiet"
        );
        assert!(
            branch_count_alert(20, &flag),
            "after resetting under threshold, a fresh >15 sweep fires again"
        );
    }

    /// #P4 RED8 — end-to-end count wiring: a bound source repo with 16
    /// non-default branches makes `max_nondefault_branch_count` report >15, so
    /// the alarm predicate fires on the first sweep. Real git fixture + a seeded
    /// `binding.json` so `bound_source_repos` discovers the repo.
    #[test]
    fn sixteen_branches_trip_the_residue_alarm_red8() {
        fn git(dir: &Path, args: &[&str]) {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("AGEND_GIT_BYPASS", "1")
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("git");
        }
        let tag = format!("{}-p4-red8", std::process::id());
        let home = std::env::temp_dir().join(format!("agend-p4-home-{tag}"));
        let repo = std::env::temp_dir().join(format!("agend-p4-repo-{tag}"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        std::fs::write(repo.join("README.md"), "init").ok();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "init"]);
        // 16 non-default branches → strictly greater than the 15 threshold.
        for i in 0..16 {
            git(&repo, &["branch", &format!("feat/b{i}")]);
        }
        // Seed binding so bound_source_repos(home) discovers this repo.
        let bdir = crate::paths::runtime_dir(&home).join("agent-x");
        std::fs::create_dir_all(&bdir).unwrap();
        std::fs::write(
            bdir.join("binding.json"),
            serde_json::json!({ "source_repo": repo.display().to_string() }).to_string(),
        )
        .unwrap();

        let n = max_nondefault_branch_count(&home);
        assert!(
            n > 15,
            "16 non-default branches must count as >15 (got {n})"
        );
        let flag = AtomicBool::new(false);
        assert!(
            branch_count_alert(n, &flag),
            "the residue alarm must fire on the first sweep for a >15 repo"
        );
        assert!(
            !branch_count_alert(n, &flag),
            "and must not re-fire on the immediately-following sweep"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }
}
