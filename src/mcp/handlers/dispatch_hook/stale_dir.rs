use super::{DispatchError, ErrorCode, Stage};
use std::path::Path;

pub(super) fn preflight(
    home: &Path,
    target: &str,
    branch: &str,
    source_repo: &Path,
    reused: bool,
    auto_created_branch: bool,
    fetch_attempted: bool,
) -> Result<(), DispatchError> {
    if !reused && crate::binding::read(home, target).is_none() {
        let stale_path = crate::worktree::worktree_path(home, target, branch);
        let marker_path = stale_path.join(crate::worktree_pool::MANAGED_MARKER);
        let stale_directory = std::fs::symlink_metadata(&stale_path)
            .is_ok_and(|metadata| metadata.file_type().is_dir());
        let regular_marker = std::fs::symlink_metadata(&marker_path)
            .is_ok_and(|metadata| metadata.file_type().is_file());
        if stale_directory && regular_marker {
            if let Ok(marker) = std::fs::read_to_string(marker_path) {
                if auto_created_branch {
                    let _ = crate::git_helpers::git_bypass(source_repo, &["branch", "-D", branch]);
                }
                let context = serde_json::json!({
                    "path": stale_path.display().to_string(),
                    "marker": marker,
                    "hint": format!(
                        "call release_worktree with instance='{target}', branch='{branch}', force=true, and repository_path='{}'",
                        source_repo.display()
                    ),
                });
                return Err(DispatchError {
                    message: format!(
                        "stale daemon-managed worktree directory remains at {}; force-release it before dispatch",
                        stale_path.display()
                    ),
                    code: ErrorCode::StaleWorktreeDir,
                    stage: Stage::StaleWorktreePreflight,
                    fetch_attempted,
                    raw: Some(context.to_string()),
                });
            }
        }
    }
    Ok(())
}
