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

/// Remove a worktree; delete its branch only when `delete_branch` is set.
///
/// Worktree and branch deletion are decoupled so unpushed remote-gone work is
/// preserved. Windows retries up to 3 times with 200/400ms backoff for locks;
/// `dry_run` skips both git calls and reports success.
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
        let output = match crate::git_helpers::git_bypass(
            repo_root,
            &["worktree", "remove", "--force", worktree_path],
        ) {
            Ok(output) => output,
            Err(error) => {
                failure = Some(format!(
                    "sweep worktree remove failed: could not execute git: {error}"
                ));
                continue;
            }
        };
        if output.status.success() {
            wt_ok = true;
            break;
        }
        let status = output.status.code().map(|c| format!("exit status {c}"));
        let status = status.unwrap_or_else(|| output.status.to_string());
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        failure = Some(format!(
            "sweep worktree remove failed: {status}; stderr: {}",
            if stderr.is_empty() {
                "<empty>"
            } else {
                &stderr
            }
        ));
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
            // Route Delete through janitor with the caller-local remover, retaining
            // retry behavior and final command diagnostics.
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
    note_binding_active_probe();
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
        // #2874: canonicalize once; skip rows proven unrelated before worktree identity.
        let cs = std::fs::canonicalize(source);
        if matches!(&cs, Ok(s) if bound_branch != branch || *s != canonical_repo) {
            continue;
        }
        if excluded_worktree.is_some() && binding["worktree"].as_str().is_none_or(str::is_empty) {
            return None;
        }
        if excluded_worktree
            .zip(binding["worktree"].as_str())
            .is_some_and(|(excluded, bound)| excluded == bound)
        {
            continue;
        }
        return if cs.is_ok() { Some(true) } else { None };
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
        let terminal = merged || squash || scaffold;
        // #3011: a NON-terminal candidate is already a Keep — `branch_lifecycle_disposition`
        // returns Keep on `!terminal` before it reads task/holder/PR evidence
        // (worktree/disposition.rs:151-161) — so the strict task-ledger replay and the
        // binding scan cannot change its outcome. Skip both, exactly as the `open_pr`
        // field below already skips its network probe for the same reason. `None` (not
        // `Some(false)`) is the honest "unresolved" value and is the fail-closed input.
        // Terminal candidates still probe FRESHLY, per candidate — deliberately NOT a
        // sweep-wide snapshot.
        let (task_active, binding_active) = if terminal {
            (
                match home {
                    Some(h) => crate::branch_sweep::branch_has_active_task(h, branch),
                    None => Some(false),
                },
                match home {
                    Some(h) => branch_has_active_binding(h, repo_root, branch),
                    None => Some(false),
                },
            )
        } else {
            (None, None)
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
                terminal,
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

// #3011: counts calls into `branch_has_active_binding`, to prove that
// non-terminal candidates in `prune_orphaned_branches_with_home` skip the
// per-agent binding scan.
#[cfg(test)]
std::thread_local! {
    static BINDING_ACTIVE_PROBE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_binding_active_probe() {
    BINDING_ACTIVE_PROBE_COUNT.with(|count| count.set(count.get() + 1));
}
#[cfg(not(test))]
fn note_binding_active_probe() {}

/// Read the count and zero it — so a caller can also use this as the "start
/// from zero" setup step, with no separate reset accessor.
#[cfg(test)]
pub(crate) fn take_binding_active_probe_count() -> usize {
    BINDING_ACTIVE_PROBE_COUNT.with(|count| count.replace(0))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests;

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
