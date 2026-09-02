use std::path::Path;

/// The trunk base that init-commit cleanup may safely rewrite against.
///
/// Prefer an explicit `origin/HEAD`. Otherwise accept exactly one conventional
/// trunk; ambiguity and absence fail closed. This deliberately does not use
/// `git_helpers::default_branch`, whose local-HEAD fallback can name the
/// worktree's feature branch.
pub(super) fn resolve(worktree: &Path) -> Result<String, String> {
    if let Ok(head) = crate::git_helpers::git_cmd(
        worktree,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        if !head.is_empty() {
            return Ok(head);
        }
    }

    let trunk_exists = |rev: &str| {
        crate::git_helpers::git_ok(
            worktree,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{rev}^{{commit}}"),
            ],
        )
    };

    match (trunk_exists("origin/main"), trunk_exists("origin/master")) {
        (true, false) => Ok("origin/main".to_string()),
        (false, true) => Ok("origin/master".to_string()),
        (true, true) => Err(
            "ambiguous default branch: both origin/main and origin/master exist and \
             origin/HEAD is unset — refusing to pick a rewrite base (run \
             `git remote set-head origin -a`)"
                .to_string(),
        ),
        (false, false) => Err(
            "cannot resolve the trunk for init-commit cleanup: origin/HEAD is unset \
             and neither origin/main nor origin/master exists"
                .to_string(),
        ),
    }
}
