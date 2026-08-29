//! Inline `worktree_cleanup` test module, re-homed verbatim from
//! `src/worktree_cleanup.rs` so that production file stays under the
//! `src_file_size_invariant` anti-monolith ceiling (it sat at 2499 of 2500).
//!
//! Declared as `#[cfg(test)] mod tests;` at its ORIGINAL position in
//! `worktree_cleanup.rs`, so the module keeps the name `tests`, every test
//! retains its `worktree_cleanup::tests::*` path, and the first `#[cfg(test)]`
//! line — the production/test cutoff used by
//! `tests/daemon_git_helper_invariant.rs` — does not move.

use super::*;
use parking_lot::Mutex;

pub(super) static ENV_LOCK: Mutex<()> = Mutex::new(());

fn setup_test_repo(tag: &str) -> PathBuf {
    setup_test_repo_with_default(tag, "main")
}

pub(super) fn setup_test_repo_with_default(tag: &str, default_branch: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agend-wt-v2-{}-{}-{}",
        tag,
        std::process::id(),
        C.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).ok();
    git_in(&dir, &["init", "-b", default_branch]);
    std::fs::write(dir.join("README.md"), "init").ok();
    git_in(&dir, &["add", "."]);
    git_in(&dir, &["commit", "-m", "init"]);
    dir
}

