//! Worktree auto-cleanup v2 — runtime registry based.
//!
//! On by default; gated **opt-out** via `AGEND_WORKTREE_AUTO_CLEANUP=0`
//! (any other value, or unset, leaves it enabled — see `auto_cleanup_enabled`).
//! Sweeps worktrees whose branches are merged into main OR whose remote
//! tracking ref has been deleted (squash-merged PRs), using live
//! `binding.json` state (`binding::bound_source_repos`) to find repos and the
//! daemon's AgentConfig registry to detect in-use worktrees. Also prunes
//! orphaned local branches with no worktree.
//!
//! #2605: repo discovery used to come from `AgentConfig.worktree_source`, a
//! spawn-time cache keyed on the LEGACY `{repo}/.worktrees/...` layout — under
//! the current workspace-spawn + post-spawn-bind architecture this was always
//! empty, so this module's git-mutating paths have never actually run against
//! the canonical repo in production (see `BRANCH-AUDIT-20260704.md`). Fixing
//! repo discovery activates a delete path with zero production track record,
//! so real deletion is additionally gated **opt-in** via
//! `AGEND_WORKTREE_PRUNE_LIVE=1` (see `prune_live_enabled`) — default is
//! dry-run: candidates are logged (`tracing` + `event_log`), nothing is
//! deleted, until an operator diffs the dry-run output against a fresh audit
//! and flips the gate.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Returns true unless `AGEND_WORKTREE_AUTO_CLEANUP` is explicitly set to "0".
/// Cleanup is on by default — set `AGEND_WORKTREE_AUTO_CLEANUP=0` to disable.
pub fn auto_cleanup_enabled() -> bool {
    std::env::var("AGEND_WORKTREE_AUTO_CLEANUP")
        .ok()
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// #2605 first-activation safety gate: returns true only when
/// `AGEND_WORKTREE_PRUNE_LIVE` is explicitly set to "1". Unlike
/// `auto_cleanup_enabled` (opt-out, already-trusted feature), this is
/// deliberately **opt-in** and independent of it — the repo-discovery fix this
/// gate ships alongside activates a `git branch -D` / `git worktree remove`
/// path that has never run against the canonical repo before, so it must not
/// go live merely because the already-on `AGEND_WORKTREE_AUTO_CLEANUP` default
/// stays on. While this returns false, `sweep_from_registry` computes the
/// exact same eligibility (squash gate, worktree-occupancy fail-closed check)
/// but skips every mutating git call, logging would-delete candidates
/// instead.
pub fn prune_live_enabled() -> bool {
    std::env::var("AGEND_WORKTREE_PRUNE_LIVE")
        .ok()
        .is_some_and(|v| v == "1")
}

/// Entry for a git worktree.
#[derive(Debug, Clone)]
pub struct WorktreeEntry {
    pub path: String,
    pub branch: String,
}

/// List all git worktrees (excluding the main worktree).
///
/// #2550 W2: converged onto `git_worktree::list_porcelain` (via
/// `git_helpers::git_bypass`) — that parser flushes on the NEXT `worktree`
/// line plus an explicit final flush after the loop, so unlike the ad-hoc
/// blank-line-triggered loop this replaced, it does NOT depend on a
/// trailing blank-line record terminator (the exact fragility the old
/// TRIM-SENSITIVE comment here warned about — `git_bypass` doesn't trim
/// either way, so this was already safe, but the new parser isn't even
/// exposed to that failure mode). Adds the #1897 60s LOCAL_GIT_TIMEOUT
/// bound `git_bypass` provides — the raw `Command` this replaced had NO
/// timeout, unlike the other 3 porcelain call sites already converged here.
fn list_worktrees(repo_root: &Path) -> Vec<WorktreeEntry> {
    crate::git_worktree::list_porcelain(repo_root)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(path, branch)| {
            let branch = branch?;
            if branch == "main" || branch == "master" {
                return None;
            }
            Some(WorktreeEntry {
                path: path.display().to_string(),
                branch,
            })
        })
        .collect()
}

