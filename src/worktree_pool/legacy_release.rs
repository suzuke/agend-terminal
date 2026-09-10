use std::path::{Path, PathBuf};

/// A missing binding is normally an idempotent no-op. A surviving marker-owned
/// worktree is the exception: preserve it and return an observable recovery
/// outcome. The scan is read-only; source/linkage proofs only select typed
/// recovery guidance and never authorize implicit deletion.
pub(super) fn absent_release_outcome(home: &Path, agent: &str) -> super::ReleaseOutcome {
    let Some(candidate) = find_survivor_candidate(home, agent) else {
        return super::idempotent_absent();
    };
    let layout = if candidate.flat { "flat" } else { "nested" };
    let error = if candidate.flat {
        if let Some(source_repo) = candidate.source_repo.as_ref() {
            format!(
                "release refused: {layout} daemon-managed worktree for '{agent}' survives at '{}'; use repo action=release with path='{}' repository_path='{}'",
                candidate.target.display(),
                candidate.target.display(),
                source_repo.display()
            )
        } else {
            format!(
                "release refused: owned {layout} daemon-managed worktree for '{agent}' survives at '{}'; repository identity is unproven — path is preserved for GC/archive recovery",
                candidate.target.display()
            )
        }
    } else if candidate.source_repo.is_some() {
        format!(
            "release refused: nested daemon-managed worktree for '{agent}' survives at '{}'; source identity is authenticated, but nested survivor is preserved for GC/archive recovery — use an explicit path-addressed release route",
            candidate.target.display()
        )
    } else {
        format!(
            "release refused: owned {layout} daemon-managed worktree for '{agent}' survives at '{}'; repository identity is unproven — path is preserved for GC/archive recovery",
            candidate.target.display()
        )
    };
    super::ReleaseOutcome {
        error: Some(error),
        ..super::ReleaseOutcome::default()
    }
}

struct SurvivorCandidate {
    target: PathBuf,
    source_repo: Option<PathBuf>,
    flat: bool,
}

fn find_survivor_candidate(home: &Path, agent: &str) -> Option<SurvivorCandidate> {
    let mut candidates = Vec::new();
    super::collect_managed_worktrees(
        &super::daemon_managed_worktree_root(home),
        super::MARKER_WALK_MAX_DEPTH,
        &mut candidates,
    );
    candidates.sort();
    candidates.into_iter().find_map(|target| {
        let target = dunce::canonicalize(target).ok()?;
        if crate::binding::managed_marker_agent(&target).as_deref() != Some(agent) {
            return None;
        }
        let source_repo = super::marker_source_repo(&target)
            .and_then(|path| dunce::canonicalize(path).ok())
            .filter(|source_repo| super::target_source_repo_matches(&target, source_repo));
        let flat = legacy_flat_target_path(home, &target, agent);
        Some(SurvivorCandidate {
            target,
            source_repo,
            flat,
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
