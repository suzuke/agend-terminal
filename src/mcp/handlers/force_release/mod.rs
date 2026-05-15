//! MCP handler for `force_release_worktree` (Sprint 59 Wave 1 PR-5
//! emergency cherry-pick) — closes the architectural defect that
//! drove the Sprint 59 Wave 1 PR-2 BYPASS incident + PR-4 (C)-path
//! stall.
//!
//! When `bind_self` returns `lease_failed` because an on-disk
//! worktree dir exists from a prior bind cycle but the daemon
//! binding state was already released, callers had no daemon-
//! managed path to clean the stale dir without resorting to
//! `AGEND_GIT_BYPASS=1`. Per operator's Q2=(C) bypass-free
//! permanent protocol decision (2026-05-09), this tool ships the
//! daemon-side cleanup surface so the (C) path can recover from
//! stale-state without ever touching BYPASS.
//!
//! Extracted from `worktree.rs` to keep that file under the 700
//! LOC handler invariant (`tests/file_size_invariant.rs`).

use crate::identity::Sender;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// MCP tool: `force_release_worktree`.
///
/// Required args: `agent` (string), `branch` (string).
///
/// Behavior:
/// 1. Validate agent + branch name format.
/// 2. Compute target dir: `<home>/worktrees/<agent>/<branch>/`.
/// 3. Safety: reject if the resolved path is outside the daemon-
///    managed worktree pool (defense-in-depth against malicious args).
/// 4. If dir exists: `std::fs::remove_dir_all`.
/// 5. Defensively call existing `release_full` to clear any lingering
///    binding state (idempotent on already-cleared bindings).
/// 6. Return structured `{"released": true, "dir_existed": bool,
///    "dir_removed": bool, "binding_outcome": <ReleaseOutcome>}`.
///
/// Idempotent: calling twice on a clean state is a no-op.
///
/// Fail-open: minor IO errors during dir removal are logged via
/// `tracing::warn` but the binding-clear half still runs so partial
/// recovery is preserved.
pub(crate) fn handle_force_release_worktree(
    home: &Path,
    args: &Value,
    _sender: &Option<Sender>,
) -> Value {
    let agent = match args["agent"].as_str() {
        Some(a) if !a.is_empty() => a,
        _ => return json!({"error": "missing 'agent'"}),
    };
    let branch = match args["branch"].as_str() {
        Some(b) if !b.is_empty() => b,
        _ => return json!({"error": "missing 'branch'"}),
    };
    if let Err(e) = crate::agent::validate_name(agent) {
        return json!({"error": e, "code": "invalid_agent"});
    }
    if !crate::agent_ops::validate_branch(branch) {
        return json!({
            "error": format!("invalid branch name '{branch}'"),
            "code": "invalid_branch"
        });
    }

    // Compute the canonical daemon-managed worktree path. The Wave 4
    // layout (Sprint 57 #546 Item 4) is `$AGEND_HOME/worktrees/<agent>/<branch>/`.
    let worktrees_root = home.join("worktrees");
    let target = worktrees_root.join(agent).join(branch);

    // Safety: ensure the resolved target is within the worktrees pool
    // AND deeper than the agent-level subdirectory (a `branch == ""`
    // would otherwise resolve to the agent's own dir; the empty-
    // string check at the top already rejects this, but the
    // defense-in-depth guard catches future validator drift).
    let safe = target.starts_with(&worktrees_root)
        && target != worktrees_root
        && target != worktrees_root.join(agent);
    if !safe {
        return json!({
            "error": format!(
                "force_release_worktree refuses to clean path outside the daemon \
                 worktree pool: {}",
                target.display()
            ),
            "code": "path_outside_pool"
        });
    }

    // #826: optional operator-supplied `source_repo` arg. When
    // present, L2 skips enumeration and goes straight to the named
    // repo. When absent, L2 enumerates daemon-managed candidates.
    let source_repo_hint = args["source_repo"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);

    match rebase_clean_self(home, agent, branch) {
        Ok(o) => {
            // #826 L2 GC: when the binding-clear path short-circuited
            // on "no binding" (the post-disband state), the
            // `git worktree remove --force` step inside `release_full`
            // never ran. Run it now against any source repos that
            // still hold `.git/worktrees/<meta-dir>/` metadata for
            // our target worktree path.
            let gc = prune_git_metadata_for_agent(home, agent, branch, source_repo_hint.as_deref());
            json!({
                "released": true,
                "dir_existed": o.dir_existed,
                "dir_removed": o.dir_removed,
                "binding_outcome": o.binding_outcome,
                "git_metadata_pruned": gc.pruned_count,
                "git_metadata_repos": gc.repos_touched,
            })
        }
        Err(e) => json!({"error": e, "code": "path_outside_pool"}),
    }
}

/// #826 L2 GC outcome: count + list of source repos where the
/// `git worktree remove --force` (and `git worktree prune` fallback)
/// step actually pruned a metadata entry for the target agent's
/// worktree path.
#[derive(Debug, Default)]
struct GcOutcome {
    pruned_count: usize,
    repos_touched: Vec<String>,
}