pub(super) fn git_in(dir: &Path, args: &[&str]) {
    Command::new("git")
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

/// #t-…81457-1: build a branch with a single OLD dated commit (checked out
/// then back to main), so `is_branch_merged`'s age gate treats it as
/// genuinely merged rather than a suspiciously-fresh zero-commit branch.
/// Mirrors `make_squash_orphan`'s dating approach but for a plain
/// fast-forward-mergeable branch (no divergence from main).
fn make_old_dated_branch(repo: &Path, branch: &str, tip_date: &str) {
    git_in(repo, &["checkout", "-b", branch]);
    std::fs::write(repo.join("feat.txt"), "feature").ok();
    git_in(repo, &["add", "."]);
    git_commit_dated(repo, "feature work", tip_date);
    git_in(repo, &["checkout", "main"]);
}

/// #2605: fake daemon `home` for `bound_source_repos` — repo discovery now
/// reads live `binding.json` state instead of the old configs-tuple field.
fn tmp_home(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "agend-wt-v2-home-{}-{}-{}",
        tag,
        std::process::id(),
        C.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// Seed `home/runtime/<agent>/binding.json` so `binding::bound_source_repos`
/// reports `source_repo` as a live-bound repo.
fn write_source_repo_binding(home: &Path, agent: &str, source_repo: &Path) {
    let dir = crate::paths::runtime_dir(home).join(agent);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("binding.json"),
        serde_json::to_string(&serde_json::json!({
            "source_repo": source_repo.display().to_string()
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn test_flag_disabled_default() {
    let _lock = ENV_LOCK.lock();
    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    assert!(auto_cleanup_enabled());
}

#[test]
fn test_flag_disabled_explicit() {
    let _lock = ENV_LOCK.lock();
    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "0");
    assert!(!auto_cleanup_enabled());
    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
}

#[test]
fn test_flag_enabled() {
    let _lock = ENV_LOCK.lock();
    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
    assert!(auto_cleanup_enabled());
    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
}

// ── PR-D6: AGEND_WORKTREE_PRUNE_LIVE retired ──
// The old #2695 prune_live-gate tests (`prune_live_enabled_by_default_p2`,
// `prune_live_disabled_only_for_explicit_0_p2`, `prune_live_enabled_when_set_to_1`)
// asserted the now-removed dry-run/live selector. They are RE-TARGETED to the
// retirement contract below: the retired var is detected + warned at boot and
// otherwise ignored; sweep gating is `AGEND_WORKTREE_AUTO_CLEANUP` only.

/// PR-D6 (a): the retired var, when set to ANY value, is detected so the
/// boot path warns (fail-loud, not silent) and returns `true`.
/// F3: `traced_test` asserts the ACTUAL `tracing::warn!` fired via
/// `logs_contain` — a return-value-only assert stayed green even with the
/// warn deleted (the exact bug this fix closes).
#[tracing_test::traced_test]
#[test]
fn prune_live_retired_boot_warn_fires_when_set_d6() {
    let _lock = ENV_LOCK.lock();
    std::env::set_var("AGEND_WORKTREE_PRUNE_LIVE", "1");
    assert!(
        warn_if_prune_live_retired(),
        "retired PRUNE_LIVE set ⇒ boot check must fire the warn (return true)"
    );
    std::env::set_var("AGEND_WORKTREE_PRUNE_LIVE", "0");
    assert!(
        warn_if_prune_live_retired(),
        "even PRUNE_LIVE=0 must be detected + warned — it is honored no longer"
    );
    std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
    assert!(
        logs_contain("is retired and ignored"),
        "the retired-flag boot warn must actually be emitted (not just a true return)"
    );
}

/// PR-D6 (a): unset ⇒ no warn (nothing to fail loud about).
/// F3: negative-direction — `traced_test` + `!logs_contain` proves the warn
/// stays SILENT when the flag is unset.
#[tracing_test::traced_test]
#[test]
fn prune_live_retired_no_warn_when_unset_d6() {
    let _lock = ENV_LOCK.lock();
    std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
    assert!(
        !warn_if_prune_live_retired(),
        "PRUNE_LIVE unset ⇒ boot check must be silent (return false)"
    );
    assert!(
        !logs_contain("is retired and ignored"),
        "no retired-flag warn may be emitted when the flag is unset"
    );
}

// ── PR-B rider (dev2 #2695 seat2): occupancy fail-CLOSED ──

/// A `git worktree list` failure must surface as `Err` (fail-closed), NOT
/// collapse to an empty Vec — the former `unwrap_or_default()` fail-OPEN let
/// a caller treat "occupancy unknown" as "nothing occupied" and reap a branch
/// whose worktree is merely un-enumerable. A non-git-repo path makes the
/// enumeration fail; this is the fail-open path dev2 flagged as uncovered.
#[test]
fn list_worktrees_fails_closed_on_git_error() {
    let non_repo = std::env::temp_dir().join(format!(
        "agend-not-a-repo-{}-{}",
        std::process::id(),
        "riderfailclosed"
    ));
    let _ = std::fs::remove_dir_all(&non_repo);
    std::fs::create_dir_all(&non_repo).unwrap();
    assert!(
        list_worktrees(&non_repo).is_err(),
        "list_worktrees must fail-CLOSED (Err) when git worktree enumeration fails, \
             not collapse to an empty Vec"
    );
    std::fs::remove_dir_all(&non_repo).ok();
}

/// Happy path: a valid repo (no extra worktrees) enumerates to `Ok` (empty
/// after excluding the main worktree) — the fail-closed change must not break
/// normal enumeration.
#[test]
fn list_worktrees_ok_on_valid_repo() {
    let repo = setup_test_repo("rider-lw-ok");
    assert!(
        list_worktrees(&repo).is_ok(),
        "a valid repo must enumerate worktrees as Ok"
    );
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn test_sweep_noop_when_flag_disabled() {
    let _lock = ENV_LOCK.lock();
    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "0");
    let home = tmp_home("noop-disabled");
    let configs = HashMap::new();
    let removed = sweep_from_registry(&home, &configs, &[]);
    assert!(removed.is_empty());
    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    std::fs::remove_dir_all(&home).ok();
}

/// PR-D6 re-target of `test_sweep_dry_run_by_default_identifies_but_does_not_delete`
/// (a #2695 prune_live-gate test). Old contract: `PRUNE_LIVE=0` forced dry-run,
/// so the merged worktree was REPORTED but NOT removed. New contract: the
/// retired var is IGNORED — with `AUTO_CLEANUP=1` the sweep runs LIVE and the
/// worktree IS removed, PROVING `PRUNE_LIVE` no longer gates anything (covers
/// new groups (a)-ignored + (b)-on-live in one live-removal assertion).
#[test]
fn sweep_ignores_retired_prune_live_and_runs_live_d6() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("v2-d6-live");
    // #t-…81457-1: `is_branch_merged` now age-gates on the shared tip's
    // commit date (indistinguishable, from git state alone, between a
    // zero-commit branch and a genuinely ff-merged one) — give feat/done
    // an OLD dated commit so it clears the gate like a real merged branch.
    make_old_dated_branch(&repo, "feat/done", "2024-01-01T00:00:00 +0000");
    let wt = repo.join("wt-done");
    git_in(
        &repo,
        &["worktree", "add", wt.to_str().unwrap(), "feat/done"],
    );
    git_in(&repo, &["merge", "feat/done"]);

    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
    // Retired var set to its old "dry-run" value — must have NO effect now.
    std::env::set_var("AGEND_WORKTREE_PRUNE_LIVE", "0");
    let home = tmp_home("v2-d6-live");
    let mut configs = HashMap::new();
    configs.insert("other-agent".to_string(), Some(repo.join("other")));
    write_source_repo_binding(&home, "other-agent", &repo);
    let removed = sweep_from_registry(&home, &configs, &[]);
    assert!(
        removed.iter().any(|(b, _, _)| b == "feat/done"),
        "the merged worktree must still be reported: {removed:?}"
    );
    assert!(
        !wt.exists(),
        "PR-D6: PRUNE_LIVE=0 is retired/ignored — AUTO_CLEANUP=1 ⇒ LIVE sweep must \
             actually remove the worktree"
    );
    std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// PR-D6 (b), OFF half: master switch `AGEND_WORKTREE_AUTO_CLEANUP=0` ⇒ NO
/// sweep, even for a genuinely-merged reclaimable worktree. Pairs with
/// `sweep_ignores_retired_prune_live_and_runs_live_d6` (the ON/live half) to
/// pin the full collapsed contract: gating is AUTO_CLEANUP only.
#[test]
fn sweep_off_when_auto_cleanup_disabled_d6() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("v2-d6-off");
    make_old_dated_branch(&repo, "feat/done", "2024-01-01T00:00:00 +0000");
    let wt = repo.join("wt-done");
    git_in(
        &repo,
        &["worktree", "add", wt.to_str().unwrap(), "feat/done"],
    );
    git_in(&repo, &["merge", "feat/done"]);

    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "0");
    let home = tmp_home("v2-d6-off");
    let mut configs = HashMap::new();
    configs.insert("other-agent".to_string(), Some(repo.join("other")));
    write_source_repo_binding(&home, "other-agent", &repo);
    let removed = sweep_from_registry(&home, &configs, &[]);
    assert!(
        removed.is_empty(),
        "AUTO_CLEANUP=0 ⇒ no sweep, nothing reported: {removed:?}"
    );
    assert!(
        wt.exists(),
        "AUTO_CLEANUP=0 ⇒ the merged worktree must survive untouched"
    );
    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_v2_merged_worktree_removed() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("v2-merged");
    // #t-…81457-1: see the "v2-dry-run" test above for why this needs an
    // old dated commit now.
    make_old_dated_branch(&repo, "feat/done", "2024-01-01T00:00:00 +0000");
    let wt = repo.join("wt-done");
    git_in(
        &repo,
        &["worktree", "add", wt.to_str().unwrap(), "feat/done"],
    );
    git_in(&repo, &["merge", "feat/done"]);

    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
    std::env::set_var("AGEND_WORKTREE_PRUNE_LIVE", "1"); // exercise real deletion
    let home = tmp_home("v2-merged");
    // No active agent using this worktree
    let mut configs = HashMap::new();
    configs.insert("other-agent".to_string(), Some(repo.join("other")));
    write_source_repo_binding(&home, "other-agent", &repo);
    let removed = sweep_from_registry(&home, &configs, &[]);
    assert!(
        removed.iter().any(|(b, _, _)| b == "feat/done"),
        "merged worktree must be removed: {removed:?}"
    );
    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_v2_dirty_worktree_preserved() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("v2-dirty");
    git_in(&repo, &["branch", "feat/dirty"]);
    let wt = repo.join("wt-dirty");
    git_in(
        &repo,
        &["worktree", "add", wt.to_str().unwrap(), "feat/dirty"],
    );
    git_in(&repo, &["merge", "feat/dirty"]);
    std::fs::write(wt.join("uncommitted.txt"), "dirty").ok();

    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
    std::env::set_var("AGEND_WORKTREE_PRUNE_LIVE", "1");
    let home = tmp_home("v2-dirty");
    let mut configs = HashMap::new();
    configs.insert("agent".to_string(), Some(repo.join("other")));
    write_source_repo_binding(&home, "agent", &repo);
    let removed = sweep_from_registry(&home, &configs, &[]);
    assert!(removed.is_empty(), "dirty worktree must NOT be removed");
    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn test_v2_unmerged_worktree_preserved() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("v2-unmerged");
    git_in(&repo, &["branch", "feat/wip"]);
    let wt = repo.join("wt-wip");
    git_in(
        &repo,
        &["worktree", "add", wt.to_str().unwrap(), "feat/wip"],
    );
    std::fs::write(wt.join("new.txt"), "x").ok();
    git_in(&wt, &["add", "."]);
    git_in(&wt, &["commit", "-m", "wip"]);

    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
    std::env::set_var("AGEND_WORKTREE_PRUNE_LIVE", "1");
    let home = tmp_home("v2-unmerged");
    let mut configs = HashMap::new();
    configs.insert("agent".to_string(), Some(repo.join("other")));
    write_source_repo_binding(&home, "agent", &repo);
    let removed = sweep_from_registry(&home, &configs, &[]);
    assert!(removed.is_empty(), "unmerged worktree must NOT be removed");
    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

#[test]
#[cfg(unix)] // Windows path format — t-20260424173948421544-1
fn test_v2_active_runtime_worktree_not_removed_under_bootstrap_redirect() {
    // Production shape: agent's working_dir is <repo>/.worktrees/<agent>,
    // and the repo is discovered via a live binding.json (#2605). Sweep
    // must NOT remove the active worktree.
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("v2-active");
    git_in(&repo, &["branch", "feat/active"]);
    let wt = repo.join("wt-active");
    git_in(
        &repo,
        &["worktree", "add", wt.to_str().unwrap(), "feat/active"],
    );
    git_in(&repo, &["merge", "feat/active"]);
    // Merged + clean, but agent is actively using this worktree

    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
    std::env::set_var("AGEND_WORKTREE_PRUNE_LIVE", "1");
    let home = tmp_home("v2-active");
    let mut configs = HashMap::new();
    // Agent's working_dir points to the worktree (bootstrap redirect)
    configs.insert("active-agent".to_string(), Some(wt.clone()));
    write_source_repo_binding(&home, "active-agent", &repo);
    let removed = sweep_from_registry(&home, &configs, &[]);
    assert!(
        removed.is_empty(),
        "active agent worktree must NOT be removed: {removed:?}"
    );
    assert!(wt.exists(), "worktree dir must still exist");
    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

// ── #t-…81457-1 (PRUNE_LIVE first-day false positives): three independent
// occupancy/eligibility gaps found live on 2026-07-06 within the first
// sweep tick after PRUNE_LIVE went on. Each RED test below reproduces one
// class against the pre-fix code. ──

/// Seed `home/runtime/<agent>/binding.json` with BOTH `source_repo` and
/// `worktree` — the real production shape (`binding::bind`'s writer sets
/// both). `write_source_repo_binding` (above) only sets `source_repo`,
/// which is enough for repo-discovery tests but not for exercising
/// worktree-occupancy via the binding registry.
fn write_full_binding(home: &Path, agent: &str, branch: &str, source_repo: &Path, worktree: &Path) {
    let dir = crate::paths::runtime_dir(home).join(agent);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("binding.json"),
        serde_json::to_string(&serde_json::json!({
            "branch": branch,
            "source_repo": source_repo.display().to_string(),
            "worktree": worktree.display().to_string(),
        }))
        .unwrap(),
    )
    .unwrap();
}

/// #t-…81457-1 primary fix: a worktree with a LIVE `binding.json` entry
/// must never be swept, even when the daemon's in-memory `configs`
/// (AgentConfig.working_dir) registry hasn't caught up yet — the exact
/// dev3 PRUNE_LIVE incident (auto-bind at 11:04, sweep at 11:08, `configs`
/// still empty for that agent). Pre-fix, `is_in_use` only ever consulted
/// `configs`/`fleet_dirs`, never `binding.json`, so this worktree — merged
/// AND clean, exactly like dev3's — was eligible and got removed.
#[test]
fn sweep_skips_worktree_known_only_via_binding_json_not_yet_in_configs_registry() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("binding-only-occupancy");
    // Old dated commit so `is_branch_merged`'s age gate (fix #2) does NOT
    // protect this worktree — isolates fix #1 (binding-registry occupancy)
    // as the ONLY thing standing between this live-bound worktree and removal.
    make_old_dated_branch(&repo, "feat/fresh-bind", "2024-01-01T00:00:00 +0000");
    let wt = repo.join("wt-fresh-bind");
    git_in(
        &repo,
        &["worktree", "add", wt.to_str().unwrap(), "feat/fresh-bind"],
    );
    git_in(&repo, &["merge", "feat/fresh-bind"]);

    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
    std::env::set_var("AGEND_WORKTREE_PRUNE_LIVE", "1");
    let home = tmp_home("binding-only-occupancy");
    // `configs` deliberately EMPTY — the in-memory registry hasn't caught
    // up to the fresh bind yet. binding.json is the only live signal.
    let configs: HashMap<String, Option<PathBuf>> = HashMap::new();
    write_full_binding(&home, "dev3", "feat/fresh-bind", &repo, &wt);

    let removed = sweep_from_registry(&home, &configs, &[]);
    assert!(
        removed.is_empty(),
        "a worktree with a LIVE binding.json entry must never be swept, even \
             when `configs` hasn't caught up yet (the exact dev3 PRUNE_LIVE \
             incident): {removed:?}"
    );
    assert!(
        wt.exists(),
        "the live-bound worktree directory must survive"
    );

    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// #t-…81457-1 REJECTED rework (reviewer4 r0): an unreadable/corrupt
/// `binding.json` for ANY agent is an AMBIGUITY, not an absence — it could
/// be hiding the very live binding that would have protected the worktree
/// under test. Pre-rework, `bound_worktree_paths` silently skipped it
/// (same as a missing file), so an old (age-gate-cleared), clean, merged
/// worktree with no binding of its OWN was still removed even though a
/// SIBLING agent's binding.json existed but failed to parse. Reproduces
/// reviewer4's exact repro shape: this must now skip the ENTIRE sweep
/// round (fail closed), not just the ambiguous row.
#[test]
fn sweep_fails_closed_when_any_binding_json_is_corrupt() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("corrupt-binding-ambiguity");
    make_old_dated_branch(&repo, "feat/live-bound", "2024-01-01T00:00:00 +0000");
    let wt = repo.join("wt-live-bound");
    git_in(
        &repo,
        &["worktree", "add", wt.to_str().unwrap(), "feat/live-bound"],
    );
    git_in(&repo, &["merge", "feat/live-bound"]);

    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
    std::env::set_var("AGEND_WORKTREE_PRUNE_LIVE", "1");
    let home = tmp_home("corrupt-binding-ambiguity");
    let configs: HashMap<String, Option<PathBuf>> = HashMap::new();
    // One valid binding for repo discovery ...
    write_source_repo_binding(&home, "other-agent", &repo);
    // ... and one CORRUPT binding.json for a DIFFERENT agent — unrelated to
    // `feat/live-bound` on its face, but the daemon cannot know that from a
    // file it failed to parse.
    let corrupt_dir = crate::paths::runtime_dir(&home).join("dev3");
    std::fs::create_dir_all(&corrupt_dir).unwrap();
    std::fs::write(corrupt_dir.join("binding.json"), b"not valid json").unwrap();

    let removed = sweep_from_registry(&home, &configs, &[]);
    assert!(
        removed.is_empty(),
        "an unreadable/corrupt binding.json anywhere must fail the WHOLE \
             sweep round closed, even for an unrelated, otherwise-eligible \
             worktree: {removed:?}"
    );
    assert!(wt.exists(), "the worktree must survive the ambiguous round");

    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// #t-…81457-1 REJECTED rework, negative control: a MISSING binding.json
/// (the normal steady state — most agents are never bound) must NOT
/// trigger the fail-closed ambiguity path, or every legitimate cleanup
/// case regresses (the 26 real candidates PRUNE_LIVE's first tick
/// correctly reaped would silently stop being collected).
#[test]
fn sweep_still_removes_when_no_binding_json_exists_at_all() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("no-binding-normal");
    make_old_dated_branch(&repo, "feat/done", "2024-01-01T00:00:00 +0000");
    let wt = repo.join("wt-done");
    git_in(
        &repo,
        &["worktree", "add", wt.to_str().unwrap(), "feat/done"],
    );
    git_in(&repo, &["merge", "feat/done"]);

    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
    std::env::set_var("AGEND_WORKTREE_PRUNE_LIVE", "1");
    let home = tmp_home("no-binding-normal");
    let configs: HashMap<String, Option<PathBuf>> = HashMap::new();
    // Repo discovery needs ONE valid binding; no agent has a binding
    // pointing at `wt` itself, and no binding.json anywhere is corrupt.
    write_source_repo_binding(&home, "other-agent", &repo);

    let removed = sweep_from_registry(&home, &configs, &[]);
    assert!(
        removed.iter().any(|(b, _, _)| b == "feat/done"),
        "a genuinely unbound, old, clean, merged worktree must still be \
             removed — a merely-absent binding.json is not an ambiguity: {removed:?}"
    );

    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// #t-…96214-1 (#2657 lead 二席 r1 nit): an UNREADABLE `runtime_dir` (the
/// directory exists but a permission / fd-exhaustion error blocks the scan)
/// is an AMBIGUITY, not an absence — a live `binding.json` may be hiding
/// behind it. Pre-fix, `read_dir` failure fell through `let Ok(..) else
/// return Ok(Vec::new())`, silently reporting "no bindings" and letting the
/// sweep proceed to removals. It must now fail the round closed (`Err`),
/// mirroring the per-file unreadable branch already below it. Unix-only:
/// relies on `chmod 000` being enforced (self-skips where it is not, e.g.
/// the process runs as root).
#[cfg(unix)]
#[test]
fn bound_worktree_paths_unreadable_runtime_dir_is_ambiguous() {
    use std::os::unix::fs::PermissionsExt;
    let home = tmp_home("unreadable-runtime-dir");
    // Give runtime_dir real content so the ONLY variable under test is its
    // readability, not its existence.
    write_source_repo_binding(&home, "some-agent", &home);
    let rt = crate::paths::runtime_dir(&home);
    std::fs::set_permissions(&rt, std::fs::Permissions::from_mode(0o000)).unwrap();

    // If the mode isn't enforced for this process (root, or a permissive
    // filesystem), the read still succeeds and the ambiguity cannot be
    // reproduced — restore + skip rather than assert a false failure.
    if std::fs::read_dir(&rt).is_ok() {
        std::fs::set_permissions(&rt, std::fs::Permissions::from_mode(0o755)).ok();
        std::fs::remove_dir_all(&home).ok();
        return;
    }

    let result = bound_worktree_paths_or_ambiguous(&home);
    // Restore perms BEFORE asserting so cleanup runs even if the assert panics.
    std::fs::set_permissions(&rt, std::fs::Permissions::from_mode(0o755)).ok();
    assert!(
        result.is_err(),
        "an unreadable runtime_dir must be reported as ambiguity (Err), not \
             an empty binding set — else the sweep proceeds to removals blind to \
             a possibly-live binding: {result:?}"
    );

    std::fs::remove_dir_all(&home).ok();
}

/// #t-…96214-1 negative control: a MISSING `runtime_dir` (no agent has ever
/// bound — nothing created `home/runtime` yet) is the normal absence state,
/// exactly like a missing per-agent `binding.json`. It must return
/// `Ok(empty)`, NOT `Err`, or every steady-state sweep on a fresh home would
/// fail closed and stop reaping legitimately-orphaned worktrees. Pins the
/// NotFound-is-absence half of the fix against a future blanket-`Err`
/// refactor.
#[test]
fn bound_worktree_paths_missing_runtime_dir_is_absence_not_ambiguity() {
    let home = tmp_home("missing-runtime-dir");
    // tmp_home creates `home` but NOT `home/runtime`.
    assert!(!crate::paths::runtime_dir(&home).exists());
    assert_eq!(
        bound_worktree_paths_or_ambiguous(&home),
        Ok(Vec::new()),
        "a missing runtime_dir is a genuine absence (no agent ever bound), \
             not an ambiguity"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// #t-…81457-1 depth fix #1: `is_branch_merged`'s is-ancestor check is
/// trivially true for a branch whose tip is IDENTICAL to the default
/// branch (zero commits ever made) — nothing has actually been merged,
/// there's nothing to merge. This is a unit-level pin on the exact
/// function so the fix can't regress even if the occupancy fix (above)
/// changes shape later — "單靠 ① 未來 binding 生命週期一變又漏" (lead).
#[test]
fn is_branch_merged_rejects_zero_commit_branch_tip_equals_default() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("zero-commit-merged-unit");
    git_in(&repo, &["branch", "feat/never-touched"]);

    assert!(
        !is_branch_merged(&repo, "feat/never-touched"),
        "a branch whose tip is IDENTICAL to main (zero commits, nothing ever \
             diverged) must not be classified as merged — there is nothing to merge"
    );

    std::fs::remove_dir_all(&repo).ok();
}

/// #t-…81457-1 depth fix #1, integration level: the same zero-commit
/// scenario through the full sweep, with NO occupancy signal at all (no
/// binding, no configs) — isolates this fix from the binding-registry fix
/// above. This is dev3's actual incident mechanics minus the binding gap.
#[test]
fn sweep_does_not_treat_zero_commit_worktree_as_merged() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("zero-commit-sweep");
    git_in(&repo, &["branch", "feat/fresh-no-commits"]);
    let wt = repo.join("wt-fresh-no-commits");
    git_in(
        &repo,
        &[
            "worktree",
            "add",
            wt.to_str().unwrap(),
            "feat/fresh-no-commits",
        ],
    );

    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
    std::env::set_var("AGEND_WORKTREE_PRUNE_LIVE", "1");
    let home = tmp_home("zero-commit-sweep");
    let configs: HashMap<String, Option<PathBuf>> = HashMap::new();
    write_source_repo_binding(&home, "other-agent", &repo); // repo discovery only

    let removed = sweep_from_registry(&home, &configs, &[]);
    assert!(
        removed.is_empty(),
        "a zero-commit branch (tip==main, nothing diverged) must not be \
             classified merged just because is-ancestor is trivially true: {removed:?}"
    );
    assert!(wt.exists());

    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// #t-…81457-1 depth fix #2, LIVE self-reproduced incident: production
/// branch creation (`bind_self`'s `ensure_branch_fetch` → `git branch
/// <name> origin/main`) auto-sets upstream tracking to `origin/main`
/// (`branch.<name>.merge = refs/heads/main`), NOT to a same-named remote
/// branch — because the branch has never been pushed under its own name.
/// `is_remote_gone`'s `refs/remotes/{remote}/{branch}` existence check
/// assumes the upstream mirrors the LOCAL branch's own name; it never does
/// for a from-origin/main tracked branch, so a legitimately-never-pushed
/// branch is misclassified as "remote gone". Self-reproduced live: this
/// agent's own fresh worktree AND gapfix-dev2's were both
/// `worktree_auto_removed reason=remote-gone` within ~70s of bind, before
/// either had pushed (event-log confirmed, same tick).
#[test]
fn is_remote_gone_does_not_misfire_for_never_pushed_branch_tracking_main() {
    let _lock = ENV_LOCK.lock();
    let remote_dir = std::env::temp_dir().join(format!(
        "agend-wt-v2-neverpushed-remote-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&remote_dir).ok();
    git_in(&remote_dir, &["init", "--bare", "-b", "main"]);

    let repo = std::env::temp_dir().join(format!(
        "agend-wt-v2-neverpushed-clone-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&repo);
    Command::new("git")
        .args([
            "clone",
            remote_dir.to_str().unwrap(),
            repo.to_str().unwrap(),
        ])
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("clone");
    std::fs::write(repo.join("README.md"), "init").ok();
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "init"]);
    git_in(&repo, &["push", "-u", "origin", "main"]);

    // Production shape: `git branch <name> origin/main`, NEVER pushed
    // under its own name.
    git_in(&repo, &["branch", "fix/never-pushed", "origin/main"]);

    assert!(
        !is_remote_gone(&repo, "fix/never-pushed"),
        "a branch that tracks origin/main (never pushed under its own name) \
             must NOT be classified remote-gone — refs/remotes/origin/<name> was \
             never supposed to exist for it in the first place"
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&remote_dir).ok();
}

#[test]
fn test_v2_remote_gone_worktree_removed() {
    // Simulate squash-merge: branch is NOT merged (different hash) but
    // remote tracking ref is gone after `git fetch --prune`.
    let _lock = ENV_LOCK.lock();

    // Create "remote" bare repo
    let remote_dir = std::env::temp_dir().join(format!(
        "agend-wt-v2-remote-gone-{}-{}",
        std::process::id(),
        std::sync::atomic::AtomicU32::new(0).fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&remote_dir).ok();
    git_in(&remote_dir, &["init", "--bare", "-b", "main"]);

    // Clone it
    let repo = std::env::temp_dir().join(format!(
        "agend-wt-v2-remote-gone-clone-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&repo);
    Command::new("git")
        .args([
            "clone",
            remote_dir.to_str().unwrap(),
            repo.to_str().unwrap(),
        ])
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("clone");
    std::fs::write(repo.join("README.md"), "init").ok();
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "init"]);
    git_in(&repo, &["push", "-u", "origin", "main"]);

    // Create a feature branch, push it, then delete remote ref
    git_in(&repo, &["checkout", "-b", "feat/squashed"]);
    std::fs::write(repo.join("feat.txt"), "feature").ok();
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "feature work"]);
    git_in(&repo, &["push", "-u", "origin", "feat/squashed"]);
    git_in(&repo, &["checkout", "main"]);

    // Create worktree on that branch
    let wt = repo.join("wt-squashed");
    git_in(
        &repo,
        &["worktree", "add", wt.to_str().unwrap(), "feat/squashed"],
    );

    // Simulate: remote branch deleted (squash-merged on GitHub)
    git_in(&remote_dir, &["branch", "-D", "feat/squashed"]);
    git_in(&repo, &["fetch", "--prune"]);

    // Branch is NOT merged (different commit hash) but remote is gone
    assert!(
        !is_branch_merged(&repo, "feat/squashed"),
        "branch should NOT be detected as merged"
    );
    assert!(
        is_remote_gone(&repo, "feat/squashed"),
        "branch remote should be detected as gone"
    );

    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
    std::env::set_var("AGEND_WORKTREE_PRUNE_LIVE", "1");
    let home = tmp_home("v2-remote-gone");
    let mut configs = HashMap::new();
    configs.insert("other".to_string(), Some(repo.join("other")));
    write_source_repo_binding(&home, "other", &repo);
    let removed = sweep_from_registry(&home, &configs, &[]);
    assert!(
        removed
            .iter()
            .any(|(b, _, r)| b == "feat/squashed" && *r == "remote-gone"),
        "#2605 review finding: a remote-gone worktree's removal event must carry \
             reason \"remote-gone\", NOT a hardcoded \"merged\" — the whole point of \
             the dry-run/audit-diff is an honest reason per candidate: {removed:?}"
    );
    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&remote_dir).ok();
    std::fs::remove_dir_all(&home).ok();
}

// ── #1750-B3: local squash-merge orphan auto-GC ──

/// Commit like `git_in`'s commit but with a fixed author+committer DATE, so
/// `branch_tip_age` is deterministic regardless of wall-clock.
pub(super) fn git_commit_dated(dir: &Path, msg: &str, date: &str) {
    Command::new("git")
        .args(["commit", "-m", msg])
        .current_dir(dir)
        .env("AGEND_GIT_BYPASS", "1")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .output()
        .expect("dated commit");
}

/// Build a LOCAL squash-merge orphan: `branch` carries `feat.txt`, then main
/// diverges (`other.txt`) and cherry-picks `branch`'s patch — so `branch` is
/// NOT a `--merged` ancestor (different SHA) but IS squash-merged (git cherry
/// shows `-`). `branch`'s tip is committed at `tip_date`.
fn make_squash_orphan(repo: &Path, branch: &str, tip_date: &str) {
    git_in(repo, &["checkout", "-b", branch]);
    std::fs::write(repo.join("feat.txt"), "feature").ok();
    git_in(repo, &["add", "."]);
    git_commit_dated(repo, "feature work", tip_date);
    git_in(repo, &["checkout", "main"]);
    // Diverge main on a DIFFERENT file so the cherry-pick applies cleanly.
    std::fs::write(repo.join("other.txt"), "main-side").ok();
    git_in(repo, &["add", "."]);
    git_in(repo, &["commit", "-m", "main diverge"]);
    git_in(repo, &["cherry-pick", branch]);
}

/// PR-D·D3 equivalence pin: `branch_reap_delete` — the `branch_disposition`
/// delegation — must reproduce BOTH pre-D3 branch-reap gates byte-for-byte:
///   phase-2 `prune_orphaned_branches`: delete iff merged, squash, or a
///   recovery-ref-backed TTL has expired
///   phase-1 `branch_safe_to_delete`:   delete iff `merged || squash` (scaffold=false)
/// Both reduce to `provably_in_default || recoverable_ttl`. The CR-2026-06-14
/// invariant — remote-gone ALONE is never a delete trigger — is pinned by
/// asserting `remote_gone` NEVER changes the result across the full domain.
#[test]
fn branch_reap_delete_equals_pre_d3_gate() {
    for provably_in_default in [true, false] {
        for remote_gone in [true, false] {
            for recoverable_ttl in [true, false] {
                assert_eq!(
                    branch_reap_delete(provably_in_default, remote_gone, recoverable_ttl),
                    provably_in_default || recoverable_ttl,
                    "branch reap DRIFT: provably={provably_in_default} \
                         remote_gone={remote_gone} ttl={recoverable_ttl} \
                         (remote-gone must NEVER flip the result — CR-2026-06-14)"
                );
            }
        }
    }
}

#[test]
fn recovery_backed_delete_uses_expected_tip_cas() {
    let repo = setup_test_repo("stale-idle-cas");
    let branch = "feat/cas-protected";
    make_unmerged_dated_branch(&repo, branch, "2024-01-01T00:00:00 +0000");
    let old_tip = branch_tip_info(&repo, branch).expect("old tip").0;
    git_in(&repo, &["checkout", branch]);
    std::fs::write(repo.join("new.txt"), "new work").expect("write");
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "advance branch"]);
    git_in(&repo, &["checkout", "main"]);
    let new_tip = branch_tip_info(&repo, branch).expect("new tip").0;
    assert_ne!(old_tip, new_tip);

    let result = delete_branch_ref_cas(&repo, branch, &old_tip);

    assert!(result.is_err(), "stale expected tip must fail closed");
    assert_eq!(
        branch_tip_info(&repo, branch).expect("branch survives").0,
        new_tip
    );
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn prune_squash_merged_old_branch_1750_b3() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("b3-squash-old");
    // Old tip (well past SQUASH_GC_MIN_TIP_AGE) + squash-merged into main.
    make_squash_orphan(&repo, "feat/squash-old", "2024-01-01T00:00:00 +0000");
    // Precondition: the squash-blind signals MISS it (the #1750 bug).
    assert!(
        !is_branch_merged(&repo, "feat/squash-old"),
        "not a --merged ancestor"
    );
    assert!(
        !is_remote_gone(&repo, "feat/squash-old"),
        "no remote configured"
    );

    let pruned = prune_orphaned_branches(&repo, false);
    assert!(
        pruned
            .iter()
            .any(|(b, r)| b == "feat/squash-old" && *r == "squash-merged"),
        "#1750-B3/#2605: a squash-merged orphan past the age floor must be auto-GC'd \
             with reason \"squash-merged\" (not \"merged\"), got: {pruned:?}"
    );
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn prune_skips_squash_merged_too_new_1750_b3() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("b3-squash-new");
    // Squash-merged but tip committed NOW (git_in default date) → under the
    // age floor → must NOT be deleted yet (a later sweep reaps it).
    git_in(&repo, &["checkout", "-b", "feat/squash-new"]);
    std::fs::write(repo.join("feat.txt"), "feature").ok();
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "feature work"]); // now-dated tip
    git_in(&repo, &["checkout", "main"]);
    std::fs::write(repo.join("other.txt"), "main-side").ok();
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "main diverge"]);
    git_in(&repo, &["cherry-pick", "feat/squash-new"]);

    assert!(
        crate::branch_sweep::is_squash_merged(&repo, "main", "feat/squash-new"),
        "precondition: detected as squash-merged"
    );
    let pruned = prune_orphaned_branches(&repo, false);
    assert!(
        !pruned.iter().any(|(b, _)| b == "feat/squash-new"),
        "#1750-B3: a squash-merged branch under the tip-age floor must NOT be GC'd yet"
    );
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn prune_skips_young_unmerged_branch_1750_b3() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("b3-unmerged");
    // A genuinely unmerged branch with a fresh tip — neither squash nor TTL
    // eligibility may fire.
    git_in(&repo, &["checkout", "-b", "feat/wip"]);
    std::fs::write(repo.join("feat.txt"), "wip").ok();
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "wip"]);
    git_in(&repo, &["checkout", "main"]);

    assert!(
        !crate::branch_sweep::is_squash_merged(&repo, "main", "feat/wip"),
        "precondition: NOT squash-merged"
    );
    let pruned = prune_orphaned_branches(&repo, false);
    assert!(
        !pruned.iter().any(|(b, _)| b == "feat/wip"),
        "#1750-B3: a fresh unmerged branch must NOT be GC'd"
    );
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn prune_skips_checked_out_squash_orphan_1750_b3() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("b3-squash-checkedout");
    make_squash_orphan(&repo, "feat/squash-wt", "2024-01-01T00:00:00 +0000");
    // Check the squash-merged branch out in a worktree → must be skipped.
    let wt = repo.join("wt-squash");
    git_in(
        &repo,
        &["worktree", "add", wt.to_str().unwrap(), "feat/squash-wt"],
    );

    let pruned = prune_orphaned_branches(&repo, false);
    assert!(
        !pruned.iter().any(|(b, _)| b == "feat/squash-wt"),
        "#1750-B3: a squash-merged branch checked out in a worktree must NOT be GC'd"
    );
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn prune_orphaned_branches_dry_run_reports_but_keeps_branch_2605() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("b3-dry-run");
    // A genuinely merged (fast-forward) branch — unambiguously eligible.
    // #t-…81457-1: old dated commit so it clears is_branch_merged's age gate.
    make_old_dated_branch(&repo, "feat/merged", "2024-01-01T00:00:00 +0000");
    git_in(&repo, &["merge", "feat/merged"]);

    let pruned = prune_orphaned_branches(&repo, true);
    assert!(
        pruned
            .iter()
            .any(|(b, r)| b == "feat/merged" && *r == "merged"),
        "#2605: dry-run must still report the eligible candidate with its real \
             reason (\"merged\"): {pruned:?}"
    );
    assert!(
        crate::git_helpers::git_ok(&repo, &["rev-parse", "--verify", "feat/merged"]),
        "#2605: dry-run must NOT actually delete the branch"
    );
    std::fs::remove_dir_all(&repo).ok();
}

