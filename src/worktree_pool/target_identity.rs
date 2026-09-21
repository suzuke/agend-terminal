use std::path::{Path, PathBuf};

use super::MANAGED_MARKER;

pub(crate) fn marker_branch(worktree: &Path) -> Option<String> {
    std::fs::read_to_string(worktree.join(MANAGED_MARKER))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("branch="))
        .map(|s| s.trim().to_string())
}

pub(crate) fn marker_source_repo(worktree: &Path) -> Option<PathBuf> {
    std::fs::read_to_string(worktree.join(MANAGED_MARKER))
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("source_repo="))
        .map(|s| PathBuf::from(s.trim()))
}

pub(crate) fn git_pointer_source_repo(worktree: &Path) -> Option<PathBuf> {
    let content = std::fs::read_to_string(worktree.join(".git")).ok()?;
    let gitdir = content
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:").map(str::trim))?;
    let gitdir = PathBuf::from(gitdir);
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        worktree.join(gitdir)
    };
    let canonical = gitdir.canonicalize().ok()?;
    let worktrees = canonical.parent()?;
    if worktrees.file_name().and_then(|n| n.to_str()) != Some("worktrees") {
        return None;
    }
    Some(worktrees.parent()?.parent()?.to_path_buf())
}

pub(crate) fn target_source_repo_matches(worktree: &Path, source_repo: &Path) -> bool {
    let source = source_repo.canonicalize().ok();
    let Some(source) = source else { return false };
    let marker = marker_source_repo(worktree).and_then(|p| p.canonicalize().ok());
    let pointer = git_pointer_source_repo(worktree).and_then(|p| p.canonicalize().ok());
    match (marker, pointer) {
        (Some(marker), Some(pointer)) => marker == source && pointer == source,
        (Some(marker), None) => marker == source,
        (None, Some(pointer)) => pointer == source,
        (None, None) => false,
    }
}

/// Pin a worktree (operator override — prevents GC in Phase 4).
pub fn pin(worktree_path: &Path) {
    let pin_file = worktree_path.join(".agend-pinned");
    let _ = std::fs::write(&pin_file, chrono::Utc::now().to_rfc3339());
}

/// Unpin a worktree (allow GC again).
pub fn unpin(worktree_path: &Path) {
    let pin_file = worktree_path.join(".agend-pinned");
    let _ = std::fs::remove_file(pin_file);
}

/// Check if a worktree is pinned.
pub fn is_pinned(worktree_path: &Path) -> bool {
    worktree_path.join(".agend-pinned").exists()
}

/// Reconcile orphan leases at daemon startup (log only, no delete in Phase 3).
pub fn reconcile_orphan_leases(home: &Path) {
    for (agent_name, v) in crate::binding::binding_scan_all(home) {
        if let Some(wt_path) = v["worktree"].as_str() {
            if !Path::new(wt_path).exists() {
                tracing::warn!(
                    agent = agent_name.as_str(),
                    worktree = wt_path,
                    "orphan lease: worktree path missing"
                );
            }
        }
    }
}
