//! Worktree pool — daemon-managed lease/release lifecycle for git worktrees.
//!
//! Builds on existing `worktree.rs` (creation) + `binding.rs` (state).
//! Phase 3: lease/release + daemon-tag + E4.5 enforcement. GC deferred to Phase 4.

use std::path::{Path, PathBuf};

// #1639: the daemon-internal git wrapper lives in `git_helpers::git_bypass`
// (the #781-centralized single source for the `AGEND_GIT_BYPASS=1` contract).
// Call sites below go through it directly rather than a local copy.

/// Marker file placed in daemon-managed worktrees (R14 mitigation).
pub(crate) const MANAGED_MARKER: &str = ".agend-managed";

/// Root directory for daemon-managed worktrees in the new layout.
/// `<home>/worktrees/` — contains `<agent>/<branch>/` subdirectories.
/// Used by lease, gc_candidates, and reconcile_hooks.
pub fn daemon_managed_worktree_root(home: &Path) -> PathBuf {
    home.join("worktrees")
}

/// A lease on a worktree — returned by `lease()`, consumed by `release()`.
#[derive(Debug, Clone)]
pub struct WorktreeLease {
    pub agent: String,
    pub branch: String,
    pub path: PathBuf,
}

/// Lease a worktree for an agent + branch. Creates if needed, tags as daemon-managed.
/// Rejects `main` branch per E4.5 enforcement.
pub fn lease(
    home: &Path,
    source_repo: &Path,
    agent: &str,
    branch: &str,
) -> Result<WorktreeLease, String> {
    crate::agent_ops::ensure_not_protected(branch)?;

    // Create worktree using existing infrastructure. Sprint 57 Wave 4
    // (#546 Item 4): the new external layout requires `home` to
    // resolve the canonical path `$AGEND_HOME/worktrees/<agent>/<branch>/`.
    let info = match crate::worktree::create(home, source_repo, agent, Some(branch)) {
        Some(info) => info,
        None => return Err(format!("failed to create worktree for {agent}@{branch}")),
    };

    // #1137: marker is now written inside worktree::create() immediately
    // after checkout. Re-write here is idempotent and ensures the marker
    // is present for reused worktrees (which skip the create path).
    let marker = info.path.join(MANAGED_MARKER);
    let _ = std::fs::write(
        &marker,
        format!(
            "agent={agent}\nbranch={branch}\nleased_at={}\n",
            chrono::Utc::now().to_rfc3339()
        ),
    );

    // Write full binding with worktree + source-repo paths.
    // source_repo persistence (P0-X r1): release_full reads this back to run
    // `git worktree remove --force` from the owning repo's cwd, which
    // prevents stale registry entries that would block re-lease.
    if let Err(e) = crate::binding::bind_full(home, agent, "", branch, &info.path, source_repo) {
        tracing::warn!(%agent, %branch, error = %e, "lease: bind_full failed — worktree created but binding missing");
    }

    Ok(WorktreeLease {
        agent: agent.to_string(),
        branch: branch.to_string(),
        path: info.path,
    })
}

/// Release a lease — marks worktree as GC candidate (does NOT delete, Phase 4).
/// Writes `released_at` timestamp for grace period calculation.
pub fn release(home: &Path, lease: &WorktreeLease) {
    // Clear binding (task done).
    crate::binding::unbind(home, &lease.agent);
    // Write released_at into the managed marker for GC grace calculation.
    let marker = lease.path.join(MANAGED_MARKER);
    if let Ok(mut content) = std::fs::read_to_string(&marker) {
        content.push_str(&format!(
            "released_at={}\n",
            chrono::Utc::now().to_rfc3339()
        ));
        let _ = crate::store::atomic_write(&marker, content.as_bytes());
    }
    crate::event_log::log(
        home,
        "worktree_lease_released",
        &lease.agent,
        &format!("branch={} path={}", lease.branch, lease.path.display()),
    );
}

/// Check if a worktree is daemon-managed (has .agend-managed marker).
pub fn is_daemon_managed(worktree_path: &Path) -> bool {
    worktree_path.join(MANAGED_MARKER).exists()
}

/// Outcome of a hard release — emitted by `release_full` and serialized
/// directly into the `release_worktree` MCP tool response.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ReleaseOutcome {
    pub released: bool,
    /// #1465: true when `released` is a no-op success because the agent had
    /// no binding to begin with (release is idempotent — the target state
    /// was already reached). Distinguishes "nothing to do, success" from an
    /// actual teardown. Never true alongside an `error`.
    pub already_released: bool,
    pub worktree_removed: bool,
    pub binding_removed: bool,
    pub branch_deleted: bool,
    // #807 Item 2: drop optional keys on success so clients
    // don't render `"error": null` as an `<error>` envelope.
    // Real failures still emit `error` (skip_serializing_if
    // drops `None` only, never `Some`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_cleanup_skipped_reason: Option<String>,
    /// #t-21: on a `dry_run=true` release, a human-readable preview of the
    /// destructive effects that were deliberately NOT performed (worktree
    /// removal + binding clear). `None` on a real release. The pre-fix bug ran
    /// `remove_worktree` + `clear_binding_state` unconditionally, so a dry_run
    /// actually destroyed the worktree and binding; now they are previewed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run_preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Delete the local branch ref after worktree release, IFF:
/// - `managed_verified` is true (caller confirmed .agend-managed marker)
/// - Branch is merged into main OR remote tracking ref is gone
///
/// SAFETY: This function ONLY receives the branch from the daemon's own
/// binding record. User-checkout branches never reach here because
/// release_full early-returns on unmanaged worktrees. The merge-base
/// check below prevents deletion of unmerged branches regardless of
/// the managed_verified flag (#1249).
///
/// Returns `(deleted, skip_reason)`:
/// - `(true, None)` — branch was deleted
/// - `(false, Some(reason))` — branch was NOT deleted, reason explains why
fn cleanup_merged_branch(
    source_repo: &Path,
    branch: &str,
    dry_run: bool,
) -> (bool, Option<String>) {
    // Never delete protected branches.
    if crate::agent_ops::is_protected_ref(branch) {
        return (false, Some(format!("branch '{branch}' is protected")));
    }

    // #t-7 (#1824 follow-up): a `git fetch --prune` MUTATES the source repo's
    // remote-tracking refs (refs/remotes/...), so it must NOT run on a dry-run —
    // a dry-run release must be observation-only. The non-dry-run path keeps the
    // fresh fetch so `is_merged` / `is_gone` below are accurate; the dry-run
    // preview falls back to the existing local refs (best-effort "would delete").
    if !dry_run {
        let remote = crate::git_helpers::primary_remote(source_repo);
        // #2004: fail-direction is safe (stale remote refs → `is_gone` stays
        // false → branch kept, self-heals on the next successful fetch), but a
        // persistently failing fetch accumulates undeletable branches invisibly
        // — surface it. Pure logging, the cleanup proceeds on local refs.
        match crate::git_helpers::git_bypass(source_repo, &["fetch", "--prune", &remote]) {
            Ok(o) if !o.status.success() => {
                tracing::warn!(
                    repo = %source_repo.display(),
                    remote = %remote,
                    stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                    "fetch --prune failed during merged-branch cleanup — merge/gone checks run on possibly-stale local refs (branch kept = safe direction)"
                );
            }
            Err(e) => {
                tracing::warn!(
                    repo = %source_repo.display(),
                    remote = %remote,
                    error = %e,
                    "fetch --prune could not run during merged-branch cleanup — merge/gone checks run on possibly-stale local refs (branch kept = safe direction)"
                );
            }
            Ok(_) => {}
        }
    }

    let default = crate::git_helpers::default_branch(source_repo);
    let is_merged = crate::git_helpers::git_bypass(
        source_repo,
        &["merge-base", "--is-ancestor", branch, &default],
    )
    .map(|o| o.status.success())
    .unwrap_or(false);

    let is_gone = {
        let remote_name = crate::git_helpers::git_bypass(
            source_repo,
            &["config", &format!("branch.{branch}.remote")],
        )
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
        if !remote_name.is_empty() {
            let remote_ref = format!("refs/remotes/{remote_name}/{branch}");
            let exists = crate::git_helpers::git_bypass(
                source_repo,
                &["rev-parse", "--verify", &remote_ref],
            )
            .map(|o| o.status.success())
            .unwrap_or(true);
            !exists
        } else {
            false
        }
    };

    if !is_merged && !is_gone {
        return (false, Some("branch not merged into main".to_string()));
    }

    if dry_run {
        return (
            false,
            Some(format!("dry-run: would delete branch '{branch}'")),
        );
    }

    let del = crate::git_helpers::git_bypass(source_repo, &["branch", "-D", branch]);
    match del {
        Ok(o) if o.status.success() => (true, None),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            (false, Some(format!("git branch -D failed: {stderr}")))
        }
        Err(e) => (false, Some(format!("git branch -D failed: {e}"))),
    }
}

/// Hard-release an agent's daemon-managed worktree + binding.
///
/// Sprint 53 P0-X: closes the gap left by P0-1's auto-bind/auto-lease.
/// Without this path, every PR-merge transition leaves a stale
/// `.worktrees/<agent>` plus `runtime/<agent>/binding.json` behind, and the
/// next dispatch trips P0-1.6's actual-HEAD check (worktree exists on prior
/// branch). Operator manually `git worktree remove`-d for every transition;
/// this function lets the `release_worktree` MCP tool do it instead.
///
/// Differs from `release()` (Phase 3 soft mark) by actually removing the
/// worktree directory via `git worktree remove --force`.
///
/// Safety: only removes worktrees carrying the `.agend-managed` marker.
/// Operator-created worktrees without the marker are left alone — surfaced
/// as `released: false, error: "...no .agend-managed marker..."`.
///
/// Idempotent (#1465): second call on the same agent sees no binding and
/// returns `released: true, already_released: true` (no error) — the release
/// target state is already reached, so it's a success no-op. A genuine
/// cleanup failure WITH a binding present still returns `released: false` +
/// `error` (idempotent success applies only to the nothing-to-do path).
///
/// Partial cleanup: if the worktree path is missing or `git worktree remove`
/// fails, the binding is still cleared so the agent is not stuck in a
/// half-released state.
/// Result of worktree directory removal attempt.
enum WorktreeRemoval {
    Removed,
    AlreadyAbsent,
    Unmanaged(String),
    Failed(String),
}

fn source_repo_from_binding(binding: &serde_json::Value, wt_path: &Path) -> PathBuf {
    binding["source_repo"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            wt_path
                .parent()
                .filter(|p| p.file_name().and_then(|n| n.to_str()) == Some(".worktrees"))
                .and_then(|p| p.parent())
                .map(PathBuf::from)
        })
        .unwrap_or_default()
}

fn remove_worktree(agent: &str, wt_path: &Path, source_repo: &Path) -> WorktreeRemoval {
    if !wt_path.exists() {
        tracing::info!(agent, path = %wt_path.display(),
            "release: worktree path already absent — pruning registry + clearing binding");
        if !source_repo.as_os_str().is_empty() {
            let _ = crate::git_helpers::git_bypass(source_repo, &["worktree", "prune"]);
        }
        return WorktreeRemoval::AlreadyAbsent;
    }
    if !is_daemon_managed(wt_path) {
        tracing::warn!(agent, path = %wt_path.display(),
            "release skipped: no .agend-managed marker — worktree left alone");
        return WorktreeRemoval::Unmanaged(format!(
            "worktree at {} has no .agend-managed marker — refusing to remove (binding NOT cleared)",
            wt_path.display()
        ));
    }

    let wt_str = wt_path.display().to_string();
    let result = if source_repo.as_os_str().is_empty() {
        // git-raw-allowed: empty source_repo → this arm intentionally runs with
        // NO `current_dir`, so `git` resolves the repo from `--force <abs wt>`
        // itself. `git_cmd`/`git_bypass` both REQUIRE a cwd; passing `wt_path
        // .parent()` is wrong (it's the worktrees-pool dir `~/.agend-terminal/
        // worktrees/<agent>/`, outside the repo tree, per lead ruling). Keep raw.
        // TODO(W1.2): audit whether the empty-source_repo branch is still
        // reachable in practice; if dead, delete this arm rather than migrate it.
        std::process::Command::new("git")
            .args(["worktree", "remove", "--force", &wt_str])
            .env("AGEND_GIT_BYPASS", "1")
            .output()
    } else {
        crate::git_helpers::git_bypass(source_repo, &["worktree", "remove", "--force", &wt_str])
    };
    match result {
        Ok(o) if o.status.success() => WorktreeRemoval::Removed,
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            tracing::warn!(agent, error = %stderr, path = %wt_path.display(),
                "git worktree remove failed — falling back to remove_dir_all");
            let _ = std::fs::remove_dir_all(wt_path);
            if !wt_path.exists() {
                if !source_repo.as_os_str().is_empty() {
                    if let Err(e) =
                        crate::git_helpers::git_bypass(source_repo, &["worktree", "prune"])
                    {
                        tracing::warn!(agent, error = %e, "git worktree prune failed");
                    }
                }
                WorktreeRemoval::Removed
            } else {
                WorktreeRemoval::Failed(format!("git worktree remove failed: {stderr}"))
            }
        }
        Err(e) => {
            tracing::warn!(agent, error = %e, "git command failed for release");
            WorktreeRemoval::Failed(format!("git command failed: {e}"))
        }
    }
}

fn clear_binding_state(home: &Path, agent: &str) {
    crate::binding::unbind(home, agent);
    crate::mcp::handlers::dispatch_hook::clear_bind_in_flight(home, agent);
}

fn resolve_branch_cleanup(
    binding: &serde_json::Value,
    managed_verified: bool,
    worktree_absent: bool,
    dry_run: bool,
    out: &mut ReleaseOutcome,
) {
    let branch = binding["branch"].as_str().unwrap_or("");
    let sr_str = binding["source_repo"].as_str().unwrap_or("");
    if !managed_verified && !worktree_absent {
        out.branch_cleanup_skipped_reason =
            Some("cannot verify .agend-managed marker — skipping branch cleanup".to_string());
    } else if !branch.is_empty() && !sr_str.is_empty() {
        let (deleted, skip_reason) = cleanup_merged_branch(Path::new(sr_str), branch, dry_run);
        out.branch_deleted = deleted;
        out.branch_cleanup_skipped_reason = skip_reason;
    } else if branch.is_empty() {
        out.branch_cleanup_skipped_reason = Some("no branch in binding".to_string());
    } else {
        out.branch_cleanup_skipped_reason = Some("no source_repo in binding".to_string());
    }
}