// ── PR-A P1 (branch-residue RCA §3): review/* scaffolding TTL prune ──

/// Create an UNMERGED branch `name` with a single tip commit dated `date`.
/// (Diverges from main; never merged — the reviewer-checkout scaffolding shape.)
fn make_unmerged_dated_branch(repo: &Path, name: &str, date: &str) {
    git_in(repo, &["checkout", "-b", name]);
    std::fs::write(repo.join("scaffold.txt"), name).ok();
    git_in(repo, &["add", "."]);
    git_commit_dated(repo, "scaffolding commit", date);
    git_in(repo, &["checkout", "main"]);
}

/// RED1 (证洞→修): an aged (>72h), unoccupied `review/*` scaffolding branch —
/// never merged, so the merged/squash paths never reap it. On the pre-P1 code
/// `prune_orphaned_branches` KEEPS it (the leak); after P1 it is deleted with
/// reason `review-scaffold-ttl`.
#[test]
fn prune_deletes_aged_unoccupied_review_scaffold_p1() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("p1-scaffold-aged");
    make_unmerged_dated_branch(&repo, "review/2342-r0", "2024-01-01T00:00:00 +0000");
    // Precondition: neither of the existing reap signals fires.
    assert!(
        !is_branch_merged(&repo, "review/2342-r0"),
        "not a --merged ancestor"
    );
    assert!(
        !crate::branch_sweep::is_squash_merged(&repo, "main", "review/2342-r0"),
        "not squash-merged"
    );

    let pruned = prune_orphaned_branches(&repo, false);
    assert!(
        pruned
            .iter()
            .any(|(b, r)| b == "review/2342-r0" && *r == "review-scaffold-ttl"),
        "PR-A P1: an aged, unoccupied review/* scaffolding branch must be GC'd with \
             reason \"review-scaffold-ttl\", got: {pruned:?}"
    );
    assert!(
        !crate::git_helpers::git_ok(&repo, &["rev-parse", "--verify", "review/2342-r0"]),
        "PR-A P1: the aged review scaffold must actually be deleted"
    );
    std::fs::remove_dir_all(&repo).ok();
}

