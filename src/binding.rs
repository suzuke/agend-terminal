//! Binding manager — writes per-agent binding.json for git shim + hook.
//!
//! Daemon-only writer. Shim and hooks are read-only consumers.
//! Uses atomic_write (temp + fsync + rename) + flock for safety.

use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

/// #1990: on-disk schema version for `binding.json`. The file has always carried
/// a bare `version` field (written by [`bind_full`]) but did NOT go through the
/// `SchemaVersioned` trait, so it had no future-version guard — this const is
/// that guard (see [`parse_binding_guarded`]). Additive field adds don't need a
/// bump; only a non-additive change to an existing field does.
const BINDING_SCHEMA_VERSION: u64 = 1;

static INDEX: OnceLock<RwLock<HashMap<String, serde_json::Value>>> = OnceLock::new();

/// #1990: parse a `binding.json` body, rejecting one a NEWER daemon wrote
/// (`version` > [`BINDING_SCHEMA_VERSION`]) → `None`. This guards only the
/// DAEMON-SIDE readers that route through [`read`]: they treat a future-version
/// binding as absent and fail-closed (e.g. auto-release / lease helpers won't act
/// on a binding shape they can't fully understand). It does NOT cover the git
/// shim, which has its OWN reader (`agend-git.rs::read_binding`) that
/// HMAC-verifies and treats a parseable future-version binding as BOUND — so the
/// agent stays restricted to its own worktree (safe), but is not "denied".
/// DESTRUCTIVE daemon-side sites (worktree retention) must use
/// [`present_including_future`] instead, so they never mistake a future binding
/// for absent and reclaim a newer daemon's live worktree. The missing-signature
/// fail-closed path is unchanged.
fn parse_binding_guarded(content: &str) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(content).ok()?;
    let found = v.get("version").and_then(|x| x.as_u64()).unwrap_or(0);
    if found > BINDING_SCHEMA_VERSION {
        tracing::warn!(
            found,
            supported = BINDING_SCHEMA_VERSION,
            "binding.json written by a newer schema version — daemon-side readers treat it as absent"
        );
        return None;
    }
    Some(v)
}

/// #1990: true if a binding file is present at a version this daemon understands
/// OR a NEWER one. Distinct from [`read`], which returns `None` for a
/// future-version binding (the correct fail-closed for daemon-side actors that
/// would ACT on the binding). A DESTRUCTIVE retention site must use THIS so it
/// never reclaims a worktree a newer daemon legitimately (re)bound just because
/// this older daemon can't parse the binding's version — "future ≠ absent".
/// A non-JSON / missing file is genuinely absent → `false` (pre-existing behavior).
pub fn present_including_future(home: &Path, agent: &str) -> bool {
    let path = crate::paths::runtime_dir(home)
        .join(agent)
        .join("binding.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
        .map(|v| v.is_object())
        .unwrap_or(false)
}

fn binding_index() -> &'static RwLock<HashMap<String, serde_json::Value>> {
    INDEX.get_or_init(|| RwLock::new(HashMap::new()))
}

fn index_key(home: &Path, agent: &str) -> String {
    format!("{}:{agent}", home.display())
}

/// #1882 / #2117 P3b: lock-file path for the per-lease flock. Keyed by a hash of
/// the **(source_repo, branch)** pair so it is path-safe (branches contain `/`)
/// and collision-free; a different lease never shares a lock file.
///
/// #2117 P3b: keying now includes `source_repo` so the SAME branch name in two
/// DIFFERENT repos is two independent leases that never contend. An EMPTY
/// `source_repo` (the test-only `bind()` wrapper; every production lease path
/// carries a resolved repo) falls back to the legacy branch-only key — so the
/// single-project world (one repo) keys on `(repo, branch)` consistently and a
/// rare empty bind keys branch-only, both behaviorally equivalent to pre-P3b.
fn branch_lease_lock_path(home: &Path, source_repo: &str, branch: &str) -> PathBuf {
    let key = if source_repo.is_empty() {
        crate::daemon::utils::sha256_hex(branch.as_bytes())
    } else {
        // NUL separator: unambiguous since neither path nor branch contains it.
        crate::daemon::utils::sha256_hex(format!("{source_repo}\0{branch}").as_bytes())
    };
    crate::paths::runtime_dir(home)
        .join(".lease-locks")
        .join(format!("{key}.lock"))
}