/// Check if a branch is merged into the default branch (local check, no API needed).
fn is_branch_merged(repo_root: &Path, branch: &str) -> bool {
    let default = crate::git_helpers::default_branch(repo_root);
    // W1.2: git_ok = always-bypass + bounded, true iff exit-0 (the
    // `output().map(success).unwrap_or(false)` idiom, byte-for-byte).
    crate::git_helpers::git_ok(
        repo_root,
        &["merge-base", "--is-ancestor", branch, &default],
    )
}

/// Check if a branch's remote tracking ref has been deleted (i.e. the PR was
/// squash-merged or the remote branch was deleted). This catches the common
/// case where `is_branch_merged` returns false because GitHub squash-merge
/// rewrites the commit hash.
fn is_remote_gone(repo_root: &Path, branch: &str) -> bool {
    // Read upstream tracking remote name
    // W1.2: git_cmd → trimmed stdout on success; the `success && !stdout.is_empty()`
    // filter becomes Ok-then-non-empty.
    let remote =
        crate::git_helpers::git_cmd(repo_root, &["config", &format!("branch.{branch}.remote")])
            .ok()
            .filter(|s| !s.is_empty());
    let Some(remote) = remote else {
        // No remote configured — not a remote-tracking branch, don't treat as "gone"
        return false;
    };
    // Check if the remote ref still exists
    let remote_ref = format!("refs/remotes/{remote}/{branch}");
    // git-raw-allowed: error→EXISTS (`unwrap_or(true)`) is a deliberate safe
    // default — a transient git error must NOT be read as "remote gone" (which
    // would auto-delete a live branch). `git_ok`'s error→false would INVERT this,
    // so do not "tidy" this into git_ok. (Already AGEND_GIT_BYPASS.)
    let exists = Command::new("git")
        .args(["rev-parse", "--verify", &remote_ref])
        .current_dir(repo_root)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(true);
    !exists
}

/// Check if a worktree has uncommitted changes.
fn is_worktree_dirty(worktree_path: &Path) -> bool {
    // git-raw-allowed: error→DIRTY (`unwrap_or(true)`) is a deliberate safe
    // default — a git error must protect uncommitted work, not let it be swept.
    // `git_ok`'s error→false would invert this (also: needs `!stdout.is_empty()`,
    // not exit-status). (Already AGEND_GIT_BYPASS.)
    Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree_path)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(true)
}

/// Remove a worktree, and delete its branch ONLY when `delete_branch` is set.
///
/// CR-2026-06-14 (data-loss): the worktree-DIR removal and the `git branch -D`
/// are DECOUPLED. Reclaiming the worktree directory is harmless, but deleting
/// the branch ref is irreversible — a remote-gone branch carrying
/// committed-but-unpushed local work would lose it. The caller passes
/// `delete_branch = true` only when the work is preserved in the default branch
/// (merged or squash-merged); otherwise the branch ref (and its unpushed
/// commits) survives even though the stale worktree dir is reclaimed.
///
/// On Windows, retries up to 3 times with exponential backoff (200ms, 400ms)
/// to absorb transient EACCES from file locks held by preceding git processes.
///
/// #2605: `dry_run` skips both git calls entirely and reports success (as if
/// removed) so the caller's candidate list reflects what WOULD happen —
/// see `prune_live_enabled`.
fn remove_worktree(
    repo_root: &Path,
    worktree_path: &str,
    branch: &str,
    delete_branch: bool,
    dry_run: bool,
) -> bool {
    if dry_run {
        return true;
    }
    let max_attempts: u32 = if cfg!(windows) { 3 } else { 1 };
    let mut wt_ok = false;
    for attempt in 0..max_attempts {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(100 * (1 << attempt)));
        }
        wt_ok = crate::git_helpers::git_ok(
            repo_root,
            &["worktree", "remove", "--force", worktree_path],
        );
        if wt_ok {
            break;
        }
    }
    if wt_ok && delete_branch {
        // W1.2: best-effort branch delete (result was already ignored).
        let _ = crate::git_helpers::git_ok(repo_root, &["branch", "-D", branch]);
    }
    wt_ok
}

/// Normalize a path: strip Windows `\\?\` UNC prefix.
fn normalize_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    PathBuf::from(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string())
}