/// RED2 (guard, 永遠保留): the SAME aged `review/*` branch, but checked out in a
/// worktree (an in-progress review), must NEVER be pruned regardless of age.
#[test]
fn prune_keeps_occupied_review_scaffold_p1() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("p1-scaffold-occupied");
    make_unmerged_dated_branch(&repo, "review/2342-r0", "2024-01-01T00:00:00 +0000");
    let wt = repo.join("wt-review");
    git_in(
        &repo,
        &["worktree", "add", wt.to_str().unwrap(), "review/2342-r0"],
    );

    let pruned = prune_orphaned_branches(&repo, false);
    assert!(
        !pruned.iter().any(|(b, _)| b == "review/2342-r0"),
        "PR-A P1: a review/* branch occupied by a worktree (review in progress) must \
             NOT be pruned even when aged: {pruned:?}"
    );
    std::fs::remove_dir_all(&repo).ok();
}

/// RED3 (guard, young→保留): a `review/*` scaffolding branch whose tip is
/// recent (under `REVIEW_SCAFFOLD_TTL`) must NOT be pruned yet.
#[test]
fn prune_keeps_young_review_scaffold_p1() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("p1-scaffold-young");
    // now-dated tip (git default date) → well under the 72h TTL.
    git_in(&repo, &["checkout", "-b", "review/fresh"]);
    std::fs::write(repo.join("scaffold.txt"), "fresh").ok();
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "fresh review scaffold"]);
    git_in(&repo, &["checkout", "main"]);

    let pruned = prune_orphaned_branches(&repo, false);
    assert!(
        !pruned.iter().any(|(b, _)| b == "review/fresh"),
        "PR-A P1: a review/* branch under REVIEW_SCAFFOLD_TTL must NOT be pruned: {pruned:?}"
    );
    std::fs::remove_dir_all(&repo).ok();
}

