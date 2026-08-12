use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Git for Windows passes `<worktree>/.git` through child-process `GIT_DIR`.
/// Keep the complete path below the observed ~220-unit ceiling.
pub(crate) const WORKTREE_GIT_PATH_MAX_UNITS: usize = 220;
const INSTANCE_NAME_MAX_UNITS: usize = 64; // crate::agent::validate_name contract
const REPOSITORY_LABEL_MAX_UNITS: usize = 16;
const REPOSITORY_DIGEST_HEX_UNITS: usize = 32;
/// The component is `<instance>-<label>-<digest>`: 64 + 1 + 16 + 1 + 32.
pub(crate) const WORKTREE_COMPONENT_MAX_UNITS: usize =
    INSTANCE_NAME_MAX_UNITS + 1 + REPOSITORY_LABEL_MAX_UNITS + 1 + REPOSITORY_DIGEST_HEX_UNITS;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct WorktreeTarget {
    pub path: PathBuf,
    pub mangled: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WorktreeTargetError {
    PathBudget { path_units: usize, max_units: usize },
    IdentityConflict,
}

impl WorktreeTargetError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::PathBudget { .. } => "worktree_path_budget",
            Self::IdentityConflict => "worktree_identity_conflict",
        }
    }

    pub(crate) fn into_response(self) -> Value {
        match self {
            Self::PathBudget {
                path_units,
                max_units,
            } => json!({
                "error": "checkout worktree path exceeds the safe Git path budget",
                "code": "worktree_path_budget",
                "stage": "preflight",
                "path_units": path_units,
                "max_path_units": max_units,
            }),
            Self::IdentityConflict => json!({
                "error": "legacy and bounded checkout worktree identities both exist",
                "code": "worktree_identity_conflict",
                "stage": "preflight",
            }),
        }
    }
}

/// Resolve the checkout target. The RED snapshot retains the historical
/// full-source mangling; the GREEN implementation will replace only the new
/// target while keeping the legacy candidate for safe adoption.
pub(crate) fn resolve_worktree_target(
    home: &Path,
    instance_name: &str,
    source_path: &str,
) -> Result<WorktreeTarget, WorktreeTargetError> {
    let mangled = legacy_mangled(instance_name, source_path);
    Ok(WorktreeTarget {
        path: home.join("worktrees").join(&mangled),
        mangled,
    })
}

fn legacy_mangled(instance_name: &str, source_path: &str) -> String {
    format!(
        "{instance_name}-{}",
        source_path.replace(['/', '\\', ':'], "_").replace('~', "")
    )
}

fn path_units(path: &Path) -> usize {
    #[cfg(windows)]
    {
        path.to_string_lossy().encode_utf16().count()
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY_WORKTREE_GIT_PATH_MAX_UNITS: usize = 220;
    const POLICY_WORKTREE_COMPONENT_MAX_UNITS: usize = 114;

    fn assert_policy_constants_are_unchanged() {
        assert_eq!(
            WORKTREE_GIT_PATH_MAX_UNITS,
            POLICY_WORKTREE_GIT_PATH_MAX_UNITS
        );
        assert_eq!(
            WORKTREE_COMPONENT_MAX_UNITS,
            POLICY_WORKTREE_COMPONENT_MAX_UNITS
        );
    }

    #[test]
    fn bounded_target_path_and_component() {
        assert_policy_constants_are_unchanged();
        let home = Path::new("/tmp/agend-checkout-path-budget");
        let source = format!("/tmp/{}", "nested-source/".repeat(20));
        let target = resolve_worktree_target(home, "reviewer", &source).unwrap();
        let component = target.path.file_name().unwrap().to_string_lossy();
        assert!(
            component.len() <= POLICY_WORKTREE_COMPONENT_MAX_UNITS,
            "target component is not bounded: {}",
            component.len()
        );
        assert!(
            path_units(&target.path.join(".git")) <= POLICY_WORKTREE_GIT_PATH_MAX_UNITS,
            "target/.git is not bounded: {}",
            path_units(&target.path.join(".git"))
        );
    }

    #[test]
    fn formerly_colliding_source_paths_get_distinct_targets() {
        let home = Path::new("/tmp/agend-checkout-path-collision");
        let left = resolve_worktree_target(home, "reviewer", "/tmp/a/b/repo").unwrap();
        let right = resolve_worktree_target(home, "reviewer", "/tmp/a_b/repo").unwrap();
        assert_ne!(left.path, right.path);
    }

    #[test]
    fn overlong_home_returns_typed_budget_error() {
        assert_policy_constants_are_unchanged();
        let home = PathBuf::from(format!("/tmp/{}", "long-home/".repeat(30)));
        let candidate = home
            .join("worktrees")
            .join("reviewer-_tmp_repo")
            .join(".git");
        let candidate_units = path_units(&candidate);
        assert!(
            candidate_units > POLICY_WORKTREE_GIT_PATH_MAX_UNITS,
            "fixture must measure an over-budget candidate: {candidate_units}"
        );
        let err = resolve_worktree_target(&home, "reviewer", "/tmp/repo").unwrap_err();
        assert_eq!(err.code(), "worktree_path_budget");
        assert_eq!(
            err,
            WorktreeTargetError::PathBudget {
                path_units: candidate_units,
                max_units: POLICY_WORKTREE_GIT_PATH_MAX_UNITS,
            }
        );
    }
}