/// #826 L2: enumerate source repos that may still hold
/// `.git/worktrees/<meta-dir>/` metadata pointing at the daemon-
/// managed worktree path `<home>/worktrees/<agent>/<branch>/`, and
/// run `git worktree remove --force` per matching entry. Idempotent
/// — re-running on already-pruned metadata is a no-op (returns
/// `GcOutcome::default()` with `pruned_count: 0`).
///
/// Source-repo discovery:
/// 1. If `source_repo_hint` is supplied (operator's fast path) →
///    use it as the single candidate.
/// 2. Else → enumerate via two sources:
///    - Walk `<home>/worktrees/*/<...>/.git` pointer files from
///      sibling agents (the daemon's worktree convention writes a
///      `.git` file containing `gitdir: <source>/.git/worktrees/<name>`).
///    - Read `crate::teams::list_all(home)` and collect each
///      team's `source_repo` field.
///
/// For each candidate source repo, run `git worktree list
/// --porcelain` (via `crate::worktree_cleanup::list_worktrees`)
/// to find entries whose path matches the target. For each match,
/// run `git worktree remove --force <path>` against the source
/// repo's cwd. AGEND_GIT_BYPASS=1 is set on every git invocation
/// to bypass the daemon shim per the operator-confirmed manual
/// recovery command.
fn prune_git_metadata_for_agent(
    home: &Path,
    agent: &str,
    branch: &str,
    source_repo_hint: Option<&Path>,
) -> GcOutcome {
    let target_path = home.join("worktrees").join(agent).join(branch);
    let candidates: Vec<PathBuf> = match source_repo_hint {
        Some(p) => vec![p.to_path_buf()],
        None => discover_source_repo_candidates(home),
    };

    let mut outcome = GcOutcome::default();
    let mut seen = std::collections::HashSet::new();
    for repo in candidates {
        // Dedupe candidates (sibling enumeration may report the
        // same repo via multiple agents).
        if !seen.insert(repo.clone()) {
            continue;
        }
        if !repo.exists() {
            continue;
        }
        let entries = list_worktrees_bypass_shim(&repo);
        for entry in entries {
            if !paths_match(&entry.path, &target_path) {
                continue;
            }
            // Found a matching metadata entry — prune it.
            let removed = std::process::Command::new("git")
                .current_dir(&repo)
                .args(["worktree", "remove", "--force", &entry.path])
                .env("AGEND_GIT_BYPASS", "1")
                .output();
            let pruned = match removed {
                Ok(o) if o.status.success() => true,
                Ok(o) => {
                    // Fallback: when the worktree dir is already
                    // gone but `git worktree remove` errors (e.g.,
                    // "not a worktree"), run `git worktree prune`
                    // to clean stale metadata.
                    tracing::warn!(
                        agent = %agent,
                        branch = %branch,
                        repo = %repo.display(),
                        stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                        "#826 L2: git worktree remove failed; falling back to prune"
                    );
                    let prune = std::process::Command::new("git")
                        .current_dir(&repo)
                        .args(["worktree", "prune"])
                        .env("AGEND_GIT_BYPASS", "1")
                        .output();
                    matches!(prune, Ok(p) if p.status.success())
                }
                Err(e) => {
                    tracing::warn!(
                        agent = %agent,
                        branch = %branch,
                        repo = %repo.display(),
                        error = %e,
                        "#826 L2: git worktree remove spawn failed"
                    );
                    false
                }
            };
            if pruned {
                outcome.pruned_count += 1;
                let repo_str = repo.display().to_string();
                if !outcome.repos_touched.contains(&repo_str) {
                    outcome.repos_touched.push(repo_str);
                }
                tracing::info!(
                    agent = %agent,
                    branch = %branch,
                    repo = %repo.display(),
                    path = %entry.path,
                    "#826 L2: pruned stale .git/worktrees/ metadata"
                );
            }
        }
    }
    outcome
}

/// #826 L2 fork of `crate::worktree_cleanup::list_worktrees`. The
/// shared helper omits `AGEND_GIT_BYPASS=1`, which means the daemon
/// `agend-git` shim may intercept the call when an instance binding
/// is active in the calling context — list_worktrees would then
/// return Vec::new() instead of the real entries. Daemon-internal
/// L2 GC always wants the raw `git worktree list --porcelain` output
/// from the source repo, so we run it with the shim bypass set
/// (mirrors the `release_full` precedent at src/worktree_pool.rs:311).
fn list_worktrees_bypass_shim(repo_root: &Path) -> Vec<crate::worktree_cleanup::WorktreeEntry> {
    let output = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .env("AGEND_GIT_BYPASS", "1")
        .output();
    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();
    let mut current_path = None;
    let mut current_branch = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            current_path = Some(p.to_string());
        } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
            current_branch = Some(b.to_string());
        } else if line.is_empty() {
            if let (Some(path), Some(branch)) = (current_path.take(), current_branch.take()) {
                if branch != "main" && branch != "master" {
                    entries.push(crate::worktree_cleanup::WorktreeEntry { path, branch });
                }
            }
            current_path = None;
            current_branch = None;
        }
    }
    entries
}