/// Generic unmerged refs eventually leave the ordinary branch namespace once
/// the 90d stale-idle TTL expires, but only after their exact tips are protected
/// by durable recovery refs.
#[test]
fn prune_recovers_and_deletes_stale_idle_generic_branches() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("p1-spike-feat");
    make_unmerged_dated_branch(&repo, "spike/2342-inbound", "2024-01-01T00:00:00 +0000");
    make_unmerged_dated_branch(&repo, "feat/real-wip", "2024-01-01T00:00:00 +0000");

    let spike_tip = branch_tip_info(&repo, "spike/2342-inbound")
        .expect("spike tip")
        .0;
    let feat_tip = branch_tip_info(&repo, "feat/real-wip").expect("feat tip").0;

    let pruned = prune_orphaned_branches(&repo, false);
    assert!(
        pruned
            .iter()
            .any(|(b, r)| b == "spike/2342-inbound" && *r == "stale-idle-ttl"),
        "stale spike ref must retire through the recovery-backed TTL: {pruned:?}"
    );
    assert!(
        pruned
            .iter()
            .any(|(b, r)| b == "feat/real-wip" && *r == "stale-idle-ttl"),
        "stale feature ref must retire through the recovery-backed TTL: {pruned:?}"
    );
    let recovery_tips = crate::git_helpers::git_cmd(
        &repo,
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "refs/agend/recovery/branch/",
        ],
    )
    .expect("list recovery refs");
    assert!(recovery_tips.lines().any(|tip| tip == spike_tip));
    assert!(recovery_tips.lines().any(|tip| tip == feat_tip));
    std::fs::remove_dir_all(&repo).ok();
}

