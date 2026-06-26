//! Workspace-as-worktree reconciliation and rollback.

use super::{
    enumerate_managed_worktrees, is_daemon_managed, remove_worktree, source_repo_from_binding,
    WorktreeRemoval, MANAGED_MARKER,
};
use std::path::{Path, PathBuf};

/// #2234 Phase 0 (cure-(B) safety, independent value): tear down a per-agent
/// WORKSPACE directory that is a git WORKTREE via `git worktree remove --force`
/// from the OWNING repo, so no orphan registration survives in
/// `<canonical>/.git/worktrees/`. Returns `true` when it took responsibility
/// (the path is a worktree) — the caller MUST then NOT `remove_dir_all` (the
/// orphan-leaving bug #2234). Returns `false` for a NON-worktree (`.git` is a
/// directory = pre-(B) `git init`'d standalone clone, or absent = plain dir) →
/// the caller keeps its byte-identical `remove_dir_all`.
///
/// r6/lead dialectic #1 (the critical safety direction): the **gitlink alone**
/// gates this path. The `.agend-managed` marker is logged as a confidence
/// signal but is NEVER a veto — a managed worktree whose marker write was lost
/// (interrupted reconcile) still has a gitlink, and falling through to
/// `remove_dir_all` would orphan it. This fn is only ever called for the
/// per-agent workspace path (daemon-owned by construction), so removal is
/// unconditional once a gitlink is present.
///
/// Work-at-risk guard (must-resolve #2): a worktree with uncommitted/untracked
/// changes OR local commits not on any remote is backed up WHOLE to
/// `<home>/reconcile-backups/<agent>-<epoch>/` BEFORE removal. If the backup
/// FAILS, removal is ABORTED fail-closed (returns `true`, dir left in place for
/// operator recovery) — never destroy work without a durable backup.
pub fn teardown_workspace_worktree(home: &Path, agent: &str, working_dir: &Path) -> bool {
    // Discriminator: a git WORKTREE has a `.git` gitlink FILE; a `git init`'d
    // standalone clone has a `.git` DIRECTORY; a plain dir has neither.
    if !working_dir.join(".git").is_file() {
        return false;
    }
    if !is_daemon_managed(working_dir) {
        tracing::warn!(agent, path = %working_dir.display(),
            "#2234 teardown: workspace worktree missing .agend-managed marker \
             (interrupted reconcile?) — removing via git anyway, NOT remove_dir_all");
    }

    let source_repo = resolve_owning_repo(home, agent, working_dir);

    if worktree_has_work_at_risk(working_dir) {
        match backup_worktree_dir(home, agent, None, working_dir) {
            Ok(dest) => tracing::warn!(agent, backup = %dest.display(),
                "#2234 teardown: workspace worktree had uncommitted/unpushed work — backed up before removal"),
            Err(e) => {
                tracing::error!(agent, path = %working_dir.display(), error = %e,
                    "#2234 teardown: backup FAILED — aborting removal (fail-closed); worktree left for operator recovery");
                return true;
            }
        }
    }

    // Mirror `remove_worktree`'s git call, WITHOUT the marker veto: run from the
    // owning repo so the registration is cleared (not just the dir).
    let wt_str = working_dir.display().to_string();
    let result = if source_repo.as_os_str().is_empty() {
        // git-raw-allowed: defensive fallback when the owning repo can't be
        // resolved (effectively unreachable — a real gitlink always yields a
        // common-dir). Mirrors `remove_worktree`'s empty-source_repo arm: git
        // must resolve the repo from the absolute `<wt>` itself, so this runs
        // with NO `current_dir` — `git_bypass`/`git_cmd` both REQUIRE a cwd.
        std::process::Command::new("git")
            .args(["worktree", "remove", "--force", &wt_str])
            .env("AGEND_GIT_BYPASS", "1")
            .output()
    } else {
        crate::git_helpers::git_bypass(&source_repo, &["worktree", "remove", "--force", &wt_str])
    };
    let removed = matches!(&result, Ok(o) if o.status.success());
    if !removed {
        if let Ok(o) = &result {
            tracing::warn!(agent, error = %String::from_utf8_lossy(&o.stderr).trim(), path = %working_dir.display(),
                "#2234 teardown: git worktree remove failed — falling back to remove_dir_all + prune");
        }
        let _ = std::fs::remove_dir_all(working_dir);
        if !source_repo.as_os_str().is_empty() {
            let _ = crate::git_helpers::git_bypass(&source_repo, &["worktree", "prune"]);
        }
    }
    true
}

