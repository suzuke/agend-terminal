//! S2 guarded force/rebase release authority.
//!
//! A force cleanup is still a release transaction.  The only extra authority
//! is the typed `ManagedTargetIdentity` proof used for an absent binding.

use crate::binding::GuardedBinding;
use crate::worktree_pool::ReleaseOutcome;
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct ManagedTargetIdentity {
    pub(crate) worktree: PathBuf,
    pub(crate) source_repo: PathBuf,
}

#[derive(Debug)]
pub(crate) struct ForceReleaseResult {
    pub(crate) outcome: ReleaseOutcome,
    pub(crate) dir_existed: bool,
    pub(crate) dir_removed: bool,
}

/// Resolve and execute the force/rebase transaction.  The branch lease is
/// acquired before the agent and binding locks; both the binding fingerprint
/// and the managed-target proof are revalidated by the callee immediately
/// before removal.
pub(crate) fn force_release(
    home: &Path,
    agent: &str,
    branch: &str,
    explicit_repo: Option<&Path>,
    sender: Option<&str>,
) -> Result<ForceReleaseResult, String> {
    let state = crate::binding::preflight_guarded_binding(home, agent);
    let (identity, expected) = match state {
        GuardedBinding::Opaque(reason) => {
            return Err(format!(
                "force release refused: opaque binding state ({reason}); binding evidence preserved"
            ))
        }
        GuardedBinding::Known { value, fingerprint } => {
            let bound_branch = value["branch"].as_str().unwrap_or("");
            if bound_branch != branch {
                return Err(format!(
                    "force release refused: binding branch '{bound_branch}' does not match requested '{branch}'; binding preserved"
                ));
            }
            let identity = resolve_known_identity(home, agent, branch, &value, explicit_repo)?;
            (Some(identity), Some(fingerprint))
        }
        GuardedBinding::Absent => {
            let target = crate::worktree::worktree_path(home, agent, branch);
            // A missing target is already at the desired state.  This is also
            // the sanctioned GC-metadata path; no destructive identity proof is
            // needed because there is no directory to remove.
            if !target.exists() {
                return Ok(ForceReleaseResult {
                    outcome: ReleaseOutcome {
                        released: true,
                        already_released: true,
                        ..ReleaseOutcome::default()
                    },
                    dir_existed: false,
                    dir_removed: false,
                });
            }
            let identity = resolve_absent_identity(home, agent, branch, &target, explicit_repo)?;
            (Some(identity), None)
        }
    };

    let identity = identity.expect("identity exists for a present target");
    let _branch_lock = crate::binding::acquire_branch_lease_lock(
        home,
        &identity.source_repo.display().to_string(),
        branch,
    )
    .map_err(|e| format!("force release branch lease lock failed: {e}"))?;

    let dir_existed = identity.worktree.exists();
    let outcome = match expected.as_ref() {
        Some(expected) => {
            crate::worktree_pool::release_bound_target_exact_under_branch_lock_for_force(
                home,
                agent,
                expected,
                &identity.worktree,
                &identity.source_repo,
                sender,
            )
        }
        None => crate::worktree_pool::release_absent_target_under_branch_lock(
            home,
            agent,
            &identity.worktree,
            &identity.source_repo,
            sender,
        ),
    };
    let dir_removed = outcome.worktree_removed
        || (dir_existed && !identity.worktree.exists() && outcome.error.is_none());
    Ok(ForceReleaseResult {
        outcome,
        dir_existed,
        dir_removed,
    })
}

fn resolve_known_identity(
    home: &Path,
    agent: &str,
    branch: &str,
    binding: &Value,
    explicit_repo: Option<&Path>,
) -> Result<ManagedTargetIdentity, String> {
    let worktree = binding["worktree"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "force release refused: known binding has no worktree".to_string())?;
    let binding_repo = binding["source_repo"].as_str().filter(|s| !s.is_empty());
    resolve_identity(
        home,
        agent,
        branch,
        &worktree,
        binding_repo.map(Path::new),
        explicit_repo,
        worktree.exists(),
    )
}

fn resolve_absent_identity(
    home: &Path,
    agent: &str,
    branch: &str,
    worktree: &Path,
    explicit_repo: Option<&Path>,
) -> Result<ManagedTargetIdentity, String> {
    resolve_identity(home, agent, branch, worktree, None, explicit_repo, true)
}

