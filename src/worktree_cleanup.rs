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
//! empty, so this module's git-mutating paths had never actually run against
//! the canonical repo in production (see `BRANCH-AUDIT-20260704.md`). Fixing
//! repo discovery activated a delete path that was, for a rollout window,
//! gated opt-in via `AGEND_WORKTREE_PRUNE_LIVE`, then flipped to default-LIVE
//! opt-out (PR-A, #2695). PR-D6 RETIRED that gate entirely: sweep gating is now
//! `AGEND_WORKTREE_AUTO_CLEANUP` ONLY (on ⇒ live sweep, real deletion; `"0"` ⇒
//! no sweep). `AGEND_WORKTREE_PRUNE_LIVE` is ignored — a boot-time check
//! (`warn_if_prune_live_retired`) fails LOUD if an operator still has it set.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

mod occupancy;
pub(crate) use occupancy::{binding_scan_all_strict, bound_worktree_paths_or_ambiguous, is_in_use};

/// Returns true unless `AGEND_WORKTREE_AUTO_CLEANUP` is explicitly set to "0".
/// Cleanup is on by default — set `AGEND_WORKTREE_AUTO_CLEANUP=0` to disable.
pub fn auto_cleanup_enabled() -> bool {
    std::env::var("AGEND_WORKTREE_AUTO_CLEANUP")
        .ok()
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// PR-D6: `AGEND_WORKTREE_PRUNE_LIVE` is RETIRED. Sweep gating collapsed to
/// `AGEND_WORKTREE_AUTO_CLEANUP` only — when cleanup is enabled the sweep always
/// mutates (live); there is no dry-run env toggle anymore. History: the flag was
/// a first-activation dry-run/live selector (opt-in, then default-LIVE opt-out
/// under PR-A #2695) for a delete path that has since matured over a long
/// operator soak; a second, separate dry-run toggle overlapping the master
/// off-switch was pure confusion surface.
///
/// FAIL-LOUD, not silent: if an operator still has the retired var set (any
/// value), warn once at daemon boot so they learn it is ignored. Returns whether
/// the retired var was present (so a test can assert the boot-warn path fired);
/// the daemon calls this for its logging side effect at startup.
pub(crate) fn warn_if_prune_live_retired() -> bool {
    if std::env::var("AGEND_WORKTREE_PRUNE_LIVE").is_ok() {
        tracing::warn!(
            "AGEND_WORKTREE_PRUNE_LIVE is retired and ignored — sweep gating is now \
             AGEND_WORKTREE_AUTO_CLEANUP only"
        );
        true
    } else {
        false
    }
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
///
/// PR-B rider (dev2 #2695 seat2): **fail-CLOSED**. A `git worktree list`
/// failure is an AMBIGUITY, not "no worktrees" — swallowing it (the former
/// `unwrap_or_default()`) collapsed the occupancy dimension to empty, so a
/// caller's "not occupied → eligible to delete" check would fail-OPEN and could
/// reap a branch whose worktree is merely un-enumerable. The error now
/// propagates as `Err(())` (logged once here) and every caller skips its
/// mutating work for this tick (mirrors the `bound_worktree_paths_or_ambiguous`
/// fail-closed convention below).
fn list_worktrees(repo_root: &Path) -> Result<Vec<WorktreeEntry>, ()> {
    // Call git directly rather than `git_worktree::list_porcelain`, which
    // collapses a NON-ZERO exit into `Ok(Vec::new())` (that swallow is exactly
    // half of the fail-OPEN dev2 flagged). Here BOTH a spawn error AND a
    // non-zero `git worktree list` exit surface as `Err(())` so the caller skips
    // its removals this tick instead of proceeding on a collapsed occupancy view.
    let out = crate::git_helpers::git_bypass(repo_root, &crate::git_worktree::LIST_PORCELAIN_ARGS)
        .map_err(|e| {
            tracing::warn!(
                repo = %repo_root.display(),
                error = %e,
                "list_worktrees: git worktree enumeration failed to spawn — caller fails closed"
            );
        })?;
    if !out.status.success() {
        tracing::warn!(
            repo = %repo_root.display(),
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "list_worktrees: git worktree list exited non-zero — occupancy dimension \
             would collapse; caller fails closed (skips its removals this tick)"
        );
        return Err(());
    }
    let canonical_repo = repo_root.canonicalize().ok();
    Ok(
        crate::git_worktree::parse_porcelain(&String::from_utf8_lossy(&out.stdout))
            .into_iter()
            .filter_map(|(path, branch)| {
                let branch = branch?;
                if branch == "main" || branch == "master" {
                    return None;
                }
                let is_canonical_repo = canonical_repo
                    .as_ref()
                    .and_then(|repo| path.canonicalize().ok().map(|worktree| worktree == *repo))
                    .unwrap_or(false);
                if is_canonical_repo {
                    return None;
                }
                Some(WorktreeEntry {
                    path: path.display().to_string(),
                    branch,
                })
            })
            .collect(),
    )
}

/// Check if a branch is merged into the default branch (local check, no API needed).
fn is_branch_merged(repo_root: &Path, branch: &str) -> bool {
    let default = crate::git_helpers::default_branch(repo_root);
    if branch == default {
        return false;
    }
    // W1.2: git_ok = always-bypass + bounded, true iff exit-0 (the
    // `output().map(success).unwrap_or(false)` idiom, byte-for-byte).
    if !crate::git_helpers::git_ok(
        repo_root,
        &["merge-base", "--is-ancestor", branch, &default],
    ) {
        return false;
    }
    // #t-…81457-1: is-ancestor is trivially TRUE when `branch`'s tip IS
    // `default`'s tip — indistinguishable, from git state alone, between a
    // brand-new zero-commit branch (nothing ever merged — dev3's PRUNE_LIVE
    // incident) and a genuinely fast-forward-merged branch (whose tip
    // legitimately became identical to default's). There is no git-content
    // signal that tells these apart. Reuse the SAME `SQUASH_GC_MIN_TIP_AGE`
    // floor the squash path already relies on for the identical reason: only
    // trust "merged" once the shared tip has sat for a while, giving a
    // just-created branch's binding-registry entry (fix #1, the primary
    // defense) time to be observed even if that check somehow lagged.
    let Some((_, tip_ts)) = branch_tip_info(repo_root, branch) else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Duration::from_secs(now.saturating_sub(tip_ts)) >= SQUASH_GC_MIN_TIP_AGE
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
    // #t-…81457-1: `refs/remotes/{remote}/{branch}`'s absence only means "the
    // remote branch was deleted" when the branch was ever tracking THAT same
    // remote branch to begin with. `bind_self`'s branch creation (`git branch
    // <name> origin/main`) auto-sets `branch.<name>.merge = refs/heads/main`
    // (tracks origin/main, not a same-named remote branch) — a branch that's
    // simply never been pushed under its own name would otherwise be
    // conflated with "was pushed, remote then deleted" and misclassified as
    // gone. Self-reproduced live: this agent's own fresh worktree and
    // gapfix-dev2's were both reaped this way within ~70s of bind, before
    // either had pushed. Require the upstream `merge` ref to actually be
    // `refs/heads/<branch>` first; on any ambiguity (missing/mismatched)
    // prefer NOT concluding gone — a false negative just waits for the next
    // sweep once the branch is genuinely pushed-then-orphaned, a false
    // positive is an irrecoverable delete.
    let merge_ref =
        crate::git_helpers::git_cmd(repo_root, &["config", &format!("branch.{branch}.merge")]).ok();
    if merge_ref.as_deref() != Some(format!("refs/heads/{branch}").as_str()) {
        return false;
    }
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
/// removed) so a caller's candidate list can reflect what WOULD happen. PR-D6:
/// the runtime sweep always passes `false` now (gating is `AUTO_CLEANUP` only);
/// the param is retained for the audit-style callers/tests that still exercise
/// the compute-but-don't-mutate path.
fn remove_worktree(
    repo_root: &Path,
    worktree_path: &str,
    branch: &str,
    delete_branch: bool,
    dry_run: bool,
) -> Result<(), String> {
    if dry_run {
        return Ok(());
    }
    let max_attempts: u32 = if cfg!(windows) { 3 } else { 1 };
    let mut wt_ok = false;
    let mut failure = None;
    for attempt in 0..max_attempts {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(100 * (1 << attempt)));
        }
        match crate::git_helpers::git_bypass(
            repo_root,
            &["worktree", "remove", "--force", worktree_path],
        ) {
            Ok(output) if output.status.success() => {
                wt_ok = true;
                break;
            }
            Ok(output) => {
                let status = output
                    .status
                    .code()
                    .map(|code| format!("exit status {code}"))
                    .unwrap_or_else(|| output.status.to_string());
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let stderr = if stderr.is_empty() {
                    "<empty>".to_string()
                } else {
                    stderr
                };
                failure = Some(format!(
                    "sweep worktree remove failed: {status}; stderr: {stderr}"
                ));
            }
            Err(error) => {
                failure = Some(format!(
                    "sweep worktree remove failed: could not execute git: {error}"
                ));
            }
        }
    }
    if wt_ok && delete_branch {
        // W1.2: best-effort branch delete (result was already ignored).
        let _ = crate::git_helpers::git_ok(repo_root, &["branch", "-D", branch]);
    }
    if wt_ok {
        Ok(())
    } else {
        Err(failure
            .unwrap_or_else(|| "sweep worktree remove failed: no attempt result".to_string()))
    }
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
/// Returns list of (branch, path, reason) that were removed. PR-D6: gating is
/// `AGEND_WORKTREE_AUTO_CLEANUP` only — when cleanup is enabled the sweep runs
/// LIVE (real removal); there is no dry-run env toggle. `reason` is one of
/// `"merged"` / `"remote-gone"` / `"squash-merged"` — the
/// ACTUAL eligibility signal, not a hardcoded guess (#2605 review finding:
/// the dry-run/audit-diff this PR exists for is meaningless if every
/// candidate claims to be "merged" regardless of why it was really swept).
/// `path` is `"(no worktree)"` for phase-2 orphan branches, which never had
/// one — never an empty string standing in for a real value.
/// V1 (d-20260712065632138568-7): best-effort durable hygiene alert — a board
/// write failure must never abort the sweep itself.
fn upsert_hygiene(home: &Path, key: String, title: String, evidence: serde_json::Value) {
    match crate::daemon::hygiene_task::upsert_system_hygiene_task(home, &key, &title, evidence) {
        Ok(outcome) => {
            tracing::info!(key = %key, task = %outcome.task_id().0, "hygiene task upserted");
        }
        Err(e) => tracing::warn!(error = %e, key = %key, "hygiene task upsert failed"),
    }
}

pub fn sweep_from_registry(
    home: &Path,
    configs: &HashMap<String, Option<PathBuf>>,
    fleet_dirs: &[PathBuf],
) -> Vec<(String, String, &'static str)> {
    if !auto_cleanup_enabled() {
        return Vec::new();
    }

    let repos: HashSet<PathBuf> = crate::binding::all_managed_repos(home)
        .into_iter()
        .collect();
    let mut active_dirs: Vec<PathBuf> = configs.values().flatten().cloned().collect();
    // Add fleet.yaml dirs as fallback for stopped agents
    active_dirs.extend(fleet_dirs.iter().cloned());
    // #t-…81457-1: `configs` (in-memory AgentConfig.working_dir) is a SEPARATE
    // registry from binding.json, updated on its own schedule — a worktree the
    // daemon itself just auto-bound (binding.json written) can be invisible to
    // `configs` until it catches up. binding.json is read fresh every call and
    // is authoritative for "is anyone bound here right now", so feed it into
    // the same occupancy check directly instead of relying solely on `configs`.
    //
    // reviewer4 REJECTED r0 of this fix: an unreadable/corrupt binding.json is
    // an AMBIGUITY, not an absence — it could be hiding a live worktree. Fail
    // the whole round closed (no removals) rather than treat it as "not
    // bound"; deletion is auto-run, "寧可漏收不可誤收". A merely-missing file
    // (agent never bound) is the normal steady state and does not trigger this.
    match bound_worktree_paths_or_ambiguous(home) {
        Ok(paths) => active_dirs.extend(paths),
        Err(()) => {
            tracing::warn!(
                "worktree-reclaim: an unreadable/corrupt binding.json was found — \
                 skipping ALL removals this sweep tick (fail-closed); will retry \
                 next tick"
            );
            return Vec::new();
        }
    }

    crate::cleanup_intents::reconcile_terminal_review_intents(home, false);

    let mut removed = Vec::new();

    for repo in &repos {
        // #2605: fetch runs UNCONDITIONALLY as the first step of every sweep —
        // `repos` was always empty before #2605, so this background
        // `fetch --prune` had never actually run against the canonical repo in
        // production; surfacing its real behavior (including failures) matters
        // regardless of what the eligibility checks below decide.
        //
        // #2004: fail-direction is safe (stale local refs → merge/gone checks
        // below run on possibly-stale data, never MORE aggressive than
        // reality — a real merge/squash is never missed by staying stale, it
        // just waits for the next successful fetch), but a persistently
        // failing fetch accumulates undeletable branches invisibly — surface
        // it. Pure logging, the sweep proceeds on local refs.
        let remote = crate::git_helpers::primary_remote(repo);
        // V1 (d-20260712065632138568-7): a failing fetch is a persistent-
        // ambiguity signal (undeletable branches accumulate invisibly, #2004)
        // — surfaced as a durable hygiene task, no longer log-only. The upsert
        // dedups on the episode key, so a repo stuck failing every tick keeps
        // ONE task (occurrences counts the ticks).
        let fetch_fail = match crate::git_helpers::git_bypass(repo, &["fetch", "--prune", &remote])
        {
            Ok(o) if !o.status.success() => {
                let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                tracing::warn!(
                    repo = %repo.display(),
                    remote = %remote,
                    stderr = %stderr,
                    "fetch --prune failed during worktree/branch sweep — merge/gone checks run on possibly-stale local refs"
                );
                Some(format!("fetch --prune failed: {stderr}"))
            }
            Err(e) => {
                tracing::warn!(
                    repo = %repo.display(),
                    remote = %remote,
                    error = %e,
                    "fetch --prune could not run during worktree/branch sweep — merge/gone checks run on possibly-stale local refs"
                );
                Some(format!("fetch --prune could not run: {e}"))
            }
            Ok(_) => None,
        };
        if let Some(reason) = fetch_fail {
            upsert_hygiene(
                home,
                format!("residue-fetch-degraded:{}", repo.display()),
                format!("[hygiene] fetch --prune failing: {}", repo.display()),
                serde_json::json!({
                    "repo": repo.display().to_string(),
                    "remote": remote,
                    "reason": reason,
                }),
            );
        }

        // CR-2026-06-14: needed to decide whether a stale worktree's branch is
        // safe to `branch -D` (its work is in the default branch) vs must be kept
        // (committed-but-unpushed local work that a remote-gone signal alone
        // would otherwise destroy). Mirrors the phase-2 `prune_orphaned_branches`
        // safety gate.
        let default = crate::git_helpers::default_branch(repo);

        // Phase 1: clean worktrees (existing logic + remote-gone)
        // PR-B rider: fail-closed — if worktree enumeration errors, skip this
        // repo's reclaim this tick rather than proceed with a collapsed occupancy view.
        let entries = match list_worktrees(repo) {
            Ok(e) => e,
            Err(()) => continue,
        };
        for entry in &entries {
            let wt_path = Path::new(&entry.path);

            // PR-D·D3: occupancy/marker protection (L0) delegated to the shared
            // `disposition::l0_protected` — the SAME fail-direction the classifier
            // applies (in-use `Some(true)` / unresolvable `None` → protected). The
            // sweep never consulted the marker/pin dims, so they are pass-through
            // (daemon_managed=true, pinned=false) → byte-identical to the prior
            // `is_in_use`-only skip. (The tick-level `list_worktrees Err → continue`
            // above is the `None`/unresolvable arm of the same L0 fail-direction.)
            if crate::worktree::disposition::l0_protected(
                true,
                false,
                Some(is_in_use(wt_path, &active_dirs)),
            ) {
                tracing::debug!(branch = %entry.branch, path = %entry.path, "skipping worktree (in use by agent — L0 protected)");
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
            // PR-D·D3: the branch-safe-to-delete (L4) decision delegates to
            // `branch_disposition` via `branch_reap_delete`. `merged || squash` is
            // the ProvablyInDefault signal; `gone` alone keeps the branch
            // (CR-2026-06-14). No scaffold-TTL arm here — that is phase-2's
            // orphaned-branch concern (this entry still HAS a worktree).
            let branch_safe_to_delete = branch_reap_delete(
                merged || is_squash_gc_eligible(repo, &entry.branch, &default),
                gone,
                false,
            );
            let reason = if merged { "merged" } else { "remote-gone" };

            tracing::info!(
                branch = %entry.branch,
                path = %entry.path,
                reason,
                delete_branch = branch_safe_to_delete,
                "removing stale worktree"
            );
            // PR-D·D5: route the dir-Delete through the shared `janitor::dispose`
            // Delete arm — the sweep passes its OWN remover (`remove_worktree`:
            // git_bypass + windows-retry + `branch -D`), so its deliberate wrapper is
            // preserved (D5-Q3 ruling B). The remover returns its final command
            // diagnostics directly as the janitor error. `agent`/`candidate` are
            // inert on the Delete arm; the Err reason is unused here (the sweep
            // skips, it does not archive-fallthrough).
            let outcome = crate::daemon::janitor::dispose(
                home,
                crate::worktree::disposition::Disposition::Delete,
                "",
                None,
                || {
                    remove_worktree(
                        repo,
                        &entry.path,
                        &entry.branch,
                        branch_safe_to_delete,
                        false,
                    )
                },
            );
            match outcome {
                crate::daemon::janitor::DispositionOutcome::Deleted(Ok(())) => {
                    removed.push((entry.branch.clone(), entry.path.clone(), reason));
                }
                // V1 (d-20260712065632138568-7): a PROVEN-eligible candidate
                // whose removal failed may not be silently skipped — that is
                // the actionable residue signal (replaces the raw-count alarm).
                crate::daemon::janitor::DispositionOutcome::Deleted(Err(fail)) => {
                    upsert_hygiene(
                        home,
                        format!("residue-remove-failed:{}:{}", repo.display(), entry.branch),
                        format!("[hygiene] worktree remove failed: {}", entry.branch),
                        serde_json::json!({
                            "repo": repo.display().to_string(),
                            "branch": entry.branch,
                            "path": entry.path,
                            "reason": fail,
                            "eligibility": {
                                "merged": merged,
                                "remote_gone": gone,
                                "delete_branch": branch_safe_to_delete,
                            },
                        }),
                    );
                }
                _ => {}
            }
        }

        // Phase 2: prune orphaned branches (no worktree, remote gone or merged).
        // PR-D6: sweep is always live now (gated by AUTO_CLEANUP only), so the
        // helpers' still-supported `dry_run` param is passed `false` here.
        prune_stale_worktrees(repo, false);
        let pruned = prune_orphaned_branches_with_home(Some(home), repo, false);
        for (branch, reason) in pruned {
            removed.push((branch, "(no worktree)".to_string(), reason));
        }
    }
    // Durable retry: settle any cleanup intents whose branches are now
    // confirmed merged. This is the retry consumer for intents that survived
    // a failed poller settlement or whose CI watch was removed before the
    // settlement succeeded.
    crate::cleanup_intents::sweep_settle_merged(home);
    removed
}

/// #1750-B3: minimum branch-tip age before the SQUASH-merged path will auto-GC
/// a branch. The `--merged`/remote-gone signals are definitive and need no age
/// belt, but the cherry/tree-diff squash detection is heuristic — a young branch
/// that happens to be tree-equal to main (or a PR merged moments ago that a
/// human may still follow up on locally) is left for a later tick. A
/// genuinely-orphaned squash-merged branch's tip predates the merge, so it
/// clears this floor on the next sweep.
// #P3 (branch-residue): `pub(crate)` so `worktree_pool::merged_branch_disposition`
// can phrase its "younger than the {N}h GC floor" keep reason with the SAME N.
pub(crate) const SQUASH_GC_MIN_TIP_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// PR-A P1 (branch-residue RCA §3): a reviewer-checkout scaffolding branch
/// (`review/*` etc. — see [`crate::branch_sweep::is_reviewer_checkout`]) whose
/// tip has been idle at least this long is GC-eligible. Such branches never
/// carry a PR and never merge, so neither the merged nor the squash reap path
/// ever fires — a TTL is their only live terminal path. Longer than
/// [`SQUASH_GC_MIN_TIP_AGE`] (24h) because a review can legitimately sit idle
/// across a weekend; a checked-out (in-progress) review is protected by the
/// worktree-occupancy check regardless of age.
const REVIEW_SCAFFOLD_TTL: Duration = Duration::from_secs(72 * 60 * 60);

/// #1750-B3/#P1-2607: `branch`'s tip SHA + committer-date unix timestamp, in
/// ONE `git log` call (`%H%x09%ct`) — was two separate concerns (a `rev-parse`
/// for the SHA, a `log --format=%ct` for the age) collapsed into one spawn,
/// since #P1-2607's cache needs the tip SHA as its key anyway.
fn branch_tip_info(repo_root: &Path, branch: &str) -> Option<(String, u64)> {
    // W1.2: git_cmd → trimmed stdout; spawn-error + non-zero both collapse to `None`.
    let out = crate::git_helpers::git_cmd(repo_root, &["log", "-1", "--format=%H%x09%ct", branch])
        .ok()?;
    let (sha, ts_str) = out.split_once('\t')?;
    let ts: u64 = ts_str.parse().ok()?;
    Some((sha.to_string(), ts))
}

/// PR-A P1 (branch-residue RCA §3): is `branch` a disposable reviewer-checkout
/// scaffolding branch whose tip has aged past [`REVIEW_SCAFFOLD_TTL`]? Reuses
/// the single-source [`crate::branch_sweep::is_reviewer_checkout`] regex
/// (`review/*` / `tmp*` / `pr\d+_head`) — no literal duplication. The caller
/// enforces occupancy (a checked-out review is skipped before this is reached),
/// so an in-progress review is never eligible regardless of age. Fail-closed:
/// missing tip info → not eligible.
fn is_stale_review_scaffold(repo_root: &Path, branch: &str) -> bool {
    if !crate::branch_sweep::is_reviewer_checkout(branch) {
        return false;
    }
    let Some((_, tip_ts)) = branch_tip_info(repo_root, branch) else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Duration::from_secs(now.saturating_sub(tip_ts)) >= REVIEW_SCAFFOLD_TTL
}

/// PR-D·D3: the branch-reap decision, delegated to the shared
/// [`crate::worktree::disposition::branch_disposition`] classifier (L4). The
/// SHARED "provably in default → delete" fail-direction lives in the classifier;
/// this caller only gathers the signals and adds the one override the classifier
/// does not encode.
///
/// - `provably_in_default` (merged-ancestor OR squash-merged past the age floor)
///   → `BranchSignal::ProvablyInDefault → DeleteBranch`.
/// - `remote_gone` alone → `BranchSignal::RemoteGoneOnly → KeepBranch`
///   (CR-2026-06-14: remote-gone is NEVER an independent delete trigger — the
///   branch may hold committed-but-unpushed local work).
/// - `scaffold_ttl` is a review-scaffold branch that is `NotMerged` (the classifier
///   KEEPS it) but has aged past its TTL — an EXPLICIT external arm reproduces the
///   reap the classifier can't model. Same shape as D2's #2010 `reviewer_bypass`:
///   an override the pure classifier deliberately does not encode, kept outside so
///   the delegation stays honest. Locked byte-for-byte by
///   [`tests::branch_reap_delete_equals_pre_d3_gate`].
fn branch_reap_delete(provably_in_default: bool, remote_gone: bool, scaffold_ttl: bool) -> bool {
    use crate::worktree::disposition::{branch_disposition, BranchDisposition, BranchSignal};
    let signal = if provably_in_default {
        BranchSignal::ProvablyInDefault
    } else if remote_gone {
        BranchSignal::RemoteGoneOnly
    } else {
        BranchSignal::NotMerged
    };
    matches!(branch_disposition(signal), BranchDisposition::DeleteBranch) || scaffold_ttl
}

/// #P1-2607: process-wide cache of the STRUCTURAL squash-merged check (git
/// cherry + #1280 tree-diff fallback), keyed by `(repo, branch, tip_sha,
/// default_tip_sha)`. A branch's relationship to `default` depends on BOTH
/// tips — `default` advancing to absorb the branch's patch (a real
/// squash-merge landing) flips a previously-false verdict to true even
/// though the branch's own tip never moved. #2614: the key used to omit
/// `default_tip_sha`, so that transition was cached as permanently
/// ineligible once checked before the squash-merge landed — live prune
/// never reaped the branch and dry-run systematically under-reported it.
/// Entries for long-deleted branches or superseded `default` tips are never
/// looked up again and just sit unused; at realistic branch-churn rates
/// that's a few KB over the daemon's lifetime, not worth evicting.
///
/// This is the fix for the #2607 freeze incident: `sweep_from_registry`'s
/// dry-run mode never consumes candidates, so — before this cache — EVERY
/// ~10-minute sweep round re-ran the expensive cherry/tree-diff check for
/// ALL 172 accumulated candidates from scratch (83s, synchronously blocking
/// the daemon's main tick loop). With this cache, only a candidate whose tip
/// (or `default`'s tip) actually moved since the last round pays that cost
/// again.
type SquashCacheKey = (PathBuf, String, String, String);
static SQUASH_MERGED_CACHE: std::sync::OnceLock<parking_lot::Mutex<HashMap<SquashCacheKey, bool>>> =
    std::sync::OnceLock::new();

/// #P3 (branch-residue): positive-only cache of a TRUE structural squash-merge,
/// keyed WITHOUT `default_tip_sha` — `(repo, branch, tip_sha)`. A positive
/// squash verdict is MONOTONIC (once a branch's patch is in `default`, `default`
/// only ever advances further; it never "un-absorbs" the patch), so `default`
/// advancing must NOT bust a cached TRUE and force the expensive cherry/tree-diff
/// re-run. This set is checked BEFORE `default`'s tip is even resolved. The
/// `default_tip`-keyed [`SQUASH_MERGED_CACHE`] above still holds FALSE verdicts,
/// which are NOT monotonic (a false can flip to true once `default` advances to
/// absorb the branch), so those MUST stay default-tip-keyed to re-evaluate.
type SquashPositiveKey = (PathBuf, String, String);
static SQUASH_MERGED_POSITIVE: std::sync::OnceLock<
    parking_lot::Mutex<std::collections::HashSet<SquashPositiveKey>>,
> = std::sync::OnceLock::new();

/// #1750-B3: is `branch` a squash-merge orphan eligible for auto-GC? True when
/// it is squash-merged into the default branch AND its tip is older than
/// [`SQUASH_GC_MIN_TIP_AGE`]. Reuses `branch_sweep`'s detection (git cherry +
/// #1280 tree-diff fallback) so the auto path matches the operator sweep.
///
/// t-...50899-10: `pub(crate)` so `worktree_pool::cleanup_merged_branch` reuses
/// the SAME squash-safe delete gate this file's `prune_orphaned_branches`
/// uses, instead of treating a remote-gone branch as independently deletable.
///
/// #P1-2607: age is checked FIRST off the one mandatory `branch_tip_info`
/// call (short-circuits the expensive squash check for branches too young
/// to qualify regardless — a bonus, not just a cache hit) and the
/// structural squash-merged result is cache-checked by tip SHA before
/// falling back to the real `is_squash_merged` computation. Same final
/// boolean as the pre-#P1-2607 `is_squash_merged(..) && age(..)` — only the
/// evaluation order and repeat-call cost changed.
///
/// #2614: `default`'s tip is also resolved unconditionally (one more cheap
/// `branch_tip_info` spawn, NOT the expensive cherry/tree-diff check) and
/// folded into the cache key — if `default` can't be resolved, fail closed
/// (not eligible) rather than caching under an incomplete key.
pub(crate) fn is_squash_gc_eligible(repo_root: &Path, branch: &str, default: &str) -> bool {
    let Some((tip_sha, tip_ts)) = branch_tip_info(repo_root, branch) else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if Duration::from_secs(now.saturating_sub(tip_ts)) < SQUASH_GC_MIN_TIP_AGE {
        return false;
    }

    // #P3: a positive squash verdict is monotonic — check the default-tip-FREE
    // positive set FIRST. A hit short-circuits to `true` without even resolving
    // `default`'s tip (so `default` advancing can't needlessly bust it).
    let pos_key = (repo_root.to_path_buf(), branch.to_string(), tip_sha.clone());
    let positive = SQUASH_MERGED_POSITIVE
        .get_or_init(|| parking_lot::Mutex::new(std::collections::HashSet::new()));
    if positive.lock().contains(&pos_key) {
        return true;
    }

    let Some((default_tip_sha, _)) = branch_tip_info(repo_root, default) else {
        return false;
    };

    let key = (
        repo_root.to_path_buf(),
        branch.to_string(),
        tip_sha,
        default_tip_sha,
    );
    let cache = SQUASH_MERGED_CACHE.get_or_init(|| parking_lot::Mutex::new(HashMap::new()));
    if let Some(&cached) = cache.lock().get(&key) {
        return cached;
    }
    let result = crate::branch_sweep::is_squash_merged(repo_root, default, branch);
    // #P3: only TRUE is monotonic — record it in the positive set so a later
    // `default` advance is a cheap positive-set hit. FALSE stays in the
    // default-tip-keyed bool cache (it can flip to true when `default` advances).
    if result {
        positive.lock().insert(pos_key);
    }
    cache.lock().insert(key, result);
    result
}

pub(crate) fn branch_has_active_binding(home: &Path, repo: &Path, branch: &str) -> Option<bool> {
    branch_has_other_active_binding(home, repo, branch, None)
}

pub(crate) fn branch_has_other_active_binding(
    home: &Path,
    repo: &Path,
    branch: &str,
    excluded_worktree: Option<&str>,
) -> Option<bool> {
    let canonical_repo = std::fs::canonicalize(repo).ok()?;
    let bindings = binding_scan_all_strict(home).ok()?;
    for (_, binding) in bindings {
        let bound_branch = binding["branch"]
            .as_str()
            .filter(|branch| !branch.is_empty())?;
        let source = binding["source_repo"]
            .as_str()
            .filter(|source| !source.is_empty())?;
        if excluded_worktree.is_some() && binding["worktree"].as_str().is_none_or(str::is_empty) {
            return None;
        }
        if excluded_worktree
            .zip(binding["worktree"].as_str())
            .is_some_and(|(excluded, bound)| excluded == bound)
        {
            continue;
        }
        let Ok(source) = std::fs::canonicalize(source) else {
            return None;
        };
        if bound_branch == branch && source == canonical_repo {
            return Some(true);
        }
    }
    Some(false)
}

/// Run `git worktree prune` then delete local branches whose remote tracking
/// ref is gone, that are merged into main, or that are squash-merge orphans
/// (#1750-B3). Skips branches checked out in any worktree.
///
/// #2605: `dry_run` computes the exact same eligibility (merged/squash gate,
/// worktree-occupancy skip) but skips the actual `git branch -D` — eligible
/// branches are still returned (with their real reason: `"merged"` or
/// `"squash-merged"`) so the caller can log/audit the candidate list.
#[allow(dead_code)]
fn prune_orphaned_branches(repo_root: &Path, dry_run: bool) -> Vec<(String, &'static str)> {
    prune_orphaned_branches_with_home(None, repo_root, dry_run)
}

fn prune_orphaned_branches_with_home(
    home: Option<&Path>,
    repo_root: &Path,
    dry_run: bool,
) -> Vec<(String, &'static str)> {
    let default = crate::git_helpers::default_branch(repo_root);
    // Collect branches currently checked out in worktrees — cannot delete these
    // PR-B rider (dev2 #2695 seat2): fail-CLOSED occupancy. If the worktree list
    // can't be determined, the occupancy dimension would collapse to "nothing
    // occupied" and a branch whose worktree is merely un-enumerable could be
    // reaped — skip ALL branch pruning this tick instead (mirrors :382).
    let wt_branches: HashSet<String> = match list_worktrees(repo_root) {
        Ok(entries) => entries.into_iter().map(|e| e.branch).collect(),
        Err(()) => {
            tracing::warn!(
                repo = %repo_root.display(),
                "prune_orphaned_branches: worktree occupancy could not be determined — \
                 skipping ALL branch pruning this tick (fail-closed)"
            );
            return Vec::new();
        }
    };

    // W1.2: git_cmd → trimmed stdout on success; spawn-error + non-zero collapse to `Err → []`.
    let branches: Vec<String> =
        match crate::git_helpers::git_cmd(repo_root, &["branch", "--format=%(refname:short)"]) {
            Ok(stdout) => stdout
                .lines()
                .filter(|b| *b != default.as_str() && !crate::protected_refs::is_protected_ref(b))
                .map(String::from)
                .collect(),
            _ => return Vec::new(),
        };
    // Snapshot the open-PR inventory once for this repository/sweep.  Each
    // branch below consumes the bounded snapshot instead of issuing its own
    // synchronous SCM lookup; an Unknown snapshot keeps all terminal
    // candidates fail-closed.
    let open_pr_snapshot = crate::branch_sweep::open_pr_snapshot(repo_root, &default);

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
        // PR-A P1 (branch-residue RCA §3): disposable reviewer-checkout
        // scaffolding (review/* etc.) that is unoccupied (checked above) and
        // aged past REVIEW_SCAFFOLD_TTL. These never carry a PR and never merge,
        // so the merged/squash paths never reap them — a TTL is their only live
        // terminal path (H1 in the RCA).
        let scaffold = !merged && !squash && is_stale_review_scaffold(repo_root, branch);
        let provenance = if merged {
            crate::worktree::disposition::BranchProvenance::Merged
        } else if squash {
            crate::worktree::disposition::BranchProvenance::SquashMerged
        } else if scaffold {
            crate::worktree::disposition::BranchProvenance::ReviewerResidue
        } else {
            crate::worktree::disposition::BranchProvenance::Unknown
        };
        let task_active = match home {
            Some(h) => crate::branch_sweep::branch_has_active_task(h, branch),
            None => Some(false),
        };
        let binding_active = match home {
            Some(h) => branch_has_active_binding(h, repo_root, branch),
            None => Some(false),
        };
        let active_holder = match (wt_branches.contains(branch), binding_active) {
            (true, _) | (_, Some(true)) => Some(true),
            (false, Some(false)) => Some(false),
            _ => None,
        };
        let open_pr = if merged || squash || scaffold {
            match open_pr_snapshot.status_for(branch) {
                crate::branch_sweep::OpenPrStatus::Open => Some(true),
                crate::branch_sweep::OpenPrStatus::NotOpen => Some(false),
                crate::branch_sweep::OpenPrStatus::Unknown => None,
            }
        } else {
            // Unknown provenance is already a KEEP decision; avoid an
            // unnecessary network probe for branches that cannot be deleted.
            Some(false)
        };
        let lifecycle = crate::worktree::disposition::branch_lifecycle_disposition(
            &crate::worktree::disposition::BranchLifecycleInput {
                provenance,
                terminal: merged || squash || scaffold,
                active_holder,
                task_active,
                open_pr,
                // Reviewer residue is snapshotted into a recovery ref before
                // deletion below; merged/squash work is already in `default`.
                unique_unpreserved_work: Some(false),
            },
        );
        // PR-D·D3: the reap decision (L4) delegates to `branch_disposition` via
        // `branch_reap_delete`. `merged || squash` = ProvablyInDefault → delete;
        // `scaffold` is the explicit external-arm override (a NotMerged branch the
        // classifier keeps, aged past its TTL). Phase-2 does not compute remote-gone
        // (CR-2026-06-14 dropped it as a trigger) → remote_gone=false. Byte-identical
        // to the prior `!merged && !squash && !scaffold` continue-gate.
        if !branch_reap_delete(merged || squash, false, scaffold)
            || !matches!(
                lifecycle,
                crate::worktree::disposition::BranchLifecycleDisposition::Delete
            )
        {
            continue;
        }
        // The scaffolding path never merged, so its commits survive only in the
        // object store once the branch ref is gone — capture the tip SHA BEFORE
        // deletion so the log keeps them recoverable.
        let scaffold_tip = if scaffold {
            branch_tip_info(repo_root, branch).map(|(sha, _)| sha)
        } else {
            None
        };
        if scaffold && !dry_run {
            if let Some(tip) = scaffold_tip.as_deref() {
                if crate::branch_sweep::prepare_branch_recovery(
                    home,
                    repo_root,
                    branch,
                    tip,
                    "review-scaffold-ttl",
                )
                .is_err()
                {
                    continue;
                }
            } else {
                continue;
            }
        }
        let ok = dry_run || crate::git_helpers::git_ok(repo_root, &["branch", "-D", branch]);
        if ok {
            let reason = if merged {
                "merged"
            } else if squash {
                "squash-merged"
            } else {
                "review-scaffold-ttl"
            };
            if dry_run {
                tracing::info!(branch, reason, tip_sha = ?scaffold_tip, "would prune orphaned branch (dry-run)");
            } else {
                tracing::info!(branch, reason, tip_sha = ?scaffold_tip, "pruned orphaned branch");
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
    fn write_full_binding(
        home: &Path,
        agent: &str,
        branch: &str,
        source_repo: &Path,
        worktree: &Path,
    ) {
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
    ///   phase-2 `prune_orphaned_branches`: delete iff `merged || squash || scaffold`
    ///   phase-1 `branch_safe_to_delete`:   delete iff `merged || squash` (scaffold=false)
    /// Both reduce to `provably_in_default || scaffold_ttl`. The CR-2026-06-14
    /// invariant — remote-gone ALONE is never a delete trigger — is pinned by
    /// asserting `remote_gone` NEVER changes the result across the full domain.
    #[test]
    fn branch_reap_delete_equals_pre_d3_gate() {
        for provably_in_default in [true, false] {
            for remote_gone in [true, false] {
                for scaffold_ttl in [true, false] {
                    assert_eq!(
                        branch_reap_delete(provably_in_default, remote_gone, scaffold_ttl),
                        provably_in_default || scaffold_ttl,
                        "branch reap DRIFT: provably={provably_in_default} \
                         remote_gone={remote_gone} scaffold={scaffold_ttl} \
                         (remote-gone must NEVER flip the result — CR-2026-06-14)"
                    );
                }
            }
        }
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

    /// RED4 (guard, 不動): `spike/*` (intentional design record) and an aged but
    /// UNMERGED `feat/*` (real WIP) must never be touched by the scaffolding path
    /// — neither matches the reviewer-checkout regex / is disposable.
    #[test]
    fn prune_keeps_spike_and_unmerged_feat_p1() {
        let _lock = ENV_LOCK.lock();
        let repo = setup_test_repo("p1-spike-feat");
        make_unmerged_dated_branch(&repo, "spike/2342-inbound", "2024-01-01T00:00:00 +0000");
        make_unmerged_dated_branch(&repo, "feat/real-wip", "2024-01-01T00:00:00 +0000");

        let pruned = prune_orphaned_branches(&repo, false);
        assert!(
            !pruned.iter().any(|(b, _)| b == "spike/2342-inbound"),
            "PR-A P1: spike/* is a retained design record and must NOT be pruned: {pruned:?}"
        );
        assert!(
            !pruned.iter().any(|(b, _)| b == "feat/real-wip"),
            "PR-A P1: an unmerged feat/* carrying real work must NOT be pruned: {pruned:?}"
        );
        std::fs::remove_dir_all(&repo).ok();
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

        let (branch_tip, _) =
            branch_tip_info(&repo, "feat/lagging").expect("tip info must resolve");

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
}

#[cfg(all(test, unix))]
mod reconcile_ordering_tests;

#[cfg(test)]
mod lifecycle_r1_tests;

#[cfg(test)]
mod protected_sweep_tests;

#[cfg(test)]
mod windows_cleanup_diagnostics_tests;

#[cfg(test)]
mod review_repro_worktree_git;
