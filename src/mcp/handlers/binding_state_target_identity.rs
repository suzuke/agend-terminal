use serde_json::{json, Value};
use std::path::Path;

pub(super) fn probe_target_identity(
    wt_path: &Path,
    expected_branch: &str,
    expected_worktree: &str,
) -> Value {
    let actual_head = crate::git_helpers::git_cmd(wt_path, &["rev-parse", "HEAD"]);
    let actual_branch = crate::git_helpers::git_cmd(wt_path, &["symbolic-ref", "--short", "HEAD"]);
    match (&actual_head, &actual_branch) {
        (Ok(head), Ok(branch)) => {
            let matches = branch == expected_branch;
            json!({
                "expected_branch": expected_branch,
                "expected_worktree": expected_worktree,
                "actual_branch": branch,
                "actual_head": head,
                "probe_status": "ok",
                "matches_binding": matches,
            })
        }
        _ => {
            let err = actual_head
                .as_ref()
                .err()
                .map(|e| format!("{e}"))
                .or_else(|| actual_branch.as_ref().err().map(|e| format!("{e}")))
                .unwrap_or_else(|| "unknown probe failure".into());
            json!({
                "expected_branch": expected_branch,
                "expected_worktree": expected_worktree,
                "probe_status": "error",
                "probe_error": err,
                "matches_binding": false,
            })
        }
    }
}