pub fn release_full(home: &Path, agent: &str, dry_run: bool) -> ReleaseOutcome {
    let mut out = ReleaseOutcome::default();

    let Some(binding) = crate::binding::read(home, agent) else {
        // #1465: release is idempotent. "No binding" means the release
        // target state is already reached → report a success no-op rather
        // than an error, so automated dispatch/release can treat release as
        // a safe always-succeeds operation. (A genuine cleanup failure WITH
        // a binding present still returns `released:false` + `error` below —
        // idempotent success applies ONLY to this nothing-to-do path.)
        out.released = true;
        out.already_released = true;
        return out;
    };

    let wt_path_str = binding["worktree"].as_str().unwrap_or("");
    let mut managed_verified = false;
    let mut worktree_absent = false;

    if !wt_path_str.is_empty() {
        let wt_path = Path::new(wt_path_str);
        let source_repo = source_repo_from_binding(&binding, wt_path);

        if dry_run {
            // #t-21: dry_run is observation-only. Classify the worktree with the
            // SAME checks `remove_worktree` uses (path-exists → absent;
            // missing .agend-managed marker → refuse; managed+present → would
            // remove) but perform NO mutation. This closes the bug where
            // `remove_worktree` deleted the worktree on a dry run before the
            // (dry-run-honoring) branch cleanup ever ran.
            if !wt_path.exists() {
                worktree_absent = true;
            } else if !is_daemon_managed(wt_path) {
                out.error = Some(format!(
                    "worktree at {} has no .agend-managed marker — refusing to remove (binding NOT cleared)",
                    wt_path.display()
                ));
                return out;
            } else {
                managed_verified = true;
            }
        } else {
            match remove_worktree(agent, wt_path, &source_repo) {
                WorktreeRemoval::Removed => {
                    managed_verified = true;
                    out.worktree_removed = true;
                }
                WorktreeRemoval::AlreadyAbsent => {
                    worktree_absent = true;
                }
                WorktreeRemoval::Unmanaged(err) => {
                    out.error = Some(err);
                    // #1879 (WT-LEAK-2): refusing to delete an UNMANAGED
                    // (operator-created) worktree protects operator data, but the
                    // daemon's stale binding to it must STILL be cleared — leaving
                    // it leaks the binding + blocks a same-agent re-bind. The
                    // worktree-protection refusal must not also skip binding
                    // cleanup. (This arm is already the non-dry_run path; the
                    // dry_run classifier above mutates nothing.)
                    clear_binding_state(home, agent);
                    out.binding_removed = true;
                    return out;
                }
                WorktreeRemoval::Failed(err) => {
                    managed_verified = true;
                    out.error = Some(err);
                }
            }
        }
    }

    if dry_run {
        // #t-21: preserve the binding + worktree; report what WOULD happen.
        // `released` is an observation-success (matches the dry-run branch
        // cleanup contract), but nothing was actually removed.
        let wt_preview = if worktree_absent {
            format!("worktree {wt_path_str} already absent")
        } else if managed_verified {
            format!("would remove worktree {wt_path_str}")
        } else {
            "no worktree to remove".to_string()
        };
        out.dry_run_preview = Some(format!(
            "dry-run: {wt_preview}; would clear binding for '{agent}'"
        ));
        out.released = true;
    } else {
        clear_binding_state(home, agent);
        out.binding_removed = true;
        // #1465 guardrail: only report `released` when no cleanup step failed.
        // A `WorktreeRemoval::Failed` set `out.error` above — idempotent success
        // must NOT mask a real execution error as success (reviewer contract:
        // "binding present but cleanup failed → released:false + error").
        if out.error.is_none() {
            out.released = true;
        }
    }

    resolve_branch_cleanup(
        &binding,
        managed_verified,
        worktree_absent,
        dry_run,
        &mut out,
    );

    crate::event_log::log(
        home,
        "worktree_released_full",
        agent,
        &format!(
            "wt_removed={} binding_removed={} error={}",
            out.worktree_removed,
            out.binding_removed,
            out.error.as_deref().unwrap_or("")
        ),
    );

    out
}

/// Sprint 57 Wave 2 Track B (#546 Item 2) — scan ci-watches dir,
/// remove `agent` from EVERY watch whose subscriber list contains
/// them. Replaces the Sprint 55 P0-B EC7 single-(repo, branch)-pair
/// helper that left ad-hoc watches outside the binding-branch
/// orphaned on release. Agent names are unique within the fleet so
/// removing the name from any matching watch is always correct on
/// release; the cross-repo bleed risk that the EC7 r1 review flagged
/// only applies when the predicate is "branch matches" — `agent`
/// matches doesn't have that ambiguity.
///
/// Per-watch behaviour: if `agent` was the last subscriber → delete
/// the watch file entirely; otherwise rewrite it with the shrunk
/// subscriber list. Best-effort: read/parse/write failures are
/// logged but never abort release.
#[allow(dead_code)] // #931: kept as the documented rollback target — see
                    // the comment block at the former call site in `release_full`. Slated for
                    // removal one Sprint after #931 lands assuming no rollback fires.
fn unsubscribe_all_ci_watches_for_agent(home: &Path, agent: &str) {
    let ci_dir = crate::daemon::ci_watch::ci_watches_dir(home);
    let Ok(entries) = std::fs::read_dir(&ci_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        // #692: flock protects read-modify-write against concurrent ci_watch tick
        let lock_path = path.with_extension("lock");
        let _lock = match crate::store::acquire_file_lock(&lock_path) {
            Ok(l) => l,
            Err(_) => continue, // skip if can't lock
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut watch) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let watch_branch = watch["branch"].as_str().unwrap_or("?").to_string();
        let watch_repo = watch["repo"].as_str().unwrap_or("?").to_string();
        let mut subs: Vec<String> = crate::daemon::ci_watch::parse_subscribers(&watch);
        let before = subs.len();
        subs.retain(|s| s != agent);
        if subs.len() == before {
            continue; // agent wasn't subscribed; nothing to do
        }
        if subs.is_empty() {
            let _ = std::fs::remove_file(&path);
            tracing::info!(%agent, repo = %watch_repo, branch = %watch_branch, path = %path.display(),
                "ci-watch unsubscribed last subscriber → removed watch file");
            continue;
        }
        let subs_json: Vec<serde_json::Value> = subs
            .iter()
            .map(|name| serde_json::json!({"instance": name}))
            .collect();
        watch["subscribers"] = serde_json::json!(subs_json);
        watch["instance"] = serde_json::json!(subs.first().cloned().unwrap_or_default());
        let _ = crate::store::atomic_write(
            &path,
            serde_json::to_string_pretty(&watch)
                .unwrap_or_default()
                .as_bytes(),
        );
        tracing::info!(%agent, repo = %watch_repo, branch = %watch_branch, remaining = subs.len(),
            "ci-watch unsubscribed agent; subscribers shrunk");
    }
}

/// Pin a worktree (operator override — prevents GC in Phase 4).
pub fn pin(worktree_path: &Path) {
    let pin_file = worktree_path.join(".agend-pinned");
    let _ = std::fs::write(&pin_file, chrono::Utc::now().to_rfc3339());
}

/// Unpin a worktree (allow GC again).
pub fn unpin(worktree_path: &Path) {
    let pin_file = worktree_path.join(".agend-pinned");
    let _ = std::fs::remove_file(pin_file);
}

/// Check if a worktree is pinned.
pub fn is_pinned(worktree_path: &Path) -> bool {
    worktree_path.join(".agend-pinned").exists()
}

/// Reconcile orphan leases at daemon startup (log only, no delete in Phase 3).
pub fn reconcile_orphan_leases(home: &Path) {
    let runtime_dir = crate::paths::runtime_dir(home);
    if !runtime_dir.exists() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(&runtime_dir) {
        for entry in entries.flatten() {
            let binding_path = entry.path().join("binding.json");
            if !binding_path.exists() {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&binding_path) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(wt_path) = v["worktree"].as_str() {
                        if !Path::new(wt_path).exists() {
                            tracing::warn!(
                                agent = entry.file_name().to_string_lossy().as_ref(),
                                worktree = wt_path,
                                "orphan lease: worktree path missing"
                            );
                        }
                    }
                }
            }
        }
    }
}

// ── Phase 4: GC scan + dry-run + cutover ────────────────────────────────

/// Grace period before a released worktree becomes a GC candidate.
const GC_GRACE_HOURS: i64 = 24;

/// t-worktree-leak PR-2: hard age cap for the force-reclaim backstop. A
/// never-released lease whose agent shows NO liveness AND whose `leased_at` is
/// older than this is force-reclaimed. Configurable (`AGEND_WORKTREE_FORCE_RECLAIM_DAYS`).
fn force_reclaim_age_days() -> i64 {
    crate::env_util::env_parse_min::<i64>("AGEND_WORKTREE_FORCE_RECLAIM_DAYS", 7, 1)
}

/// reviewer-2 #5: force-reclaim post-boot grace (seconds). After a daemon restart
/// the live-agent registry (the process-liveness signal) is empty until agents
/// re-spawn; suspend force-reclaim for this window so a mid-respawn agent is not
/// reclaimed during the liveness blind spot. Fixed const 600s / 10 min
/// (#env-cleanup: was env-overridable via
/// `AGEND_WORKTREE_FORCE_RECLAIM_BOOT_GRACE_SECS`; demoted to YAGNI).
const FORCE_RECLAIM_BOOT_GRACE_SECS: u64 = 600;

fn force_reclaim_boot_grace_secs() -> u64 {
    FORCE_RECLAIM_BOOT_GRACE_SECS
}

/// Pure boot-grace predicate: is `now_unix` within `grace_secs` of `boot_unix`?
/// Unknown boot time → conservative `true` (suspend reclaim — never reclaim when
/// we cannot tell how long the daemon has been up).
fn within_boot_grace(boot_unix: Option<u64>, now_unix: u64, grace_secs: u64) -> bool {
    match boot_unix {
        Some(b) => now_unix.saturating_sub(b) < grace_secs,
        None => true,
    }
}

/// reviewer-2 #5: is the running daemon still inside its post-boot grace window?
/// No active daemon run dir → NOT in grace (tests / non-daemon contexts — GC only
/// runs inside the daemon). Daemon present but boot time unreadable → conservative
/// in-grace (suspend).
fn daemon_within_boot_grace(home: &Path) -> bool {
    let Some(run_dir) = crate::daemon::find_active_run_dir(home) else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    within_boot_grace(
        crate::daemon::read_daemon_boot_unix(&run_dir),
        now,
        force_reclaim_boot_grace_secs(),
    )
}

/// PR-2: liveness recency window (mirrors the binding-reconcile heartbeat window,
/// binding.rs:380). A heartbeat / PTY input within this counts as alive.
const LIVENESS_WINDOW_MS: u64 = 3_600_000; // 1h

/// PR-2: per-agent jitter ceiling (hours) added to the age cap, so a fleet whose
/// leases all crossed the cap together (e.g. after a long daemon outage) is
/// reclaimed spread across ticks rather than in a single thundering-herd archive.
const FORCE_RECLAIM_JITTER_HOURS: i64 = 6;

/// t-worktree-leak PR-2: how a candidate was selected — drives the retention
/// sweep's action (clean releases just archive; force-reclaims also emit a LOUD
/// confidence-classified ALERT).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcKind {
    /// Released past the grace TTL — the normal, expected path.
    CleanRelease,
    /// Never released, agent abandoned (no liveness) AND past the age cap — the
    /// force-reclaim backstop tail (no-event abandonment / dead agent).
    ForceReclaim,
}

/// A worktree identified as a GC candidate.
#[derive(Debug, Clone)]
pub struct GcCandidate {
    pub path: PathBuf,
    pub agent: String,
    pub reason: String,
    /// t-worktree-leak PR-2: selection kind (clean-release vs force-reclaim).
    pub kind: GcKind,
}

/// Scan for GC candidates: daemon-tagged, past grace TTL, not pinned, no active binding.
/// Max directory depth the marker-walk descends under the worktree root. Covers
/// flat (`<agent>-<enc>/` = depth 1), nested (`<agent>/<branch>/` = depth 2), and
/// slash-branch (`<agent>/fix/xxx/` = depth 3) layouts with headroom; bounded so a
/// pathological tree can't make the walk unbounded.
const MARKER_WALK_MAX_DEPTH: usize = 5;

/// t-worktree-leak (reviewer-2 #4): recursively collect daemon-managed worktree
/// dirs (those holding a `.agend-managed` marker) under `root`, to any depth up to
/// `max_depth`. Once a dir carries the marker it IS a worktree → collected and NOT
/// descended into (so we never walk a worktree's own working tree). This replaces
/// the old fixed-depth scan that missed slash-branch worktrees.
fn collect_managed_worktrees(root: &Path, max_depth: usize, out: &mut Vec<PathBuf>) {
    if max_depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        if p.join(MANAGED_MARKER).exists() {
            out.push(p); // a worktree — collect, don't descend into its working tree
        } else {
            collect_managed_worktrees(&p, max_depth - 1, out);
        }
    }
}

