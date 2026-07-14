//! Exact-owner metadata cleanup for `force_release_worktree`. Extracted from
//! `mod.rs` to keep the parent file under the `tests/file_size_invariant.rs`
//! 750-LOC ceiling.
//!
//! When `force_release_worktree` runs after `full_delete_instance`
//! (PR #834 / #828 cascade) the agent's daemon binding is already
//! gone. `worktree_pool::release_full` then early-returns on "no
//! binding" before its own `git worktree remove --force` step. The production
//! path now prunes only the exact proven owner inside the S2 transaction; the
//! old agent-wide discovery helper below is test-only compatibility coverage.

#[cfg(test)]
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

/// #826 L2 GC outcome: count + list of source repos where the
/// `git worktree remove --force` (and `git worktree prune` fallback)
/// step actually pruned a metadata entry for the target agent's
/// worktree path.
#[derive(Debug, Default)]
pub(crate) struct GcOutcome {
    pub(crate) pruned_count: usize,
    pub(crate) repos_touched: Vec<String>,
}

/// Remove metadata for one already-proven `(source_repo, target)` pair.
/// Unlike the legacy agent-wide discovery helper below, this function never
/// enumerates candidate repositories and is called while S2 holds
/// `L(repo,branch) -> A -> B`.
pub(crate) fn prune_exact_git_metadata(
    source_repo: &Path,
    target: &Path,
    agent: &str,
    branch: &str,
) -> GcOutcome {
    let mut outcome = GcOutcome::default();
    for entry in list_worktrees_bypass_shim(source_repo) {
        if !paths_match(&entry.path, target) {
            continue;
        }
        let removed = crate::git_helpers::git_bypass(
            source_repo,
            &["worktree", "remove", "--force", &entry.path],
        );
        let pruned = match removed {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                tracing::warn!(
                    agent = %agent,
                    branch = %branch,
                    repo = %source_repo.display(),
                    stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                    "S2 exact metadata remove failed; falling back to exact metadata directory removal"
                );
                remove_exact_metadata_dir(source_repo, target)
            }
            Err(e) => {
                tracing::warn!(
                    agent = %agent,
                    branch = %branch,
                    repo = %source_repo.display(),
                    error = %e,
                    "S2 exact metadata remove spawn failed"
                );
                false
            }
        };
        if pruned {
            outcome.pruned_count = 1;
            outcome
                .repos_touched
                .push(source_repo.display().to_string());
        }
        break;
    }
    outcome
}

fn remove_exact_metadata_dir(source_repo: &Path, target: &Path) -> bool {
    let metadata_root = source_repo.join(".git").join("worktrees");
    let Ok(entries) = std::fs::read_dir(metadata_root) else {
        return false;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(worktree_path) = std::fs::read_to_string(dir.join("worktree")) else {
            continue;
        };
        if paths_match(worktree_path.trim(), target) {
            return std::fs::remove_dir_all(dir).is_ok();
        }
    }
    false
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
/// --porcelain` (via the local `list_worktrees_bypass_shim`) to find
/// entries whose path matches the target. For each match, run
/// `git worktree remove --force <path>` against the source repo's
/// cwd. AGEND_GIT_BYPASS=1 is set on every git invocation to bypass
/// the daemon shim per the operator-confirmed manual recovery command.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn prune_git_metadata_for_agent(
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
    let mut seen = HashSet::new();
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
            // #1899: bounded via git_bypass (LOCAL 60s).
            let removed = crate::git_helpers::git_bypass(
                &repo,
                &["worktree", "remove", "--force", &entry.path],
            );
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
                    // #1899: bounded via git_bypass (LOCAL 60s).
                    let prune = crate::git_helpers::git_bypass(&repo, &["worktree", "prune"]);
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

/// #826 L2 fork of `crate::worktree_cleanup::list_worktrees` — #2550 W2
/// converged both onto the shared `git_worktree::list_porcelain` core
/// (always bypass-enabled via `git_helpers::git_bypass`; mirrors the
/// `release_full` precedent at src/worktree_pool.rs:311), keeping this
/// site's own main/master exclusion + `WorktreeEntry` conversion as a thin
/// caller-specific layer on top (unchanged from the pre-#2550 fork).
fn list_worktrees_bypass_shim(repo_root: &Path) -> Vec<crate::worktree_cleanup::WorktreeEntry> {
    crate::git_worktree::list_porcelain(repo_root)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(path, branch)| {
            let branch = branch?;
            if branch == "main" || branch == "master" {
                return None;
            }
            Some(crate::worktree_cleanup::WorktreeEntry {
                path: path.display().to_string(),
                branch,
            })
        })
        .collect()
}