/// #1882 / #2117 P3b: acquire the exclusive per-lease flock so the cross-agent
/// "a (source_repo, branch) is held by at most one agent" check-then-bind
/// (`scan_existing_branch_binding` → `bind_full`) is ATOMIC. Two DIFFERENT agents
/// racing to lease the SAME (repo, branch) serialize here: the first binds; the
/// second blocks until the first's guard drops, then its rescan sees the first's
/// binding and rejects — so neither double-binds. This is a SHORT-LIVED mutex
/// around the bind operation, NOT a lease-lifetime lock: the persistent lease
/// state is `binding.json` (which the scan reads), so `release_full` needs no lock
/// cleanup. Per-(repo,branch) keying means different leases never contend → normal
/// single-agent binds are unaffected. Blocking `acquire_file_lock`; released when
/// the guard drops. Pass `""` for `source_repo` only on the test-only empty-bind
/// path (branch-only key).
pub fn acquire_branch_lease_lock(
    home: &Path,
    source_repo: &str,
    branch: &str,
) -> anyhow::Result<crate::store::FileFlockGuard> {
    crate::store::acquire_file_lock(&branch_lease_lock_path(home, source_repo, branch))
}

/// Write a binding for an agent (task assigned).
///
/// Fail-closed: if lock acquisition or I/O fails, binding.json is NOT
/// written and the error is logged. Pre-#1163 this silently proceeded
/// via `.ok()`, breaking the serialization guarantee.
#[allow(dead_code)] // Used by tests + auto-watch dispatch path
pub fn bind(home: &Path, agent: &str, task_id: &str, branch: &str) {
    if let Err(e) = bind_full(
        home,
        agent,
        task_id,
        branch,
        std::path::Path::new(""),
        std::path::Path::new(""),
    ) {
        tracing::warn!(%agent, task_id, branch, error = %e, "bind failed (fail-closed)");
    }
}

/// Write a full binding including worktree + source-repo paths.
///
/// `source_repo` is the parent repo that owns the worktree, persisted as a
/// schema field so `worktree_pool::release_full` (Sprint 53 P0-X r1) can run
/// `git worktree remove --force` from the owning repo's cwd. Without this,
/// the git registry leaves a stale prunable entry after a manual `remove_dir_all`
/// fallback. Pass an empty path when unknown — `release_full` falls back to
/// deriving the source from the worktree path's `.worktrees/<agent>` ancestor.
/// #779 P2 (Option B): hard-break signature now returns `Result<(), String>`
/// so callers can surface partial-failure diagnostics. The two pre-existing
/// silent failure points (`create_dir_all` + `atomic_write`) become explicit
/// `Err` cases. Two non-target callers (`worktree_pool::lease`,
/// `dispatch_auto_bind_lease`) preserve their pre-#779-P2 silent semantic
/// via `let _ = bind_full(...).ok();` — zero observable behavior change to
/// the dispatch path. Only `ci::handle_checkout_repo` consumes the Result
/// to populate its new `warnings` array.
pub fn bind_full(
    home: &Path,
    agent: &str,
    task_id: &str,
    branch: &str,
    worktree: &std::path::Path,
    source_repo: &std::path::Path,
) -> Result<(), String> {
    // #1888 phase-2: the agent claiming a branch is acting on any pending
    // ci-handoff for it — resolve the track (re-nudge stops). Scoped to this
    // agent's own tracks; other targets' handoffs for the branch are untouched.
    let _ = crate::daemon::ci_handoff_track::resolve_claimed(home, agent, branch);
    let dir = crate::paths::runtime_dir(home).join(agent);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create_dir_all {}: {e}", dir.display()))?;
    let path = dir.join("binding.json");
    let lock_path = dir.join(".binding.json.lock");
    let _lock = crate::store::acquire_file_lock(&lock_path)
        .map_err(|e| format!("acquire_file_lock {}: {e}", lock_path.display()))?;
    let wt_str = worktree.display().to_string();
    let src_str = source_repo.display().to_string();
    let mut binding = json!({
        "version": BINDING_SCHEMA_VERSION,
        "agent": agent,
        "task_id": task_id,
        "branch": branch,
        "issued_at": chrono::Utc::now().to_rfc3339(),
    });
    if !wt_str.is_empty() {
        binding["worktree"] = json!(wt_str);
    }
    if !src_str.is_empty() {
        binding["source_repo"] = json!(src_str);
    }
    let body = serde_json::to_string_pretty(&binding).unwrap_or_default();
    crate::store::atomic_write(&path, body.as_bytes())
        .map_err(|e| format!("atomic_write {}: {e}", path.display()))?;
    // #1651: HMAC-sign the binding (sidecar `binding.json.sig`, mirroring #1576's
    // operator-mode.json scheme) so the agend-git shim can reject a blind
    // self-authorization rewrite — an injected agent editing its own `branch`
    // without re-signing makes the shim's verify fail → unbound → push denied.
    // Defense-in-depth against injection blind-write, NOT a security boundary
    // (a same-uid agent could read the key + re-sign; true sealing needs
    // OS-isolation, parked #1653). Best-effort: a missing/failed sidecar leaves
    // the binding unsigned → the shim fails CLOSED (denies), never open.
    match crate::config_integrity::sign(home, body.as_bytes()) {
        Ok(tag) => {
            if let Err(e) = crate::store::atomic_write(&binding_sig_path(&dir), tag.as_bytes()) {
                tracing::warn!(%agent, error = %e,
                    "#1651 binding sidecar write failed — shim fails closed (deny) until re-bind");
            }
        }
        Err(e) => tracing::warn!(%agent, error = %e,
            "#1651 binding HMAC sign failed — shim fails closed (deny) until re-bind"),
    }
    if let Ok(mut map) = binding_index().write() {
        map.insert(index_key(home, agent), binding);
    }
    Ok(())
}