fn resolve_identity(
    _home: &Path,
    agent: &str,
    branch: &str,
    worktree: &Path,
    binding_repo: Option<&Path>,
    explicit_repo: Option<&Path>,
    require_marker: bool,
) -> Result<ManagedTargetIdentity, String> {
    if require_marker {
        if !crate::worktree_pool::is_daemon_managed(worktree) {
            return Err(format!(
                "force release refused: target {} lacks .agend-managed marker; use the GC archive or operator channel for orphan recovery",
                worktree.display()
            ));
        }
        if crate::binding::managed_marker_agent(worktree).as_deref() != Some(agent) {
            return Err(format!(
                "force release refused: .agend-managed marker agent does not exactly equal target '{agent}'; use the GC archive or operator channel"
            ));
        }
        if marker_field(worktree, "branch").as_deref() != Some(branch) {
            return Err(format!(
                "force release refused: .agend-managed marker branch does not equal target '{branch}'; use the GC archive or operator channel"
            ));
        }
    }

    let binding_repo = binding_repo.map(canonical_repo).transpose()?;
    let marker_repo = marker_field(worktree, "source_repo")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .map(|p| canonical_repo(&p))
        .transpose()?;
    let git_repo = worktree_owner_from_git(worktree)?;
    let explicit_repo = explicit_repo.map(canonical_repo).transpose()?;

    let mut owner: Option<PathBuf> = None;
    for candidate in [binding_repo, marker_repo, git_repo, explicit_repo.clone()]
        .into_iter()
        .flatten()
    {
        if let Some(existing) = &owner {
            if existing != &candidate {
                return Err(format!(
                    "force release refused: ambiguous owning repositories '{}' and '{}'",
                    existing.display(),
                    candidate.display()
                ));
            }
        } else {
            owner = Some(candidate);
        }
    }
    let source_repo = owner.ok_or_else(|| {
        "force release refused: no proven owning repository for managed target; use the GC archive or operator channel".to_string()
    })?;

    Ok(ManagedTargetIdentity {
        worktree: worktree.to_path_buf(),
        source_repo,
    })
}

fn canonical_repo(path: &Path) -> Result<PathBuf, String> {
    if !path.exists() {
        return Err(format!(
            "force release refused: owning repository '{}' is unreadable",
            path.display()
        ));
    }
    path.canonicalize().map_err(|e| {
        format!(
            "force release refused: canonicalize '{}': {e}",
            path.display()
        )
    })
}

fn marker_field(worktree: &Path, field: &str) -> Option<String> {
    std::fs::read_to_string(worktree.join(crate::worktree_pool::MANAGED_MARKER))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{field}=")))
        .map(|s| s.trim().to_string())
}

/// Derive the owning repository from a real git worktree's `.git` pointer.
/// A normal directory with a `.git` directory is intentionally not accepted as
/// proof of a worktree owner: it is not an unambiguous managed target.
fn worktree_owner_from_git(worktree: &Path) -> Result<Option<PathBuf>, String> {
    let git_file = worktree.join(".git");
    if !git_file.is_file() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&git_file)
        .map_err(|e| format!("force release refused: unreadable .git pointer: {e}"))?;
    let gitdir = content
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:").map(str::trim))
        .ok_or_else(|| "force release refused: malformed .git pointer".to_string())?;
    let gitdir = PathBuf::from(gitdir);
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        worktree.join(gitdir)
    };
    let canonical = gitdir
        .canonicalize()
        .map_err(|e| format!("force release refused: unreadable git worktree owner: {e}"))?;
    let worktrees_dir = canonical
        .parent()
        .ok_or_else(|| "force release refused: malformed git worktree owner".to_string())?;
    if worktrees_dir.file_name().and_then(|n| n.to_str()) != Some("worktrees") {
        return Err(
            "force release refused: .git pointer is not owned by a canonical repository"
                .to_string(),
        );
    }
    let dot_git = worktrees_dir
        .parent()
        .ok_or_else(|| "force release refused: malformed git worktree owner".to_string())?;
    let source = dot_git
        .parent()
        .ok_or_else(|| "force release refused: malformed git worktree owner".to_string())?;
    Ok(Some(source.to_path_buf()))
}
