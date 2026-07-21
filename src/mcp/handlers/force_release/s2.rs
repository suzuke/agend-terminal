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
    pub(crate) git_metadata_pruned: usize,
    pub(crate) git_metadata_repos: Vec<String>,
}

/// Disk-fresh target classification. `Path::exists()` is deliberately not
/// used by destructive callers: permission errors, dangling symlinks, and
/// non-directory entries must remain opaque rather than becoming Absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetState {
    Absent,
    Present,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RebaseTestPhase {
    BeforeRepair,
    BeforeMarkerCommit,
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
pub(crate) mod rebase_test_seam {
    use super::RebaseTestPhase;
    use std::cell::RefCell;
    use std::sync::Arc;

    type Hook = Arc<dyn Fn(RebaseTestPhase) -> Option<String> + Send + Sync>;

    thread_local! {
        static LOCAL_HOOK: RefCell<Option<Hook>> = RefCell::new(None);
    }

    pub(crate) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            LOCAL_HOOK.with(|slot| *slot.borrow_mut() = None);
        }
    }

    pub(crate) fn install(
        callback: impl Fn(RebaseTestPhase) -> Option<String> + Send + Sync + 'static,
    ) -> Guard {
        LOCAL_HOOK.with(|slot| *slot.borrow_mut() = Some(Arc::new(callback)));
        Guard
    }

    pub(crate) fn hit(phase: RebaseTestPhase) -> Option<String> {
        LOCAL_HOOK.with(|slot| slot.borrow().as_ref().and_then(|callback| callback(phase)))
    }
}

pub(crate) fn classify_target(path: &Path) -> Result<TargetState, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(TargetState::Present),
        Ok(_) => Err(format!(
            "force release refused: opaque target metadata at {}",
            path.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TargetState::Absent),
        Err(e) => Err(format!(
            "force release refused: opaque target metadata at {}: {e}",
            path.display()
        )),
    }
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
    let permit = crate::mcp::handlers::dispatch_hook::LifecyclePermit::acquire(
        home,
        agent,
        crate::mcp::handlers::dispatch_hook::LifecycleOperation::Release,
    )
    .map_err(|error| format!("force release refused: {error}"))?;
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
            let worktree = value["worktree"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .ok_or_else(|| {
                    "force release refused: known binding has no worktree".to_string()
                })?;
            let target_state = classify_target(&worktree)?;
            let identity = resolve_known_identity(
                home,
                agent,
                branch,
                &value,
                explicit_repo,
                matches!(target_state, TargetState::Present),
            )?;
            (Some(identity), Some(fingerprint))
        }
        GuardedBinding::Absent => {
            let target = unbound(home, agent, branch, explicit_repo)?;
            let target_state = classify_target(&target)?;
            let identity = match target_state {
                TargetState::Present => {
                    resolve_absent_identity(home, agent, branch, &target, explicit_repo, true)?
                }
                TargetState::Absent => {
                    let Some(explicit_repo) = explicit_repo else {
                        return Err(
                            "force release refused: absent target has no explicit or proven owning repository; use the GC archive or operator channel".to_string(),
                        );
                    };
                    resolve_absent_identity(
                        home,
                        agent,
                        branch,
                        &target,
                        Some(explicit_repo),
                        false,
                    )?
                }
            };
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

    let dir_existed = matches!(classify_target(&identity.worktree)?, TargetState::Present);
    let outcome = match expected.as_ref() {
        Some(expected) => {
            crate::worktree_pool::release_bound_target_exact_under_branch_lock_for_force(
                home,
                agent,
                expected,
                &identity.worktree,
                &identity.source_repo,
                sender,
                &permit,
            )
        }
        None => crate::worktree_pool::release_absent_target_under_branch_lock(
            home,
            agent,
            branch,
            &identity.worktree,
            &identity.source_repo,
            sender,
            &permit,
            None,
        ),
    };
    let dir_removed = outcome.worktree_removed
        || (dir_existed
            && matches!(classify_target(&identity.worktree), Ok(TargetState::Absent))
            && outcome.error.is_none());
    let git_metadata_pruned = outcome.git_metadata_pruned;
    let git_metadata_repos = outcome.git_metadata_repos.clone();
    Ok(ForceReleaseResult {
        outcome,
        dir_existed,
        dir_removed,
        git_metadata_pruned,
        git_metadata_repos,
    })
}

fn resolve_known_identity(
    home: &Path,
    agent: &str,
    branch: &str,
    binding: &Value,
    explicit_repo: Option<&Path>,
    require_marker: bool,
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
        require_marker,
    )
}