/// Canonicalize `p`, or — when it does not exist yet — resolve symlinks on its
/// longest EXISTING ancestor and re-append the missing tail. Returns `None`
/// only when not even an existing ancestor canonicalizes.
///
/// #worktree-git-7: an active agent's `working_dir` recorded through a symlink
/// alias whose leaf is transiently absent (mid-creation) would fail
/// `canonicalize()` outright; the prior fail-OPEN fell back to the raw path,
/// whose textual prefix misses the canonical worktree → `is_in_use` returned
/// false and the sweep could remove a worktree out from under a live agent.
fn canonicalize_lenient(p: &Path) -> Option<PathBuf> {
    if let Ok(c) = p.canonicalize() {
        return Some(c);
    }
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cur = p;
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            tail.push(name);
        }
        if let Ok(c) = parent.canonicalize() {
            let mut out = c;
            out.extend(tail.iter().rev());
            return Some(out);
        }
        cur = parent;
    }
    None
}

/// Check if a worktree path is in use by any active agent.
fn is_in_use(wt_path: &Path, active_dirs: &[PathBuf]) -> bool {
    let wt_norm = normalize_path(
        &wt_path
            .canonicalize()
            .unwrap_or_else(|_| wt_path.to_path_buf()),
    );
    active_dirs.iter().any(|wd| match canonicalize_lenient(wd) {
        Some(canon) => {
            let wd_norm = normalize_path(&canon);
            wd_norm.starts_with(&wt_norm) || wd.starts_with(wt_path)
        }
        // Fail-CLOSED: not even an existing ancestor canonicalizes, so we cannot
        // prove this active dir is outside the worktree. Treat the ambiguity as
        // in-use rather than risk sweeping a worktree a live agent is using.
        None => true,
    })
}