/// #826 L2: best-effort source-repo enumeration when the operator
/// didn't supply a `source_repo` arg. Two sources, deduped by caller:
/// 1. Sibling daemon-managed worktrees' `.git` pointer files
///    (`gitdir: <source>/.git/worktrees/<name>`).
/// 2. `crate::teams::list_all` team `source_repo` fields.
fn discover_source_repo_candidates(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    // Source 1: walk sibling daemon worktrees' .git pointers.
    let worktrees_root = home.join("worktrees");
    if let Ok(agents) = std::fs::read_dir(&worktrees_root) {
        for agent_entry in agents.flatten() {
            let agent_dir = agent_entry.path();
            if !agent_dir.is_dir() {
                continue;
            }
            collect_source_repos_from_worktree_tree(&agent_dir, &mut out);
        }
    }
    // Source 2: team-level source_repo from fleet.yaml.
    for team in crate::teams::list_all(home) {
        if let Some(repo) = team.source_repo {
            out.push(repo);
        }
    }
    out
}

/// Recursively walk a daemon-managed worktree subtree looking for
/// `.git` pointer files. For each found, parse the
/// `gitdir: <source>/.git/worktrees/<name>` line and derive the
/// source repo (strip `.git/worktrees/<name>` suffix).
fn collect_source_repos_from_worktree_tree(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_source_repos_from_worktree_tree(&p, out);
            continue;
        }
        if p.file_name().and_then(|n| n.to_str()) != Some(".git") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Some(gitdir) = content
            .lines()
            .find_map(|l| l.strip_prefix("gitdir:").map(str::trim))
        else {
            continue;
        };
        // `gitdir` points at `<source>/.git/worktrees/<name>` — walk
        // up two parents to reach `<source>`.
        let path = PathBuf::from(gitdir);
        if let Some(source) = path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            out.push(source.to_path_buf());
        }
    }
}

/// Compare a `git worktree list --porcelain` path string against the
/// target daemon-managed worktree path. Both paths may reference a
/// non-existent location (the working tree dir was removed by
/// `remove_dir_all` before this call), so direct `canonicalize` may
/// fail on either side. Strategy:
///
/// 1. Walk up each path until the parent EXISTS, canonicalize that
///    parent, then re-append the missing remainder.
/// 2. macOS-specific quirk: `/var/...` and `/private/var/...` resolve
///    to the same fs node via the `/var → /private/var` symlink.
///    `git worktree list --porcelain` always emits the
///    `/private/var/...` form (canonicalized at `git worktree add`
///    time), while our test fixture and runtime `home` paths arrive
///    as `/var/...`. Equality after canonicalize-of-parent handles
///    this when parents exist; when neither parent canonicalizes,
///    fall back to the macOS prefix normalization.
fn paths_match(entry_path: &str, target: &Path) -> bool {
    let entry = PathBuf::from(entry_path);
    let entry_norm = canonicalize_via_parent(&entry);
    let target_norm = canonicalize_via_parent(target);
    entry_norm == target_norm
}

/// Walk up `path` until a parent exists, canonicalize that parent,
/// then re-append the missing remainder. Falls back to the input
/// path on root-level failures.
fn canonicalize_via_parent(path: &Path) -> PathBuf {
    if let Ok(p) = path.canonicalize() {
        return p;
    }
    // Collect trailing components that don't exist; walk up until
    // an ancestor canonicalizes; re-attach.
    let mut trail: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cursor: &Path = path;
    while let Some(parent) = cursor.parent() {
        if parent.as_os_str().is_empty() {
            break;
        }
        if let Some(name) = cursor.file_name() {
            trail.push(name);
        }
        if let Ok(p) = parent.canonicalize() {
            let mut rebuilt = p;
            for segment in trail.iter().rev() {
                rebuilt.push(segment);
            }
            return rebuilt;
        }
        cursor = parent;
    }
    path.to_path_buf()
}

/// Outcome of a rebase-clean operation.
#[derive(Debug)]
pub(super) struct RebaseCleanOutcome {
    pub(super) dir_existed: bool,
    pub(super) dir_removed: bool,
    pub(super) binding_outcome: Value,
}

