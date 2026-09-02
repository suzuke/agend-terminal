use std::path::Path;

use super::{DispatchError, ErrorCode, Stage};

pub(super) fn resolve(
    source: &Path,
    expected_head: Option<&str>,
) -> Result<Option<String>, DispatchError> {
    let Some(expected) = expected_head else {
        return Ok(None);
    };
    if !matches!(expected.len(), 40 | 64) || !expected.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(error(
            ErrorCode::InvalidExpectedHead,
            Stage::ValidateExpectedHead,
            format!("expected_head must be a full 40- or 64-hex commit SHA, got '{expected}'"),
            None,
        ));
    }
    let commit = format!("{expected}^{{commit}}");
    let output = crate::git_helpers::git_bypass(source, &["rev-parse", "--verify", &commit])
        .map_err(|failure| {
            error(
                ErrorCode::ExpectedHeadMismatch,
                Stage::ValidateExpectedHead,
                format!("expected_head '{expected}' does not resolve to a commit"),
                Some(failure.to_string()),
            )
        })?;
    if !output.status.success() {
        return Err(error(
            ErrorCode::ExpectedHeadMismatch,
            Stage::ValidateExpectedHead,
            format!("expected_head '{expected}' does not resolve to a commit"),
            Some(String::from_utf8_lossy(&output.stderr).trim().to_string()),
        ));
    }
    Ok(Some(expected.to_ascii_lowercase()))
}

pub(super) fn branch_tip(source: &Path, branch: &str) -> Option<String> {
    let branch_ref = format!("refs/heads/{branch}");
    let output =
        crate::git_helpers::git_bypass(source, &["rev-parse", "--verify", &branch_ref]).ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .to_ascii_lowercase()
    })
}

pub(super) fn verify(worktree: &Path, expected: &str) -> Result<(), DispatchError> {
    let output = crate::git_helpers::git_bypass(worktree, &["rev-parse", "HEAD"]).map_err(|e| {
        error(
            ErrorCode::ExpectedHeadMismatch,
            Stage::VerifyExpectedHead,
            format!("could not verify bound worktree HEAD against expected_head '{expected}'"),
            Some(e.to_string()),
        )
    })?;
    let actual = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_ascii_lowercase();
    if output.status.success() && actual == expected {
        return Ok(());
    }
    Err(error(
        ErrorCode::ExpectedHeadMismatch,
        Stage::VerifyExpectedHead,
        format!("bound worktree HEAD '{actual}' does not match expected_head '{expected}'"),
        Some(String::from_utf8_lossy(&output.stderr).trim().to_string()),
    ))
}

pub(super) fn restore_branch(source: &Path, branch: &str, prior_tip: Option<&str>) {
    let branch_ref = format!("refs/heads/{branch}");
    let args = match prior_tip {
        Some(tip) => vec!["update-ref", branch_ref.as_str(), tip],
        None => vec!["update-ref", "-d", branch_ref.as_str()],
    };
    let _ = crate::git_helpers::git_bypass(source, &args);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn rollback_bound_worktree(
    home: &Path,
    target: &str,
    source: &Path,
    branch: &str,
    worktree: &Path,
    workspace_as_worktree: bool,
    prior_tip: Option<&str>,
    permit: &super::LifecyclePermit,
) {
    let _ = crate::binding::unbind_with_permit(home, target, permit);
    if workspace_as_worktree {
        let _ = crate::worktree_pool::detach_workspace_to_holding(worktree);
    } else {
        let worktree = worktree.display().to_string();
        let _ =
            crate::git_helpers::git_bypass(source, &["worktree", "remove", "--force", &worktree]);
    }
    restore_branch(source, branch, prior_tip);
}

fn error(code: ErrorCode, stage: Stage, message: String, raw: Option<String>) -> DispatchError {
    DispatchError {
        message,
        code,
        stage,
        fetch_attempted: false,
        raw,
    }
}