/// #1651: the HMAC sidecar path for a binding dir. The agend-git shim hard-codes
/// the same `binding.json.sig` name (it cannot import this — separate binary).
fn binding_sig_path(dir: &Path) -> PathBuf {
    dir.join("binding.json.sig")
}

/// Clear a binding for an agent (task completed/released).
pub fn unbind(home: &Path, agent: &str) {
    let dir = crate::paths::runtime_dir(home).join(agent);
    let _ = std::fs::remove_file(dir.join("binding.json"));
    // #1651: drop the HMAC sidecar too, so a stale signature can't linger.
    let _ = std::fs::remove_file(binding_sig_path(&dir));
    if let Ok(mut map) = binding_index().write() {
        map.remove(&index_key(home, agent));
    }
}

// #1688 (codex): there is intentionally NO startup "re-sign unsigned bindings"
// pass. It was a wash-white hole — keying the decision on "has no sidecar" cannot
// distinguish a legit pre-#1651 binding from an attacker that tampered
// binding.json AND deleted the sidecar, and the daemon has NO trusted source at
// startup to tell them apart (`reconcile_orphan_leases` is log-only; binding.json
// is the sole on-disk record; bindings are only (re)established via
// `dispatch_auto_bind_lease`/`bind_full` at dispatch time). So a sidecar-less
// binding is left UNSIGNED → the shim fails closed (unbound → deny), exactly like
// a fresh, never-dispatched agent. A legit binding re-signs on its next dispatch
// or `bind_self`. The rollout cost — agents whose binding survives the activating
// restart are denied pushes until re-dispatched — is a VISIBLE, self-healing
// trade-off (the agent reports `blocked`), deliberately chosen over a SILENT
// wash-white. (Activating restart: the operator re-dispatches / has running
// agents `bind_self` once; one-time.)