/// Resolve the canonical repo that OWNS a worktree, from its gitlink's
/// common-dir (the binding may already be cleared at teardown). Falls back to
/// the binding's recorded `source_repo`.
fn resolve_owning_repo(home: &Path, agent: &str, working_dir: &Path) -> PathBuf {
    if let Ok(o) = crate::git_helpers::git_bypass(working_dir, &["rev-parse", "--git-common-dir"]) {
        if o.status.success() {
            let raw = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !raw.is_empty() {
                let common = if Path::new(&raw).is_absolute() {
                    PathBuf::from(&raw)
                } else {
                    working_dir.join(&raw)
                };
                let common = dunce::canonicalize(&common).unwrap_or(common);
                // common = `<repo>/.git`; its parent is the repo root.
                if let Some(repo) = common.parent() {
                    return repo.to_path_buf();
                }
            }
        }
    }
    crate::binding::read(home, agent)
        .map(|b| source_repo_from_binding(&b, working_dir))
        .unwrap_or_default()
}

/// True if a worktree holds work that must not be silently destroyed:
/// uncommitted/untracked changes, or — when a remote exists to be ahead of —
/// local commits not reachable from any remote-tracking ref (committed-orphan).
pub(crate) fn worktree_has_work_at_risk(wt: &Path) -> bool {
    // Uncommitted/untracked work — EXCLUDING the daemon's own `.agend-managed`
    // marker, which `git status --porcelain` reports as untracked but is
    // regenerable metadata, not work (every leased/provisioned worktree carries
    // it, so counting it would force a backup on EVERY release/teardown). Parse
    // porcelain directly rather than `has_uncommitted_changes` so we can drop the
    // marker line; fail-closed (spawn/non-zero → treat as at-risk) is preserved.
    match crate::git_helpers::git_bypass(wt, &["status", "--porcelain"]) {
        Ok(o) if o.status.success() => {
            // Porcelain line = `XY <path>` (status code + space + path). The ONLY
            // line to ignore is the root marker `?? .agend-managed`; match the path
            // EXACTLY (porcelain path starts at byte 3) so a real file whose name
            // merely ENDS with `.agend-managed` is NOT mistaken for the marker.
            let is_marker_line = |l: &str| l.get(3..) == Some(MANAGED_MARKER);
            let dirty = String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| !is_marker_line(l));
            if dirty {
                return true;
            }
        }
        _ => return true, // fail-closed
    }
    // "Unpushed" only has meaning when a remote exists; in a remote-less repo
    // every commit looks unreachable-from-remotes, which is not work-at-risk.
    let has_remote = crate::git_helpers::git_bypass(wt, &["remote"])
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    if !has_remote {
        return false;
    }
    crate::git_helpers::git_bypass(wt, &["rev-list", "--count", "HEAD", "--not", "--remotes"])
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u64>()
                .ok()
        })
        .map(|n| n > 0)
        .unwrap_or(false)
}