pub fn gc_candidates(home: &Path) -> Vec<GcCandidate> {
    let mut candidates = Vec::new();
    // t-worktree-leak PR-2: snapshot the live-agent set ONCE per pass (the
    // force-reclaim liveness check consults it per candidate; this is the
    // process-alive signal that protects idle-but-running agents).
    let live_agents: std::collections::HashSet<String> =
        crate::runtime::list_agents_with_fallback(home)
            .into_iter()
            .collect();

    // New layout: <home>/worktrees/<agent>/<branch>/ — but a SLASH branch
    // (`fix/xxx`, `feat/yyy` = the common case) nests an EXTRA level
    // (`<agent>/fix/xxx`), so the old fixed 1-level descent enumerated `fix` (not a
    // worktree, no marker) and missed the real worktree entirely (reviewer-2 #4, a
    // pre-existing GC bug that silently disabled GC — clean-release AND
    // force-reclaim — for most branches). Walk DOWN to the `.agend-managed` marker
    // instead of assuming a fixed depth.
    let new_root = daemon_managed_worktree_root(home);
    let mut managed = Vec::new();
    collect_managed_worktrees(&new_root, MARKER_WALK_MAX_DEPTH, &mut managed);
    for wt_path in &managed {
        if let Some(candidate) = evaluate_candidate(home, wt_path, &live_agents) {
            candidates.push(candidate);
        }
    }

    // Legacy layout: <home>/workspace/*/.worktrees/*/
    let workspace = crate::paths::workspace_dir(home);
    if workspace.exists() {
        if let Ok(entries) = std::fs::read_dir(&workspace) {
            for entry in entries.flatten() {
                let wt_base = entry.path().join(".worktrees");
                if !wt_base.is_dir() {
                    continue;
                }
                if let Ok(wts) = std::fs::read_dir(&wt_base) {
                    for wt in wts.flatten() {
                        let wt_path = wt.path();
                        if !wt_path.is_dir() {
                            continue;
                        }
                        if let Some(candidate) = evaluate_candidate(home, &wt_path, &live_agents) {
                            candidates.push(candidate);
                        }
                    }
                }
            }
        }
    }

    candidates
}

/// t-worktree-leak PR-2 safety #1: does the agent show ANY sign of life? This is
/// MULTI-signal — never just heartbeat — so an idle-but-running agent (no recent
/// heartbeat) is still protected. A positive on ANY signal → the worktree is
/// NEVER force-reclaimed, regardless of age (liveness-AND-age). Reads that fail
/// lean toward "alive" (conservative — never mis-reclaim).
fn agent_has_liveness(
    home: &Path,
    agent: &str,
    live_agents: &std::collections::HashSet<String>,
) -> bool {
    // (process) In the live-agent registry — covers idle-but-running agents that
    // are not currently heartbeating.
    if live_agents.contains(agent) {
        return true;
    }
    let hb = crate::daemon::heartbeat_pair::snapshot_for(agent);
    let now = crate::daemon::heartbeat_pair::now_ms();
    // (heartbeat) any MCP tool call within the recency window.
    if hb.heartbeat_at_ms != 0 && now.saturating_sub(hb.heartbeat_at_ms) < LIVENESS_WINDOW_MS {
        return true;
    }
    // (PTY) recent terminal input.
    if hb.last_input_at_ms != 0 && now.saturating_sub(hb.last_input_at_ms) < LIVENESS_WINDOW_MS {
        return true;
    }
    // (waiting_on) actively declared a blocker → alive.
    if hb.waiting_on_since_ms.is_some() {
        return true;
    }
    // (ci-watch) subscribed to a live ci-watch → active CI-tracked work.
    if agent_is_ci_watch_subscriber(home, agent) {
        return true;
    }
    false
}

/// t-worktree-leak PR-2: fresh multi-signal liveness check for `agent` (snapshots
/// the live-agent set itself). Used by the retention sweep's pre-archive fencing
/// re-validation so an agent that came back to life between enumeration and
/// archive is spared.
pub(crate) fn is_agent_alive(home: &Path, agent: &str) -> bool {
    let live_agents: std::collections::HashSet<String> =
        crate::runtime::list_agents_with_fallback(home)
            .into_iter()
            .collect();
    agent_has_liveness(home, agent, &live_agents)
}

/// PR-2: is `agent` a subscriber on any live ci-watch? codex gap ②: this is a
/// liveness source, so every read failure FAILS TOWARD ALIVE (returns `true`,
/// blocking reclaim) rather than silently treating the agent as not-subscribed —
/// a mis-read must never let us reclaim a live agent. The ONE exception is the
/// watch dir being genuinely absent (NotFound), which is a real "no watches"
/// state, not a read failure.
fn agent_is_ci_watch_subscriber(home: &Path, agent: &str) -> bool {
    let dir = crate::daemon::ci_watch::ci_watches_dir(home);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true, // can't enumerate watches → fail-toward-alive
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // A watch file we cannot read/parse COULD carry this agent's subscription
        // → fail-toward-alive rather than skip it.
        let Ok(content) = std::fs::read_to_string(&path) else {
            return true;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) else {
            return true;
        };
        if let Some(subs) = v.get("subscribers").and_then(|s| s.as_array()) {
            if subs
                .iter()
                .any(|s| s.get("instance").and_then(|i| i.as_str()) == Some(agent))
            {
                return true;
            }
        }
    }
    false
}

/// PR-2: is a never-released lease past the (per-agent jittered) force-reclaim age
/// cap? The deterministic jitter spreads a fleet whose leases all crossed the cap
/// together across ticks (anti-thundering-herd, safety #3). No `leased_at` → not
/// reclaimable (conservative).
fn leased_at_force_reclaimable(leased_at: Option<&str>, agent: &str) -> bool {
    let Some(ts) = leased_at else {
        return false;
    };
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return false;
    };
    let age = chrono::Utc::now().signed_duration_since(dt.with_timezone(&chrono::Utc));
    let jitter_h = (fnv1a(agent) % (FORCE_RECLAIM_JITTER_HOURS.max(1) as u64)) as i64;
    let cap = chrono::Duration::days(force_reclaim_age_days()) + chrono::Duration::hours(jitter_h);
    age > cap
}

/// Stable per-agent FNV-1a hash → deterministic jitter (no randomness, so reclaim
/// timing is reproducible).
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn evaluate_candidate(
    home: &Path,
    wt_path: &Path,
    live_agents: &std::collections::HashSet<String>,
) -> Option<GcCandidate> {
    // Must be daemon-managed (R14).
    if !is_daemon_managed(wt_path) {
        return None;
    }
    // Must not be pinned.
    if is_pinned(wt_path) {
        return None;
    }
    // Resolve agent name: read from .agend-managed marker (authoritative),
    // fall back to parent dir name (new layout) or file_name (legacy).
    let marker = wt_path.join(MANAGED_MARKER);
    let marker_content = std::fs::read_to_string(&marker).unwrap_or_default();
    let agent_name = marker_content
        .lines()
        .find(|l| l.starts_with("agent="))
        .and_then(|l| l.strip_prefix("agent="))
        .map(String::from)
        .or_else(|| {
            // New layout: <home>/worktrees/<agent>/<branch>/ → parent is agent
            wt_path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(String::from)
        })
        .unwrap_or_default();
    if agent_name.is_empty() {
        return None;
    }
    let binding_present = crate::binding::read(home, &agent_name).is_some();
    let released_at = marker_content
        .lines()
        .find_map(|l| l.strip_prefix("released_at="));

    match released_at {
        // ── Clean-release path: explicitly released, past the grace TTL. ──
        Some(ts) => {
            // A released lease should already be unbound; if it is still bound,
            // that is a contradiction — leave it alone (conservative).
            if binding_present {
                return None;
            }
            match chrono::DateTime::parse_from_rfc3339(ts) {
                Ok(dt) => {
                    let age =
                        chrono::Utc::now().signed_duration_since(dt.with_timezone(&chrono::Utc));
                    if age < chrono::Duration::hours(GC_GRACE_HOURS) {
                        return None; // still within grace
                    }
                }
                // #1870 (H1): a malformed `released_at=` (e.g. a partial-write /
                // crash-truncated marker) MUST NOT be treated as "past grace" — the
                // grace window protects a just-released worktree's WIP. But it is
                // also `Some(garbage)`, so it never reaches the never-released
                // force-reclaim arm below → pre-#1882 it leaked FOREVER (both GC
                // paths skipped it). #1882 (WT-LEAK-1): treat "corrupt released_at ≈
                // never-released" — hand off to the SAME force-reclaim backstop. Its
                // liveness + leased_at age-cap guards (NOT the unparseable grace
                // window) protect a still-used / recently-leased worktree; only an
                // abandoned (no liveness, leased past the cap) corrupt-marker
                // worktree is reclaimed. This does NOT reintroduce the H1
                // WIP-destruction (that was the grace-window bypass).
                Err(_) => {
                    return force_reclaim_candidate(
                        home,
                        wt_path,
                        agent_name,
                        &marker_content,
                        live_agents,
                        "malformed released_at marker",
                    );
                }
            }
            Some(GcCandidate {
                path: wt_path.to_path_buf(),
                agent: agent_name,
                reason: format!("daemon-tagged, released >{}h, not pinned", GC_GRACE_HOURS),
                kind: GcKind::CleanRelease,
            })
        }
        // ── t-worktree-leak PR-2 force-reclaim backstop: NEVER released. ──
        // This is ONLY the no-event-abandonment / dead-agent tail (the
        // invariant + sweeper in PR-1 handle every worktree that DID see a
        // merge/close/task-done event; the 7-day expired-intent path hands off
        // here). liveness-AND-age: ANY live signal → never reclaim (even past the
        // cap); otherwise require the per-agent-jittered age cap.
        None => force_reclaim_candidate(
            home,
            wt_path,
            agent_name,
            &marker_content,
            live_agents,
            "never-released lease",
        ),
    }
}

/// t-worktree-leak PR-2 force-reclaim backstop: reclaim a worktree ONLY when it
/// is genuinely abandoned — not in the daemon's post-boot grace window (#5), NO
/// liveness signal for its agent, AND its `leased_at` is past the per-agent
/// force-reclaim age cap. ANY live signal → never reclaim (even past the cap).
/// Shared by the never-released (`released_at` absent) arm AND the #1882 WT-LEAK-1
/// corrupt-`released_at` fall-through. `marker_state` names why we're here, for
/// the candidate's reason. The liveness + age-cap guards (NOT the grace window)
/// are what protect a just-leased / just-released worktree from premature reclaim.
fn force_reclaim_candidate(
    home: &Path,
    wt_path: &Path,
    agent_name: String,
    marker_content: &str,
    live_agents: &std::collections::HashSet<String>,
    marker_state: &str,
) -> Option<GcCandidate> {
    // reviewer-2 #5: suspend force-reclaim during the daemon's post-boot grace
    // window (the process-liveness signal is still re-establishing).
    if daemon_within_boot_grace(home) {
        return None;
    }
    if agent_has_liveness(home, &agent_name, live_agents) {
        return None;
    }
    let leased_at = marker_content
        .lines()
        .find_map(|l| l.strip_prefix("leased_at="));
    if !leased_at_force_reclaimable(leased_at, &agent_name) {
        return None;
    }
    Some(GcCandidate {
        path: wt_path.to_path_buf(),
        agent: agent_name,
        reason: format!(
            "force-reclaim: {marker_state}, no liveness signal, leased >{}d (abandoned)",
            force_reclaim_age_days()
        ),
        kind: GcKind::ForceReclaim,
    })
}

/// Dry-run: log candidates without deleting. Returns candidate list.
pub fn gc_dry_run(home: &Path) -> Vec<GcCandidate> {
    let candidates = gc_candidates(home);
    for c in &candidates {
        tracing::info!(
            agent = %c.agent,
            path = %c.path.display(),
            reason = %c.reason,
            "gc_dry_run candidate"
        );
    }
    if !candidates.is_empty() {
        crate::event_log::log(
            home,
            "gc_dry_run",
            "",
            &format!("{} candidates identified", candidates.len()),
        );
    }
    candidates
}

/// Result of a single GC removal attempt.
#[derive(Debug, Clone)]
pub struct GcResult {
    pub path: PathBuf,
    pub agent: String,
    pub removed: bool,
    pub error: Option<String>,
}

/// Execute GC: remove all candidates identified by [`gc_candidates`].
/// Each candidate is removed via `git worktree remove --force` with
/// `remove_dir_all` fallback (mirrors [`release_full`] deletion pattern).
pub fn gc_run(home: &Path) -> Vec<GcResult> {
    let candidates = gc_candidates(home);
    if candidates.is_empty() {
        return Vec::new();
    }
    let mut results = Vec::new();
    for c in &candidates {
        let result = gc_remove_one(home, c);
        results.push(result);
    }
    let removed_count = results.iter().filter(|r| r.removed).count();
    let removed_paths: Vec<String> = results
        .iter()
        .filter(|r| r.removed)
        .map(|r| r.path.display().to_string())
        .collect();
    if removed_count > 0 {
        crate::event_log::log(
            home,
            "gc_run",
            "",
            &format!(
                "{removed_count} worktrees removed: [{}]",
                removed_paths.join(", ")
            ),
        );
    }
    results
}