/// Runtime-based sweep: uses live `binding.json` state to find repos and
/// AgentConfig working_dirs to detect in-use worktrees.
///
/// `home`: daemon home — repo discovery reads every live agent's
/// `binding.json` fresh via `binding::bound_source_repos` (#2605; self-heals
/// for binds that happen after spawn, unlike the old spawn-time cache).
/// `configs`: map of agent name → working_dir from daemon's live registry.
/// `fleet_dirs`: fallback working_directories from fleet.yaml for stopped agents.
///
/// Returns list of (branch, path, reason) that were removed — or, while
/// `prune_live_enabled()` is false, that WOULD have been removed (dry-run
/// candidates; no git mutation beyond the always-on `fetch --prune` below).
/// `reason` is one of `"merged"` / `"remote-gone"` / `"squash-merged"` — the
/// ACTUAL eligibility signal, not a hardcoded guess (#2605 review finding:
/// the dry-run/audit-diff this PR exists for is meaningless if every
/// candidate claims to be "merged" regardless of why it was really swept).
/// `path` is `"(no worktree)"` for phase-2 orphan branches, which never had
/// one — never an empty string standing in for a real value.
pub fn sweep_from_registry(
    home: &Path,
    configs: &HashMap<String, Option<PathBuf>>,
    fleet_dirs: &[PathBuf],
) -> Vec<(String, String, &'static str)> {
    if !auto_cleanup_enabled() {
        return Vec::new();
    }
    let dry_run = !prune_live_enabled();

    let repos: HashSet<PathBuf> = crate::binding::bound_source_repos(home)
        .into_iter()
        .collect();
    let mut active_dirs: Vec<PathBuf> = configs.values().flatten().cloned().collect();
    // Add fleet.yaml dirs as fallback for stopped agents
    active_dirs.extend(fleet_dirs.iter().cloned());

    let mut removed = Vec::new();

    for repo in &repos {
        // #2605: fetch runs UNCONDITIONALLY, including during dry_run — unlike
        // e.g. `worktree_pool::cleanup_merged_branch`'s dry-run-skips-fetch
        // convention, this sweep's dry-run window exists partly to observe the
        // fetch itself: `repos` was always empty before #2605, so this
        // background `fetch --prune` has never actually run against the
        // canonical repo in production. The operator needs to see its real
        // behavior (including failures) during the observation period.
        //
        // #2004: fail-direction is safe (stale local refs → merge/gone checks
        // below run on possibly-stale data, never MORE aggressive than
        // reality — a real merge/squash is never missed by staying stale, it
        // just waits for the next successful fetch), but a persistently
        // failing fetch accumulates undeletable branches invisibly — surface
        // it. Pure logging, the sweep proceeds on local refs.
        let remote = crate::git_helpers::primary_remote(repo);
        match crate::git_helpers::git_bypass(repo, &["fetch", "--prune", &remote]) {
            Ok(o) if !o.status.success() => {
                tracing::warn!(
                    repo = %repo.display(),
                    remote = %remote,
                    stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                    "fetch --prune failed during worktree/branch sweep — merge/gone checks run on possibly-stale local refs"
                );
            }
            Err(e) => {
                tracing::warn!(
                    repo = %repo.display(),
                    remote = %remote,
                    error = %e,
                    "fetch --prune could not run during worktree/branch sweep — merge/gone checks run on possibly-stale local refs"
                );
            }
            Ok(_) => {}
        }

        // CR-2026-06-14: needed to decide whether a stale worktree's branch is
        // safe to `branch -D` (its work is in the default branch) vs must be kept
        // (committed-but-unpushed local work that a remote-gone signal alone
        // would otherwise destroy). Mirrors the phase-2 `prune_orphaned_branches`
        // safety gate.
        let default = crate::git_helpers::default_branch(repo);

        // Phase 1: clean worktrees (existing logic + remote-gone)
        let entries = list_worktrees(repo);
        for entry in &entries {
            let wt_path = Path::new(&entry.path);

            if is_in_use(wt_path, &active_dirs) {
                tracing::debug!(branch = %entry.branch, path = %entry.path, "skipping worktree (in use by agent)");
                continue;
            }

            if is_worktree_dirty(wt_path) {
                tracing::debug!(branch = %entry.branch, path = %entry.path, "skipping dirty worktree");
                continue;
            }

            let merged = is_branch_merged(repo, &entry.branch);
            let gone = is_remote_gone(repo, &entry.branch);
            if !merged && !gone {
                continue;
            }

            // CR-2026-06-14 (data-loss): reclaim the stale worktree DIR on
            // (merged || gone), but `branch -D` ONLY when the work is preserved
            // in the default branch — merged (ancestor) or squash-merged. A
            // remote-gone worktree whose branch is NEITHER carries
            // committed-but-unpushed local work; deleting the ref would lose it
            // irrecoverably (phase-1 only skips *dirty* worktrees, not
            // committed-but-unpushed ones). The worktree dir is still reclaimed.
            let branch_safe_to_delete =
                merged || is_squash_gc_eligible(repo, &entry.branch, &default);
            let reason = if merged { "merged" } else { "remote-gone" };

            tracing::info!(
                branch = %entry.branch,
                path = %entry.path,
                reason,
                delete_branch = branch_safe_to_delete,
                dry_run,
                "removing stale worktree"
            );
            if remove_worktree(
                repo,
                &entry.path,
                &entry.branch,
                branch_safe_to_delete,
                dry_run,
            ) {
                removed.push((entry.branch.clone(), entry.path.clone(), reason));
            }
        }

        // Phase 2: prune orphaned branches (no worktree, remote gone or merged)
        prune_stale_worktrees(repo, dry_run);
        let pruned = prune_orphaned_branches(repo, dry_run);
        for (branch, reason) in pruned {
            removed.push((branch, "(no worktree)".to_string(), reason));
        }
    }
    removed
}

/// #1750-B3: minimum branch-tip age before the SQUASH-merged path will auto-GC
/// a branch. The `--merged`/remote-gone signals are definitive and need no age
/// belt, but the cherry/tree-diff squash detection is heuristic — a young branch
/// that happens to be tree-equal to main (or a PR merged moments ago that a
/// human may still follow up on locally) is left for a later tick. A
/// genuinely-orphaned squash-merged branch's tip predates the merge, so it
/// clears this floor on the next sweep.
const SQUASH_GC_MIN_TIP_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// #1750-B3: age of `branch`'s tip commit (committer date), or `None` if it
/// can't be resolved. `%ct` is a unix timestamp (seconds), so no date parsing.
fn branch_tip_age(repo_root: &Path, branch: &str) -> Option<Duration> {
    // W1.2: git_cmd → trimmed stdout; spawn-error + non-zero both collapse to `None`.
    let ts_str =
        crate::git_helpers::git_cmd(repo_root, &["log", "-1", "--format=%ct", branch]).ok()?;
    let ts: u64 = ts_str.parse().ok()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(Duration::from_secs(now.saturating_sub(ts)))
}