/// Back up a worktree WHOLE to `<home>/reconcile-backups/<agent>-<epoch>/`,
/// skipping the regenerable build cache (`target`) and the gitlink (`.git`).
/// Conservative (lead Q2): never auto-deleted — operator / gc reclaim later.
/// Back up `wt` to `<home>/reconcile-backups/<agent>[-<branch>]-<epoch>/`. The
/// optional `branch` discriminator (lead Q1 ruling) keeps backups UNIQUE when a
/// single dispatch releases MULTIPLE stale holders in the same wall-clock second
/// (#2234 Phase 1c `release_stale_branch_holders`) — without it `<agent>-<epoch>`
/// would collide and the second copy would merge into the first. `None`
/// (teardown's single-worktree path) keeps the original `<agent>-<epoch>` name.
fn backup_worktree_dir(
    home: &Path,
    agent: &str,
    branch: Option<&str>,
    wt: &Path,
) -> std::io::Result<PathBuf> {
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = match branch {
        Some(b) => format!("{agent}-{}-{epoch}", sanitize_backup_segment(b)),
        None => format!("{agent}-{epoch}"),
    };
    let dest = home.join("reconcile-backups").join(name);
    std::fs::create_dir_all(&dest)?;
    copy_dir_excluding(wt, &dest, &["target", ".git"])?;
    Ok(dest)
}