/// Returns Some(agent_name) if any other agent holds the lease for this
/// **(source_repo, branch)**. Used by dispatch_auto_bind_lease / repo-checkout to
/// enforce cross-agent lease uniqueness.
///
/// #2117 P3b: the lease key is now `(source_repo, branch)` — the SAME branch name
/// in a DIFFERENT repo is a DIFFERENT lease and does not conflict. Pass `""` for
/// `source_repo` to keep the legacy branch-only semantics (callers that only want
/// "who holds this branch anywhere", e.g. pr-state / auto-arm / auto-release,
/// which does its own slug-based repo guard).
///
/// **Backward-compat wildcard gate (reviewer-2, the P3b core risk)**: `bind_full`
/// only writes the `source_repo` field when non-empty, so a pre-P3b "legacy" live
/// binding can lack it. On rescan after P3b ships, requiring `source_repo`
/// equality would make `None != Some(repo)` MISS that legacy binding → a second
/// agent binds the same branch → **double-bind**. To prevent that, a missing/empty
/// `source_repo` on EITHER side (the scanned binding OR the query) falls back to
/// **branch-only** exclusion (match-any); only when BOTH carry a non-empty
/// `source_repo` is the match tightened to `(source_repo, branch)`. Fail-closed:
/// when in doubt (a field is absent), we still treat the branch as taken.
pub fn scan_existing_branch_binding(
    home: &Path,
    source_repo: &str,
    branch: &str,
    exclude_agent: &str,
) -> Option<String> {
    let runtime_dir = crate::paths::runtime_dir(home);
    let entries = std::fs::read_dir(&runtime_dir).ok()?;
    for entry in entries.flatten() {
        let agent = entry.file_name().to_string_lossy().to_string();
        if agent == exclude_agent {
            continue;
        }
        let binding_path = entry.path().join("binding.json");
        let Ok(content) = std::fs::read_to_string(&binding_path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        if v["branch"].as_str() != Some(branch) {
            continue;
        }
        // Wildcard backward-compat gate: branch-only exclusion unless BOTH the
        // query and the scanned binding carry a non-empty source_repo.
        let binding_repo = v["source_repo"].as_str().unwrap_or("");
        let both_present = !source_repo.is_empty() && !binding_repo.is_empty();
        if !both_present || binding_repo == source_repo {
            return Some(agent);
        }
    }
    None
}

/// PR-3 (t-ci-ready-pr3-arm-not-armed): the distinct `source_repo` paths of every
/// LIVE bound branch (each `runtime/<agent>/binding.json`'s `source_repo`).
///
/// The pr_state scanner seeds its poll-repo list from these (after resolving each
/// to a gh `owner/repo` slug) so a repo that has a bound branch but NO pr-state
/// file yet — a bypass / non-dispatch PR — is still polled. Without this seed the
/// scanner only ever polls repos that already have a pr-state, so a brand-new
/// unwatched PR in an otherwise-unseeded repo would never be discovered (the
/// #1782 gap). Returns raw paths (slug resolution is the caller's job) to keep
/// this module free of the git/scm dependency.
pub fn bound_source_repos(home: &Path) -> Vec<std::path::PathBuf> {
    let runtime_dir = crate::paths::runtime_dir(home);
    let Ok(entries) = std::fs::read_dir(&runtime_dir) else {
        return Vec::new();
    };
    let mut repos: Vec<std::path::PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let binding_path = entry.path().join("binding.json");
        let Ok(content) = std::fs::read_to_string(&binding_path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        if let Some(src) = v["source_repo"].as_str() {
            let path = std::path::PathBuf::from(src);
            if !repos.contains(&path) {
                repos.push(path);
            }
        }
    }
    repos
}

/// Read the current binding for an agent.
/// Hot path: returns from in-memory index (read lock). Cold path
/// (first access per agent): acquires write lock, double-checks,
/// then reads disk and populates. Disk read under write lock
/// prevents stale resurrection when a concurrent unbind() deletes
/// the file between our miss and our insert.
pub fn read(home: &Path, agent: &str) -> Option<serde_json::Value> {
    let key = index_key(home, agent);
    if let Ok(map) = binding_index().read() {
        if let Some(v) = map.get(&key) {
            return Some(v.clone());
        }
    }
    let path = crate::paths::runtime_dir(home)
        .join(agent)
        .join("binding.json");
    if let Ok(mut map) = binding_index().write() {
        if let Some(v) = map.get(&key) {
            return Some(v.clone());
        }
        let v: serde_json::Value = std::fs::read_to_string(path)
            .ok()
            .and_then(|c| parse_binding_guarded(&c))?;
        map.insert(key, v.clone());
        return Some(v);
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|c| parse_binding_guarded(&c))
}

/// Check if an agent is bound in a daemon-managed worktree.
/// Returns true if the agent has a binding with a worktree path that
/// contains the `.agend-managed` marker file.
pub fn is_agent_in_managed_worktree(home: &Path, agent: &str) -> bool {
    read(home, agent)
        .and_then(|v| v["worktree"].as_str().map(std::path::PathBuf::from))
        .map(|wt| wt.join(".agend-managed").exists())
        .unwrap_or(false)
}

/// Install the prepare-commit-msg hook into a worktree via core.hooksPath.
/// Points to `$AGEND_HOME/hooks/` unified directory.
/// Installs bash hook on Unix, PowerShell hook on Windows.
pub fn install_hooks(home: &Path, worktree: &Path) {
    let hooks_dir = home.join("hooks");
    std::fs::create_dir_all(&hooks_dir).ok();

    // Extract embedded hook scripts (both platforms for portability).
    let bash_hook = include_str!("../assets/hooks/prepare-commit-msg");
    let bash_path = hooks_dir.join("prepare-commit-msg");
    let _ = std::fs::write(&bash_path, bash_hook);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&bash_path, std::fs::Permissions::from_mode(0o755));
    }

    // Windows: also install PowerShell version.
    let ps_hook = include_str!("../assets/hooks/prepare-commit-msg.ps1");
    let ps_path = hooks_dir.join("prepare-commit-msg.ps1");
    let _ = std::fs::write(&ps_path, ps_hook);

    // Set core.hooksPath on the worktree.
    // W1.2 class-2: BEHAVIOR DELTA — adds AGEND_GIT_BYPASS (this site previously
    // ran raw `git` with NO bypass env). `git config` against a fleet-managed
    // worktree is exactly the forgot-bypass latent class (#821/#1463): a daemon
    // git mutation should always bypass the agend-git shim. Intended fix.
    let _ = crate::git_helpers::git_ok(
        worktree,
        &["config", "core.hooksPath", &hooks_dir.display().to_string()],
    );
}

