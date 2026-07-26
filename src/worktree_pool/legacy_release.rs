use std::path::Path;

pub(super) fn legacy_flat_target_path(home: &Path, target: &Path, agent: &str) -> bool {
    let Ok(root) = super::daemon_managed_worktree_root(home).canonicalize() else {
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