#[test]
fn prune_keeps_stale_idle_branch_with_active_task() {
    use crate::task_events::{InstanceName, TaskEvent, TaskId};
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("stale-idle-active-task");
    let home = tmp_home("stale-idle-active-task");
    let branch = "feat/stale-but-active";
    make_unmerged_dated_branch(&repo, branch, "2024-01-01T00:00:00 +0000");
    let board = crate::task_events::board_root(&home, crate::task_events::DEFAULT_PROJECT);
    std::fs::create_dir_all(&board).ok();
    crate::task_events::append(
        &board,
        &InstanceName::from("test"),
        TaskEvent::Created {
            task_id: TaskId("t-stale-active".into()),
            title: "live work".into(),
            description: String::new(),
            priority: "normal".into(),
            tags: Vec::new(),
            owner: None,
            depends_on: Vec::new(),
            parent_id: None,
            governing_decision_id: None,
            review_class: None,
            branch: Some(branch.into()),
            due_at: None,
            routed_to: None,
            bind: None,
            eta_secs: None,
        },
    )
    .expect("seed active task");

    let pruned = prune_orphaned_branches_with_home(Some(&home), &repo, false);

    assert!(
        !pruned.iter().any(|(name, _)| name == branch),
        "active task must keep even a TTL-old branch: {pruned:?}"
    );
    assert!(branch_exists(&repo, branch));
    let tasks = hygiene_tasks(&home);
    let key = format!("residue-lifecycle-blocked:{}:{branch}", repo.display());
    assert!(
        tasks.iter().any(|(task_key, evidence)| {
            task_key == &key && evidence["task_active"] == serde_json::Value::Bool(true)
        }),
        "a TTL-old branch blocked by a live task must be observable: {tasks:?}"
    );
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

// ── #P1-2607: squash-eligibility tip-SHA cache ──

/// The #2607-freeze incident's second fix: `is_squash_gc_eligible`'s
/// expensive structural check must be reused across calls for the SAME
/// tip, not re-run every sweep round. Proven by inserting a cache entry
/// for a fixed tip, then observing that a second call for that EXACT
/// tip does not add another entry (the branch-keyed set is unique per
/// test repo path, so this is immune to interference from other tests
/// sharing the same process-wide cache).
#[test]
fn is_squash_gc_eligible_reuses_cache_for_same_tip_p1_2607() {
    let repo = setup_test_repo("p1-2607-cache");
    make_squash_orphan(&repo, "feat/cache-me", "2024-01-01T00:00:00 +0000");
    let (tip_sha, _) = branch_tip_info(&repo, "feat/cache-me")
        .expect("tip info must resolve for an existing branch");
    let (default_tip_sha, _) =
        branch_tip_info(&repo, "main").expect("tip info must resolve for main");
    let key = (
        repo.clone(),
        "feat/cache-me".to_string(),
        tip_sha,
        default_tip_sha,
    );

    let cache = SQUASH_MERGED_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    assert!(
        !cache.lock().contains_key(&key),
        "precondition: this fresh tip must not already be cached"
    );

    assert!(is_squash_gc_eligible(&repo, "feat/cache-me", "main"));
    assert!(
        cache.lock().contains_key(&key),
        "first call must populate the cache for this (repo, branch, tip_sha)"
    );

    // Second call for the identical tip: must still be true (cache hit),
    // and must not have replaced the entry with a different key (the
    // tip hasn't moved, so the key is unchanged).
    assert!(is_squash_gc_eligible(&repo, "feat/cache-me", "main"));
    assert!(cache.lock().contains_key(&key));

    std::fs::remove_dir_all(&repo).ok();
}

/// A branch's tip moving (new commit) must fall out of the OLD cache
/// entry and be re-evaluated fresh under its NEW tip-SHA key — the
/// cache must never pin a branch to a stale verdict once its tip
/// changes.
#[test]
fn is_squash_gc_eligible_recomputes_after_tip_moves_p1_2607() {
    let repo = setup_test_repo("p1-2607-cache-move");
    make_squash_orphan(&repo, "feat/moves", "2024-01-01T00:00:00 +0000");
    let (old_tip, _) = branch_tip_info(&repo, "feat/moves").expect("tip info must resolve");
    assert!(is_squash_gc_eligible(&repo, "feat/moves", "main"));

    // Move the tip: a fresh, unmerged, TOO-YOUNG commit — must now be
    // ineligible (age floor), proving the stale cache entry (keyed on
    // the OLD tip) is not consulted for the new tip.
    git_in(&repo, &["checkout", "feat/moves"]);
    std::fs::write(repo.join("more.txt"), "more").ok();
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "new unmerged work"]); // now-dated tip
    git_in(&repo, &["checkout", "main"]);
    let (new_tip, _) = branch_tip_info(&repo, "feat/moves").expect("tip info must resolve");
    assert_ne!(old_tip, new_tip, "precondition: the tip must have moved");

    assert!(
        !is_squash_gc_eligible(&repo, "feat/moves", "main"),
        "after the tip moves to a fresh young commit, eligibility must be \
             re-derived under the NEW tip key, not answered from the OLD tip's \
             cached (stale) verdict"
    );

    std::fs::remove_dir_all(&repo).ok();
}