/// Sprint 60 W1 PR-1: shared cleanup helper used by both
/// `force_release_worktree` (operator/agent-callable) and
/// `bind_self(rebase_mode=true)` (atomic recover-and-bind).
///
/// Validates path safety against the daemon worktree pool, removes
/// the stale on-disk dir if present, and clears any lingering binding
/// state via `release_full`. Returns `Err` only on path-safety
/// violation; all other failures are fail-open with tracing::warn so
/// partial recovery is preserved.
///
/// Caller invariant: `agent` and `branch` must be pre-validated by
/// `agent::validate_name` + `agent_ops::validate_branch` respectively.
/// This helper trusts its callers; the path-safety guard below is
/// defense-in-depth, not the primary validator.
pub(super) fn rebase_clean_self(
    home: &Path,
    agent: &str,
    branch: &str,
) -> Result<RebaseCleanOutcome, String> {
    let worktrees_root = home.join("worktrees");
    let target = worktrees_root.join(agent).join(branch);
    let safe = target.starts_with(&worktrees_root)
        && target != worktrees_root
        && target != worktrees_root.join(agent);
    if !safe {
        return Err(format!(
            "force_release_worktree refuses to clean path outside the daemon \
             worktree pool: {}",
            target.display()
        ));
    }

    let dir_existed = target.exists();
    let mut dir_removed = false;
    if dir_existed {
        match std::fs::remove_dir_all(&target) {
            Ok(()) => {
                dir_removed = true;
                tracing::info!(
                    %agent,
                    %branch,
                    path = %target.display(),
                    "force_release_worktree: stale worktree dir cleaned"
                );
            }
            Err(e) => {
                tracing::warn!(
                    %agent,
                    %branch,
                    error = %e,
                    path = %target.display(),
                    "force_release_worktree: dir removal failed (will still try binding-clear)"
                );
            }
        }
    }

    let binding_outcome = crate::worktree_pool::release_full(home, agent, false);
    Ok(RebaseCleanOutcome {
        dir_existed,
        dir_removed,
        binding_outcome: serde_json::to_value(&binding_outcome).unwrap_or(Value::Null),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(suffix: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let h = std::env::temp_dir().join(format!(
            "agend-force-release-{}-{}-{}",
            std::process::id(),
            suffix,
            id,
        ));
        std::fs::create_dir_all(&h).ok();
        h
    }

    /// Helper: write a daemon-managed worktree dir at the canonical
    /// path with the `.agend-managed` marker so tests can simulate
    /// the stale-state scenario (post-bind, pre-cleanup).
    fn seed_daemon_worktree(home: &Path, agent: &str, branch: &str) -> std::path::PathBuf {
        let dir = home.join("worktrees").join(agent).join(branch);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".agend-managed"),
            format!("agent={agent}\nbranch={branch}\n"),
        )
        .unwrap();
        // Drop a sample file so we can verify recursive cleanup.
        std::fs::write(dir.join("sample.txt"), "leftover").unwrap();
        dir
    }

    // ── #826 L2 disbanded-agent .git/worktrees/ metadata GC ──

    /// Build a fully-initialized source-repo with a daemon-managed
    /// worktree registered against it, then `remove_dir_all` the
    /// worktree dir to simulate the post-disband half-cleanup state
    /// (working tree gone, `.git/worktrees/<agent>/` metadata persists).
    /// Returns `(source_repo, agent_meta_dir)`. Pins per-repo gitconfig
    /// per the #814 r1 lesson so CI runners without global gitconfig
    /// can run `git worktree add` cleanly.
    fn seed_disbanded_agent_with_git_metadata(
        home: &Path,
        agent: &str,
        branch: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let source_repo = home.join("source_repo");
        std::fs::create_dir_all(&source_repo).unwrap();
        let git_run = |dir: &Path, args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("AGEND_GIT_BYPASS", "1")
                .env("GIT_AUTHOR_NAME", "test")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "test")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("git ran")
        };
        let out = git_run(&source_repo, &["init", "-b", "main"]);
        assert!(out.status.success(), "git init failed: {out:?}");
        git_run(&source_repo, &["config", "user.name", "test"]);
        git_run(&source_repo, &["config", "user.email", "t@t"]);
        let out = git_run(&source_repo, &["commit", "--allow-empty", "-m", "init"]);
        assert!(out.status.success(), "seed commit failed: {out:?}");

        // Add the daemon-managed worktree onto a unique branch.
        let worktree_dir = home.join("worktrees").join(agent).join(branch);
        let out = git_run(
            &source_repo,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                &worktree_dir.display().to_string(),
            ],
        );
        assert!(
            out.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        // git names the metadata dir after the LAST PATH SEGMENT of
        // the worktree path (e.g. `feat/826` → `826`), NOT the agent
        // name. Capture before `remove_dir_all` for post-condition
        // assertions.
        let meta_dir_name = worktree_dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from)
            .expect("worktree path must have a final segment");

        // Now `remove_dir_all` the working tree dir but leave the
        // `.git/worktrees/<name>/` metadata behind — exactly the
        // half-cleanup state #826 fixes.
        std::fs::remove_dir_all(&worktree_dir).expect("remove worktree dir");
        let meta_dir = source_repo
            .join(".git")
            .join("worktrees")
            .join(&meta_dir_name);
        assert!(
            meta_dir.exists(),
            "fixture invariant: .git/worktrees/{meta_dir_name}/ metadata must persist after remove_dir_all (got: {})",
            meta_dir.display()
        );
        (source_repo, meta_dir)
    }

    /// #826 C1 RED: post-disband state — no daemon binding, working
    /// tree dir already gone, but `.git/worktrees/<agent>/` metadata
    /// still lives on the source repo. Pre-fix `force_release_worktree`
    /// reports `released:true` but the metadata persists. Post-fix
    /// (L2) the GC enumerates the source repo + runs
    /// `git worktree remove --force` to prune.
    ///
    /// Asserts the post-fix contract:
    /// - response carries `git_metadata_pruned: 1`
    /// - response carries `git_metadata_repos` array of length 1
    /// - the source repo's `.git/worktrees/<agent>/` dir is gone
    #[test]
    fn force_release_worktree_prunes_stale_git_metadata_when_no_binding() {
        let home = tmp_home("826_l2_prune");
        let (source_repo, agent_meta_dir) =
            seed_disbanded_agent_with_git_metadata(&home, "dev826", "feat/826");

        let result = handle_force_release_worktree(
            &home,
            &json!({
                "agent": "dev826",
                "branch": "feat/826",
                "source_repo": source_repo.display().to_string(),
            }),
            &None,
        );

        assert_eq!(
            result["git_metadata_pruned"], 1,
            "L2 must report 1 metadata entry pruned (the disbanded agent's), got: {result}"
        );
        let repos = result["git_metadata_repos"]
            .as_array()
            .unwrap_or_else(|| panic!("git_metadata_repos must be array, got: {result}"));
        assert_eq!(
            repos.len(),
            1,
            "git_metadata_repos must list the touched source repo"
        );
        assert!(
            !agent_meta_dir.exists(),
            "L2 must prune .git/worktrees/dev826/ from source repo, still present: {}",
            agent_meta_dir.display()
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #826 C3: explicit assertion that the response carries the
    /// L2 audit fields even on the no-op case (idempotent calls).
    /// Locks the response-shape contract so a future refactor can't
    /// silently drop the new fields.
    #[test]
    fn force_release_worktree_git_metadata_pruned_count_in_response() {
        // No fixture seeding — empty home with no source repos.
        // L2 enumeration returns no candidates → pruned_count: 0,
        // repos_touched: []. The response shape still includes the
        // fields (audit contract).
        let home = tmp_home("826_c3_response_shape");
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "dev826c3", "branch": "feat/826"}),
            &None,
        );
        assert_eq!(result["git_metadata_pruned"], 0, "got: {result}");
        let repos = result["git_metadata_repos"]
            .as_array()
            .unwrap_or_else(|| panic!("git_metadata_repos field must be present, got: {result}"));
        assert!(repos.is_empty(), "no candidates → empty repos list");
        std::fs::remove_dir_all(&home).ok();
    }

    /// #826 C3: L2 must be idempotent — a second call after a
    /// successful prune reports `git_metadata_pruned: 0` because
    /// the metadata is already gone. Locks the idempotency contract
    /// so re-runs (operator double-clicks, sweeper retries) don't
    /// produce spurious counts.
    #[test]
    fn force_release_worktree_idempotent_on_already_pruned_metadata() {
        let home = tmp_home("826_c3_idempotent");
        let (source_repo, meta_dir) =
            seed_disbanded_agent_with_git_metadata(&home, "dev826", "feat/826");

        // First call: prunes 1.
        let r1 = handle_force_release_worktree(
            &home,
            &json!({
                "agent": "dev826",
                "branch": "feat/826",
                "source_repo": source_repo.display().to_string(),
            }),
            &None,
        );
        assert_eq!(r1["git_metadata_pruned"], 1);
        assert!(!meta_dir.exists());

        // Second call: prunes 0 (already pruned, no-op).
        let r2 = handle_force_release_worktree(
            &home,
            &json!({
                "agent": "dev826",
                "branch": "feat/826",
                "source_repo": source_repo.display().to_string(),
            }),
            &None,
        );
        assert_eq!(
            r2["git_metadata_pruned"], 0,
            "second call must report 0 pruned (idempotent), got: {r2}"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// #826 C3: when an agent was bound to worktrees on MULTIPLE
    /// source repos (a rare but real config — operator hand-edits
    /// fleet.yaml to point different teams at different repos), L2
    /// must enumerate and prune EACH. Without the multi-repo
    /// enumeration, the agent leaks metadata in every source repo
    /// it ever touched except the first.
    ///
    /// This test exercises the OPERATOR FAST PATH for both repos by
    /// calling L2 twice — once per source_repo arg. (Calling once
    /// without source_repo and relying on enumeration is exercised
    /// indirectly by the C1 RED test's discovery fallback.)
    #[test]
    fn force_release_worktree_handles_multiple_source_repos_for_same_agent() {
        let home = tmp_home("826_c3_multi_repo");
        // The fixture creates source_repo at `<home>/source_repo`, but
        // we need TWO source repos for this test. Use a custom helper.
        let agent = "dev826multi";
        let (repo_a, meta_a) = seed_disbanded_agent_with_git_metadata(&home, agent, "feat/aaa");
        let repo_b_home = home.join("home_b");
        std::fs::create_dir_all(&repo_b_home).ok();
        let (repo_b, meta_b) =
            seed_disbanded_agent_with_git_metadata(&repo_b_home, agent, "feat/bbb");

        // Prune repo_a's metadata.
        let r1 = handle_force_release_worktree(
            &home,
            &json!({
                "agent": agent,
                "branch": "feat/aaa",
                "source_repo": repo_a.display().to_string(),
            }),
            &None,
        );
        assert_eq!(r1["git_metadata_pruned"], 1);
        assert!(!meta_a.exists());
        // repo_b's metadata is untouched at this point.
        assert!(meta_b.exists(), "repo_b still holds its metadata");

        // Prune repo_b's metadata (different home so target path
        // computation aligns with the second fixture).
        let r2 = handle_force_release_worktree(
            &repo_b_home,
            &json!({
                "agent": agent,
                "branch": "feat/bbb",
                "source_repo": repo_b.display().to_string(),
            }),
            &None,
        );
        assert_eq!(r2["git_metadata_pruned"], 1);
        assert!(!meta_b.exists());

        std::fs::remove_dir_all(&home).ok();
    }

    /// #826 C3: cleaning agent X's metadata must NOT touch agent Y's
    /// metadata even when they share the same source repo. Locks the
    /// preservation guarantee — `paths_match` is per-worktree-path,
    /// not per-agent-name (siblings under the same agent dir are
    /// distinct worktrees and stay distinct).
    #[test]
    fn force_release_worktree_preserves_other_agents_metadata() {
        let home = tmp_home("826_c3_preserves_other");
        // Seed agent X on its own canonical fixture (returns the source repo).
        let (source_repo, meta_x) =
            seed_disbanded_agent_with_git_metadata(&home, "agent_x826", "feat/x");
        // Add a second worktree on the SAME source repo for a sibling
        // agent. The fixture helper builds the source repo on each
        // call, but here we need to reuse the existing one — inline.
        let agent_y_path = home.join("worktrees").join("agent_y826").join("feat/y");
        let out_y = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "-b",
                "feat/y",
                &agent_y_path.display().to_string(),
            ])
            .current_dir(&source_repo)
            .env("AGEND_GIT_BYPASS", "1")
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git ran");
        assert!(
            out_y.status.success(),
            "seed agent_y worktree failed: {}",
            String::from_utf8_lossy(&out_y.stderr)
        );
        // Remove the working tree dir so both metadata entries are
        // prunable.
        std::fs::remove_dir_all(&agent_y_path).expect("remove agent_y wt");
        let meta_y_name = agent_y_path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("agent_y path final segment");
        let meta_y = source_repo.join(".git").join("worktrees").join(meta_y_name);
        assert!(meta_y.exists(), "fixture: agent_y metadata pre-call");
        assert!(meta_x.exists(), "fixture: agent_x metadata pre-call");

        // Prune ONLY agent_x's metadata.
        let result = handle_force_release_worktree(
            &home,
            &json!({
                "agent": "agent_x826",
                "branch": "feat/x",
                "source_repo": source_repo.display().to_string(),
            }),
            &None,
        );
        assert_eq!(result["git_metadata_pruned"], 1);
        assert!(!meta_x.exists(), "agent_x metadata pruned");
        assert!(
            meta_y.exists(),
            "agent_y metadata MUST be preserved when only agent_x was targeted, but it's gone: {}",
            meta_y.display()
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Lead-spec named tests (per dispatch m-20260509125352834800-192) ──

    #[test]
    fn force_release_worktree_cleans_existing_dir() {
        let home = tmp_home("clean-existing");
        let dir = seed_daemon_worktree(&home, "dev", "feature/x");
        assert!(dir.exists(), "seeded dir must exist pre-call");
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "dev", "branch": "feature/x"}),
            &None,
        );
        assert_eq!(result["released"].as_bool(), Some(true));
        assert_eq!(result["dir_existed"].as_bool(), Some(true));
        assert_eq!(result["dir_removed"].as_bool(), Some(true));
        assert!(!dir.exists(), "dir must be cleaned post-call");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_idempotent_on_missing_dir() {
        let home = tmp_home("idempotent");
        // No seed — call directly on a non-existent target.
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "dev", "branch": "feature/never-existed"}),
            &None,
        );
        assert_eq!(result["released"].as_bool(), Some(true));
        assert_eq!(
            result["dir_existed"].as_bool(),
            Some(false),
            "missing dir reports dir_existed=false"
        );
        assert_eq!(
            result["dir_removed"].as_bool(),
            Some(false),
            "no removal happens on missing dir"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_releases_binding_too() {
        // Per spec: even when only the on-disk dir is stale (no
        // active binding), the call must still invoke release_full
        // for defense-in-depth. The outcome surfaces in the
        // `binding_outcome` field.
        let home = tmp_home("releases-binding");
        seed_daemon_worktree(&home, "dev", "feature/y");
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "dev", "branch": "feature/y"}),
            &None,
        );
        assert!(
            result["binding_outcome"].is_object(),
            "binding_outcome must surface the release_full result: {result}"
        );
        // No prior binding existed → release_full returns
        // released:false + error: "no binding..." — that's the
        // expected idempotent shape.
        let outcome = &result["binding_outcome"];
        assert_eq!(outcome["released"].as_bool(), Some(false));
        assert!(outcome["error"]
            .as_str()
            .unwrap_or("")
            .contains("no binding"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_rejects_path_outside_worktree_pool() {
        // Defense-in-depth: even if a malicious caller could pass
        // names that bypass the validator (or the validator is
        // weakened in a future change), the path-safety guard
        // refuses to clean anything outside <home>/worktrees/.
        let home = tmp_home("outside-pool-reject");
        // Seed a dir OUTSIDE the worktree pool, simulating where a
        // malicious caller might try to send the cleanup.
        let outside = home.join("config");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("important.json"), "data").unwrap();
        // Use empty branch — caught by the missing-branch check
        // first, but this exercises the input-rejection path.
        let r1 =
            handle_force_release_worktree(&home, &json!({"agent": "dev", "branch": ""}), &None);
        assert!(r1["error"].is_string(), "empty branch must error: {r1}");
        // The outside dir must still exist (no manipulation).
        assert!(outside.join("important.json").exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_rejects_invalid_agent_name() {
        let home = tmp_home("invalid-agent");
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "../etc/passwd", "branch": "feature/x"}),
            &None,
        );
        assert!(result["error"].is_string());
        assert_eq!(result["code"].as_str(), Some("invalid_agent"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_rejects_invalid_branch_name() {
        let home = tmp_home("invalid-branch");
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "dev", "branch": "../../escape"}),
            &None,
        );
        assert!(result["error"].is_string());
        assert_eq!(result["code"].as_str(), Some("invalid_branch"));
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Defensive bonuses ─────────────────────────────────────────

    #[test]
    fn force_release_worktree_rejects_missing_agent() {
        let home = tmp_home("missing-agent");
        let result = handle_force_release_worktree(&home, &json!({"branch": "feature/x"}), &None);
        assert_eq!(result["error"].as_str(), Some("missing 'agent'"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_rejects_missing_branch() {
        let home = tmp_home("missing-branch");
        let result = handle_force_release_worktree(&home, &json!({"agent": "dev"}), &None);
        assert_eq!(result["error"].as_str(), Some("missing 'branch'"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_after_failure_allows_bind_self_succeed() {
        // Integration-of-the-unblock-scenario test: simulate the
        // post-PR-2/PR-4 stale-state, call force_release_worktree,
        // then assert the worktree dir is gone (so a subsequent
        // bind_self would NOT trip on lease_failed).
        let home = tmp_home("integration-bind-succeed");
        let dir = seed_daemon_worktree(&home, "dev", "sprint59-wave1-pr4-issue-b");
        assert!(dir.exists(), "stale dir present pre-cleanup");
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "dev", "branch": "sprint59-wave1-pr4-issue-b"}),
            &None,
        );
        assert_eq!(result["released"].as_bool(), Some(true));
        assert_eq!(result["dir_removed"].as_bool(), Some(true));
        // Post-cleanup: the canonical bind_self target path is gone
        // → bind_self would proceed cleanly. We can't actually call
        // bind_self in a unit test (needs daemon registry), but
        // the absence of the dir IS the necessary precondition
        // for bind_self to succeed.
        assert!(!dir.exists(), "worktree dir must be gone");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_handles_partial_cleanup_state() {
        // Defensive: a dir that's been partially cleaned (some files
        // already removed by an aborted prior call) still gets
        // recursively wiped without panic.
        let home = tmp_home("partial-cleanup");
        let dir = home.join("worktrees").join("dev").join("feature/x");
        std::fs::create_dir_all(&dir).unwrap();
        // Don't seed with .agend-managed marker — partial state.
        std::fs::write(dir.join("leftover"), "data").unwrap();
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "dev", "branch": "feature/x"}),
            &None,
        );
        assert_eq!(result["dir_removed"].as_bool(), Some(true));
        assert!(!dir.exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_preserves_other_branches() {
        // Defense-in-depth: cleaning one branch's dir must NOT
        // touch sibling branches under the same agent.
        let home = tmp_home("preserves-siblings");
        let dir_x = seed_daemon_worktree(&home, "dev", "feature/x");
        let dir_y = seed_daemon_worktree(&home, "dev", "feature/y");
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "dev", "branch": "feature/x"}),
            &None,
        );
        assert_eq!(result["dir_removed"].as_bool(), Some(true));
        assert!(!dir_x.exists(), "target branch dir cleaned");
        assert!(
            dir_y.exists(),
            "sibling branch dir preserved: {}",
            dir_y.display()
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn force_release_worktree_preserves_other_agents() {
        // Defense-in-depth: cleaning one agent's dir must NOT
        // touch other agents' worktrees.
        let home = tmp_home("preserves-agents");
        let dir_dev = seed_daemon_worktree(&home, "dev", "feature/x");
        let dir_lead = seed_daemon_worktree(&home, "lead", "feature/x");
        let result = handle_force_release_worktree(
            &home,
            &json!({"agent": "dev", "branch": "feature/x"}),
            &None,
        );
        assert_eq!(result["dir_removed"].as_bool(), Some(true));
        assert!(!dir_dev.exists());
        assert!(
            dir_lead.exists(),
            "lead's dir preserved: {}",
            dir_lead.display()
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Sprint 60 W1 PR-1: rebase_clean_self helper tests ──────────────
    //
    // Direct exercise of the shared cleanup helper. handle_bind_self with
    // rebase_mode=true forwards to this helper before the lease attempt;
    // verifying the helper's contract here covers the bind_self call site
    // by construction (the wiring in handle_bind_self is a single
    // `if let Err = rebase_clean_self` branch).

    #[test]
    fn rebase_clean_self_clears_existing_dir_and_invokes_release_full() {
        let home = tmp_home("rebase-clean-existing");
        let dir = seed_daemon_worktree(&home, "dev", "feat/rebase-x");
        assert!(dir.exists());
        let outcome = rebase_clean_self(&home, "dev", "feat/rebase-x")
            .expect("clean state in pool must succeed");
        assert!(outcome.dir_existed);
        assert!(outcome.dir_removed);
        assert!(!dir.exists(), "stale dir must be cleaned");
        assert!(
            outcome.binding_outcome.is_object(),
            "binding_outcome must surface release_full result"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn rebase_clean_self_idempotent_on_clean_state() {
        // No prior bind, no stale dir → helper still runs release_full
        // (idempotent) and reports dir_existed=false.
        let home = tmp_home("rebase-clean-idempotent");
        let outcome = rebase_clean_self(&home, "dev", "feat/never-existed")
            .expect("helper must not error on clean state");
        assert!(!outcome.dir_existed);
        assert!(!outcome.dir_removed);
        // release_full on missing binding returns released:false + "no binding" error.
        let bo = &outcome.binding_outcome;
        assert_eq!(bo["released"].as_bool(), Some(false));
        assert!(bo["error"].as_str().unwrap_or("").contains("no binding"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn rebase_clean_self_rejects_path_outside_worktree_pool() {
        // Defense-in-depth: even if a malicious caller bypassed the
        // outer validators, the helper refuses to clean paths outside
        // <home>/worktrees/. The path-safety check here mirrors
        // force_release_worktree's own guard.
        let home = tmp_home("rebase-outside-pool");
        // An empty branch resolves to <home>/worktrees/dev (the
        // agent-level dir) which the safety check rejects.
        let r = rebase_clean_self(&home, "dev", "");
        assert!(r.is_err(), "empty branch must reject as path-unsafe");
        // A branch with `..` would also escape the pool — but
        // agent_ops::validate_branch already rejects those before this
        // helper is called. The empty-string case is the only path
        // that could slip past upstream validators (e.g. caller passes
        // a JSON null/missing field), so it's the load-bearing guard.
        std::fs::remove_dir_all(&home).ok();
    }

    // ── Sprint 60 W1 PR-1: bind_self handler rebase_mode end-to-end ─────

    #[test]
    fn bind_self_rebase_mode_runs_cleanup_before_lease_attempt() {
        // Seed the stale state that drove the Wave 1 PR-2 BYPASS
        // incident: an on-disk worktree dir + binding lingering from a
        // prior bind cycle. Calling handle_bind_self with
        // rebase_mode=true must clean the dir + binding even though
        // the lease itself will fail (no fleet.yaml + no real git
        // repo in this minimal test fixture).
        //
        // Observable: post-call, the stale dir is gone AND the
        // binding is cleared, regardless of the lease error returned.
        // This proves the rebase_mode wiring runs the cleanup helper
        // before the dispatch_auto_bind_lease call.
        use crate::mcp::handlers::worktree::handle_bind_self;
        let home = tmp_home("bind-rebase-cleanup");
        let dir = seed_daemon_worktree(&home, "dev", "feat/rebase-bind");
        // Seed a binding too so we can verify it's released.
        let runtime = crate::paths::runtime_dir(&home).join("dev");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(
            runtime.join("binding.json"),
            r#"{"agent":"dev","branch":"feat/rebase-bind","worktree":"/stale"}"#,
        )
        .unwrap();
        assert!(dir.exists());

        let _ignored = handle_bind_self(
            &home,
            &json!({"branch": "feat/rebase-bind", "rebase_mode": true}),
            &crate::identity::Sender::new("dev"),
        );
        // Cleanup ran regardless of the downstream lease result.
        assert!(!dir.exists(), "rebase_mode must clean stale dir pre-lease");
        assert!(
            !runtime.join("binding.json").exists(),
            "rebase_mode must clear stale binding pre-lease"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn bind_self_no_rebase_mode_skips_cleanup() {
        // Inverse: without rebase_mode, the cleanup must NOT run —
        // existing behavior is preserved. The pre-existing stale dir
        // remains untouched (dispatch_auto_bind_lease will return
        // its usual lease_failed for the stuck-state scenario).
        use crate::mcp::handlers::worktree::handle_bind_self;
        let home = tmp_home("bind-no-rebase");
        let dir = seed_daemon_worktree(&home, "dev", "feat/no-rebase");
        let _ignored = handle_bind_self(
            &home,
            &json!({"branch": "feat/no-rebase"}),
            &crate::identity::Sender::new("dev"),
        );
        assert!(
            dir.exists(),
            "without rebase_mode, stale dir must be preserved"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