/// #1750-B3: is `branch` a squash-merge orphan eligible for auto-GC? True when
/// it is squash-merged into the default branch AND its tip is older than
/// [`SQUASH_GC_MIN_TIP_AGE`]. Reuses `branch_sweep`'s detection (git cherry +
/// #1280 tree-diff fallback) so the auto path matches the operator sweep.
///
/// t-...50899-10: `pub(crate)` so `worktree_pool::cleanup_merged_branch` reuses
/// the SAME squash-safe delete gate this file's `prune_orphaned_branches`
/// uses, instead of treating a remote-gone branch as independently deletable.
pub(crate) fn is_squash_gc_eligible(repo_root: &Path, branch: &str, default: &str) -> bool {
    crate::branch_sweep::is_squash_merged(repo_root, default, branch)
        && branch_tip_age(repo_root, branch).is_some_and(|age| age >= SQUASH_GC_MIN_TIP_AGE)
}

/// Run `git worktree prune` then delete local branches whose remote tracking
/// ref is gone, that are merged into main, or that are squash-merge orphans
/// (#1750-B3). Skips branches checked out in any worktree.
///
/// #2605: `dry_run` computes the exact same eligibility (merged/squash gate,
/// worktree-occupancy skip) but skips the actual `git branch -D` — eligible
/// branches are still returned (with their real reason: `"merged"` or
/// `"squash-merged"`) so the caller can log/audit the candidate list.
fn prune_orphaned_branches(repo_root: &Path, dry_run: bool) -> Vec<(String, &'static str)> {
    let default = crate::git_helpers::default_branch(repo_root);
    // Collect branches currently checked out in worktrees — cannot delete these
    let wt_branches: HashSet<String> = list_worktrees(repo_root)
        .into_iter()
        .map(|e| e.branch)
        .collect();

    // W1.2: git_cmd → trimmed stdout on success; spawn-error + non-zero collapse to `Err → []`.
    let branches: Vec<String> =
        match crate::git_helpers::git_cmd(repo_root, &["branch", "--format=%(refname:short)"]) {
            Ok(stdout) => stdout
                .lines()
                .filter(|b| *b != default.as_str())
                .map(String::from)
                .collect(),
            _ => return Vec::new(),
        };

    let mut pruned = Vec::new();
    for branch in &branches {
        if wt_branches.contains(branch) {
            continue;
        }
        let merged = is_branch_merged(repo_root, branch);
        // #1750-B3: also reap squash-merge orphans (the 95/99 case the
        // squash-blind `--merged` missed) — gated on tip-age for the heuristic
        // squash detection only.
        //
        // CR-2026-06-14: `is_remote_gone` is NO LONGER an independent delete
        // trigger. Remote-gone alone is not proof the branch's work is preserved
        // — a branch pushed once, then deleted on the remote while local commits
        // kept accruing, is remote-gone yet carries committed-but-unpushed work
        // that `git branch -D` destroys irrecoverably. Reap a branch ONLY when
        // its work IS in the default branch (every commit reachable): merged
        // (ancestor) or squash-merged. A remote-gone branch that is NEITHER has
        // unpushed local commits → KEEP. A squash-merged branch whose remote was
        // auto-deleted stays reapable — it is now caught by the squash check
        // (which no longer excludes the gone case) instead of the unguarded
        // remote-gone trigger.
        let squash = !merged && is_squash_gc_eligible(repo_root, branch, &default);
        if !merged && !squash {
            continue;
        }
        let ok = dry_run || crate::git_helpers::git_ok(repo_root, &["branch", "-D", branch]);
        if ok {
            let reason = if merged { "merged" } else { "squash-merged" };
            if dry_run {
                tracing::info!(branch, reason, "would prune orphaned branch (dry-run)");
            } else {
                tracing::info!(branch, reason, "pruned orphaned branch");
            }
            pruned.push((branch.clone(), reason));
        }
    }
    pruned
}