/// #2614: `is_squash_merged`'s verdict for a FIXED branch tip depends on
/// `default`'s content too — a branch not yet reflected in `default` is
/// (correctly) ineligible, but once `default` absorbs the branch's patch
/// (a real squash-merge), the SAME branch tip becomes eligible. The old
/// 3-tuple cache key `(repo, branch, tip_sha)` ignored `default`'s tip
/// entirely, so this transition was cached as permanently ineligible —
/// live prune would never reap the branch and dry-run would systematically
/// under-report it.
#[test]
fn is_squash_gc_eligible_recomputes_after_default_tip_moves_2614() {
    let repo = setup_test_repo("2614-default-tip-move");
    git_in(&repo, &["checkout", "-b", "feat/lagging"]);
    std::fs::write(repo.join("feat.txt"), "feature").ok();
    git_in(&repo, &["add", "."]);
    git_commit_dated(&repo, "feature work", "2024-01-01T00:00:00 +0000");
    git_in(&repo, &["checkout", "main"]);

    let (branch_tip, _) = branch_tip_info(&repo, "feat/lagging").expect("tip info must resolve");

    // Precondition: `main` hasn't absorbed the branch's patch yet — not
    // squash-merged. This (false) verdict is what gets cached.
    assert!(
        !is_squash_gc_eligible(&repo, "feat/lagging", "main"),
        "precondition: branch not yet reflected in default → ineligible"
    );

    // Advance `main` the way a real squash-merge PR does — cherry-pick the
    // branch's patch. The branch's OWN tip does not move.
    git_in(&repo, &["cherry-pick", "feat/lagging"]);
    let (branch_tip_after, _) =
        branch_tip_info(&repo, "feat/lagging").expect("tip info must resolve");
    assert_eq!(
        branch_tip, branch_tip_after,
        "precondition: branch's own tip must NOT move — only `main` advances"
    );

    assert!(
        is_squash_gc_eligible(&repo, "feat/lagging", "main"),
        "#2614: once `main` absorbs the branch's patch, eligibility must be \
             RECOMPUTED — `default`'s tip is part of the cache key, so a stale \
             entry keyed only on branch tip must not keep returning the old \
             (false) verdict forever"
    );

    std::fs::remove_dir_all(&repo).ok();
}

/// #P3 (branch-residue): a POSITIVE (true) squash verdict is monotonic —
/// once a branch's patch is in `default`, `default` only advances further,
/// so `default`'s tip changing must NOT bust the cached TRUE and force the
/// expensive cherry/tree-diff re-run. Proven by seeding the positive-only
/// set (keyed WITHOUT default_tip) for a branch that is structurally NOT
/// squash-merged, then advancing `default`'s tip: a genuine recompute would
/// return FALSE, so the call returning TRUE proves the positive set is
/// consulted first (before default_tip is even resolved). Pre-#P3 (no
/// positive set) this recomputes under the new 4-tuple key → returns FALSE.
#[test]
fn is_squash_gc_eligible_positive_cache_survives_default_advance_p3() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("p3-positive-cache");
    // An OLD (age-floor-clearing) branch that is NOT actually squash-merged
    // — a genuine recompute returns false.
    make_unmerged_dated_branch(&repo, "feat/positive", "2024-01-01T00:00:00 +0000");
    assert!(
        !crate::branch_sweep::is_squash_merged(&repo, "main", "feat/positive"),
        "precondition: structurally NOT squash-merged (recompute would say false)"
    );
    let (tip_sha, _) = branch_tip_info(&repo, "feat/positive").expect("tip info must resolve");

    // Seed the positive-only set for (repo, branch, tip_sha) — simulating a
    // prior sweep that computed TRUE and recorded the monotonic positive.
    let positive =
        SQUASH_MERGED_POSITIVE.get_or_init(|| Mutex::new(std::collections::HashSet::new()));
    positive
        .lock()
        .insert((repo.clone(), "feat/positive".to_string(), tip_sha.clone()));

    // Advance `default`'s tip (a fresh commit on main) so the 4-tuple bool
    // cache key would differ — forcing a recompute absent the positive set.
    std::fs::write(repo.join("advance.txt"), "main advances").ok();
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "advance main"]);

    assert!(
        is_squash_gc_eligible(&repo, "feat/positive", "main"),
        "#P3: the positive set (keyed without default_tip) must be consulted \
             FIRST and return true without recompute, even though `default` \
             advanced and the structural check would now say false"
    );

    std::fs::remove_dir_all(&repo).ok();
}

// ── V1 (d-20260712065632138568-7): cleanup-failure hygiene producers ──

