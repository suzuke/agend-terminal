use std::path::Path;

/// #2496: strict, same-agent-only exception to guard-b. ALL conditions must
/// hold for metadata-only catch-up; any failure keeps guard-b's existing reject.
pub(super) fn same_agent_metadata_catchup_allowed(
    home: &Path,
    agent: &str,
    worktree: &Path,
    source_repo: &Path,
    branch: &str,
    ex_branch: &str,
) -> bool {
    if !crate::worktree::is_git_repo(worktree) || !crate::worktree_pool::is_daemon_managed(worktree)
    {
        return false;
    }
    // A marker for a different agent proves this is not stale metadata owned
    // by the caller and must remain protected.
    if let Some(marker_agent) = super::managed_marker_agent(worktree) {
        if marker_agent != agent {
            return false;
        }
    }
    if crate::worktree::has_uncommitted_changes(worktree) {
        return false;
    }
    let actual_branch =
        crate::git_helpers::git_cmd(worktree, &["branch", "--show-current"]).unwrap_or_default();
    if actual_branch != branch {
        return false;
    }
    let source_repo_str = source_repo.display().to_string();
    if super::scan_existing_branch_binding(home, &source_repo_str, branch, agent).is_some()
        || super::agent_has_active_ci_watch_on_branch(home, agent, ex_branch)
        || super::branch_has_active_task(home, ex_branch)
    {
        return false;
    }
    true
}