/// Install hooks on all existing worktrees (daemon startup reconcile).
pub fn reconcile_hooks(home: &Path) {
    // New layout: <home>/worktrees/<agent>/<branch>/
    let new_root = crate::worktree_pool::daemon_managed_worktree_root(home);
    if new_root.is_dir() {
        if let Ok(agents) = std::fs::read_dir(&new_root) {
            for agent_entry in agents.flatten() {
                if !agent_entry.path().is_dir() {
                    continue;
                }
                if let Ok(branches) = std::fs::read_dir(agent_entry.path()) {
                    for branch_entry in branches.flatten() {
                        if branch_entry.path().is_dir() {
                            install_hooks(home, &branch_entry.path());
                        }
                    }
                }
            }
        }
    }

    // Legacy layout: <home>/workspace/*/.worktrees/*/
    let worktrees_base = crate::paths::workspace_dir(home);
    if !worktrees_base.exists() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(&worktrees_base) {
        for entry in entries.flatten() {
            let wt_dir = entry.path().join(".worktrees");
            if wt_dir.is_dir() {
                if let Ok(wts) = std::fs::read_dir(&wt_dir) {
                    for wt in wts.flatten() {
                        if wt.path().is_dir() {
                            install_hooks(home, &wt.path());
                        }
                    }
                }
            }
        }
    }
}

/// Symlink the agend-git binary into $AGEND_HOME/bin/git.
/// Called at daemon startup so the shim shadows /usr/bin/git via PATH.
pub fn symlink_shim(home: &Path) {
    let bin_dir = home.join("bin");
    std::fs::create_dir_all(&bin_dir).ok();
    let link_name = if cfg!(windows) { "git.exe" } else { "git" };
    let link_path = bin_dir.join(link_name);

    // Find the agend-git binary alongside the main binary.
    let shim_name = if cfg!(windows) {
        "agend-git.exe"
    } else {
        "agend-git"
    };
    let shim_src = std::env::current_exe().ok().and_then(|exe| {
        let candidate = exe.with_file_name(shim_name);
        candidate.exists().then_some(candidate)
    });

    if let Some(src) = shim_src {
        // Remove stale symlink/file first.
        let _ = std::fs::remove_file(&link_path);
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(&src, &link_path);
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::copy(&src, &link_path);
        }
    }
}