fn resolve_absent_identity(
    home: &Path,
    agent: &str,
    branch: &str,
    worktree: &Path,
    explicit_repo: Option<&Path>,
    require_marker: bool,
) -> Result<ManagedTargetIdentity, String> {
    resolve_identity(
        home,
        agent,
        branch,
        worktree,
        None,
        explicit_repo,
        require_marker,
    )
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

fn unbound(home: &Path, agent: &str, branch: &str, repo: Option<&Path>) -> Result<PathBuf, String> {
    let legacy = crate::worktree::worktree_path(home, agent, branch);
    if matches!(classify_target(&legacy)?, TargetState::Present) {
        return Ok(legacy);
    }
    let Some(repo) = repo else {
        return Ok(legacy);
    };
    let repo = canonical_repo(repo)?;
    let entries = match std::fs::read_dir(home.join("worktrees")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(legacy),
        Err(error) => return Err(format!("force release refused: unreadable pool: {error}")),
    };
    let prefix = format!("{agent}-");
    let mut found = None;
    for entry in entries {
        let entry = entry.map_err(|e| format!("force release refused: unreadable entry: {e}"))?;
        let path = entry.path();
        if !entry.file_name().to_string_lossy().starts_with(&prefix)
            || marker_field(&path, "agent").as_deref() != Some(agent)
            || marker_field(&path, "branch").as_deref() != Some(branch)
        {
            continue;
        }
        let marker_repo = marker_field(&path, "source_repo").ok_or("target missing source_repo")?;
        if canonical_repo(Path::new(&marker_repo))? != repo {
            continue;
        }
        if found.replace(path).is_some() {
            return Err("force release refused: ambiguous exact managed targets".to_string());
        }
    }
    Ok(found.unwrap_or(legacy))
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

/// Guarded repair used by the live `bind_self(rebase_mode=true)` entry point.
/// Every metadata catch-up or `git switch` is performed only after the same
/// disk-fresh Known/Absent read, canonical `L(repo,branch)` lease, A/B locks,
/// fingerprint CAS, and marker/owner revalidation as force release.
pub(crate) fn rebase_repair(
    home: &Path,
    agent: &str,
    branch: &str,
    explicit_repo: Option<&Path>,
    sender: Option<&str>,
    permit: &crate::mcp::handlers::dispatch_hook::LifecyclePermit,
) -> Result<super::repair::RepairResult, super::repair::RepairBlocked> {
    use super::repair::{RebaseContinuation, RepairAction, RepairBlocked, RepairResult};

    #[cfg(test)]
    if let Some(reason) = rebase_test_seam::hit(RebaseTestPhase::BeforeRepair) {
        return Err(RepairBlocked::PathUnsafe(reason));
    }

    let state = crate::binding::preflight_guarded_binding(home, agent);
    match state {
        GuardedBinding::Opaque(reason) => Err(RepairBlocked::Opaque(reason)),
        GuardedBinding::Absent => {
            let target = crate::worktree::worktree_path(home, agent, branch);
            let target_state = classify_target(&target).map_err(RepairBlocked::TargetOpaque)?;
            if matches!(target_state, TargetState::Absent) && explicit_repo.is_none() {
                // No destructive mutation is needed for a genuinely fresh
                // bind. The subsequent dispatch owns branch provisioning.
                return Ok(RepairResult::no_continuation(RepairAction::NoOp));
            }
            let identity = if matches!(target_state, TargetState::Present) {
                resolve_absent_identity(home, agent, branch, &target, explicit_repo, true)
            } else {
                let repo = explicit_repo.ok_or_else(|| {
                    "absent target has no explicit or proven owning repository".to_string()
                });
                repo.and_then(|repo| {
                    resolve_absent_identity(home, agent, branch, &target, Some(repo), false)
                })
            }
            .map_err(|e| RepairBlocked::PathUnsafe(e.to_string()))?;
            let _lease = crate::binding::acquire_branch_lease_lock(
                home,
                &identity.source_repo.display().to_string(),
                branch,
            )
            .map_err(|e| RepairBlocked::PathUnsafe(e.to_string()))?;
            let outcome = crate::worktree_pool::release_absent_target_under_branch_lock(
                home,
                agent,
                branch,
                &identity.worktree,
                &identity.source_repo,
                sender,
                permit,
                None,
            );
            if let Some(error) = outcome.error {
                return Err(RepairBlocked::PathUnsafe(error));
            }
            if matches!(target_state, TargetState::Present) {
                Ok(RepairResult::no_continuation(
                    RepairAction::StaleStateCleared,
                ))
            } else {
                Ok(RepairResult::no_continuation(RepairAction::NoOp))
            }
        }
        GuardedBinding::Known { value, fingerprint } => {
            let recorded_branch = value["branch"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| RepairBlocked::Opaque("known binding has no branch".to_string()))?;
            let worktree = value["worktree"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .ok_or_else(|| {
                    RepairBlocked::Opaque("known binding has no worktree".to_string())
                })?;
            let target_state = classify_target(&worktree).map_err(RepairBlocked::TargetOpaque)?;
            let binding_repo = value["source_repo"].as_str().filter(|s| !s.is_empty());
            if matches!(target_state, TargetState::Absent) {
                if recorded_branch != branch {
                    if let Some(blocked) = super::repair::reject_if_branch_has_dependents(
                        home,
                        agent,
                        recorded_branch,
                        branch,
                    ) {
                        return Err(blocked);
                    }
                    return Err(RepairBlocked::BindingChanged);
                }
                let identity = resolve_identity(
                    home,
                    agent,
                    recorded_branch,
                    &worktree,
                    binding_repo.map(Path::new),
                    explicit_repo,
                    false,
                )
                .map_err(|e| RepairBlocked::PathUnsafe(e.to_string()))?;
                let _lease = crate::binding::acquire_branch_lease_lock(
                    home,
                    &identity.source_repo.display().to_string(),
                    branch,
                )
                .map_err(|e| RepairBlocked::PathUnsafe(e.to_string()))?;
                let outcome =
                    crate::worktree_pool::release_bound_target_exact_under_branch_lock_for_force(
                        home,
                        agent,
                        &fingerprint,
                        &worktree,
                        &identity.source_repo,
                        sender,
                        permit,
                    );
                if let Some(error) = outcome.error {
                    return Err(RepairBlocked::PathUnsafe(error));
                }
                return Ok(RepairResult::no_continuation(
                    RepairAction::StaleStateCleared,
                ));
            }

            let actual_before = current_branch(&worktree)?;
            let identity = resolve_identity(
                home,
                agent,
                &actual_before,
                &worktree,
                binding_repo.map(Path::new),
                explicit_repo,
                true,
            )
            .map_err(|e| RepairBlocked::PathUnsafe(e.to_string()))?;
            let _lease = crate::binding::acquire_branch_lease_lock(
                home,
                &identity.source_repo.display().to_string(),
                branch,
            )
            .map_err(|e| RepairBlocked::PathUnsafe(e.to_string()))?;
            let _agent_lock = crate::binding::acquire_agent_mutation_lock(home, agent)
                .map_err(RepairBlocked::PathUnsafe)?;
            let _binding_lock = crate::binding::acquire_binding_file_lock(home, agent)
                .map_err(RepairBlocked::PathUnsafe)?;
            let current = match crate::binding::guarded_binding_disk_fresh(home, agent) {
                GuardedBinding::Known {
                    value,
                    fingerprint: live,
                } if live == fingerprint
                    && value["worktree"].as_str() == Some(worktree.to_string_lossy().as_ref()) =>
                {
                    value
                }
                GuardedBinding::Opaque(reason) => return Err(RepairBlocked::Opaque(reason)),
                GuardedBinding::Absent | GuardedBinding::Known { .. } => {
                    return Err(RepairBlocked::BindingChanged)
                }
            };
            let actual = current_branch(&worktree)?;
            if actual != actual_before {
                return Err(RepairBlocked::BindingChanged);
            }
            resolve_identity(
                home,
                agent,
                &actual,
                &worktree,
                current["source_repo"].as_str().map(Path::new),
                Some(&identity.source_repo),
                true,
            )
            .map_err(RepairBlocked::PathUnsafe)?;
            if crate::worktree::has_uncommitted_changes(&worktree) {
                return Err(RepairBlocked::Dirty);
            }
            if let Some(other) = crate::binding::scan_existing_branch_binding(
                home,
                &identity.source_repo.display().to_string(),
                branch,
                agent,
            ) {
                return Err(RepairBlocked::OtherAgentHoldsBranch(other));
            }
            if let Some(blocked) =
                super::repair::reject_if_branch_has_dependents(home, agent, recorded_branch, branch)
            {
                return Err(blocked);
            }
            if actual != recorded_branch {
                if let Some(blocked) =
                    super::repair::reject_if_branch_has_dependents(home, agent, &actual, branch)
                {
                    return Err(blocked);
                }
            }
            if actual == branch {
                drop(_binding_lock);
                drop(_agent_lock);
                drop(_lease);
                return Ok(RepairResult::no_continuation(RepairAction::MetadataOnly));
            }
            let marker_body = std::fs::read(worktree.join(crate::worktree_pool::MANAGED_MARKER))
                .map_err(|e| {
                    RepairBlocked::SwitchFailed(format!("read marker before switch: {e}"))
                })?;
            let binding_body =
                std::fs::read(crate::paths::binding_path(home, agent)).map_err(|e| {
                    RepairBlocked::SwitchFailed(format!("read binding before switch: {e}"))
                })?;
            let binding_signature = std::fs::read(
                crate::paths::runtime_dir(home)
                    .join(agent)
                    .join("binding.json.sig"),
            )
            .ok();
            use crate::git_helpers::{git_cmd, GitError};
            match git_cmd(&worktree, &["switch", branch]) {
                Ok(_) => {
                    if let Err(error) =
                        update_marker_branch(&worktree, agent, branch, &identity.source_repo)
                    {
                        let restore = git_cmd(&worktree, &["switch", &actual_before])
                            .map(|_| ())
                            .map_err(|e| format!("restore branch after marker failure: {e}"));
                        let marker_restore = std::fs::write(
                            worktree.join(crate::worktree_pool::MANAGED_MARKER),
                            &marker_body,
                        )
                        .map_err(|e| format!("restore marker after marker failure: {e}"));
                        if let Err(restore_error) = restore {
                            return Err(RepairBlocked::SwitchFailed(format!(
                                "{error}; {restore_error}"
                            )));
                        }
                        if let Err(restore_error) = marker_restore {
                            return Err(RepairBlocked::SwitchFailed(format!(
                                "{error}; {restore_error}"
                            )));
                        }
                        return Err(RepairBlocked::SwitchFailed(error));
                    }
                }
                Err(GitError::NonZero { stderr, .. }) => {
                    return Err(RepairBlocked::SwitchFailed(stderr))
                }
                Err(GitError::Spawn(e)) => return Err(RepairBlocked::SwitchFailed(e.to_string())),
            }
            drop(_binding_lock);
            drop(_agent_lock);
            drop(_lease);
            Ok(RepairResult {
                action: RepairAction::SwitchedBranch,
                continuation: Some(RebaseContinuation {
                    worktree,
                    source_repo: identity.source_repo,
                    requested_branch: branch.to_string(),
                    previous_branch: actual_before,
                    marker_body,
                    binding_body,
                    binding_signature,
                    binding_fingerprint: fingerprint,
                }),
            })
        }
    }
}

fn current_branch(worktree: &Path) -> Result<String, super::repair::RepairBlocked> {
    crate::git_helpers::git_cmd(worktree, &["branch", "--show-current"])
        .map_err(|e| super::repair::RepairBlocked::SwitchFailed(e.to_string()))
        .and_then(|branch| {
            if branch.is_empty() {
                Err(super::repair::RepairBlocked::SwitchFailed(
                    "worktree has no current branch".to_string(),
                ))
            } else {
                Ok(branch)
            }
        })
}

fn update_marker_branch(
    worktree: &Path,
    agent: &str,
    branch: &str,
    source_repo: &Path,
) -> Result<(), String> {
    let marker = worktree.join(crate::worktree_pool::MANAGED_MARKER);
    let body = std::fs::read_to_string(&marker)
        .map_err(|e| format!("read managed marker {}: {e}", marker.display()))?;
    let mut lines = Vec::new();
    let mut saw_agent = false;
    let mut saw_branch = false;
    let mut saw_repo = false;
    for line in body.lines() {
        if line.starts_with("agent=") {
            lines.push(format!("agent={agent}"));
            saw_agent = true;
        } else if line.starts_with("branch=") {
            lines.push(format!("branch={branch}"));
            saw_branch = true;
        } else if line.starts_with("source_repo=") {
            lines.push(format!("source_repo={}", source_repo.display()));
            saw_repo = true;
        } else {
            lines.push(line.to_string());
        }
    }
    if !saw_agent {
        lines.push(format!("agent={agent}"));
    }
    if !saw_branch {
        lines.push(format!("branch={branch}"));
    }
    if !saw_repo {
        lines.push(format!("source_repo={}", source_repo.display()));
    }
    #[cfg(test)]
    if let Some(reason) = rebase_test_seam::hit(RebaseTestPhase::BeforeMarkerCommit) {
        return Err(reason);
    }
    std::fs::write(marker, format!("{}\n", lines.join("\n")))
        .map_err(|e| format!("write managed marker: {e}"))
}
