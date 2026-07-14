//! Test-only compatibility coverage for the historical agent-wide metadata GC.
//!
//! Production force release uses the exact-owner path in [`super::gc`]. These
//! helpers remain available to legacy tests that exercise source-repository
//! discovery after a disbanded worktree.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// #826 L2: enumerate source repos that may still hold
/// `.git/worktrees/<meta-dir>/` metadata pointing at the daemon-managed
/// worktree path `<home>/worktrees/<agent>/<branch>/`, and prune each exact
/// matching entry. This is retained for test-only compatibility coverage.
#[allow(dead_code)]
pub(crate) fn prune_git_metadata_for_agent(
    home: &Path,
    agent: &str,
    branch: &str,
    source_repo_hint: Option<&Path>,
) -> super::gc::GcOutcome {
    let target_path = home.join("worktrees").join(agent).join(branch);
    let candidates: Vec<PathBuf> = match source_repo_hint {
        Some(p) => vec![p.to_path_buf()],
        None => discover_source_repo_candidates(home),
    };

    let mut outcome = super::gc::GcOutcome::default();
    let mut seen = HashSet::new();
    for repo in candidates {
        if !seen.insert(repo.clone()) || !repo.exists() {
            continue;
        }
        let entries = list_worktrees_bypass_shim(&repo);
        for entry in entries {
            if !super::gc::paths_match(&entry.path, &target_path) {
                continue;
            }
            outcome.matched = true;
            let removed = crate::git_helpers::git_bypass(
                &repo,
                &["worktree", "remove", "--force", &entry.path],
            );
            let pruned = match removed {
                Ok(o) if o.status.success() => true,
                Ok(o) => {
                    tracing::warn!(
                        agent = %agent,
                        branch = %branch,
                        repo = %repo.display(),
                        stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                        "#826 L2: git worktree remove failed; falling back to prune"
                    );
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

fn discover_source_repo_candidates(home: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let worktrees_root = home.join("worktrees");
    if let Ok(agents) = std::fs::read_dir(&worktrees_root) {
        for agent_entry in agents.flatten() {
            let agent_dir = agent_entry.path();
            if agent_dir.is_dir() {
                collect_source_repos_from_worktree_tree(&agent_dir, &mut out);
            }
        }
    }
    for team in crate::teams::list_all(home) {
        if let Some(repo) = team.source_repo {
            out.push(repo);
        }
    }
    out
}

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
            .find_map(|line| line.strip_prefix("gitdir:").map(str::trim))
        else {
            continue;
        };
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