/// Filesystem-safe slug for a branch in a backup dir name (`feat/x` → `feat-x`).
fn sanitize_backup_segment(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn copy_dir_excluding(src: &Path, dst: &Path, exclude: &[&str]) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if exclude.iter().any(|e| name == std::ffi::OsStr::new(e)) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_dir_excluding(&from, &to, exclude)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// #2234 cure-(B) gray-rollout gate. The workspace-as-worktree behavior is OFF
/// by default — `resolve_auto_worktree` then returns `None` for workspace dirs
/// exactly as pre-(B) (byte-identical). `AGEND_WORKSPACE_AS_WORKTREE=1` (or
/// `true`) enables it; an optional `AGEND_WORKSPACE_AS_WORKTREE_AGENTS=a,b`
/// allowlist scopes the validation phase to named agents (empty/unset = all
/// agents once the flag is on). Per lead Q1: a flag (not a per-instance field) —
/// default off → opt-in a few agents → flip the default at cutover.
pub fn workspace_as_worktree_enabled(agent: &str) -> bool {
    // Test-injectable seam (#2234 Phase 1c): tests enable (B) via a THREAD-LOCAL
    // override (`workspace_worktree_test_seam::force`) instead of a process-global
    // `set_var`, so a flag-ON test never leaks the flag to other tests running in
    // parallel in the same binary (the env-leak flake class r6 caught twice). The
    // production read-path below is byte-identical — the daemon still reads
    // `AGEND_WORKSPACE_AS_WORKTREE` (+ allowlist). Compiled out of release builds.
    #[cfg(test)]
    if let Some(forced) = workspace_worktree_test_seam::get() {
        return forced;
    }
    workspace_as_worktree_from_env(
        std::env::var("AGEND_WORKSPACE_AS_WORKTREE").ok().as_deref(),
        std::env::var("AGEND_WORKSPACE_AS_WORKTREE_AGENTS")
            .ok()
            .as_deref(),
        agent,
    )
}

/// Pure flag decision over (flag, allowlist) inputs — unit-testable without any
/// process-global env mutation. `AGEND_WORKSPACE_AS_WORKTREE` must be `1`/`true`;
/// a non-empty `AGEND_WORKSPACE_AS_WORKTREE_AGENTS` then scopes to listed agents.
pub(crate) fn workspace_as_worktree_from_env(
    flag: Option<&str>,
    allowlist: Option<&str>,
    agent: &str,
) -> bool {
    if !matches!(flag, Some("1") | Some("true")) {
        return false;
    }
    match allowlist {
        Some(list) if !list.trim().is_empty() => list.split(',').any(|a| a.trim() == agent),
        _ => true,
    }
}

/// #2234 cure-(B): make the agent's per-agent workspace dir BE a daemon-managed
/// worktree of `source_repo` (its `.git` a gitlink FILE), so the agent's cwd ==
/// its bound worktree and the cwd<->worktree dual-truth disappears — while the
/// cwd PATH stays byte-identical (the #1919 property: `claude --continue` keys
/// its session on the cwd path, so an in-place branch switch never orphans it).
/// Idempotent. Three states of `target`:
///   (i)   absent / empty       → `git worktree add` (produces a real gitlink).
///   (ii)  standalone clone      → backup the WHOLE dir (fail-closed: backup Err
///         (`.git` is a DIR)        → ABORT, leave the standalone untouched) →
///                                   remove → add. A standalone may carry a
///                                   committed-but-unpushed (orphan) commit that
///                                   `has_uncommitted` misses, so we back up the
///                                   whole dir, not just uncommitted work.
///   (iii) already a worktree    → verify its gitlink common-dir resolves to
///         (`.git` gitlink FILE)    `source_repo`; match → NO-OP (idempotent, no
///                                   backup); foreign → fall through to (ii).
///
/// HOLDING-CLEAN BY CONSTRUCTION (relied on by the Phase-1c no-`--force` in-place
/// checkout's atomicity): a freshly-provisioned worktree is created detached at
/// the repo HEAD (or on `branch` when given) with a clean tree. Phase-1c's
/// dispatch then does the in-place `git checkout <task-branch>` — without
/// `--force`, which git aborts atomically if the tree were dirty. Because this
/// fn only ever hands back a clean holding tree, that checkout cannot silently
/// lose work.
///
/// Returns the worktree path (== `target`) on success; `Err` is fail-safe — the
/// caller (`resolve_auto_worktree`) keeps the workspace as a non-worktree, so the
/// agent stays on the pre-(B) path under the #2254 drift-WARN safety net.
pub fn reconcile_workspace_to_worktree(
    home: &Path,
    agent: &str,
    target: &Path,
    source_repo: &Path,
    branch: Option<&str>,
) -> Result<PathBuf, String> {
    // (iii) already a daemon worktree rooted at source_repo → idempotent no-op.
    if target.join(".git").is_file() && worktree_common_dir_matches(target, source_repo) {
        return Ok(target.to_path_buf());
    }
    // (ii) standalone clone OR foreign worktree: the target must be EMPTY before
    // `git worktree add`, so back up any work then remove. (i) empty/absent skips
    // the backup (nothing at risk).
    if target.exists() {
        let non_empty = std::fs::read_dir(target)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
        if non_empty {
            backup_worktree_dir(home, agent, branch, target).map_err(|e| {
                format!(
                    "reconcile aborted (fail-closed): backup of {} failed: {e} — workspace left untouched",
                    target.display()
                )
            })?;
        }
        std::fs::remove_dir_all(target)
            .map_err(|e| format!("reconcile: remove {} failed: {e}", target.display()))?;
    }
    provision_worktree_at(agent, target, source_repo, branch)?;
    // r6 #4: confirm `git worktree add` produced a real gitlink FILE (the
    // discriminator the whole (B) lifecycle keys on).
    if !target.join(".git").is_file() {
        return Err(format!(
            "reconcile: post-add .git is not a gitlink file at {}",
            target.display()
        ));
    }
    Ok(target.to_path_buf())
}

/// True if `target`'s git common-dir resolves to `source_repo` (i.e. it is a
/// worktree OF that canonical repo, not a foreign one).
pub(crate) fn worktree_common_dir_matches(target: &Path, source_repo: &Path) -> bool {
    let Ok(o) = crate::git_helpers::git_bypass(target, &["rev-parse", "--git-common-dir"]) else {
        return false;
    };
    if !o.status.success() {
        return false;
    }
    let raw = String::from_utf8_lossy(&o.stdout).trim().to_string();
    if raw.is_empty() {
        return false;
    }
    let common = if Path::new(&raw).is_absolute() {
        PathBuf::from(&raw)
    } else {
        target.join(&raw)
    };
    // `common` is `<repo>/.git`; compare its parent to `source_repo`.
    let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    common
        .parent()
        .map(|repo| canon(repo) == canon(source_repo))
        .unwrap_or(false)
}

/// `git worktree add` at an arbitrary `target` (the workspace path), HOLDING:
/// detached at HEAD when `branch` is None, else on `branch` (new via `-b`,
/// falling back to an existing branch). Writes the `.agend-managed` marker.
fn provision_worktree_at(
    agent: &str,
    target: &Path,
    source_repo: &Path,
    branch: Option<&str>,
) -> Result<(), String> {
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let target_str = target.display().to_string();
    use crate::git_helpers::{git_cmd, GitError};
    let add = |args: &[&str]| git_cmd(source_repo, args);
    let result = match branch {
        // Holding state: detached at the repo HEAD — Phase-1c checks out the task
        // branch in place at dispatch.
        None => add(&["worktree", "add", "--detach", &target_str]),
        Some(b) => match add(&["worktree", "add", "-b", b, &target_str]) {
            // Branch already exists → attach to it (mirror worktree::create).
            Err(GitError::NonZero { stderr, .. }) if stderr.contains("already exists") => {
                add(&["worktree", "add", &target_str, b])
            }
            other => other,
        },
    };
    match result {
        Ok(_) => {
            let _ = std::fs::write(
                target.join(MANAGED_MARKER),
                format!("agent={agent}\nreconciled=workspace-as-worktree\n"),
            );
            Ok(())
        }
        Err(e) => Err(format!(
            "reconcile: git worktree add at {} failed: {e}",
            target.display()
        )),
    }
}

/// #2234 Phase 1c: the (B) replacement for `lease` in dispatch — prepare the
/// agent's WORKSPACE worktree for `branch`. Idempotent reconcile (spawn already
/// provisioned it; re-assert covers a dispatch racing a not-yet-spawned agent or
/// a deferred reconcile) → free `branch` from any stale legacy holders (each
/// work-at-risk backed up before `--force`) → in-place `git checkout` (no
/// `--force`; atomic abort on dirty). Returns the workspace worktree path to bind.
pub fn prepare_workspace_worktree(
    home: &Path,
    agent: &str,
    source_repo: &Path,
    branch: &str,
) -> Result<PathBuf, String> {
    let ws = crate::paths::workspace_dir(home).join(agent);
    reconcile_workspace_to_worktree(home, agent, &ws, source_repo, None)?;
    release_stale_branch_holders(home, agent, source_repo, branch, &ws)?;
    checkout_workspace_branch(&ws, branch)?;
    Ok(ws)
}

/// #2234 Phase 1c (must-resolve #1, r6 confluence catch): free `branch` from any
/// STALE legacy holders (the pre-(B) `worktrees/<agent>/<branch>` pool) before
/// the workspace worktree's in-place checkout — else git refuses with "branch
/// already checked out at <other>". Drives off the canonical
/// [`enumerate_managed_worktrees`] (single source of truth over /workspace +
/// /worktrees). Only releases this `agent`'s registered holders of `branch` that
/// are NOT the workspace worktree itself; other residuals are left for GC (off
/// the dispatch critical path → lower blast). Any single release failing
/// (including a fail-closed backup abort) ABORTS the whole dispatch.
pub fn release_stale_branch_holders(
    home: &Path,
    agent: &str,
    source_repo: &Path,
    branch: &str,
    workspace_path: &Path,
) -> Result<(), String> {
    let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let ws = canon(workspace_path);
    for wt in enumerate_managed_worktrees(home, source_repo) {
        let is_self = canon(&wt.path) == ws;
        let holds_branch = wt.branch.as_deref() == Some(branch);
        let mine = wt.agent.as_deref() == Some(agent);
        if !is_self && mine && holds_branch {
            release_one_stale_holder(home, agent, source_repo, branch, &wt.path)?;
        }
    }
    Ok(())
}

/// Release a single stale legacy worktree, WORK-AT-RISK guarded (must-resolve
/// #1): the old bound worktree is exactly where the shim's `-C <worktree>`
/// redirected commits, so it may hold uncommitted/unpushed work that a bare
/// `--force` would destroy. Mirror Phase-0 teardown: if work-at-risk, back up
/// the WHOLE dir FIRST (keyed by branch so concurrent same-second releases don't
/// collide) — and if that backup FAILS, ABORT fail-closed (never `--force`
/// without a durable backup). All Phase-0 primitives reused verbatim.
pub(crate) fn release_one_stale_holder(
    home: &Path,
    agent: &str,
    source_repo: &Path,
    branch: &str,
    holder: &Path,
) -> Result<(), String> {
    if worktree_has_work_at_risk(holder) {
        backup_worktree_dir(home, agent, Some(branch), holder).map_err(|e| {
            format!(
                "release aborted (fail-closed): backup of stale holder {} failed: {e}",
                holder.display()
            )
        })?;
    }
    match remove_worktree(agent, holder, source_repo) {
        WorktreeRemoval::Removed | WorktreeRemoval::AlreadyAbsent => Ok(()),
        WorktreeRemoval::Unmanaged(m) => Err(m),
        WorktreeRemoval::Failed(e) => Err(e),
    }
}

/// #2234 Phase 1c: in-place `git checkout <branch>` of the workspace worktree
/// (the (B) replacement for leasing a fresh per-branch worktree). NO `--force` —
/// git aborts atomically if the tree is dirty/conflicting, leaving HEAD on the
/// prior branch so the caller can reject the dispatch without a half-applied
/// state (the holding-clean invariant makes this the normal clean case).
pub fn checkout_workspace_branch(workspace: &Path, branch: &str) -> Result<(), String> {
    use crate::git_helpers::git_cmd;
    git_cmd(workspace, &["checkout", branch])
        .map(|_| ())
        .map_err(|e| format!("in-place checkout of '{branch}' failed: {e}"))
}

/// #2234 Phase 1c rollback: return the workspace worktree to its HOLDING state
/// (detached at HEAD) — used when `bind_full` fails AFTER a successful checkout.
/// NEVER deletes the (permanent) workspace worktree; the just-checked-out branch
/// carries no agent work yet (bind_full runs synchronously right after checkout,
/// before the agent is told the dispatch succeeded), so detaching is safe.
pub fn detach_workspace_to_holding(workspace: &Path) -> Result<(), String> {
    use crate::git_helpers::git_cmd;
    git_cmd(workspace, &["checkout", "--detach"])
        .map(|_| ())
        .map_err(|e| format!("rollback detach failed: {e}"))
}

/// #2234 cure-(B) ROLLBACK primitive (flag-independent — callable in ANY flag
/// state). Revert an agent whose `/workspace/<agent>` was converted to a (B)
/// worktree back to a standalone, so OFF legacy dispatch works correctly again
/// (it leases a SEPARATE `worktrees/<agent>/<branch>`; if `/workspace` stayed a
/// gitlink worktree, OFF would re-introduce the #2234 cwd↔binding split AND a
/// same-branch lease would hit git's "already checked out at /workspace").
///
/// **Work-safety**:
/// - COMMITTED work is preserved BY CONSTRUCTION — `/workspace` is a worktree of
///   the canonical repo, so its commits live in canonical's object store and the
///   branch ref (`refs/heads/<X>`) lives in canonical. `git worktree remove
///   --force` removes ONLY the working dir + admin, never the branch ref/commits;
///   the agent's next OFF lease of branch `<X>` checks them back out. (Empirically
///   verified.) We deliberately do NOT restore from `reconcile-backups` — that is
///   the FORWARD (standalone→worktree) snapshot and does not contain
///   post-conversion commits, so using it would LOSE that work.
/// - UNCOMMITTED/untracked work is the only at-risk class → Phase-0
///   `worktree_has_work_at_risk` + whole-dir backup, fail-closed (a backup error
///   ABORTS the revert and leaves `/workspace` untouched).
///
/// No-op (`Ok`) when `/workspace` is NOT a (B) worktree (already standalone /
/// absent / plain dir). Restores a clean git-init standalone, matching pre-(B).
/// Edge: a deleted (never re-leased) agent leaves its branch + commits in
/// canonical (branch_sweep keeps unpushed branches) — recoverable, not lost.
pub fn reverse_reconcile(home: &Path, agent: &str) -> Result<(), String> {
    let ws = crate::paths::workspace_dir(home).join(agent);
    // Only a real (B) worktree has a `.git` gitlink FILE. Standalone (dir) / plain
    // dir / absent are already OFF-compatible → nothing to revert.
    if !ws.join(".git").is_file() {
        return Ok(());
    }
    // Save uncommitted/untracked work BEFORE any destructive step (committed work
    // is already safe in canonical). Fail-closed: backup error → abort, untouched.
    if worktree_has_work_at_risk(&ws) {
        backup_worktree_dir(home, agent, None, &ws).map_err(|e| {
            format!(
                "reverse_reconcile aborted (fail-closed): backup of {} failed: {e} — workspace left untouched",
                ws.display()
            )
        })?;
    }
    // Remove the (B) worktree via the SAME primitive dev-2's Phase 2 / Phase-0
    // teardown use (git worktree remove --force from the owning repo; branch ref +
    // commits remain in canonical).
    let source_repo = resolve_owning_repo(home, agent, &ws);
    match remove_worktree(agent, &ws, &source_repo) {
        WorktreeRemoval::Removed | WorktreeRemoval::AlreadyAbsent => {}
        WorktreeRemoval::Unmanaged(m) => {
            return Err(format!("reverse_reconcile: {m}"));
        }
        WorktreeRemoval::Failed(e) => {
            return Err(format!("reverse_reconcile: worktree remove failed: {e}"));
        }
    }
    // Clear the (B) binding so the next dispatch leases fresh (legacy path).
    crate::binding::unbind(home, agent);
    // Restore `/workspace/<agent>` as a clean standalone (git-init), matching the
    // pre-(B) state. `git worktree remove` deleted the dir, so recreate it; the
    // next spawn's `ensure_project_root` would also do this, but doing it here
    // makes the revert self-contained + testable.
    let _ = std::fs::create_dir_all(&ws);
    crate::instructions::ensure_project_root(&ws);
    Ok(())
}

/// #2234 Phase 1c test seam: a THREAD-LOCAL (B)-flag override. Tests force the
/// flag on/off for their OWN thread — `workspace_as_worktree_enabled` runs
/// synchronously on the caller thread (dispatch / resolve_auto_worktree), so the
/// override is observed there but is invisible to other tests' threads. This
/// roots out the process-global `set_var` leak class (no serial-grouping needed).
#[cfg(test)]
pub(crate) mod workspace_worktree_test_seam {
    use std::cell::Cell;
    thread_local! {
        static OVERRIDE: Cell<Option<bool>> = const { Cell::new(None) };
    }
    pub(crate) fn get() -> Option<bool> {
        OVERRIDE.with(|c| c.get())
    }
    fn set(v: Option<bool>) {
        OVERRIDE.with(|c| c.set(v));
    }
    /// RAII: force the flag for the current thread; restores on drop (incl. panic).
    #[must_use]
    pub(crate) struct ForceGuard;
    pub(crate) fn force(enabled: bool) -> ForceGuard {
        set(Some(enabled));
        ForceGuard
    }
    impl Drop for ForceGuard {
        fn drop(&mut self) {
            set(None);
        }
    }
}