/// #826 L2: best-effort source-repo enumeration when the operator
/// didn't supply a `source_repo` arg. Two sources, deduped by caller:
/// 1. Sibling daemon-managed worktrees' `.git` pointer files
///    (`gitdir: <source>/.git/worktrees/<name>`).
/// 2. `crate::teams::list_all` team `source_repo` fields.
#[cfg(test)]
#[allow(dead_code)]
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
#[cfg(test)]
#[allow(dead_code)]
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    // #2548 PR-2: `force_release_worktree` folded into
    // `release_worktree(force:true)` — these tests now drive the merged
    // handler directly (every call below adds `"force": true`).
    use crate::mcp::handlers::worktree::handle_release_worktree;
    use serde_json::json;
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp_home(suffix: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let h = std::env::temp_dir().join(format!(
            "agend-force-release-gc-{}-{}-{}",
            std::process::id(),
            suffix,
            id,
        ));
        std::fs::create_dir_all(&h).ok();
        h
    }

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
    #[test]
    fn force_release_denies_non_owner_non_orchestrator_audit2_002() {
        let home = tmp_home("audit2_002_acl");
        let (source_repo, _) = seed_disbanded_agent_with_git_metadata(&home, "victim", "feat/x");
        let repo = source_repo.display().to_string();

        // A peer that is neither the owner nor its orchestrator is denied.
        let attacker = crate::identity::Sender::new("attacker");
        let denied = handle_release_worktree(
            &home,
            &json!({"instance": "victim", "branch": "feat/x", "repository_path": repo, "force": true}),
            &attacker,
        );
        assert_eq!(
            denied["code"], "not_owner_or_orchestrator",
            "non-owner must be denied: {denied}"
        );

        // The owner itself is allowed (no ACL error).
        let owner = crate::identity::Sender::new("victim");
        let ok = handle_release_worktree(
            &home,
            &json!({"instance": "victim", "branch": "feat/x", "repository_path": repo, "force": true}),
            &owner,
        );
        assert_ne!(
            ok["code"], "not_owner_or_orchestrator",
            "owner must be allowed: {ok}"
        );
    }

    /// - response carries `git_metadata_repos` array of length 1
    /// - the source repo's `.git/worktrees/<agent>/` dir is gone
    #[test]
    fn force_release_worktree_prunes_stale_git_metadata_when_no_binding() {
        let home = tmp_home("826_l2_prune");
        let (source_repo, agent_meta_dir) =
            seed_disbanded_agent_with_git_metadata(&home, "dev826", "feat/826");

        let result = handle_release_worktree(
            &home,
            &json!({
                "instance": "dev826",
                "branch": "feat/826",
                "repository_path": source_repo.display().to_string(),
                "force": true,
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
        let source = home.join("source-repo");
        std::fs::create_dir_all(&source).unwrap();
        let result = handle_release_worktree(
            &home,
            &json!({"instance": "dev826c3", "branch": "feat/826", "repository_path": source, "force": true}),
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
        let r1 = handle_release_worktree(
            &home,
            &json!({
                "instance": "dev826",
                "branch": "feat/826",
                "repository_path": source_repo.display().to_string(),
                "force": true,
            }),
            &None,
        );
        assert_eq!(r1["git_metadata_pruned"], 1);
        assert!(!meta_dir.exists());

        // Second call: prunes 0 (already pruned, no-op).
        let r2 = handle_release_worktree(
            &home,
            &json!({
                "instance": "dev826",
                "branch": "feat/826",
                "repository_path": source_repo.display().to_string(),
                "force": true,
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
        let r1 = handle_release_worktree(
            &home,
            &json!({
                "instance": agent,
                "branch": "feat/aaa",
                "repository_path": repo_a.display().to_string(),
                "force": true,
            }),
            &None,
        );
        assert_eq!(r1["git_metadata_pruned"], 1);
        assert!(!meta_a.exists());
        // repo_b's metadata is untouched at this point.
        assert!(meta_b.exists(), "repo_b still holds its metadata");

        // Prune repo_b's metadata (different home so target path
        // computation aligns with the second fixture).
        let r2 = handle_release_worktree(
            &repo_b_home,
            &json!({
                "instance": agent,
                "branch": "feat/bbb",
                "repository_path": repo_b.display().to_string(),
                "force": true,
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
        let result = handle_release_worktree(
            &home,
            &json!({
                "instance": "agent_x826",
                "branch": "feat/x",
                "repository_path": source_repo.display().to_string(),
                "force": true,
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
}