fn gc_remove_one(home: &Path, candidate: &GcCandidate) -> GcResult {
    let wt_path = &candidate.path;

    // t-worktree-leak PR-2 (codex gap ① CRITICAL): a force-reclaim candidate MUST
    // NEVER be hard-deleted. Route it through the SINGLE safe deletion path
    // (retention's `maybe_remove_candidate`: pre-archive liveness re-check +
    // atomic archive-to-trash + unbind + LOUD confidence ALERT), so the daemon
    // `gc_run` path and the retention sweep cannot diverge into an irrecoverable
    // delete. Clean-release candidates keep the historical hard-delete below.
    if candidate.kind == GcKind::ForceReclaim {
        use crate::daemon::retention::worktrees::{maybe_remove_candidate, RemovalOutcome};
        let outcome = maybe_remove_candidate(home, candidate);
        return GcResult {
            path: wt_path.clone(),
            agent: candidate.agent.clone(),
            removed: matches!(outcome, RemovalOutcome::Removed),
            error: match outcome {
                RemovalOutcome::Skipped { reason } => Some(reason),
                RemovalOutcome::Removed => None,
            },
        };
    }

    // Acquire the same binding lock that bind_full() uses, making
    // GC deletion and bind mutually exclusive (eliminates TOCTOU).
    let lock_path = crate::paths::runtime_dir(home)
        .join(&candidate.agent)
        .join(".binding.json.lock");
    let _lock = match crate::store::acquire_file_lock(&lock_path) {
        Ok(l) => l,
        Err(e) => {
            return GcResult {
                path: wt_path.clone(),
                agent: candidate.agent.clone(),
                removed: false,
                error: Some(format!("skipped: binding lock acquisition failed: {e}")),
            };
        }
    };

    // Re-validate under lock: binding/pinned/grace state may have
    // changed since gc_candidates() enumerated this worktree. t-worktree-leak
    // PR-2: re-snapshot liveness here too, so a force-reclaim candidate whose
    // agent came back to life between enumeration and removal is spared (fencing).
    let live_agents: std::collections::HashSet<String> =
        crate::runtime::list_agents_with_fallback(home)
            .into_iter()
            .collect();
    if evaluate_candidate(home, wt_path, &live_agents).is_none() {
        return GcResult {
            path: wt_path.clone(),
            agent: candidate.agent.clone(),
            removed: false,
            error: Some("skipped: pre-deletion re-validation failed".to_string()),
        };
    }

    let source_repo = resolve_source_repo(wt_path);

    let mut result = GcResult {
        path: wt_path.clone(),
        agent: candidate.agent.clone(),
        removed: false,
        error: None,
    };

    // git-raw-allowed: `source_repo` may be None, in which case this runs with
    // NO `current_dir` and git resolves the repo from the absolute worktree path.
    // `git_cmd`/`git_bypass` both REQUIRE a cwd; there is no repo-tree dir to pass
    // when source_repo is None (wt_path's parent is the worktrees-pool dir, per
    // lead ruling). The conditional `current_dir` is the whole point — keep raw.
    let mut cmd = std::process::Command::new("git");
    cmd.args([
        "worktree",
        "remove",
        "--force",
        &wt_path.display().to_string(),
    ])
    .env("AGEND_GIT_BYPASS", "1");
    if let Some(ref sr) = source_repo {
        cmd.current_dir(sr);
    }
    match cmd.output() {
        Ok(o) if o.status.success() => {
            result.removed = true;
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            tracing::warn!(
                agent = %candidate.agent,
                path = %wt_path.display(),
                error = %stderr,
                "gc: git worktree remove failed — falling back to remove_dir_all"
            );
            let _ = std::fs::remove_dir_all(wt_path);
            if !wt_path.exists() {
                if let Some(ref sr) = source_repo {
                    // W1.2: best-effort prune (result already ignored).
                    let _ = crate::git_helpers::git_ok(sr, &["worktree", "prune"]);
                }
                result.removed = true;
            } else {
                result.error = Some(format!("git worktree remove failed: {stderr}"));
            }
        }
        Err(e) => {
            tracing::warn!(
                agent = %candidate.agent,
                path = %wt_path.display(),
                error = %e,
                "gc: git command failed"
            );
            result.error = Some(format!("git command failed: {e}"));
        }
    }
    result
}

/// Resolve the source (owning) repo from a worktree's `.git` file.
/// A git worktree's `.git` is a file containing `gitdir: <path>` pointing
/// to `<source>/.git/worktrees/<name>`. We walk up from that to find the
/// source repo root.
fn resolve_source_repo(wt_path: &Path) -> Option<PathBuf> {
    let git_file = wt_path.join(".git");
    let content = std::fs::read_to_string(&git_file).ok()?;
    let gitdir_line = content.lines().find(|l| l.starts_with("gitdir:"))?;
    let gitdir = gitdir_line.strip_prefix("gitdir:")?.trim();
    let gitdir_path = if Path::new(gitdir).is_absolute() {
        PathBuf::from(gitdir)
    } else {
        wt_path.join(gitdir).canonicalize().ok()?
    };
    // gitdir_path is <source>/.git/worktrees/<name>
    // Walk up: worktrees → .git → source_repo
    gitdir_path.parent()?.parent()?.parent().map(PathBuf::from)
}

