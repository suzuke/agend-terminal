//! Worktree pool — daemon-managed lease/release lifecycle for git worktrees.
//!
//! Builds on existing `worktree.rs` (creation) + `binding.rs` (state).
//! Phase 3: lease/release + daemon-tag + E4.5 enforcement. GC deferred to Phase 4.

use std::path::{Path, PathBuf};

/// Marker file placed in daemon-managed worktrees (R14 mitigation).
const MANAGED_MARKER: &str = ".agend-managed";

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
    // E4.5: reject protected-branch lease. Single source of truth for
    // the protected set is `agent_ops::is_protected_ref` so adding a
    // new protected ref propagates here and to every other E4.5 site
    // (Sprint 57 Wave 2 Track B #546 Item 3).
    if crate::agent_ops::is_protected_ref(branch) {
        return Err(format!(
            "E4.5 violation: cannot lease worktree for protected branch '{branch}'"
        ));
    }

    // Create worktree using existing infrastructure. Sprint 57 Wave 4
    // (#546 Item 4): the new external layout requires `home` to
    // resolve the canonical path `$AGEND_HOME/worktrees/<agent>/<branch>/`.
    let info = match crate::worktree::create(home, source_repo, agent, Some(branch)) {
        Some(info) => info,
        None => return Err(format!("failed to create worktree for {agent}@{branch}")),
    };

    // Tag as daemon-managed (R14: only daemon-tagged worktrees are GC candidates).
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
    crate::binding::bind_full(home, agent, "", branch, &info.path, source_repo);

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
        let _ = std::fs::write(&marker, content);
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
    pub worktree_removed: bool,
    pub binding_removed: bool,
    pub branch_deleted: bool,
    pub branch_cleanup_skipped_reason: Option<String>,
    pub error: Option<String>,
}