/// Run `git worktree prune` to clean stale worktree bookkeeping entries.
///
/// #2605: `dry_run` skips this entirely — it's admin-metadata-only (no branch
/// or file content is touched), but the first-activation gate treats ANY git
/// mutation as out of scope for the observation window, not just data-loss-risk
/// ones, to keep the dry-run/live boundary a single simple rule.
fn prune_stale_worktrees(repo_root: &Path, dry_run: bool) {
    if dry_run {
        return;
    }
    // W1.2: best-effort prune (result was already ignored).
    let _ = crate::git_helpers::git_ok(repo_root, &["worktree", "prune"]);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn setup_test_repo(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "agend-wt-v2-{}-{}-{}",
            tag,
            std::process::id(),
            C.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).ok();
        git_in(&dir, &["init", "-b", "main"]);
        std::fs::write(dir.join("README.md"), "init").ok();
        git_in(&dir, &["add", "."]);
        git_in(&dir, &["commit", "-m", "init"]);
        dir
    }

    fn git_in(dir: &Path, args: &[&str]) {
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

    // ── #2605: first-activation safety gate ──

    #[test]
    fn prune_live_disabled_by_default() {
        let _lock = ENV_LOCK.lock();
        std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
        assert!(
            !prune_live_enabled(),
            "must default to dry-run — this path has zero production track record"
        );
    }

    #[test]
    fn prune_live_disabled_for_non_1_value() {
        let _lock = ENV_LOCK.lock();
        std::env::set_var("AGEND_WORKTREE_PRUNE_LIVE", "true");
        assert!(
            !prune_live_enabled(),
            "opt-in gate must require the exact value \"1\", not any truthy-looking string"
        );
        std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
    }

    #[test]
    fn prune_live_enabled_when_set_to_1() {
        let _lock = ENV_LOCK.lock();
        std::env::set_var("AGEND_WORKTREE_PRUNE_LIVE", "1");
        assert!(prune_live_enabled());
        std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE");
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

    #[test]
    fn test_sweep_dry_run_by_default_identifies_but_does_not_delete() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("v2-dry-run");
        git_in(&repo, &["branch", "feat/done"]);
        let wt = repo.join("wt-done");
        git_in(
            &repo,
            &["worktree", "add", wt.to_str().unwrap(), "feat/done"],
        );
        git_in(&repo, &["merge", "feat/done"]);

        std::env::set_var("AGEND_WORKTREE_AUTO_CLEANUP", "1");
        std::env::remove_var("AGEND_WORKTREE_PRUNE_LIVE"); // explicit: default dry-run
        let home = tmp_home("v2-dry-run");
        let mut configs = HashMap::new();
        configs.insert("other-agent".to_string(), Some(repo.join("other")));
        write_source_repo_binding(&home, "other-agent", &repo);
        let removed = sweep_from_registry(&home, &configs, &[]);
        assert!(
            removed.iter().any(|(b, _, _)| b == "feat/done"),
            "dry-run must still report the candidate: {removed:?}"
        );
        assert!(
            wt.exists(),
            "dry-run (AGEND_WORKTREE_PRUNE_LIVE unset) must NOT actually remove the worktree"
        );
        std::env::remove_var("AGEND_WORKTREE_AUTO_CLEANUP");
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn test_v2_merged_worktree_removed() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("v2-merged");
        git_in(&repo, &["branch", "feat/done"]);
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
    fn git_commit_dated(dir: &Path, msg: &str, date: &str) {
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
    fn prune_skips_unmerged_branch_1750_b3() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("b3-unmerged");
        // A genuinely unmerged branch (old tip) — squash detection must NOT fire.
        git_in(&repo, &["checkout", "-b", "feat/wip"]);
        std::fs::write(repo.join("feat.txt"), "wip").ok();
        git_in(&repo, &["add", "."]);
        git_commit_dated(&repo, "wip", "2024-01-01T00:00:00 +0000");
        git_in(&repo, &["checkout", "main"]);

        assert!(
            !crate::branch_sweep::is_squash_merged(&repo, "main", "feat/wip"),
            "precondition: NOT squash-merged"
        );
        let pruned = prune_orphaned_branches(&repo, false);
        assert!(
            !pruned.iter().any(|(b, _)| b == "feat/wip"),
            "#1750-B3: a real unmerged branch must NOT be GC'd"
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
        git_in(&repo, &["branch", "feat/merged"]);
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
}

#[cfg(test)]
mod review_repro_worktree_git;