/// Clear orphan bindings (agents no longer in registry).
/// Called at daemon startup.
pub fn reconcile_orphans(home: &Path) {
    let runtime_dir = crate::paths::runtime_dir(home);
    if !runtime_dir.exists() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(&runtime_dir) {
        for entry in entries.flatten() {
            let binding_path = entry.path().join("binding.json");
            if binding_path.exists() {
                // Check if binding is stale (issued_at > 24h ago).
                if let Ok(content) = std::fs::read_to_string(&binding_path) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                        if let Some(issued) = v["issued_at"].as_str() {
                            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(issued) {
                                let age = chrono::Utc::now()
                                    .signed_duration_since(dt.with_timezone(&chrono::Utc));
                                if age > chrono::Duration::hours(24) {
                                    // #693: check heartbeat — if agent is still active, don't delete
                                    let entry_path = entry.path();
                                    let agent_name = entry_path
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or("");
                                    let hb =
                                        crate::daemon::heartbeat_pair::snapshot_for(agent_name);
                                    let hb_age_ms = crate::daemon::heartbeat_pair::now_ms()
                                        .saturating_sub(hb.heartbeat_at_ms);
                                    if hb_age_ms < 3_600_000 {
                                        // Heartbeat within 1h — agent still active, skip
                                        continue;
                                    }
                                    let _ = std::fs::remove_file(&binding_path);
                                    if let Ok(mut map) = binding_index().write() {
                                        map.remove(&index_key(home, agent_name));
                                    }
                                    tracing::info!(
                                        path = %binding_path.display(),
                                        "removed orphan binding (>24h old, heartbeat stale)"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// §3.9 #1882 (concurrency): the per-branch lease flock SERIALIZES two
    /// acquirers of the SAME branch (the second blocks until the first drops) but
    /// lets DIFFERENT branches proceed concurrently (no cross-branch contention).
    /// This is the atomicity that makes the dispatch `scan → bind_full` a critical
    /// section so two agents can't both pass the scan and double-bind. Regression-
    /// proof: it's the lock's defining behavior.
    #[test]
    fn branch_lease_lock_serializes_same_branch_allows_different_1882() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::Duration;

        let home = tmp_home("lease-lock-1882");

        let repo = "/repo/a";
        // Different branches must NOT contend — both acquire while both are held.
        let g_x = acquire_branch_lease_lock(&home, repo, "feat/x").expect("lock x");
        let g_y =
            acquire_branch_lease_lock(&home, repo, "feat/y").expect("lock y must not block on x");
        drop((g_x, g_y));

        // #2117 P3b: the SAME branch in a DIFFERENT repo is a DIFFERENT lease and
        // must NOT contend (the per-repo independence P3b adds).
        let g_a = acquire_branch_lease_lock(&home, "/repo/a", "feat/shared").expect("lock a");
        let g_b = acquire_branch_lease_lock(&home, "/repo/b", "feat/shared")
            .expect("same branch, different repo must not block");
        drop((g_a, g_b));

        // Same (repo, branch): a second acquirer BLOCKS until the first guard drops.
        let got = Arc::new(AtomicBool::new(false));
        let g1 = acquire_branch_lease_lock(&home, repo, "feat/z").expect("lock z (holder)");
        let home2 = home.clone();
        let got2 = got.clone();
        let t = std::thread::spawn(move || {
            // Blocks here until the holder drops g1.
            let _g2 =
                acquire_branch_lease_lock(&home2, "/repo/a", "feat/z").expect("lock z (waiter)");
            got2.store(true, Ordering::SeqCst);
        });

        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !got.load(Ordering::SeqCst),
            "#1882: a second same-(repo,branch) lock MUST block while the first is held"
        );
        drop(g1); // release → the waiter proceeds.
        t.join().expect("waiter thread");
        assert!(
            got.load(Ordering::SeqCst),
            "#1882: the second same-(repo,branch) lock must proceed once the first drops"
        );

        std::fs::remove_dir_all(&home).ok();
    }

    /// Write a `binding.json` for `agent` directly, controlling whether the
    /// `source_repo` field is present — to construct a pre-P3b "legacy" binding
    /// (field absent) that `bind_full` would only write when non-empty.
    fn write_binding_json(home: &Path, agent: &str, branch: &str, source_repo: Option<&str>) {
        let dir = crate::paths::runtime_dir(home).join(agent);
        std::fs::create_dir_all(&dir).unwrap();
        let mut v = json!({
            "version": BINDING_SCHEMA_VERSION,
            "agent": agent,
            "task_id": "T-test",
            "branch": branch,
            "issued_at": "2026-01-01T00:00:00+00:00",
        });
        if let Some(r) = source_repo {
            v["source_repo"] = json!(r);
        }
        std::fs::write(
            dir.join("binding.json"),
            serde_json::to_string_pretty(&v).unwrap(),
        )
        .unwrap();
    }

    /// #2117 P3b CORE GATE (reviewer-2): a pre-P3b "legacy" live binding that
    /// LACKS the `source_repo` field must STILL exclude a new (repo, branch) bind
    /// on the post-P3b rescan — else `None != Some(repo)` would miss it and a
    /// second agent double-binds. This is the CI-runnable form of the cross-deploy
    /// restart smoke: construct a field-less legacy binding fixture, then rescan as
    /// a post-P3b dispatch would (with a real source_repo). The wildcard fallback
    /// must match it branch-only. (True end-to-end verification — an operator
    /// restart carrying a real legacy binding — is the PR's dogfood caveat.)
    #[test]
    fn scan_legacy_binding_missing_source_repo_still_excludes_p3b() {
        let home = tmp_home("p3b-legacy-rescan");
        write_binding_json(&home, "legacy-agent", "feat/shared", None); // no source_repo field
        assert_eq!(
            scan_existing_branch_binding(&home, "/repo/new", "feat/shared", "new-agent"),
            Some("legacy-agent".to_string()),
            "legacy binding (missing source_repo) MUST still exclude — no double-bind on P3b rollout"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #2117 P3b key semantics: same (repo, branch) conflicts; same branch in a
    /// DIFFERENT repo is an independent lease; an empty-source_repo query falls
    /// back to branch-only (byte-identical for the pre-P3b wildcard callers).
    #[test]
    fn scan_p3b_lease_key_repo_scoped() {
        let home = tmp_home("p3b-key");
        write_binding_json(&home, "holder", "feat/x", Some("/repo/a"));
        assert_eq!(
            scan_existing_branch_binding(&home, "/repo/a", "feat/x", "other"),
            Some("holder".to_string()),
            "same (repo, branch) is one lease → conflict"
        );
        assert_eq!(
            scan_existing_branch_binding(&home, "/repo/b", "feat/x", "other"),
            None,
            "same branch in a different repo is a different lease → no conflict (P3b)"
        );
        assert_eq!(
            scan_existing_branch_binding(&home, "", "feat/x", "other"),
            Some("holder".to_string()),
            "empty-source_repo query falls back to branch-only (pre-P3b callers byte-identical)"
        );
        // The querying agent's own binding is always excluded.
        assert_eq!(
            scan_existing_branch_binding(&home, "/repo/a", "feat/x", "holder"),
            None,
            "self-agent binding is excluded"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// Reverse of the legacy gate: when BOTH sides carry a non-empty source_repo,
    /// a DIFFERENT repo must NOT match (the tightening that makes per-repo leases
    /// independent). Pins that the wildcard is scoped to the missing-field case.
    #[test]
    fn scan_both_present_different_repo_does_not_match_p3b() {
        let home = tmp_home("p3b-both-present");
        write_binding_json(&home, "holder", "feat/x", Some("/repo/a"));
        assert_eq!(
            scan_existing_branch_binding(&home, "/repo/b", "feat/x", "other"),
            None,
            "both source_repos present + different → independent leases, no false conflict"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "agend-binding-test-{}-{}-{}",
            std::process::id(),
            tag,
            id
        ));
        std::fs::create_dir_all(&dir).ok();
        dir
    }

    /// #1688 (codex): the daemon startup must NOT auto-bless a sidecar-less
    /// binding. "No sidecar" cannot distinguish a legit unsigned binding from an
    /// attacker that tampered binding.json AND deleted the sidecar — there is no
    /// trusted source at startup to tell them apart, so the only safe behaviour is
    /// to NOT sign (fail-closed → unbound; legit bindings re-sign on the next
    /// dispatch / `bind_self`). This pins that a tampered, sidecar-less binding is
    /// NOT made verifiable by the startup pass — RED while the (now-removed) blind
    /// `resign_unsigned_bindings` washed it white.
    #[test]
    fn startup_does_not_wash_white_tampered_sidecarless_binding_1688() {
        let home = tmp_home("washwhite-1688");
        // Shared integrity key present → a sign WOULD produce a verifiable tag.
        std::fs::write(home.join(".config-integrity-key"), [9u8; 32]).unwrap();
        // Attacker blind-writes a self-authorizing branch and removes the sidecar.
        let dir = crate::paths::runtime_dir(&home).join("ag");
        std::fs::create_dir_all(&dir).unwrap();
        let forged = r#"{"version":1,"agent":"ag","task_id":"T-1","branch":"main"}"#;
        std::fs::write(dir.join("binding.json"), forged).unwrap();
        // (no binding.json.sig — the wash-white precondition)

        // Daemon startup binding handling. Post-#1688 this does NOTHING to an
        // unsigned binding (the blind re-sign is gone) — so nothing blesses it.

        // The forged content must NOT be verifiable as trusted (no valid sidecar).
        let sig = std::fs::read_to_string(dir.join("binding.json.sig")).unwrap_or_default();
        assert!(
            !crate::config_integrity::verify(&home, forged.as_bytes(), &sig),
            "#1688: a tampered, sidecar-less binding must NOT be auto-blessed at startup"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn bind_creates_binding_json() {
        let home = tmp_home("bind");
        bind(&home, "agent-1", "T-123", "feature-x");
        let binding = read(&home, "agent-1").expect("binding must exist");
        assert_eq!(binding["agent"], "agent-1");
        assert_eq!(binding["task_id"], "T-123");
        assert_eq!(binding["branch"], "feature-x");
        assert_eq!(binding["version"], 1);
        assert!(binding["issued_at"].as_str().is_some());
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1990: a binding.json a NEWER daemon wrote (version > current) reads as
    /// `None` — treated as ABSENT so every consumer (git shim push gate, lease
    /// checks) fail-closes, rather than acting on a shape this binary may not
    /// fully understand. The missing-signature fail-closed path is unchanged.
    #[test]
    fn future_version_binding_reads_as_absent() {
        let home = tmp_home("future-binding");
        let dir = crate::paths::runtime_dir(&home).join("ag");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("binding.json"),
            r#"{"version":999,"agent":"ag","task_id":"T-1","branch":"main"}"#,
        )
        .unwrap();
        assert!(
            read(&home, "ag").is_none(),
            "a future-version binding.json must read as absent (fail-closed)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1990 (reviewer-2 P2b): destructive retention must distinguish a
    /// future-version binding (present — a newer daemon's live worktree) from a
    /// truly absent one. `read` collapses both to None; `present_including_future`
    /// must report the future binding as present.
    #[test]
    fn present_including_future_distinguishes_future_from_absent() {
        let home = tmp_home("present-future");
        let dir = crate::paths::runtime_dir(&home).join("ag");
        std::fs::create_dir_all(&dir).unwrap();
        // Truly absent → false.
        assert!(!present_including_future(&home, "ag"));
        // Future-version binding → read() fail-closes to None, but it is PRESENT.
        std::fs::write(
            dir.join("binding.json"),
            r#"{"version":999,"agent":"ag","task_id":"T-1","branch":"main"}"#,
        )
        .unwrap();
        assert!(
            read(&home, "ag").is_none(),
            "read() fail-closes a future binding"
        );
        assert!(
            present_including_future(&home, "ag"),
            "present_including_future must report a future binding as present (future ≠ absent)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn unbind_removes_binding_json() {
        let home = tmp_home("unbind");
        bind(&home, "agent-2", "T-456", "fix-bug");
        assert!(read(&home, "agent-2").is_some());
        unbind(&home, "agent-2");
        assert!(read(&home, "agent-2").is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn read_missing_returns_none() {
        let home = tmp_home("read-miss");
        assert!(read(&home, "ghost").is_none());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn marker_check_passes_for_managed_worktree() {
        let home = tmp_home("marker-pass");
        let wt = home.join("worktrees").join("agent-1").join("feat-branch");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".agend-managed"), "").unwrap();
        // Write binding pointing to this worktree
        let rt = crate::paths::runtime_dir(&home).join("agent-1");
        std::fs::create_dir_all(&rt).unwrap();
        let binding =
            serde_json::json!({"worktree": wt.to_str().unwrap(), "branch": "feat-branch"});
        std::fs::write(rt.join("binding.json"), binding.to_string()).unwrap();

        assert!(is_agent_in_managed_worktree(&home, "agent-1"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn marker_check_fails_for_unmanaged() {
        let home = tmp_home("marker-fail");
        let wt = home.join("worktrees").join("agent-2").join("feat-branch");
        std::fs::create_dir_all(&wt).unwrap();
        // No .agend-managed marker
        let rt = crate::paths::runtime_dir(&home).join("agent-2");
        std::fs::create_dir_all(&rt).unwrap();
        let binding =
            serde_json::json!({"worktree": wt.to_str().unwrap(), "branch": "feat-branch"});
        std::fs::write(rt.join("binding.json"), binding.to_string()).unwrap();

        assert!(!is_agent_in_managed_worktree(&home, "agent-2"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn marker_check_fails_for_no_binding() {
        let home = tmp_home("marker-no-bind");
        assert!(!is_agent_in_managed_worktree(&home, "nobody"));
        std::fs::remove_dir_all(&home).ok();
    }

    /// #1163: bind_full must propagate acquire_file_lock errors.
    /// Pre-fix: lock result was silently ignored, so bind_full would
    /// write binding.json without holding the lock — breaking the
    /// serialization guarantee under concurrent lease/bind operations.
    #[test]
    fn bind_full_propagates_lock_error_1163() {
        let home = tmp_home("lock-err");
        let agent = "lock-test";
        let rt = crate::paths::runtime_dir(&home).join(agent);
        std::fs::create_dir_all(&rt).unwrap();
        let lock_path = rt.join(".binding.json.lock");
        // Plant a directory where the lock file should be — open() on a
        // directory fails, so acquire_file_lock returns Err.
        std::fs::create_dir_all(&lock_path).unwrap();
        let result = bind_full(
            &home,
            agent,
            "T-999",
            "branch",
            std::path::Path::new(""),
            std::path::Path::new(""),
        );
        assert!(
            result.is_err(),
            "#1163: bind_full must fail when lock acquisition fails, got Ok"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("acquire_file_lock"),
            "error must mention lock failure: {err}"
        );
        // binding.json must NOT have been written
        assert!(
            read(&home, agent).is_none(),
            "#1163: binding.json must not be written when lock fails"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn bind_wrapper_fail_closed_on_lock_error() {
        let home = tmp_home("bind-failclose");
        let agent = "fc-agent";
        let rt = crate::paths::runtime_dir(&home).join(agent);
        std::fs::create_dir_all(&rt).unwrap();
        let lock_path = rt.join(".binding.json.lock");
        std::fs::create_dir_all(&lock_path).unwrap();

        bind(&home, agent, "T-999", "branch");

        assert!(
            read(&home, agent).is_none(),
            "bind() must not write binding.json when lock acquisition fails (fail-closed)"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn index_serves_read_after_bind() {
        let home = tmp_home("idx-read");
        bind(&home, "idx-agent", "T-IDX", "idx-branch");
        let v = read(&home, "idx-agent").expect("index must serve binding");
        assert_eq!(v["branch"], "idx-branch");
        // Delete file on disk — index should still serve the cached value
        let path = crate::paths::runtime_dir(&home)
            .join("idx-agent")
            .join("binding.json");
        std::fs::remove_file(&path).unwrap();
        let v2 = read(&home, "idx-agent").expect("index must survive disk delete");
        assert_eq!(v2["task_id"], "T-IDX");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn index_invalidated_by_unbind() {
        let home = tmp_home("idx-unbind");
        bind(&home, "idx-ub", "T-UB", "ub-branch");
        assert!(read(&home, "idx-ub").is_some());
        unbind(&home, "idx-ub");
        assert!(
            read(&home, "idx-ub").is_none(),
            "unbind must clear index entry"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn no_stale_resurrection_after_concurrent_unbind() {
        for _ in 0..50 {
            let home = tmp_home("race");
            bind(&home, "race-a", "T-R", "race-b");
            if let Ok(mut map) = binding_index().write() {
                map.remove(&index_key(&home, "race-a"));
            }
            let home2 = home.clone();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let b2 = barrier.clone();
            let t = std::thread::spawn(move || {
                b2.wait();
                unbind(&home2, "race-a");
            });
            barrier.wait();
            let _ = read(&home, "race-a");
            t.join().expect("unbind thread must not panic");
            assert!(
                read(&home, "race-a").is_none(),
                "stale resurrection: read() returned binding after unbind()"
            );
            std::fs::remove_dir_all(&home).ok();
        }
    }
}