/// Board-side view of the hygiene tasks the sweep produced under `home`.
fn hygiene_tasks(home: &Path) -> Vec<(String, serde_json::Value)> {
    crate::task_events::replay(home)
        .map(|s| {
            s.tasks
                .values()
                .filter_map(|t| {
                    Some((
                        t.metadata
                            .get(crate::daemon::hygiene_task::ALERT_KEY_META)?
                            .as_str()?
                            .to_string(),
                        t.metadata
                            .get(crate::daemon::hygiene_task::EVIDENCE_META)
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn aged_preserved_review_intent_upserts_hygiene_task() {
    let _lock = ENV_LOCK.lock();
    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
    let home = tmp_home("aged-review-intent-hygiene");
    let repo = setup_test_repo("aged-review-intent-hygiene");
    let branch = "review/aged-blocked";
    make_unmerged_dated_branch(&repo, branch, "2024-01-01T00:00:00 +0000");
    let tip = branch_tip_info(&repo, branch).expect("tip").0;
    crate::cleanup_intents::persist_intent(
        &home,
        &repo.display().to_string(),
        branch,
        &tip,
        "t-aged-blocked",
        None,
        None,
    )
    .expect("persist intent");
    let intent_path = std::fs::read_dir(home.join("cleanup-intents"))
        .expect("intent dir")
        .flatten()
        .next()
        .expect("intent file")
        .path();
    let mut intent: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&intent_path).expect("read intent"))
            .expect("parse intent");
    intent["created_at"] =
        serde_json::Value::String((chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339());
    std::fs::write(
        &intent_path,
        serde_json::to_vec_pretty(&intent).expect("serialize intent"),
    )
    .expect("backdate intent");

    let _ = sweep_from_registry(&home, &HashMap::new(), &[]);

    let tasks = hygiene_tasks(&home);
    let (_, evidence) = tasks
        .iter()
        .find(|(key, _)| key.starts_with("cleanup-intent-preserved:"))
        .unwrap_or_else(|| panic!("aged preserved intent must be actionable: {tasks:?}"));
    assert_eq!(evidence["branch"], branch);
    assert_eq!(evidence["task_id"], "t-aged-blocked");
    assert_eq!(evidence["reason"], "preserved:task_not_terminal");
    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// V1 RED: an ELIGIBLE (merged) worktree whose removal FAILS must produce a
/// durable hygiene task with the exact repo/branch/reason — the sweep may
/// not silently skip a proven-eligible-but-undeletable candidate.
#[cfg(unix)]
#[test]
fn remove_failure_upserts_hygiene_task_v1() {
    use std::os::unix::fs::PermissionsExt;
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("v1-remove-fail");
    make_old_dated_branch(&repo, "feat/done", "2024-01-01T00:00:00 +0000");
    let wt = repo.join("wt-done");
    git_in(
        &repo,
        &["worktree", "add", wt.to_str().unwrap(), "feat/done"],
    );
    git_in(&repo, &["merge", "feat/done"]);
    // Sabotage: strip write permission so the eligible worktree cannot be
    // removed (children can't be unlinked from a non-writable dir).
    std::fs::set_permissions(&wt, std::fs::Permissions::from_mode(0o555)).unwrap();

    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
    let home = tmp_home("v1-remove-fail");
    let mut configs = HashMap::new();
    configs.insert("other-agent".to_string(), Some(repo.join("other")));
    write_source_repo_binding(&home, "other-agent", &repo);
    let removed = sweep_from_registry(&home, &configs, &[]);
    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");

    // The sabotaged WORKTREE dir must survive (its removal failed). The
    // branch itself may legitimately be reaped by phase-2 as an orphan
    // ("(no worktree)") once git dropped the admin entry — that layered
    // self-heal is fine; the failure signal is about the stuck DIR.
    assert!(
        wt.exists(),
        "sabotage must hold: the unremovable worktree dir survives"
    );
    assert!(
        !removed
            .iter()
            .any(|(b, p, _)| b == "feat/done" && p != "(no worktree)"),
        "the failed worktree removal itself may not be reported: {removed:?}"
    );
    let tasks = hygiene_tasks(&home);
    let key = format!("residue-remove-failed:{}:feat/done", repo.display());
    let hit = tasks.iter().find(|(k, _)| *k == key);
    let (_, evidence) = hit.unwrap_or_else(|| {
        panic!("eligible-but-remove-failed must upsert a hygiene task; got {tasks:?}")
    });
    assert_eq!(evidence["repo"], repo.display().to_string());
    assert_eq!(evidence["branch"], "feat/done");
    assert!(
        evidence["reason"]
            .as_str()
            .unwrap_or("")
            .contains("remove failed"),
        "exact failure reason required: {evidence}"
    );

    std::fs::set_permissions(&wt, std::fs::Permissions::from_mode(0o755)).ok();
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// V1 RED: a failing `fetch --prune` (here: unreachable remote) must upsert
/// a fetch-degraded hygiene task — a persistently failing fetch accumulates
/// undeletable branches invisibly (#2004) and may no longer stay log-only.
#[test]
fn fetch_failure_upserts_ambiguity_task_v1() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("v1-fetch-fail");
    git_in(
        &repo,
        &["remote", "add", "origin", "/nonexistent/agend-v1-fixture"],
    );
    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
    let home = tmp_home("v1-fetch-fail");
    let configs = HashMap::new();
    write_source_repo_binding(&home, "other-agent", &repo);
    let _ = sweep_from_registry(&home, &configs, &[]);
    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");

    let tasks = hygiene_tasks(&home);
    let key = format!("residue-fetch-degraded:{}", repo.display());
    assert!(
        tasks.iter().any(|(k, _)| *k == key),
        "failing fetch must upsert a fetch-degraded hygiene task; got {tasks:?}"
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// V1 negative guard (production datum m-20260712065850381686-207): a
/// deliberately KEPT branch — young review/* scaffolding, not merged, no
/// matching merged PR, not a squash orphan — is NOT eligible-but-failed and
/// must NOT produce any hygiene task.
#[test]
fn kept_review_branch_produces_no_hygiene_task_v1() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("v1-kept-review");
    // Young review scaffolding branch, no worktree: phase-2 keeps it
    // (inside its 72h TTL, unmerged, no remote counterpart).
    git_in(&repo, &["branch", "review/2746-codex-r1"]);
    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
    let home = tmp_home("v1-kept-review");
    let configs = HashMap::new();
    write_source_repo_binding(&home, "other-agent", &repo);
    let _ = sweep_from_registry(&home, &configs, &[]);
    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");

    let tasks = hygiene_tasks(&home);
    assert!(
        !tasks
            .iter()
            .any(|(k, _)| k.contains("review/2746-codex-r1")),
        "deliberately-kept branch must not be alerted: {tasks:?}"
    );

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// V1 guard: `AGEND_WORKTREE_AUTO_CLEANUP=0` is an operator opt-out — the
/// sweep does not run, so NO hygiene task may appear even with a staged
/// failure candidate present.
#[cfg(unix)]
#[test]
fn auto_cleanup_opt_out_produces_no_tasks_v1() {
    use std::os::unix::fs::PermissionsExt;
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("v1-optout");
    make_old_dated_branch(&repo, "feat/done", "2024-01-01T00:00:00 +0000");
    let wt = repo.join("wt-done");
    git_in(
        &repo,
        &["worktree", "add", wt.to_str().unwrap(), "feat/done"],
    );
    git_in(&repo, &["merge", "feat/done"]);
    std::fs::set_permissions(&wt, std::fs::Permissions::from_mode(0o555)).unwrap();

    std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "0");
    let home = tmp_home("v1-optout");
    let configs = HashMap::new();
    write_source_repo_binding(&home, "other-agent", &repo);
    let _ = sweep_from_registry(&home, &configs, &[]);
    std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");

    assert!(
        hygiene_tasks(&home).is_empty(),
        "opt-out means quiet: no sweep, no producers, no tasks"
    );

    std::fs::set_permissions(&wt, std::fs::Permissions::from_mode(0o755)).ok();
    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", branch])
        .current_dir(repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// #3011: a NON-terminal candidate is already a Keep decision at
/// `branch_lifecycle_disposition` (`!input.terminal` short-circuits before
/// task/holder evidence is even read) — the strict task-ledger replay
/// (`branch_has_active_task`) and the per-agent binding scan
/// (`branch_has_active_binding`) must not be reached for it at all.
#[test]
fn nonterminal_candidates_skip_task_and_binding_probes_3011() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("3011-skip");
    let home = tmp_home("3011-skip");

    // Several genuinely non-terminal branches: unmerged, not squash-merged
    // (different files, no cherry-pick relationship to main), not stale
    // review scaffolding (name doesn't match the reviewer-checkout pattern).
    for name in ["feat/wip-a", "feat/wip-b", "feat/wip-c"] {
        git_in(&repo, &["checkout", "-b", name]);
        std::fs::write(repo.join("wip.txt"), name).ok();
        git_in(&repo, &["add", "."]);
        git_in(&repo, &["commit", "-m", "wip"]);
        git_in(&repo, &["checkout", "main"]);
    }

    // take-and-zero doubles as the reset: discard whatever the fixture setup
    // above happened to cost, so the sweep below starts from zero.
    let _ = crate::branch_sweep::take_active_task_probe_count();
    let _ = take_binding_active_probe_count();

    let pruned = prune_orphaned_branches_with_home(Some(&home), &repo, false);

    assert_eq!(
        crate::branch_sweep::take_active_task_probe_count(),
        0,
        "#3011: non-terminal candidates must not reach branch_has_active_task \
         (the strict task-ledger replay)"
    );
    assert_eq!(
        take_binding_active_probe_count(),
        0,
        "#3011: non-terminal candidates must not reach branch_has_active_binding \
         (the per-agent binding scan)"
    );
    assert!(
        pruned.is_empty(),
        "non-terminal candidates must all be KEEP: {pruned:?}"
    );
    for name in ["feat/wip-a", "feat/wip-b", "feat/wip-c"] {
        assert!(
            branch_exists(&repo, name),
            "non-terminal branch {name} must survive the sweep"
        );
    }

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// #3011 positive control: a TERMINAL (merged) candidate must still probe
/// task/binding state FRESHLY, once per candidate — proving the skip above
/// is not a sweep-wide hoisted snapshot (which would produce a count of 1
/// no matter how many terminal candidates exist).
#[test]
fn terminal_candidate_still_probes_freshly_3011() {
    let _lock = ENV_LOCK.lock();
    let repo = setup_test_repo("3011-terminal");
    let home = tmp_home("3011-terminal");

    make_old_dated_branch(&repo, "feat/done-a", "2024-01-01T00:00:00 +0000");
    git_in(&repo, &["merge", "feat/done-a"]);
    make_old_dated_branch(&repo, "feat/done-b", "2024-01-01T00:00:00 +0000");
    git_in(&repo, &["merge", "feat/done-b"]);
    make_old_dated_branch(&repo, "feat/done-c", "2024-01-01T00:00:00 +0000");
    git_in(&repo, &["merge", "feat/done-c"]);

    // take-and-zero doubles as the reset: discard whatever the fixture setup
    // above happened to cost, so the sweep below starts from zero.
    let _ = crate::branch_sweep::take_active_task_probe_count();
    let _ = take_binding_active_probe_count();

    let pruned = prune_orphaned_branches_with_home(Some(&home), &repo, false);

    assert_eq!(
        crate::branch_sweep::take_active_task_probe_count(),
        3,
        "#3011: each terminal candidate must probe branch_has_active_task \
         freshly — 3 candidates, not 0 (skipped) and not 1 (a forbidden \
         hoisted sweep-wide snapshot)"
    );
    assert_eq!(
        take_binding_active_probe_count(),
        3,
        "#3011: each terminal candidate must probe branch_has_active_binding \
         freshly — 3 candidates, not 0 (skipped) and not 1 (a forbidden \
         hoisted sweep-wide snapshot)"
    );
    for name in ["feat/done-a", "feat/done-b", "feat/done-c"] {
        assert!(
            pruned.iter().any(|(b, r)| b == name && *r == "merged"),
            "terminal candidate {name} must actually be pruned: {pruned:?}"
        );
        assert!(
            !branch_exists(&repo, name),
            "terminal candidate {name} must actually be deleted"
        );
    }

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&home).ok();
}