/// Cleanup stale ci-watch lock files whose PRs merged >7 days ago.
pub fn gc_stale_ci_watch_locks(home: &Path) -> usize {
    let ci_dir = home.join("ci-watches");
    if !ci_dir.is_dir() {
        return 0;
    }
    let mut removed = 0;
    let cutoff = chrono::Utc::now() - chrono::Duration::days(7);
    if let Ok(entries) = std::fs::read_dir(&ci_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("lock") {
                continue;
            }
            // Check file modification time as a proxy for PR merge time.
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let modified_dt: chrono::DateTime<chrono::Utc> = modified.into();
            if modified_dt < cutoff && std::fs::remove_file(&path).is_ok() {
                tracing::info!(path = %path.display(), "gc: removed stale ci-watch lock");
                removed += 1;
            }
        }
    }
    if removed > 0 {
        crate::event_log::log(
            home,
            "gc_stale_ci_watch_locks",
            "",
            &format!("{removed} stale lock files removed"),
        );
    }
    removed
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn tmp_home(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-pool-test-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    fn tmp_repo(tag: &str) -> PathBuf {
        let dir = tmp_home(tag);
        // #1463: scratch-repo git must bypass the agend-git shim, else an
        // agent-run suite (AGEND_INSTANCE_NAME set) ChdirPass-redirects the
        // commit into the bound worktree (init-pile pollution).
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .ok();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ])
            .current_dir(&dir)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .ok();
        dir
    }

    #[test]
    fn lease_main_branch_rejected() {
        let home = tmp_home("main-reject");
        let repo = tmp_repo("main-reject-repo");
        let result = lease(&home, &repo, "agent-1", "main");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("E4.5"));
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn lease_creates_daemon_tagged_worktree() {
        let home = tmp_home("lease-tag");
        let repo = tmp_repo("lease-tag-repo");
        let result = lease(&home, &repo, "agent-2", "feat/test");
        assert!(result.is_ok());
        let l = result.expect("lease");
        assert!(l.path.exists());
        assert!(is_daemon_managed(&l.path));
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_marks_candidate_no_delete() {
        let home = tmp_home("release");
        let repo = tmp_repo("release-repo");
        let l = lease(&home, &repo, "agent-3", "feat/release").expect("lease");
        release(&home, &l);
        // Worktree still exists (no delete in Phase 3).
        assert!(l.path.exists());
        // Binding cleared.
        assert!(crate::binding::read(&home, "agent-3").is_none());
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_idempotent() {
        let home = tmp_home("release-idem");
        let repo = tmp_repo("release-idem-repo");
        let l = lease(&home, &repo, "agent-4", "feat/idem").expect("lease");
        release(&home, &l);
        release(&home, &l); // second release — no panic
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    // ── Sprint 53 P0-X: release_full (hard release) tests ───────────────
    //
    // These call the production function `release_full`, which in turn is
    // the body of the `release_worktree` MCP tool. The MCP layer test in
    // `src/mcp/handlers/worktree.rs` covers the handler contract; here we
    // focus on the filesystem semantics.
    //
    // Regression-proof: comment out the `git worktree remove` block in
    // `release_full` and `p0x_release_full_happy_path_removes_worktree_and_binding`
    // FAILS (`worktree_removed` stays false; `l.path.exists()` stays true).
    // Restore → PASS. See commit message §regression-proof.

    #[test]
    fn p0x_release_full_happy_path_removes_worktree_and_binding() {
        let home = tmp_home("p0x-happy");
        let repo = tmp_repo("p0x-happy-repo");
        let l = lease(&home, &repo, "agent-h", "feat/happy").expect("lease");
        // Pre-condition: lease created both binding + worktree.
        assert!(l.path.exists(), "pre: worktree must exist");
        assert!(crate::binding::read(&home, "agent-h").is_some());
        assert!(is_daemon_managed(&l.path));

        let outcome = release_full(&home, "agent-h", false);

        assert!(outcome.released, "happy path must report released");
        assert!(outcome.worktree_removed, "worktree must be removed");
        assert!(outcome.binding_removed, "binding must be removed");
        assert!(outcome.error.is_none(), "no error: {:?}", outcome.error);
        assert!(!l.path.exists(), "worktree dir must be gone post-release");
        assert!(crate::binding::read(&home, "agent-h").is_none());

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// §3.9 regression (#t-21 HIGH #1): `release_full(dry_run=true)` must be
    /// observation-only — the worktree directory AND binding.json must survive.
    /// Pre-fix, `remove_worktree` + `clear_binding_state` ran unconditionally,
    /// so a dry run actually destroyed both. Regression-proof: revert the
    /// `if dry_run` guard in `release_full` and this FAILS (`l.path` gone,
    /// binding cleared).
    #[test]
    fn dry_run_release_preserves_worktree_and_binding_t21() {
        let home = tmp_home("t21-dry-run");
        let repo = tmp_repo("t21-dry-run-repo");
        let l = lease(&home, &repo, "agent-dry", "feat/keep").expect("lease");
        assert!(l.path.exists(), "pre: worktree must exist");
        assert!(crate::binding::read(&home, "agent-dry").is_some());

        let outcome = release_full(&home, "agent-dry", true);

        // Observation-success, nothing actually removed.
        assert!(outcome.released, "dry-run reports observation success");
        assert!(
            !outcome.worktree_removed,
            "dry-run must NOT remove worktree"
        );
        assert!(!outcome.binding_removed, "dry-run must NOT clear binding");
        assert!(outcome.error.is_none(), "no error: {:?}", outcome.error);
        // The destructive effects are previewed, not performed.
        assert!(
            outcome.dry_run_preview.as_deref().is_some_and(
                |p| p.contains("would remove worktree") && p.contains("would clear binding")
            ),
            "dry-run must preview both effects: {:?}",
            outcome.dry_run_preview
        );
        // The actual on-disk state is untouched.
        assert!(
            l.path.exists(),
            "worktree dir MUST survive a dry-run release"
        );
        assert!(
            crate::binding::read(&home, "agent-dry").is_some(),
            "binding.json MUST survive a dry-run release"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn p0x_release_full_idempotent_second_call_noop() {
        // #1465: release is idempotent. The first call tears down; the
        // second (no binding left) is a SUCCESS no-op — `released:true,
        // already_released:true`, no error — NOT the pre-#1465 `released:
        // false + "no binding"` error (that encoded the bug this fixes).
        let home = tmp_home("p0x-idem");
        let repo = tmp_repo("p0x-idem-repo");
        lease(&home, &repo, "agent-i", "feat/idem").expect("lease");
        let r1 = release_full(&home, "agent-i", false);
        assert!(r1.released, "first call must release");
        assert!(
            !r1.already_released,
            "first call is a real teardown, not a no-op"
        );
        let r2 = release_full(&home, "agent-i", false);
        assert!(r2.released, "second call must be idempotent success");
        assert!(
            r2.already_released,
            "second call must flag already_released"
        );
        assert!(
            r2.error.is_none(),
            "idempotent no-op must NOT error: {:?}",
            r2.error
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn p0x_release_full_missing_binding_graceful() {
        // #1465: releasing an agent that never had a binding is a success
        // no-op (release target state already reached), not an error.
        let home = tmp_home("p0x-missing-binding");
        let outcome = release_full(&home, "ghost-agent", false);
        assert!(
            outcome.released,
            "missing binding must be idempotent success"
        );
        assert!(outcome.already_released, "must flag already_released");
        assert!(
            outcome.error.is_none(),
            "no-op must not error: {:?}",
            outcome.error
        );
        // Nothing was actually torn down — no worktree/binding removal.
        assert!(!outcome.worktree_removed);
        assert!(!outcome.binding_removed);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn p0x_release_full_missing_worktree_path_clears_binding_anyway() {
        // Binding exists but the worktree directory was deleted out from
        // under us (manual cleanup, daemon restart races, etc.). Spec:
        // "still remove binding (partial cleanup ok)".
        let home = tmp_home("p0x-missing-wt");
        let repo = tmp_repo("p0x-missing-wt-repo");
        let l = lease(&home, &repo, "agent-mw", "feat/mw").expect("lease");
        // Manually remove the worktree dir behind the daemon's back, but
        // leave the binding pointing at the now-stale path.
        std::fs::remove_dir_all(&l.path).ok();
        assert!(!l.path.exists(), "pre: worktree must be gone");
        assert!(crate::binding::read(&home, "agent-mw").is_some());

        let outcome = release_full(&home, "agent-mw", false);
        assert!(outcome.released, "must still release: {:?}", outcome);
        assert!(outcome.binding_removed, "binding must be cleared");
        assert!(
            !outcome.worktree_removed,
            "worktree wasn't removed by us (it was already gone)"
        );
        assert!(crate::binding::read(&home, "agent-mw").is_none());

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn p0x_release_full_unmanaged_worktree_skipped_safely() {
        // R14 safety: if the binding points at a worktree that lacks the
        // .agend-managed marker (operator-created, not daemon-leased), the
        // release MUST NOT remove the worktree. #1879 (WT-LEAK-2): the stale
        // binding IS cleared, though — leaving it leaked the binding and blocked
        // a same-agent re-bind. The worktree (operator data) survives for
        // investigation; the daemon's binding to it does not.
        let home = tmp_home("p0x-unmanaged");
        let unmanaged_wt = tmp_home("p0x-unmanaged-wt-target");
        // Hand-craft a binding pointing at an unmanaged path.
        std::fs::create_dir_all(crate::paths::runtime_dir(&home).join("agent-u")).ok();
        let binding = serde_json::json!({
            "version": 1,
            "agent": "agent-u",
            "task_id": "T-1",
            "branch": "feat/manual",
            "issued_at": chrono::Utc::now().to_rfc3339(),
            "worktree": unmanaged_wt.display().to_string(),
        });
        std::fs::write(
            crate::paths::runtime_dir(&home)
                .join("agent-u")
                .join("binding.json"),
            serde_json::to_string_pretty(&binding).unwrap(),
        )
        .unwrap();
        // Sanity: no marker.
        assert!(!is_daemon_managed(&unmanaged_wt));

        let outcome = release_full(&home, "agent-u", false);
        assert!(
            !outcome.released,
            "unmanaged worktree must NOT be released: {:?}",
            outcome
        );
        assert!(
            outcome.binding_removed,
            "#1879 WT-LEAK-2: the stale binding must be CLEARED even when the unmanaged worktree removal is refused"
        );
        assert!(
            outcome
                .error
                .as_deref()
                .unwrap_or("")
                .contains(".agend-managed"),
            "error must explain the marker check: {:?}",
            outcome.error
        );
        assert!(unmanaged_wt.exists(), "operator-created dir must survive");
        assert!(
            crate::binding::read(&home, "agent-u").is_none(),
            "#1879 WT-LEAK-2: the binding must be cleared (no leak / re-bind block)"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&unmanaged_wt).ok();
    }

    /// Helper: assert `git worktree list --porcelain` from `repo` does NOT
    /// emit any `prunable` line (registry leak indicator).
    fn assert_no_prunable_registry(repo: &Path, scenario: &str) {
        let output = std::process::Command::new("git")
            .current_dir(repo)
            .args(["worktree", "list", "--porcelain"])
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git worktree list");
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        assert!(
            !stdout.contains("prunable"),
            "[{scenario}] git worktree registry must be clean — found `prunable` entry. Output:\n{stdout}"
        );
    }

    #[test]
    fn p0x_release_full_clears_git_worktree_registry() {
        // r1 reviewer (PR #470): the prior IMPL didn't pass `.current_dir(source_repo)`
        // and the `remove_dir_all` fallback didn't `git worktree prune`, so
        // `git worktree list --porcelain` kept emitting `prunable` entries
        // that would block re-lease (registry vs filesystem skew).
        //
        // Scenario A: happy path — `release_full` invokes `git worktree
        // remove --force` from the owning repo's cwd. Registry must be clean.
        let home = tmp_home("p0x-registry-happy");
        let repo = tmp_repo("p0x-registry-happy-repo");
        let _l = lease(&home, &repo, "agent-r", "feat/registry").expect("lease");

        let outcome = release_full(&home, "agent-r", false);
        assert!(outcome.released);
        assert!(outcome.worktree_removed);
        assert_no_prunable_registry(&repo, "happy-path");

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn p0x_release_full_prunes_registry_after_external_dir_removal() {
        // Reviewer's exact failure mode: the worktree dir gets removed
        // externally (daemon crash mid-op, manual `rm`), so when `release_full`
        // runs the dir is already gone but the git registry still lists the
        // path as `prunable`. Without the explicit `git worktree prune` call
        // in the missing-path branch, the next lease re-attempt fails because
        // the registry sees the path as still claimed.
        //
        // This is the load-bearing regression-proof for the r1 fix:
        // commenting out the `git worktree prune` block in `release_full`'s
        // missing-path branch makes this test FAIL on the post-release
        // assertion. Restore → PASS.
        let home = tmp_home("p0x-registry-prune");
        let repo = tmp_repo("p0x-registry-prune-repo");
        let l = lease(&home, &repo, "agent-rm", "feat/prune").expect("lease");

        // Simulate the leak: yank the worktree dir behind git's back.
        std::fs::remove_dir_all(&l.path).ok();
        assert!(!l.path.exists(), "test setup: dir must be gone");

        // Pre-condition sanity: registry MUST list the now-missing entry as
        // `prunable` before release_full runs. If git's behavior changes and
        // this assertion no longer holds, the test setup is no longer
        // exercising the bug — flag it via panic in the assertion.
        let pre_output = std::process::Command::new("git")
            .current_dir(&repo)
            .args(["worktree", "list", "--porcelain"])
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .expect("git worktree list pre");
        let pre_stdout = String::from_utf8_lossy(&pre_output.stdout).to_string();
        assert!(
            pre_stdout.contains("prunable"),
            "test setup invariant: dir-removed worktree must show as prunable pre-release. Output:\n{pre_stdout}"
        );

        let outcome = release_full(&home, "agent-rm", false);
        assert!(outcome.released);
        assert!(outcome.binding_removed);

        // Post-condition: prune must have run, registry is clean.
        assert_no_prunable_registry(&repo, "post-external-rm");

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn p0x_release_full_via_handle_release_worktree_end_to_end() {
        // Production smoke (§5): exercise the full MCP path —
        // `handle_release_worktree(home, args, sender)` — the same function
        // the daemon dispatches `release_worktree` calls into. Asserts that
        // a leased agent + worktree gets fully cleaned up via the MCP layer.
        let home = tmp_home("p0x-prod-smoke");
        let repo = tmp_repo("p0x-prod-smoke-repo");
        let l = lease(&home, &repo, "agent-prod", "feat/prod").expect("lease");
        assert!(l.path.exists());

        let result = crate::mcp::handlers::worktree_test_release(
            &home,
            &serde_json::json!({"instance": "agent-prod"}),
        );
        assert_eq!(result["released"].as_bool(), Some(true), "{result}");
        assert_eq!(result["worktree_removed"].as_bool(), Some(true), "{result}");
        assert_eq!(result["binding_removed"].as_bool(), Some(true), "{result}");
        assert!(!l.path.exists(), "worktree must be removed by MCP path");
        assert!(crate::binding::read(&home, "agent-prod").is_none());

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn is_daemon_managed_excludes_human_worktrees() {
        let dir = tmp_home("human-wt");
        // No marker → not managed.
        assert!(!is_daemon_managed(&dir));
        // Add marker → managed.
        std::fs::write(dir.join(MANAGED_MARKER), "test").ok();
        assert!(is_daemon_managed(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pin_unpin_idempotent() {
        let dir = tmp_home("pin");
        pin(&dir);
        assert!(is_pinned(&dir));
        pin(&dir); // idempotent
        assert!(is_pinned(&dir));
        unpin(&dir);
        assert!(!is_pinned(&dir));
        unpin(&dir); // idempotent
        assert!(!is_pinned(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── Phase 4 GC tests ────────────────────────────────────────────

    fn make_gc_candidate(home: &Path, agent: &str) -> PathBuf {
        let wt = home
            .join("workspace")
            .join("repo")
            .join(".worktrees")
            .join(agent);
        std::fs::create_dir_all(&wt).ok();
        // Daemon-managed marker with old timestamp (past grace).
        let old_ts = (chrono::Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
        std::fs::write(
            wt.join(MANAGED_MARKER),
            format!("agent={agent}\nleased_at={old_ts}\nreleased_at={old_ts}\n"),
        )
        .ok();
        wt
    }

    #[test]
    fn gc_candidates_includes_only_daemon_tagged() {
        let home = tmp_home("gc-tagged");
        let wt = home
            .join("workspace")
            .join("repo")
            .join(".worktrees")
            .join("human");
        std::fs::create_dir_all(&wt).ok();
        // No .agend-managed marker → not a candidate.
        let candidates = gc_candidates(&home);
        assert!(
            candidates.is_empty(),
            "human worktree must not be candidate"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_candidates_excludes_pinned() {
        let home = tmp_home("gc-pinned");
        let wt = make_gc_candidate(&home, "pinned-agent");
        pin(&wt);
        let candidates = gc_candidates(&home);
        assert!(candidates.is_empty(), "pinned must not be candidate");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_candidates_respects_grace_ttl() {
        let home = tmp_home("gc-grace");
        let wt = home
            .join("workspace")
            .join("repo")
            .join(".worktrees")
            .join("fresh");
        std::fs::create_dir_all(&wt).ok();
        // Recent timestamp (within grace).
        let recent = chrono::Utc::now().to_rfc3339();
        std::fs::write(
            wt.join(MANAGED_MARKER),
            format!("agent=fresh\nleased_at={recent}\nreleased_at={recent}\n"),
        )
        .ok();
        let candidates = gc_candidates(&home);
        assert!(
            candidates.is_empty(),
            "fresh worktree within grace must not be candidate"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// §3.9 #1870 (H1): a worktree whose `.agend-managed` `released_at=` is
    /// MALFORMED (e.g. a partial-write / crash-truncated marker) must NOT be
    /// reclaimed — the grace window protects just-released WIP, so a parse
    /// failure fails conservative (skip GC). A valid PAST-grace `released_at`
    /// still yields a candidate (behavior unchanged). Regression-proof: revert
    /// the fix and the malformed worktree falls through to a CleanRelease
    /// candidate, so `bad-ts` appears.
    #[test]
    fn gc_candidates_skips_malformed_released_at_1870() {
        let home = tmp_home("gc-malformed-ts");
        // Malformed released_at + a RECENT lease → must be kept. #1870 stopped the
        // immediate grace-bypass reclaim; #1882 (WT-LEAK-1) then routes a corrupt
        // marker to the force-reclaim backstop — but its leased_at age-cap still
        // protects a recently-leased (possibly still-in-use) worktree. So a recent
        // `leased_at` here stays NOT a candidate (an ABANDONED corrupt marker IS
        // reclaimed — see force_reclaim_corrupt_marker_* tests).
        let recent = chrono::Utc::now().to_rfc3339();
        let bad = home
            .join("workspace")
            .join("repo")
            .join(".worktrees")
            .join("bad-ts");
        std::fs::create_dir_all(&bad).ok();
        std::fs::write(
            bad.join(MANAGED_MARKER),
            format!("agent=bad-ts\nleased_at={recent}\nreleased_at=not-a-timestamp\n"),
        )
        .ok();
        // Valid past-grace released_at → still a candidate (unchanged).
        make_gc_candidate(&home, "good-ts");

        let agents: Vec<String> = gc_candidates(&home).into_iter().map(|c| c.agent).collect();
        assert!(
            !agents.iter().any(|a| a == "bad-ts"),
            "#1870/#1882: a malformed released_at on a RECENT lease must NOT be reclaimed (age-cap protects it), got: {agents:?}"
        );
        assert!(
            agents.iter().any(|a| a == "good-ts"),
            "#1870: a valid past-grace released_at must STILL yield a candidate (unchanged), got: {agents:?}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_candidates_excludes_active_binding() {
        let home = tmp_home("gc-active");
        make_gc_candidate(&home, "active-agent");
        // Create active binding.
        crate::binding::bind(&home, "active-agent", "T-1", "feat");
        let candidates = gc_candidates(&home);
        assert!(candidates.is_empty(), "active binding must exclude from GC");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn dry_run_no_actual_delete() {
        let home = tmp_home("gc-dry");
        let wt = make_gc_candidate(&home, "dry-agent");
        gc_dry_run(&home);
        assert!(wt.exists(), "dry-run must NOT delete");
        std::fs::remove_dir_all(&home).ok();
    }

    // ------------------------------------------------------------------
    // Sprint 57 Wave 2 Track B (#546 Item 2) — release_worktree must
    // unsubscribe the released agent from EVERY ci-watch they appear
    // on, not just the binding-branch entry.
    // ------------------------------------------------------------------

    /// Helper: write a synthetic ci-watch JSON listing the given
    /// subscribers on `(repo, branch)`. Returns the watch path.
    fn write_ci_watch(
        home: &std::path::Path,
        repo: &str,
        branch: &str,
        subscribers: &[&str],
    ) -> PathBuf {
        write_ci_watch_with_extras(home, repo, branch, subscribers, None, None)
    }

    /// #931: variant that also stores `next_after_ci` (workflow chain) and
    /// `last_notified_head_sha` (polling state). Used by the decouple-fix
    /// tests to assert release_full preserves these fields.
    fn write_ci_watch_with_extras(
        home: &std::path::Path,
        repo: &str,
        branch: &str,
        subscribers: &[&str],
        next_after_ci: Option<&str>,
        last_notified_head_sha: Option<&str>,
    ) -> PathBuf {
        let ci_dir = crate::daemon::ci_watch::ci_watches_dir(home);
        std::fs::create_dir_all(&ci_dir).ok();
        let filename = crate::daemon::ci_watch::watch_filename(repo, branch);
        let path = ci_dir.join(&filename);
        let subs: Vec<serde_json::Value> = subscribers
            .iter()
            .map(|s| serde_json::json!({"instance": *s}))
            .collect();
        let mut watch = serde_json::json!({
            "repo": repo,
            "branch": branch,
            "interval_secs": 60,
            "subscribers": subs,
            "instance": subscribers.first().copied().unwrap_or(""),
            "last_run_id": 12345_u64,
            "head_sha": "deadbeefcafe",
            "last_polled_at": chrono::Utc::now().timestamp_millis(),
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
            "last_terminal_seen_at": null,
        });
        if let Some(n) = next_after_ci {
            watch["next_after_ci"] = serde_json::json!(n);
        }
        if let Some(sha) = last_notified_head_sha {
            watch["last_notified_head_sha"] = serde_json::json!(sha);
        }
        std::fs::write(&path, serde_json::to_string_pretty(&watch).unwrap()).ok();
        path
    }

    /// #931 helper: read a watch field as string (returns None if absent or
    /// not a string). Used by decouple tests to assert state preservation.
    fn read_ci_watch_field(path: &std::path::Path, field: &str) -> Option<String> {
        let content = std::fs::read_to_string(path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&content).ok()?;
        v.get(field)?.as_str().map(String::from)
    }

    /// Read a ci-watch JSON's subscriber `instance` strings. Returns
    /// empty Vec if file missing or parse fails — `assert` on the
    /// caller handles the missing-file case as appropriate.
    fn read_ci_watch_subscribers(path: &std::path::Path) -> Vec<String> {
        let Ok(content) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        let Ok(watch) = serde_json::from_str::<serde_json::Value>(&content) else {
            return Vec::new();
        };
        crate::daemon::ci_watch::parse_subscribers(&watch)
    }

    #[test]
    fn release_worktree_unsubscribes_all_agent_ci_watches() {
        // #931 INVERTED (was Sprint 57 Wave 2 Track B #546 Item 2 pin).
        //
        // Pre-#931: release_full unconditionally swept the released agent
        // out of EVERY ci-watch they appeared on (binding-branch + ad-hoc).
        // That cleanup cascaded to watch-file deletion when the released
        // agent was the sole subscriber, destroying `next_after_ci`
        // chains and polling state — 4-in-a-row PR stalls
        // (#920/#925/#928/#929) traced to this exact path.
        //
        // Post-#931 (Direction A.1): release_full no longer mutates any
        // ci-watch on the agent's behalf. Subscriptions persist across
        // release per operator intent in issue #931:
        //   "Subscription persists across bind handoff unless explicitly
        //    `unwatch`ed."
        //
        // Hygiene is delegated to:
        //   - 72h absolute TTL (`expires_at`)
        //   - 72h inactivity TTL (`last_terminal_seen_at`)
        //   - PR-terminal auto-clear (poller's `check_pr_terminal`)
        //   - Explicit `ci action=unwatch` (operator-callable)
        //
        // This test now PINS the new persist-across-release behavior so
        // a regression that re-introduces the broad sweep is caught
        // immediately. Rollback criteria documented in PR #931 body.
        let home = tmp_home("931-persist-multi");
        let repo = tmp_repo("931-persist-multi-repo");
        let l = lease(&home, &repo, "dev", "feat-track-x").expect("lease");
        assert!(l.path.exists(), "pre: worktree must exist");

        let auto_watch = write_ci_watch(&home, "owner/repo", "feat-track-x", &["dev", "lead"]);
        let main_watch = write_ci_watch(&home, "owner/repo", "main", &["dev", "lead"]);
        let bystander = write_ci_watch(&home, "owner/repo", "feat-bystander", &["lead"]);

        let outcome = release_full(&home, "dev", false);

        assert!(outcome.released, "release must succeed");
        assert!(outcome.binding_removed, "binding must be cleared");

        // Auto-watch (binding-branch): dev MUST STILL be subscribed.
        let auto_subs = read_ci_watch_subscribers(&auto_watch);
        assert!(
            auto_subs.contains(&"dev".to_string()),
            "#931: dev must persist on binding-branch watch — got {auto_subs:?}"
        );
        assert!(
            auto_subs.contains(&"lead".to_string()),
            "lead untouched on binding-branch watch — got {auto_subs:?}"
        );

        // Ad-hoc cross-branch watch on main: dev MUST STILL be subscribed.
        let main_subs = read_ci_watch_subscribers(&main_watch);
        assert!(
            main_subs.contains(&"dev".to_string()),
            "#931: dev must persist on ad-hoc main watch — got {main_subs:?}"
        );

        // Bystander: untouched (dev never subscribed).
        assert!(bystander.exists(), "bystander watch must survive untouched");

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_worktree_deletes_watch_when_last_subscriber_unsubscribes() {
        // #931 INVERTED (was P0-X bonus delete-on-empty pin).
        //
        // Pre-#931: when the released agent was the sole subscriber,
        // release_full deleted the watch file entirely — losing
        // `next_after_ci`, `last_notified_head_sha`, polling state.
        // Post-#931: file persists across release. Cleanup via TTL
        // and PR-terminal paths only.
        let home = tmp_home("931-persist-sole");
        let repo = tmp_repo("931-persist-sole-repo");
        let _l = lease(&home, &repo, "dev", "feat-x").expect("lease");

        let solo_watch = write_ci_watch(&home, "owner/repo", "main", &["dev"]);

        release_full(&home, "dev", false);

        assert!(
            solo_watch.exists(),
            "#931: sole-subscriber watch must persist across release (TTL handles cleanup)"
        );
        // Subs should still contain dev — pure persistence.
        let subs = read_ci_watch_subscribers(&solo_watch);
        assert!(
            subs.contains(&"dev".to_string()),
            "#931: dev persists in subs across release — got {subs:?}"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    // ── #931: ci_watch decouple from worktree release lifecycle ─────────
    //
    // Issue: 4-in-a-row PR stalls overnight (#920/#925/#928/#929) traced
    // to `release_full` calling `unsubscribe_all_ci_watches_for_agent`,
    // which removed the released agent from every ci-watch (binding-branch
    // and ad-hoc), cascading to watch-file deletion on sole-subscriber.
    // The cascade destroyed `next_after_ci` chains + polling state, so
    // reviewer/dev never received post-CI handoff notifications.
    //
    // Direction A.1 (operator-approved 2026-05-19): decouple subscription
    // from worktree binding entirely. Hygiene via 72h TTL + PR-terminal
    // auto-clear + explicit unwatch only.
    //
    // RED→GREEN regression-proof anchors: each test below documents the
    // pre-fix failure signature; if the call at the historic
    // `unsubscribe_all_ci_watches_for_agent` site is re-introduced, these
    // tests immediately fail.

    #[test]
    fn release_does_not_delete_ci_watch_when_agent_was_sole_subscriber_931() {
        // Anchor: pre-#931 release_full ran `remove_file(&path)` when subs
        // became empty (`unsubscribe_all_ci_watches_for_agent`,
        // `worktree_pool.rs:464-468`). The watch file gone → poller skipped
        // → `next_after_ci` target never injected. Post-#931 the file
        // persists with full state.
        let home = tmp_home("931-sole-persist");
        let repo = tmp_repo("931-sole-persist-repo");
        let _l = lease(&home, &repo, "dev", "feat/931-sole").expect("lease");

        let watch_path = write_ci_watch_with_extras(
            &home,
            "owner/repo",
            "feat/931-sole",
            &["dev"],
            Some("reviewer"),
            Some("cafe1234"),
        );
        assert!(watch_path.exists(), "pre: watch exists");

        release_full(&home, "dev", false);

        assert!(
            watch_path.exists(),
            "#931 GREEN: sole-subscriber watch file MUST persist across release"
        );
        assert_eq!(
            read_ci_watch_field(&watch_path, "next_after_ci"),
            Some("reviewer".to_string()),
            "#931 GREEN: next_after_ci chain MUST survive release"
        );
        assert_eq!(
            read_ci_watch_field(&watch_path, "last_notified_head_sha"),
            Some("cafe1234".to_string()),
            "#931 GREEN: polling state MUST survive release"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_does_not_remove_agent_from_multi_subscriber_watch_931() {
        // Anchor: pre-#931, retain(|s| s != agent) shrank subscriber lists
        // on EVERY watch the released agent appeared on, including non-
        // binding-branch ad-hoc watches (e.g. agent watching `main` to
        // follow upstream during closeout). Post-#931, no subscriber list
        // is mutated on release — operator's stated direction is full
        // persistence.
        let home = tmp_home("931-multi-persist");
        let repo = tmp_repo("931-multi-persist-repo");
        let _l = lease(&home, &repo, "dev", "feat/binding").expect("lease");

        let binding_watch =
            write_ci_watch(&home, "owner/repo", "feat/binding", &["dev", "reviewer"]);
        let other_watch = write_ci_watch(&home, "owner/repo", "feat/other", &["dev"]);

        release_full(&home, "dev", false);

        // Binding branch watch: dev preserved alongside reviewer.
        let binding_subs = read_ci_watch_subscribers(&binding_watch);
        assert!(
            binding_subs.contains(&"dev".to_string()),
            "#931 GREEN: dev preserved on binding-branch watch — got {binding_subs:?}"
        );
        assert!(
            binding_subs.contains(&"reviewer".to_string()),
            "co-subscriber preserved — got {binding_subs:?}"
        );

        // Non-binding branch watch: dev preserved untouched.
        let other_subs = read_ci_watch_subscribers(&other_watch);
        assert!(
            other_subs.contains(&"dev".to_string()),
            "#931 GREEN: dev preserved on non-binding-branch ad-hoc watch — got {other_subs:?}"
        );
        assert!(
            other_watch.exists(),
            "non-binding-branch watch file preserved"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_under_rebase_mode_preserves_subscription_931() {
        // #931 Fix 1 corollary: when `bind_self(rebase_mode=true)` triggers
        // `rebase_clean_self` (force_release/mod.rs:187), the underlying
        // `release_full` call MUST preserve the ci-watch so that the
        // immediately-following `dispatch_auto_bind_lease` re-arms the
        // existing watch via append-idempotent handle_watch_ci — keeping
        // any prior `next_after_ci` chain intact.
        //
        // Pre-#931: rebase_clean_self → release_full → file deleted
        // (sole-sub case) → re-dispatch creates fresh watch missing
        // next_after_ci → reviewer never gets [ci-ready-for-action].
        //
        // Post-#931: file persists across the rebase round-trip; the
        // re-dispatch sees the same watch JSON and appends; chain intact.
        //
        // This test exercises the release-half of the rebase cycle
        // directly (calling release_full is what rebase_clean_self does
        // internally). The full bind_self(rebase_mode=true) round-trip
        // is covered by the dispatch_hook test for next_after_ci wiring
        // (test 6) — those two together pin both halves.
        let home = tmp_home("931-rebase");
        let repo = tmp_repo("931-rebase-repo");
        let _l = lease(&home, &repo, "dev", "feat/rebase-cycle").expect("lease");

        let watch_path = write_ci_watch_with_extras(
            &home,
            "owner/repo",
            "feat/rebase-cycle",
            &["dev"],
            Some("reviewer"),
            Some("beefcafe"),
        );

        // Release (the rebase_clean_self path's release_full invocation).
        release_full(&home, "dev", false);

        // File persists with next_after_ci + state intact across release.
        assert!(
            watch_path.exists(),
            "#931 GREEN: rebase-path release_full must preserve watch file"
        );
        assert_eq!(
            read_ci_watch_field(&watch_path, "next_after_ci"),
            Some("reviewer".to_string()),
            "#931 GREEN: next_after_ci chain survives rebase-path release"
        );
        assert_eq!(
            read_ci_watch_field(&watch_path, "last_notified_head_sha"),
            Some("beefcafe".to_string()),
            "#931 GREEN: polling state survives rebase-path release"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn explicit_unwatch_wins_over_concurrent_release_931() {
        // #931 race invariant (§3.20 SOP 1 deterministic): when an operator
        // explicitly unsubscribes an agent (`ci action=unwatch`) AND a
        // concurrent release_full fires, the explicit-unwatch's destructive
        // intent (drop agent from subs; remove watch if sole) MUST be the
        // surviving outcome regardless of arrival order.
        //
        // Post-#931 Fix 1 the race is degenerate by construction:
        // release_full is a no-op against ci-watch state, so the explicit
        // unwatch alone decides the outcome. This test pins that property
        // so a future regression that re-introduces release-side mutation
        // (or worse, race-with-unwatch double-write) is caught.
        let home = tmp_home("931-unwatch-vs-release");
        let repo = tmp_repo("931-unwatch-vs-release-repo");
        let _l = lease(&home, &repo, "dev", "feat/unwatch-race").expect("lease");

        let watch_path = write_ci_watch(&home, "owner/repo", "feat/unwatch-race", &["dev"]);

        // Order 1: release then explicit unwatch via direct file mutation
        // (mirrors what `handle_unwatch_ci`'s last-subscriber path does:
        // remove the watch file). Deterministic — no sleep, no threads.
        release_full(&home, "dev", false);
        assert!(
            watch_path.exists(),
            "release_full is no-op for ci-watch post-#931"
        );

        // Simulate explicit unwatch: agent's removal cascades to file
        // deletion (sole-subscriber path of handle_unwatch_ci).
        let _ = std::fs::remove_file(&watch_path);

        assert!(
            !watch_path.exists(),
            "#931: explicit unwatch wins → watch file gone after both ops"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn poll_tick_vs_subscriber_mutation_preserves_single_delivery_931() {
        // #931 race invariant (§3.20 SOP 1 deterministic): a poll cycle
        // reading the watch file MUST see a consistent subscriber list
        // even if release_full or handle_watch_ci (subscribe) interleaves.
        //
        // Post-#931 Fix 1, release_full does not mutate ci-watch state →
        // the only mutating writer on this file is `handle_watch_ci`
        // (append) and `handle_unwatch_ci` (shrink/delete). All use
        // `crate::store::atomic_write` so a half-written file is never
        // observed by a concurrent reader (atomicity == temp-file +
        // rename invariant).
        //
        // Determinism: this test does NOT spawn threads. Instead it
        // exercises the read-modify-write contract sequentially and
        // asserts the file's parseability + subscriber stability invariant
        // at each step. SOP 1 pattern — no sleeps, no joins.
        let home = tmp_home("931-poll-mut-race");
        let repo = tmp_repo("931-poll-mut-race-repo");
        let _l = lease(&home, &repo, "dev", "feat/poll-mut").expect("lease");

        let watch_path = write_ci_watch(&home, "owner/repo", "feat/poll-mut", &["dev", "reviewer"]);

        // Snapshot 1: pre-release reading must observe both subscribers
        // and be a fully-parseable JSON (atomic-write invariant).
        let snap1 = read_ci_watch_subscribers(&watch_path);
        assert_eq!(snap1.len(), 2, "pre-release snapshot: 2 subscribers");

        // Release fires — must not corrupt file or strip subscribers.
        release_full(&home, "dev", false);

        // Snapshot 2: post-release reading STILL parses + STILL has both.
        let snap2 = read_ci_watch_subscribers(&watch_path);
        assert_eq!(
            snap1, snap2,
            "#931: release_full preserves subscriber list (poll reader sees stable state)"
        );

        // File still atomically parseable (no partial write).
        let content = std::fs::read_to_string(&watch_path).expect("readable");
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("parseable JSON");
        assert_eq!(parsed["branch"].as_str(), Some("feat/poll-mut"));

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn released_agent_still_receives_ci_pass_inject_931() {
        // #931 MANDATORY INTEGRATION TEST.
        //
        // End-to-end: after release_full, the agent's subscription on
        // the binding branch's ci-watch MUST persist so that a subsequent
        // CI-pass poll cycle still enqueues `[ci-pass]` to their inbox.
        // Pre-#931 this was impossible because release_full stripped the
        // agent and (in sole-subscriber case) deleted the file entirely.
        //
        // Note on harness: this test exercises the SUBSCRIPTION half of
        // the integration (release → subs preserved → file ready to be
        // polled), not the full HTTP→provider→enqueue chain (that's
        // already covered by `mock_success_run_updates_watch_state` and
        // others in poller.rs#tests, which use the in-process MockCiProvider).
        // The decouple fix is purely about subscriber-state preservation
        // across release; the poll path is unchanged.
        //
        // Specifically: we assert that immediately after release_full,
        // (a) the watch file exists, (b) the released agent is still in
        // subscribers, (c) the next_after_ci chain is intact, (d) the
        // poll-state fields haven't been clobbered. If all four hold,
        // the next ci_check_repo invocation by the daemon's tick loop
        // will fan out [ci-pass] to the agent verbatim — same code path
        // as the unchanged poller tests verify.
        let home = tmp_home("931-integration-still-receives");
        let repo = tmp_repo("931-integration-still-receives-repo");
        let _l = lease(&home, &repo, "dev", "feat/integration").expect("lease");

        // Pre-state: ci-watch armed with dev as sole subscriber + chain.
        let watch_path = write_ci_watch_with_extras(
            &home,
            "owner/repo",
            "feat/integration",
            &["dev"],
            Some("reviewer"),
            Some("cafefeed"),
        );

        // The operator's pattern: dev pushes PR + releases worktree
        // (frees for next task), expects CI-pass notification later.
        release_full(&home, "dev", false);

        // INTEGRATION ASSERTIONS — all four conditions for the poll
        // pipeline to fan out [ci-pass] to dev's inbox:
        assert!(
            watch_path.exists(),
            "#931 GREEN: (a) watch file present after release"
        );

        let subs = read_ci_watch_subscribers(&watch_path);
        assert!(
            subs.contains(&"dev".to_string()),
            "#931 GREEN: (b) dev still in subscribers — got {subs:?}"
        );

        assert_eq!(
            read_ci_watch_field(&watch_path, "next_after_ci"),
            Some("reviewer".to_string()),
            "#931 GREEN: (c) next_after_ci chain intact"
        );

        // Polling state: last_notified_head_sha preserved (so dedup +
        // rerun detection both keep working).
        assert_eq!(
            read_ci_watch_field(&watch_path, "last_notified_head_sha"),
            Some("cafefeed".to_string()),
            "#931 GREEN: (d) polling state preserved"
        );

        // Pre-#931, all four would fail in the sole-subscriber case
        // because the watch file was deleted entirely. The fact that the
        // existing poller test `mock_success_run_updates_watch_state`
        // demonstrates the [ci-pass] enqueue path works given a valid
        // watch file completes the end-to-end argument.

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_dry_run_does_not_mutate_subscribers_931() {
        // Defensive: dry_run=true is contract-defined as observation-only.
        // Pre-#931, even dry_run paths through release_full would invoke
        // the unsubscribe sweep (no dry_run gate around it). Post-#931
        // there's nothing to gate — but the test pins the invariant in
        // case future code re-introduces mutation on this path.
        let home = tmp_home("931-dry-run");
        let repo = tmp_repo("931-dry-run-repo");
        let _l = lease(&home, &repo, "dev", "feat/dry").expect("lease");

        let watch_path = write_ci_watch_with_extras(
            &home,
            "owner/repo",
            "feat/dry",
            &["dev", "reviewer"],
            Some("next-agent"),
            None,
        );
        let subs_before = read_ci_watch_subscribers(&watch_path);

        let outcome = release_full(&home, "dev", true);
        // dry_run skips actual git/binding teardown semantics elsewhere;
        // we only assert ci-watch state is identical pre/post.

        let subs_after = read_ci_watch_subscribers(&watch_path);
        assert_eq!(
            subs_before, subs_after,
            "#931: dry_run must not mutate subscriber list — before {subs_before:?} after {subs_after:?} outcome {outcome:?}"
        );
        assert_eq!(
            read_ci_watch_field(&watch_path, "next_after_ci"),
            Some("next-agent".to_string()),
            "#931: dry_run must preserve next_after_ci"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    // ── Issue #611: branch cleanup tests ────────────────────────────────

    #[test]
    fn release_full_deletes_merged_branch() {
        let home = tmp_home("611-merged");
        let repo = tmp_repo("611-merged-repo");
        // Lease creates the branch + worktree.
        let l = lease(&home, &repo, "agent-611m", "feat/merged").expect("lease");
        // Add a commit on the feature branch via the worktree.
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "feat",
            ])
            .current_dir(&l.path)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        // Merge feat/merged into main from the source repo (without checking it out).
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "merge",
                "feat/merged",
                "--no-ff",
                "-m",
                "merge",
            ])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();

        let outcome = release_full(&home, "agent-611m", false);

        assert!(outcome.released);
        assert!(
            outcome.branch_deleted,
            "merged branch must be deleted: {:?}",
            outcome
        );
        assert!(outcome.branch_cleanup_skipped_reason.is_none());
        // Verify branch is actually gone from the repo.
        let branch_exists = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "feat/merged"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(
            !branch_exists,
            "branch must not exist in repo after cleanup"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_full_preserves_unmerged_branch() {
        let home = tmp_home("611-unmerged");
        let repo = tmp_repo("611-unmerged-repo");
        // Lease creates the branch + worktree.
        let l = lease(&home, &repo, "agent-611u", "feat/unmerged").expect("lease");
        // Add a commit on the feature branch (not merged into main).
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "wip",
            ])
            .current_dir(&l.path)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();

        let outcome = release_full(&home, "agent-611u", false);

        assert!(outcome.released);
        assert!(
            !outcome.branch_deleted,
            "unmerged branch must NOT be deleted"
        );
        assert_eq!(
            outcome.branch_cleanup_skipped_reason.as_deref(),
            Some("branch not merged into main")
        );
        // Verify branch still exists.
        let branch_exists = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "feat/unmerged"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(branch_exists, "unmerged branch must still exist in repo");

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_full_absent_worktree_merged_branch_cleaned_up() {
        let home = tmp_home("1249-absent-merged");
        let repo = tmp_repo("1249-absent-merged-repo");
        let l = lease(&home, &repo, "agent-1249m", "feat/absent-merged").expect("lease");
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "feat",
            ])
            .current_dir(&l.path)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "merge",
                "feat/absent-merged",
                "--no-ff",
                "-m",
                "merge",
            ])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        // Remove worktree directory to simulate absent-worktree scenario.
        std::fs::remove_dir_all(&l.path).unwrap();
        std::process::Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();

        let outcome = release_full(&home, "agent-1249m", false);

        assert!(outcome.released);
        assert!(
            outcome.branch_deleted,
            "merged branch must be deleted even when worktree absent: {outcome:?}"
        );
        assert!(outcome.branch_cleanup_skipped_reason.is_none());
        let branch_exists = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "feat/absent-merged"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(!branch_exists, "branch must not exist after cleanup");
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_full_absent_worktree_unmerged_branch_preserved() {
        let home = tmp_home("1249-absent-unmerged");
        let repo = tmp_repo("1249-absent-unmerged-repo");
        let l = lease(&home, &repo, "agent-1249u", "feat/absent-unmerged").expect("lease");
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "wip",
            ])
            .current_dir(&l.path)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        // Remove worktree directory without merging.
        std::fs::remove_dir_all(&l.path).unwrap();
        std::process::Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();

        let outcome = release_full(&home, "agent-1249u", false);

        assert!(outcome.released);
        assert!(
            !outcome.branch_deleted,
            "unmerged branch must NOT be deleted"
        );
        assert_eq!(
            outcome.branch_cleanup_skipped_reason.as_deref(),
            Some("branch not merged into main")
        );
        let branch_exists = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "feat/absent-unmerged"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(branch_exists, "unmerged branch must still exist");
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_full_dry_run_does_not_delete_branch() {
        let home = tmp_home("611-dryrun");
        let repo = tmp_repo("611-dryrun-repo");
        // Lease creates the branch + worktree.
        let l = lease(&home, &repo, "agent-611d", "feat/dryrun").expect("lease");
        // Add a commit and merge into main.
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "feat",
            ])
            .current_dir(&l.path)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "merge",
                "feat/dryrun",
                "--no-ff",
                "-m",
                "merge",
            ])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();

        let outcome = release_full(&home, "agent-611d", true);

        assert!(outcome.released);
        assert!(!outcome.branch_deleted, "dry-run must NOT delete branch");
        assert_eq!(
            outcome.branch_cleanup_skipped_reason.as_deref(),
            Some("dry-run: would delete branch 'feat/dryrun'")
        );
        // Verify branch still exists.
        let branch_exists = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "feat/dryrun"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(branch_exists, "branch must survive dry-run");

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    /// §3.9 (#t-7, #1824 follow-up): a dry-run `release_full` must be
    /// observation-only — it must NOT run the ref-mutating `git fetch --prune`
    /// inside `cleanup_merged_branch`. Proven by planting a STALE
    /// remote-tracking ref (`refs/remotes/origin/ghost`, absent on the real
    /// origin) that a `fetch --prune` WOULD remove, then asserting it survives a
    /// dry-run. Regression-proof: un-gate the fetch and `ghost` is pruned →
    /// the ref set differs.
    #[test]
    fn dry_run_release_does_not_mutate_remote_tracking_refs_t7() {
        fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("AGEND_GIT_BYPASS", "1")
                .output()
                .expect("git")
        }
        fn refs_remotes(dir: &std::path::Path) -> String {
            String::from_utf8_lossy(&git(dir, &["for-each-ref", "refs/remotes"]).stdout).to_string()
        }

        let home = tmp_home("t7-dryrun-refs");
        // A real upstream + a clone (so the clone has an `origin` remote +
        // refs/remotes/origin/*). `release_full` operates on the clone.
        let origin = tmp_repo("t7-origin");
        let source = tmp_home("t7-source");
        git(
            std::path::Path::new("/"),
            &[
                "clone",
                &origin.display().to_string(),
                &source.display().to_string(),
            ],
        );
        // Plant a stale remote-tracking ref that `fetch --prune` would remove.
        let head = String::from_utf8_lossy(&git(&source, &["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();
        git(&source, &["update-ref", "refs/remotes/origin/ghost", &head]);

        // Lease a worktree in the clone (binds source_repo=source); merge state
        // is irrelevant — the fetch runs BEFORE the merge check.
        let _l = lease(&home, &source, "agent-t7", "feat/t7").expect("lease");

        let before = refs_remotes(&source);
        assert!(
            before.contains("refs/remotes/origin/ghost"),
            "pre-cond: stale ghost ref planted: {before}"
        );

        let outcome = release_full(&home, "agent-t7", true); // dry-run
        assert!(outcome.released, "dry-run reports observation success");

        let after = refs_remotes(&source);
        assert_eq!(
            before, after,
            "dry-run must NOT mutate remote-tracking refs (no fetch --prune)"
        );
        assert!(
            after.contains("refs/remotes/origin/ghost"),
            "the prune-target stale ref must survive a dry-run: {after}"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&origin).ok();
        std::fs::remove_dir_all(&source).ok();
    }

    #[test]
    fn release_full_does_not_delete_unrelated_branch() {
        let home = tmp_home("unrelated-branch");
        let repo = tmp_repo("unrelated-branch-repo");
        // Create an unrelated user branch with its own commit
        std::process::Command::new("git")
            .args(["checkout", "-b", "user/my-feature"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-m",
                "user work",
            ])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["checkout", "main"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        // Lease a different branch
        let _l = lease(&home, &repo, "agent-x", "feat/daemon-task").expect("lease");
        let outcome = release_full(&home, "agent-x", false);
        assert!(outcome.released);
        // Unrelated branch must still exist
        let branch_exists = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "user/my-feature"])
            .current_dir(&repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(
            branch_exists,
            "unrelated user branch must NOT be deleted by release_worktree"
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn gc_new_layout_active_binding_not_candidate() {
        let home = tmp_home("gc-new-active");
        // Create new-layout worktree with active binding
        let wt = home.join("worktrees").join("dev-1").join("feat-branch");
        std::fs::create_dir_all(&wt).unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::hours(100)).to_rfc3339();
        std::fs::write(
            wt.join(MANAGED_MARKER),
            format!("agent=dev-1\nbranch=feat-branch\nleased_at={old}\nreleased_at={old}\n"),
        )
        .unwrap();
        // Create active binding for dev-1
        let rt = crate::paths::runtime_dir(&home).join("dev-1");
        std::fs::create_dir_all(&rt).unwrap();
        std::fs::write(
            rt.join("binding.json"),
            r#"{"worktree":"/tmp/x","branch":"feat-branch"}"#,
        )
        .unwrap();

        let candidates = gc_candidates(&home);
        assert!(
            candidates.is_empty(),
            "new-layout worktree with active binding must not be GC candidate"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_new_layout_released_past_grace_is_candidate() {
        let home = tmp_home("gc-new-released");
        let wt = home.join("worktrees").join("dev-2").join("old-branch");
        std::fs::create_dir_all(&wt).unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::hours(100)).to_rfc3339();
        std::fs::write(
            wt.join(MANAGED_MARKER),
            format!("agent=dev-2\nbranch=old-branch\nleased_at={old}\nreleased_at={old}\n"),
        )
        .unwrap();
        // No binding for dev-2

        let candidates = gc_candidates(&home);
        assert_eq!(
            candidates.len(),
            1,
            "new-layout released worktree past grace should be GC candidate"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── #807 Item 2 — ReleaseOutcome serialization shape ──

    #[test]
    fn test_release_outcome_success_omits_error_key() {
        // #807 Item 2 RED: pre-fix `ReleaseOutcome` always serializes
        // `error: None` → `"error": null`, which client renderers
        // (Claude Code, etc.) interpret as an `<error>` envelope on
        // what is actually a successful release. Fix: add
        // `#[serde(skip_serializing_if = "Option::is_none")]` so the
        // `error` key is absent on success.
        let outcome = ReleaseOutcome {
            released: true,
            worktree_removed: true,
            binding_removed: true,
            branch_deleted: true,
            ..Default::default()
        };
        let json = serde_json::to_value(&outcome).expect("serialize");
        let obj = json.as_object().expect("object shape");
        assert!(
            !obj.contains_key("error"),
            "success response must NOT carry `error` key (#807 cosmetic fix), got keys: {:?}",
            obj.keys().collect::<Vec<_>>()
        );
        assert!(
            !obj.contains_key("branch_cleanup_skipped_reason"),
            "success response must NOT carry `branch_cleanup_skipped_reason` when None, got keys: {:?}",
            obj.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_release_outcome_real_failure_emits_error_key() {
        // #807 Item 2 contract guarantee: actual failures STILL emit
        // the `error` field. Only the `None`-on-success case is
        // omitted — `skip_serializing_if` only drops `None`, never
        // `Some`.
        let outcome = ReleaseOutcome {
            released: false,
            error: Some("test failure".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_value(&outcome).expect("serialize");
        let obj = json.as_object().expect("object shape");
        assert!(
            obj.contains_key("error"),
            "real failure must surface `error` key, got keys: {:?}",
            obj.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            obj["error"], "test failure",
            "error message must round-trip unchanged"
        );
    }

    // ── gc_run tests ──────────────────────────────────────────────

    #[test]
    fn gc_run_returns_empty_when_no_candidates() {
        let home = tmp_home("gc-run-empty");
        let results = gc_run(&home);
        assert!(results.is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_stale_ci_watch_locks_removes_old_locks() {
        let home = tmp_home("gc-locks");
        let ci_dir = home.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).unwrap();

        // Create a lock file with an old mtime (> 7 days ago)
        let stale_lock = ci_dir.join("pr-123.lock");
        std::fs::write(&stale_lock, "locked").unwrap();
        // Set mtime to 8 days ago
        let eight_days_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 24 * 3600);
        let f = std::fs::File::options()
            .write(true)
            .open(&stale_lock)
            .unwrap();
        f.set_modified(eight_days_ago).unwrap();

        // Create a recent lock file (should NOT be removed)
        let recent_lock = ci_dir.join("pr-456.lock");
        std::fs::write(&recent_lock, "locked").unwrap();

        // Create a non-lock file (should NOT be removed)
        let json_file = ci_dir.join("pr-789.json");
        std::fs::write(&json_file, "{}").unwrap();

        let removed = gc_stale_ci_watch_locks(&home);
        assert_eq!(removed, 1, "only the stale lock should be removed");
        assert!(!stale_lock.exists(), "stale lock must be deleted");
        assert!(recent_lock.exists(), "recent lock must be preserved");
        assert!(json_file.exists(), "non-lock file must be preserved");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn gc_stale_ci_watch_locks_handles_missing_dir() {
        let home = tmp_home("gc-locks-nodir");
        let removed = gc_stale_ci_watch_locks(&home);
        assert_eq!(removed, 0);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resolve_source_repo_parses_gitdir_pointer() {
        let home = tmp_home("resolve-src");
        let fake_wt = home.join("wt");
        std::fs::create_dir_all(&fake_wt).unwrap();
        // Simulate .git file pointing to source/.git/worktrees/wt
        let source = home.join("source");
        let gitdir_target = source.join(".git").join("worktrees").join("wt");
        std::fs::create_dir_all(&gitdir_target).unwrap();
        std::fs::write(
            fake_wt.join(".git"),
            format!("gitdir: {}", gitdir_target.display()),
        )
        .unwrap();
        let resolved = resolve_source_repo(&fake_wt);
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap(), source);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn resolve_source_repo_returns_none_for_regular_repo() {
        let home = tmp_home("resolve-none");
        let fake_dir = home.join("regular");
        std::fs::create_dir_all(&fake_dir).unwrap();
        // A regular .git directory, not a worktree
        std::fs::create_dir_all(fake_dir.join(".git")).unwrap();
        let resolved = resolve_source_repo(&fake_dir);
        assert!(resolved.is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    // ── t-worktree-leak PR-2: force-reclaim backstop tests ──

    fn backdate_lease(wt_path: &Path, days_ago: i64) {
        let marker = wt_path.join(MANAGED_MARKER);
        let content = std::fs::read_to_string(&marker).unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::days(days_ago)).to_rfc3339();
        let new: String = content
            .lines()
            .map(|l| {
                if l.starts_with("leased_at=") {
                    format!("leased_at={old}")
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&marker, new).unwrap();
    }

    #[test]
    fn force_reclaim_dead_agent_past_cap_is_candidate() {
        let home = tmp_home("fr-dead");
        let repo = tmp_repo("fr-dead-repo");
        let lease = lease(&home, &repo, "dev-dead", "feat/x").expect("lease");
        backdate_lease(&lease.path, force_reclaim_age_days() + 2);
        let live: std::collections::HashSet<String> = std::collections::HashSet::new();
        let cand = evaluate_candidate(&home, &lease.path, &live);
        assert!(
            cand.is_some(),
            "dead agent, never-released, past cap → force-reclaim candidate"
        );
        assert_eq!(cand.unwrap().kind, GcKind::ForceReclaim);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Append a malformed `released_at=` to a lease's marker and drop its binding
    /// (a released worktree is unbound) — the #1882 WT-LEAK-1 corrupt-marker shape.
    fn corrupt_released_at(home: &Path, agent: &str, wt_path: &Path) {
        crate::binding::unbind(home, agent);
        let marker = wt_path.join(MANAGED_MARKER);
        let mut content = std::fs::read_to_string(&marker).unwrap();
        content.push_str("released_at=not-a-timestamp\n");
        std::fs::write(&marker, content).unwrap();
    }

    /// §3.9 #1882 (WT-LEAK-1): a corrupt-`released_at` worktree that is ABANDONED
    /// (no liveness, leased past the force-reclaim age cap) is now reclaimed via
    /// the force-reclaim backstop — pre-fix it leaked forever (the clean-release
    /// path returned None and the never-released arm was unreachable for a
    /// `Some(garbage)` released_at). Regression-proof: revert the parse-Err
    /// fall-through and this is None (leaked).
    #[test]
    fn force_reclaim_corrupt_marker_abandoned_is_candidate_1882() {
        let home = tmp_home("fr-corrupt-dead");
        let repo = tmp_repo("fr-corrupt-dead-repo");
        let lease = lease(&home, &repo, "dev-corrupt", "feat/x").expect("lease");
        corrupt_released_at(&home, "dev-corrupt", &lease.path);
        backdate_lease(&lease.path, force_reclaim_age_days() + 2);
        let live: std::collections::HashSet<String> = std::collections::HashSet::new();
        let cand = evaluate_candidate(&home, &lease.path, &live);
        assert_eq!(
            cand.map(|c| c.kind),
            Some(GcKind::ForceReclaim),
            "#1882: abandoned corrupt-marker worktree (no liveness, past cap) → force-reclaim, not leaked"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// §3.9 #1882 (WT-LEAK-1, no H1 regression): a corrupt-`released_at` worktree
    /// whose agent has a LIVENESS signal is SPARED even past the age cap — the
    /// force-reclaim liveness guard (not the unparseable grace window) protects a
    /// worktree the operator may still be using.
    #[test]
    fn force_reclaim_corrupt_marker_spares_live_1882() {
        let home = tmp_home("fr-corrupt-live");
        let repo = tmp_repo("fr-corrupt-live-repo");
        let lease = lease(&home, &repo, "dev-corrupt-live", "feat/x").expect("lease");
        corrupt_released_at(&home, "dev-corrupt-live", &lease.path);
        backdate_lease(&lease.path, force_reclaim_age_days() + 2);
        let live: std::collections::HashSet<String> =
            ["dev-corrupt-live".to_string()].into_iter().collect();
        assert!(
            evaluate_candidate(&home, &lease.path, &live).is_none(),
            "#1882: a live agent's corrupt-marker worktree must be SPARED (no H1-style WIP destruction)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn force_reclaim_spares_live_registry_agent() {
        // safety #1: any live signal → never reclaim, even past the cap.
        let home = tmp_home("fr-live");
        let repo = tmp_repo("fr-live-repo");
        let lease = lease(&home, &repo, "dev-live", "feat/x").expect("lease");
        backdate_lease(&lease.path, force_reclaim_age_days() + 2);
        let live: std::collections::HashSet<String> =
            ["dev-live".to_string()].into_iter().collect();
        assert!(
            evaluate_candidate(&home, &lease.path, &live).is_none(),
            "agent live in the registry → spared even past cap (liveness-AND-age)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn force_reclaim_spares_ci_watch_subscriber() {
        // multi-signal: a ci-watch subscription is a liveness signal (not heartbeat).
        let home = tmp_home("fr-ciw");
        let repo = tmp_repo("fr-ciw-repo");
        let lease = lease(&home, &repo, "dev-ciw", "feat/x").expect("lease");
        backdate_lease(&lease.path, force_reclaim_age_days() + 2);
        let ci_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
        std::fs::create_dir_all(&ci_dir).unwrap();
        std::fs::write(
            ci_dir.join("w.json"),
            serde_json::json!({
                "repo": "o/r", "branch": "feat/x",
                "subscribers": [{ "instance": "dev-ciw" }]
            })
            .to_string(),
        )
        .unwrap();
        let live: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert!(
            evaluate_candidate(&home, &lease.path, &live).is_none(),
            "agent subscribed to a ci-watch → spared (multi-signal liveness, not just heartbeat)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn force_reclaim_spares_recent_lease() {
        // dead agent but the lease is recent → not yet past the age cap.
        let home = tmp_home("fr-recent");
        let repo = tmp_repo("fr-recent-repo");
        let lease = lease(&home, &repo, "dev-recent", "feat/x").expect("lease");
        let live: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert!(
            evaluate_candidate(&home, &lease.path, &live).is_none(),
            "recent lease → not yet reclaimable (age gate)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    // codex gap ③: the heartbeat / PTY / waiting_on liveness signals + the
    // read-failure → fail-toward-alive path (§3.9, safety-critical).

    #[test]
    fn force_reclaim_spares_recent_heartbeat() {
        let home = tmp_home("fr-hb");
        let repo = tmp_repo("fr-hb-repo");
        let agent = "fr-hb-agent";
        let lease = lease(&home, &repo, agent, "feat/x").expect("lease");
        backdate_lease(&lease.path, force_reclaim_age_days() + 2);
        crate::daemon::heartbeat_pair::update_with(agent, |p| {
            p.heartbeat_at_ms = crate::daemon::heartbeat_pair::now_ms();
        });
        let live: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert!(
            evaluate_candidate(&home, &lease.path, &live).is_none(),
            "recent heartbeat → spared (heartbeat liveness signal)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn force_reclaim_spares_recent_pty_input() {
        let home = tmp_home("fr-pty");
        let repo = tmp_repo("fr-pty-repo");
        let agent = "fr-pty-agent";
        let lease = lease(&home, &repo, agent, "feat/x").expect("lease");
        backdate_lease(&lease.path, force_reclaim_age_days() + 2);
        crate::daemon::heartbeat_pair::update_with(agent, |p| {
            p.last_input_at_ms = crate::daemon::heartbeat_pair::now_ms();
        });
        let live: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert!(
            evaluate_candidate(&home, &lease.path, &live).is_none(),
            "recent PTY input → spared (PTY liveness signal)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn force_reclaim_spares_declared_waiting_on() {
        let home = tmp_home("fr-wait");
        let repo = tmp_repo("fr-wait-repo");
        let agent = "fr-wait-agent";
        let lease = lease(&home, &repo, agent, "feat/x").expect("lease");
        backdate_lease(&lease.path, force_reclaim_age_days() + 2);
        crate::daemon::heartbeat_pair::update_with(agent, |p| {
            p.waiting_on_since_ms = Some(crate::daemon::heartbeat_pair::now_ms());
        });
        let live: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert!(
            evaluate_candidate(&home, &lease.path, &live).is_none(),
            "declared waiting_on → spared (blocked-but-alive signal)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn force_reclaim_ci_watch_read_failure_fails_alive() {
        let home = tmp_home("fr-ciwfail");
        let repo = tmp_repo("fr-ciwfail-repo");
        let agent = "fr-ciwfail-agent";
        let lease = lease(&home, &repo, agent, "feat/x").expect("lease");
        backdate_lease(&lease.path, force_reclaim_age_days() + 2);
        // An unparseable ci-watch file → the liveness read fails → fail-toward-alive.
        let ci_dir = crate::daemon::ci_watch::ci_watches_dir(&home);
        std::fs::create_dir_all(&ci_dir).unwrap();
        std::fs::write(ci_dir.join("corrupt.json"), "{ this is not json").unwrap();
        let live: std::collections::HashSet<String> = std::collections::HashSet::new();
        assert!(
            evaluate_candidate(&home, &lease.path, &live).is_none(),
            "unparseable ci-watch → fail-toward-alive → spared (never reclaim on uncertainty)"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn gc_run_force_reclaim_archives_never_hard_deletes() {
        // codex gap ① CRITICAL: the daemon gc_run/gc_remove_one path must route a
        // force-reclaim through the SAFE helper, never hard-delete. Proof: it is
        // ARCHIVED to .trash (recoverable) rather than removed — the old
        // `git worktree remove --force` would have left nothing behind.
        let home = tmp_home("fr-gcrun");
        let repo = tmp_repo("fr-gcrun-repo");
        let lease = lease(&home, &repo, "fr-gcrun-agent", "feat/x").expect("lease");
        let cand = GcCandidate {
            path: lease.path.clone(),
            agent: "fr-gcrun-agent".to_string(),
            reason: "fr".to_string(),
            kind: GcKind::ForceReclaim,
        };
        let result = gc_remove_one(&home, &cand);
        assert!(
            result.removed,
            "force-reclaim via gc_run should archive: {:?}",
            result.error
        );
        assert!(!lease.path.exists(), "worktree moved out");
        let trash = home.join(".trash").join("worktrees");
        assert!(
            std::fs::read_dir(&trash)
                .map(|d| d.flatten().count() > 0)
                .unwrap_or(false),
            "gc_run force-reclaim must ARCHIVE to .trash (recoverable), never hard-delete"
        );
        assert!(
            crate::binding::read(&home, "fr-gcrun-agent").is_none(),
            "binding unbound after force-reclaim"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn collect_managed_worktrees_finds_slash_branch_nested() {
        // reviewer-2 #4: a slash-branch worktree nests an extra level
        // (worktrees/<agent>/fix/xxx) and was missed by the old fixed-depth scan.
        let home = tmp_home("walk-slash");
        let root = daemon_managed_worktree_root(&home);
        let nested = root.join("dev").join("fix").join("xxx");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join(MANAGED_MARKER), "agent=dev\n").unwrap();
        let flat = root.join("dev").join("track-x");
        std::fs::create_dir_all(&flat).unwrap();
        std::fs::write(flat.join(MANAGED_MARKER), "agent=dev\n").unwrap();
        let mut out = Vec::new();
        collect_managed_worktrees(&root, MARKER_WALK_MAX_DEPTH, &mut out);
        assert!(
            out.contains(&nested),
            "slash-branch nested worktree must be enumerated (reviewer-2 #4)"
        );
        assert!(out.contains(&flat), "non-slash worktree still enumerated");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn boot_grace_predicate_suspends_only_when_recent_or_unknown() {
        // reviewer-2 #5: recent boot → suspend; aged boot → proceed; unknown →
        // conservative suspend.
        assert!(
            within_boot_grace(Some(1000), 1100, 600),
            "100s after boot, 600s grace → in grace (suspend)"
        );
        assert!(
            !within_boot_grace(Some(1000), 2000, 600),
            "1000s after boot → past grace (proceed)"
        );
        assert!(
            within_boot_grace(None, 2000, 600),
            "unknown boot time → conservative suspend"
        );
    }
}

#[cfg(test)]
mod review_repro_worktree_git;
