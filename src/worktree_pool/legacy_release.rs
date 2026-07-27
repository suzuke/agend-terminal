use std::path::{Path, PathBuf};

/// A missing binding is normally an idempotent no-op. A surviving flat
/// `repo action=checkout bind:false` worktree is the exception: preserve it
/// and point the caller at the existing typed path-addressed release route.
/// The candidate scan is read-only and requires only the exact marker-owned
/// flat layout; source/linkage proofs select typed recovery guidance.
pub(super) fn absent_release_outcome(home: &Path, agent: &str) -> super::ReleaseOutcome {
    let Some(candidate) = find_flat_candidate(home, agent) else {
        return super::idempotent_absent();
    };
    let error = if let Some(source_repo) = candidate.source_repo {
        format!(
            "release refused: flat daemon-managed worktree for '{agent}' survives at '{}'; use repo action=release with path='{}' repository_path='{}'",
            candidate.target.display(),
            candidate.target.display(),
            source_repo.display()
        )
    } else {
        format!(
            "release refused: owned flat daemon-managed worktree for '{agent}' survives at '{}'; repository identity is unproven — path is preserved for GC/archive recovery",
            candidate.target.display()
        )
    };
    super::ReleaseOutcome {
        error: Some(error),
        ..super::ReleaseOutcome::default()
    }
}

struct FlatCandidate {
    target: PathBuf,
    source_repo: Option<PathBuf>,
}

fn find_flat_candidate(home: &Path, agent: &str) -> Option<FlatCandidate> {
    let mut candidates = Vec::new();
    super::collect_managed_worktrees(
        &super::daemon_managed_worktree_root(home),
        super::MARKER_WALK_MAX_DEPTH,
        &mut candidates,
    );
    candidates.into_iter().find_map(|target| {
        let target = dunce::canonicalize(target).ok()?;
        if !legacy_flat_target_path(home, &target, agent)
            || crate::binding::managed_marker_agent(&target).as_deref() != Some(agent)
        {
            return None;
        }
        let source_repo = super::marker_source_repo(&target)
            .and_then(|path| path.canonicalize().ok())
            .filter(|source_repo| super::target_source_repo_matches(&target, source_repo));
        Some(FlatCandidate {
            target,
            source_repo,
        })
    })
}

pub(super) fn legacy_flat_target_path(home: &Path, target: &Path, agent: &str) -> bool {
    let Ok(root) = dunce::canonicalize(super::daemon_managed_worktree_root(home)) else {
        return false;
    };
    target.parent() == Some(root.as_path())
        && target
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(&format!("{agent}-")))
}

pub(super) fn registered_detached_target(source_repo: &Path, target: &Path) -> bool {
    let Ok(target) = target.canonicalize() else {
        return false;
    };
    let Ok(entries) = crate::git_worktree::list_porcelain_exact(source_repo) else {
        return false;
    };
    entries.into_iter().any(|(path, branch)| {
        branch.is_none() && path.canonicalize().ok().as_ref() == Some(&target)
    })
}

pub(super) fn require_clean_legacy_target(target: &Path) -> Result<(), String> {
    let status = crate::git_helpers::git_cmd(
        target,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--ignore-submodules=none",
        ],
    )
    .map_err(|e| {
        format!("legacy target cleanliness is unverifiable: {e} — refusing (state preserved)")
    })?;
    if status
        .lines()
        .any(|line| line.get(3..).map(str::trim) != Some(super::MANAGED_MARKER))
    {
        return Err(
            "legacy target is dirty beyond its managed marker — refusing (state preserved)"
                .to_string(),
        );
    }
    Ok(())
}