/// Delete the local branch ref after worktree release, IFF:
/// - `managed_verified` is true (caller confirmed .agend-managed marker)
/// - Branch is merged into main OR remote tracking ref is gone
///
/// SAFETY: This function ONLY receives the branch from the daemon's own
/// binding record. User-checkout branches are never passed here because
/// release_full gates on the .agend-managed marker.
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

    // Prune stale remote refs before remote-gone detection
    // (GitHub deletes remote branch on merge but local ref may be stale)
    let _ = std::process::Command::new("git")
        .args(["fetch", "--prune", "origin"])
        .current_dir(source_repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output();

    // Check if branch is ancestor of main (fast-forward or true merge).
    let is_merged = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", branch, "main"])
        .current_dir(source_repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    // Check if remote tracking ref is gone (squash-merge detection).
    let is_gone = {
        let remote_name = std::process::Command::new("git")
            .args(["config", &format!("branch.{branch}.remote")])
            .current_dir(source_repo)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        if !remote_name.is_empty() {
            let remote_ref = format!("refs/remotes/{remote_name}/{branch}");
            let exists = std::process::Command::new("git")
                .args(["rev-parse", "--verify", &remote_ref])
                .current_dir(source_repo)
                .env("AGEND_GIT_BYPASS", "1")
                .output()
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

    // Delete the local branch.
    let del = std::process::Command::new("git")
        .args(["branch", "-D", branch])
        .current_dir(source_repo)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
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
/// Idempotent: second call on the same agent sees no binding, returns
/// `released: false, error: "no binding for agent X"` (per spec — not a
/// fatal error).
///
/// Partial cleanup: if the worktree path is missing or `git worktree remove`
/// fails, the binding is still cleared so the agent is not stuck in a
/// half-released state.
pub fn release_full(home: &Path, agent: &str, dry_run: bool) -> ReleaseOutcome {
    let mut out = ReleaseOutcome::default();
    let mut managed_verified = false;

    let Some(binding) = crate::binding::read(home, agent) else {
        // Idempotent no-op on second call. Per spec: not fatal.
        out.error = Some(format!("no binding for agent '{agent}'"));
        return out;
    };

    // Worktree removal: gated on (a) path present in binding, (b) path exists
    // on disk, (c) carries .agend-managed marker (R14 safety).
    let wt_path_str = binding["worktree"].as_str().unwrap_or("");
    if !wt_path_str.is_empty() {
        let wt_path = Path::new(wt_path_str);

        // Source-repo: read from binding (P0-X r1 schema field), else derive
        // from the daemon's worktree convention `<source>/.worktrees/<agent>`.
        // Without a correct cwd, `git worktree remove` may fail and the
        // fallback `remove_dir_all` would leak a stale registry entry.
        let source_repo: PathBuf = binding["source_repo"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                // Derive: parent of `.worktrees/<agent>` is the source repo.
                wt_path
                    .parent()
                    .filter(|p| p.file_name().and_then(|n| n.to_str()) == Some(".worktrees"))
                    .and_then(|p| p.parent())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(PathBuf::new);

        if !wt_path.exists() {
            tracing::info!(
                agent,
                path = %wt_path.display(),
                "release: worktree path already absent — pruning registry + clearing binding"
            );
            // Prune registry in case the source repo still lists this path.
            // Safe even when source_repo is empty — git just errors out and
            // we ignore the result.
            if !source_repo.as_os_str().is_empty() {
                let _ = std::process::Command::new("git")
                    .current_dir(&source_repo)
                    .args(["worktree", "prune"])
                    .env("AGEND_GIT_BYPASS", "1")
                    .output();
            }
        } else if !is_daemon_managed(wt_path) {
            // R14 safety: never touch operator-created worktrees.
            tracing::warn!(
                agent,
                path = %wt_path.display(),
                "release skipped: no .agend-managed marker — worktree left alone"
            );
            out.error = Some(format!(
                "worktree at {} has no .agend-managed marker — refusing to remove (binding NOT cleared)",
                wt_path.display()
            ));
            return out;
        } else {
            // git worktree remove --force, run from the OWNING repo's cwd
            // so the registry entry is cleaned up alongside the directory.
            // AGEND_GIT_BYPASS=1 bypasses the shim's worktree-deny matrix.
            managed_verified = true;
            let mut cmd = std::process::Command::new("git");
            cmd.args([
                "worktree",
                "remove",
                "--force",
                &wt_path.display().to_string(),
            ])
            .env("AGEND_GIT_BYPASS", "1");
            if !source_repo.as_os_str().is_empty() {
                cmd.current_dir(&source_repo);
            }
            let result = cmd.output();
            match result {
                Ok(o) if o.status.success() => {
                    out.worktree_removed = true;
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                    tracing::warn!(
                        agent,
                        error = %stderr,
                        path = %wt_path.display(),
                        "git worktree remove failed — falling back to remove_dir_all"
                    );
                    // Fallback: best-effort manual remove + registry prune.
                    let _ = std::fs::remove_dir_all(wt_path);
                    if !wt_path.exists() {
                        // Prune the registry so `git worktree list` doesn't
                        // keep advertising the (now-deleted) path. Without
                        // this the next lease re-attempt sees the entry,
                        // detects it as "checked out", and rejects.
                        if !source_repo.as_os_str().is_empty() {
                            let prune = std::process::Command::new("git")
                                .current_dir(&source_repo)
                                .args(["worktree", "prune"])
                                .env("AGEND_GIT_BYPASS", "1")
                                .output();
                            if let Err(e) = prune {
                                tracing::warn!(agent, error = %e, "git worktree prune failed");
                            }
                        }
                        out.worktree_removed = true;
                    } else {
                        out.error = Some(format!("git worktree remove failed: {stderr}"));
                        // Still clear binding (partial cleanup per spec).
                    }
                }
                Err(e) => {
                    tracing::warn!(agent, error = %e, "git command failed for release");
                    out.error = Some(format!("git command failed: {e}"));
                    // Still clear binding (partial cleanup per spec).
                }
            }
        }
    }

    // Always clear the binding (partial cleanup OK per spec — except when we
    // bailed early on the unmanaged-worktree safety gate above).
    crate::binding::unbind(home, agent);
    out.binding_removed = true;
    out.released = true;

    // Sprint 58 Wave 3 PR-2 (#9): defensive comprehensive cleanup of
    // every daemon-side bind-tracking layer, not just on-disk
    // `binding.json`. The dispatch-hook bind-in-flight set is RAII-
    // managed via `BindGuard::drop` — but a panic between guard
    // acquisition and the implicit `Drop` can leave a stale entry
    // that blocks re-bind silently. Since `release_full` is the
    // single source of truth for "agent is now released", it's the
    // right place to clear in-memory state too.
    crate::mcp::handlers::dispatch_hook::clear_bind_in_flight(home, agent);

    // Sprint 57 Wave 2 Track B (#546 Item 2): unsubscribe `agent` from
    // EVERY ci-watch they appear on, not just the binding-branch entry.
    // The Sprint 55 P0-B EC7 helper was scoped to the exact
    // `(released_repo, released_branch)` pair derived from binding.json
    // — but agents may have added ad-hoc watches outside their
    // binding-branch (e.g. `ci action=watch repo=… branch=main` to
    // follow upstream during a closeout cycle). Those leaked across
    // release until this enumerator landed. Best-effort: failures are
    // logged but never abort release.
    unsubscribe_all_ci_watches_for_agent(home, agent);

    // Issue #611: auto-cleanup merged local branch after release.
    // Read branch + source_repo from the binding we captured earlier.
    // Only proceed if we verified the .agend-managed marker (Finding 2).
    let branch = binding["branch"].as_str().unwrap_or("");
    let sr_str = binding["source_repo"].as_str().unwrap_or("");
    if !managed_verified {
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
fn unsubscribe_all_ci_watches_for_agent(home: &Path, agent: &str) {
    let ci_dir = home.join("ci-watches");
    let Ok(entries) = std::fs::read_dir(&ci_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
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
#[allow(dead_code)]
pub fn is_pinned(worktree_path: &Path) -> bool {
    worktree_path.join(".agend-pinned").exists()
}

/// Reconcile orphan leases at daemon startup (log only, no delete in Phase 3).
pub fn reconcile_orphan_leases(home: &Path) {
    let runtime_dir = home.join("runtime");
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

/// A worktree identified as a GC candidate.
#[derive(Debug, Clone)]
pub struct GcCandidate {
    pub path: PathBuf,
    pub agent: String,
    pub reason: String,
}

/// Scan for GC candidates: daemon-tagged, past grace TTL, not pinned, no active binding.
pub fn gc_candidates(home: &Path) -> Vec<GcCandidate> {
    let mut candidates = Vec::new();

    // New layout: <home>/worktrees/<agent>/<branch>/
    let new_root = daemon_managed_worktree_root(home);
    if new_root.is_dir() {
        if let Ok(agents) = std::fs::read_dir(&new_root) {
            for agent_entry in agents.flatten() {
                if !agent_entry.path().is_dir() {
                    continue;
                }
                if let Ok(branches) = std::fs::read_dir(agent_entry.path()) {
                    for branch_entry in branches.flatten() {
                        let wt_path = branch_entry.path();
                        if !wt_path.is_dir() {
                            continue;
                        }
                        if let Some(candidate) = evaluate_candidate(home, &wt_path) {
                            candidates.push(candidate);
                        }
                    }
                }
            }
        }
    }

    // Legacy layout: <home>/workspace/*/.worktrees/*/
    let workspace = home.join("workspace");
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
                        if let Some(candidate) = evaluate_candidate(home, &wt_path) {
                            candidates.push(candidate);
                        }
                    }
                }
            }
        }
    }

    candidates
}

fn evaluate_candidate(home: &Path, wt_path: &Path) -> Option<GcCandidate> {
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
    // Must not have active binding.
    if crate::binding::read(home, &agent_name).is_some() {
        return None;
    }
    // Must be past grace TTL (check released_at in .agend-managed marker).
    // If no released_at, worktree is still active (not yet released) → not a candidate.
    if let Some(released_line) = marker_content
        .lines()
        .find(|l| l.starts_with("released_at="))
    {
        let ts = released_line.strip_prefix("released_at=").unwrap_or("");
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
            let age = chrono::Utc::now().signed_duration_since(dt.with_timezone(&chrono::Utc));
            if age < chrono::Duration::hours(GC_GRACE_HOURS) {
                return None; // Still within grace period after release.
            }
        }
    } else {
        return None; // No released_at → still active, not a GC candidate.
    }

    Some(GcCandidate {
        path: wt_path.to_path_buf(),
        agent: agent_name,
        reason: format!("daemon-tagged, released >{}h, not pinned", GC_GRACE_HOURS),
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

/// Cutover: actually delete GC candidates. Requires AGEND_WORKTREE_GC=1 env.
/// Returns number of worktrees removed.
pub fn gc_cutover(home: &Path) -> usize {
    if std::env::var("AGEND_WORKTREE_GC").as_deref() != Ok("1") {
        tracing::debug!("gc_cutover skipped — AGEND_WORKTREE_GC not set");
        return 0;
    }
    let candidates = gc_candidates(home);
    let mut removed = 0;
    for c in &candidates {
        if std::fs::remove_dir_all(&c.path).is_ok() {
            removed += 1;
            tracing::info!(agent = %c.agent, path = %c.path.display(), "gc_cutover: removed");
        }
    }
    if removed > 0 {
        crate::event_log::log(
            home,
            "gc_cutover",
            "",
            &format!("{removed} worktrees removed"),
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
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
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

    #[test]
    fn p0x_release_full_idempotent_second_call_noop() {
        let home = tmp_home("p0x-idem");
        let repo = tmp_repo("p0x-idem-repo");
        lease(&home, &repo, "agent-i", "feat/idem").expect("lease");
        let r1 = release_full(&home, "agent-i", false);
        assert!(r1.released, "first call must release");
        let r2 = release_full(&home, "agent-i", false);
        assert!(!r2.released, "second call must report no release");
        assert!(
            r2.error.as_deref().unwrap_or("").contains("no binding"),
            "second call error must indicate no binding: {:?}",
            r2.error
        );
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn p0x_release_full_missing_binding_graceful() {
        let home = tmp_home("p0x-missing-binding");
        // No lease, no binding written. Calling release on a fresh agent.
        let outcome = release_full(&home, "ghost-agent", false);
        assert!(!outcome.released);
        assert!(!outcome.worktree_removed);
        assert!(!outcome.binding_removed);
        assert!(
            outcome
                .error
                .as_deref()
                .unwrap_or("")
                .contains("no binding"),
            "missing binding must surface clear error: {:?}",
            outcome.error
        );
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
        // release MUST NOT remove it. Binding is also kept so the operator
        // can investigate the inconsistency rather than be left half-cleaned.
        let home = tmp_home("p0x-unmanaged");
        let unmanaged_wt = tmp_home("p0x-unmanaged-wt-target");
        // Hand-craft a binding pointing at an unmanaged path.
        std::fs::create_dir_all(home.join("runtime").join("agent-u")).ok();
        let binding = serde_json::json!({
            "version": 1,
            "agent": "agent-u",
            "task_id": "T-1",
            "branch": "feat/manual",
            "issued_at": chrono::Utc::now().to_rfc3339(),
            "worktree": unmanaged_wt.display().to_string(),
        });
        std::fs::write(
            home.join("runtime").join("agent-u").join("binding.json"),
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
            !outcome.binding_removed,
            "binding must be preserved when worktree is unmanaged (operator visibility)"
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
            crate::binding::read(&home, "agent-u").is_some(),
            "binding kept for operator visibility"
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
            &serde_json::json!({"agent": "agent-prod"}),
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

    #[test]
    fn cutover_requires_env_flag() {
        let home = tmp_home("gc-cutover-no-flag");
        let wt = make_gc_candidate(&home, "no-flag-agent");
        // Explicitly unset to avoid race with cutover_deletes_with_flag test.
        unsafe { std::env::remove_var("AGEND_WORKTREE_GC") };
        let removed = gc_cutover(&home);
        assert_eq!(removed, 0, "cutover must skip without flag");
        assert!(wt.exists(), "worktree must survive without flag");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn cutover_deletes_with_flag() {
        let home = tmp_home("gc-cutover-flag");
        let wt = make_gc_candidate(&home, "flag-agent");
        unsafe { std::env::set_var("AGEND_WORKTREE_GC", "1") };
        let removed = gc_cutover(&home);
        unsafe { std::env::remove_var("AGEND_WORKTREE_GC") };
        assert_eq!(removed, 1);
        assert!(!wt.exists(), "worktree must be deleted with flag");
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
        let ci_dir = home.join("ci-watches");
        std::fs::create_dir_all(&ci_dir).ok();
        let filename = crate::daemon::ci_watch::watch_filename(repo, branch);
        let path = ci_dir.join(&filename);
        let subs: Vec<serde_json::Value> = subscribers
            .iter()
            .map(|s| serde_json::json!({"instance": *s}))
            .collect();
        let watch = serde_json::json!({
            "repo": repo,
            "branch": branch,
            "interval_secs": 60,
            "subscribers": subs,
            "instance": subscribers.first().copied().unwrap_or(""),
            "last_run_id": null,
            "head_sha": null,
            "last_polled_at": null,
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(72)).to_rfc3339(),
            "last_terminal_seen_at": null,
        });
        std::fs::write(&path, serde_json::to_string_pretty(&watch).unwrap()).ok();
        path
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
        // Reproduces the gap shape documented in PR #549 Phase A RCA:
        // the EC7 helper only unsubscribed the binding-branch watch,
        // leaving any ad-hoc watches (e.g. an agent watching `main`
        // during a closeout cycle) orphaned on release.
        //
        // Setup: agent `dev` is bound to `feat-track-x`, the auto-
        // watch for that branch is in place, AND the agent has added
        // an extra watch on `main` (cross-branch, to follow upstream).
        // A different agent `lead` shares both watches.
        let home = tmp_home("p0x-unsubscribe-all");
        let repo = tmp_repo("p0x-unsubscribe-all-repo");
        let l = lease(&home, &repo, "dev", "feat-track-x").expect("lease");
        assert!(l.path.exists(), "pre: worktree must exist");

        // Auto-watch for binding-branch (lease path skipped explicit
        // ci_watch creation; pre-populate to mirror real fleet state
        // post-`dispatch_auto_bind_lease` which auto-installs it).
        let auto_watch = write_ci_watch(&home, "owner/repo", "feat-track-x", &["dev", "lead"]);
        // Ad-hoc cross-branch watch on `main` (lead also subscribed
        // so we can verify per-agent shrink without file deletion).
        let main_watch = write_ci_watch(&home, "owner/repo", "main", &["dev", "lead"]);
        // Watch the agent isn't subscribed to — must remain untouched.
        let bystander = write_ci_watch(&home, "owner/repo", "feat-bystander", &["lead"]);

        let outcome = release_full(&home, "dev", false);

        assert!(outcome.released, "release must succeed");
        assert!(outcome.binding_removed, "binding must be cleared");

        // Auto-watch: dev was 1 of 2 subscribers → file persists, lead remains.
        let auto_subs = read_ci_watch_subscribers(&auto_watch);
        assert_eq!(
            auto_subs,
            vec!["lead".to_string()],
            "binding-branch watch must shrink to remaining subscriber, not be deleted"
        );

        // Main watch: dev was the orphan vector — must also shrink.
        // Pre-Sprint-57-Wave-2 this assertion FAILED (dev stayed
        // subscribed to main); the fix makes it pass.
        let main_subs = read_ci_watch_subscribers(&main_watch);
        assert_eq!(
            main_subs,
            vec!["lead".to_string()],
            "ad-hoc cross-branch watch must also shrink — Item 2 regression-proof"
        );

        // Bystander: dev was never subscribed → file untouched.
        assert!(bystander.exists(), "bystander watch must survive untouched");
        let bystander_subs = read_ci_watch_subscribers(&bystander);
        assert_eq!(
            bystander_subs,
            vec!["lead".to_string()],
            "bystander subscriber list must be unchanged"
        );

        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn release_worktree_deletes_watch_when_last_subscriber_unsubscribes() {
        // Defensive bonus pin: agent is the SOLE subscriber to a
        // cross-branch watch. Release must delete the watch file
        // entirely (not leave an empty subscribers array).
        let home = tmp_home("p0x-unsubscribe-last");
        let repo = tmp_repo("p0x-unsubscribe-last-repo");
        let _l = lease(&home, &repo, "dev", "feat-x").expect("lease");

        let solo_watch = write_ci_watch(&home, "owner/repo", "main", &["dev"]);

        release_full(&home, "dev", false);

        assert!(
            !solo_watch.exists(),
            "watch with no remaining subscribers must be deleted entirely"
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
        let rt = home.join("runtime").join("dev-1");
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
}
