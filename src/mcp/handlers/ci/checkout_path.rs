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
    pub legacy: bool,
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
        let code = self.code();
        match self {
            Self::PathBudget {
                path_units,
                max_units,
            } => json!({
                "error": "checkout worktree path exceeds the safe Git path budget",
                "code": code,
                "stage": "preflight",
                "path_units": path_units,
                "max_path_units": max_units,
            }),
            Self::IdentityConflict => json!({
                "error": "legacy and bounded checkout worktree identities both exist",
                "code": code,
                "stage": "preflight",
            }),
        }
    }
}

/// Resolve the checkout target while preserving an existing legacy identity.
pub(crate) fn resolve_worktree_target(
    home: &Path,
    instance_name: &str,
    source_path: &str,
) -> Result<WorktreeTarget, WorktreeTargetError> {
    let legacy_mangled = legacy_mangled(instance_name, source_path);
    let legacy_path = home.join("worktrees").join(&legacy_mangled);
    let bounded_mangled = bounded_mangled(instance_name, source_path);
    let bounded_path = home.join("worktrees").join(&bounded_mangled);
    let legacy_present = legacy_path.exists() || legacy_journal_exists(home, &legacy_mangled);
    let bounded_present = bounded_path.exists() || legacy_journal_exists(home, &bounded_mangled);

    if legacy_present && bounded_present && !same_existing_path(&legacy_path, &bounded_path) {
        return Err(WorktreeTargetError::IdentityConflict);
    }
    let target = if legacy_present {
        WorktreeTarget {
            path: legacy_path,
            mangled: legacy_mangled,
            legacy: true,
        }
    } else {
        WorktreeTarget {
            path: bounded_path,
            mangled: bounded_mangled,
            legacy: false,
        }
    };
    validate_target_budget(&target.path, &target.mangled, target.legacy)?;
    Ok(target)
}

fn validate_target_budget(
    path: &Path,
    mangled: &str,
    legacy: bool,
) -> Result<(), WorktreeTargetError> {
    let component_units = path_units(Path::new(mangled));
    let path_units = path_units(&path.join(".git"));
    if (!legacy && component_units > WORKTREE_COMPONENT_MAX_UNITS)
        || path_units > WORKTREE_GIT_PATH_MAX_UNITS
    {
        return Err(WorktreeTargetError::PathBudget {
            path_units,
            max_units: WORKTREE_GIT_PATH_MAX_UNITS,
        });
    }
    Ok(())
}

fn legacy_mangled(instance_name: &str, source_path: &str) -> String {
    format!(
        "{instance_name}-{}",
        source_path.replace(['/', '\\', ':'], "_").replace('~', "")
    )
}

fn bounded_mangled(instance_name: &str, source_path: &str) -> String {
    let label = Path::new(source_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .take(REPOSITORY_LABEL_MAX_UNITS)
        .collect::<String>();
    let label = if label.is_empty() { "repo" } else { &label };
    let digest = crate::daemon::utils::sha256_hex(source_path.as_bytes());
    format!(
        "{instance_name}-{label}-{}",
        &digest[..REPOSITORY_DIGEST_HEX_UNITS]
    )
}

fn legacy_journal_exists(home: &Path, mangled: &str) -> bool {
    let key = super::checkout_txn::journal_key(home, mangled);
    super::checkout_txn::journal_path(home, mangled).exists()
        || super::checkout_txn::journal_path(home, &key).exists()
}

fn same_existing_path(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
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
#[allow(clippy::unwrap_used, clippy::expect_used)]
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
        let home = PathBuf::from(format!(
            "/tmp/agend-checkout-path-bounded-{}/{}",
            std::process::id(),
            "long-home/".repeat(30)
        ));
        let candidate = home
            .join("worktrees")
            .join(bounded_mangled("reviewer", "/tmp/repo"))
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
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn overlong_legacy_target_returns_typed_budget_error() {
        assert_policy_constants_are_unchanged();
        let home = PathBuf::from(format!(
            "/tmp/agend-checkout-path-legacy-budget-{}/{}",
            std::process::id(),
            "long-home/".repeat(30)
        ));
        let source = "/tmp/repo";
        let legacy = home
            .join("worktrees")
            .join(legacy_mangled("reviewer", source));
        let candidate_units = path_units(&legacy.join(".git"));
        assert!(
            candidate_units > POLICY_WORKTREE_GIT_PATH_MAX_UNITS,
            "fixture must measure an over-budget legacy target: {candidate_units}"
        );
        std::fs::create_dir_all(&legacy).unwrap();
        let err = resolve_worktree_target(&home, "reviewer", source).unwrap_err();
        assert_eq!(
            err,
            WorktreeTargetError::PathBudget {
                path_units: candidate_units,
                max_units: POLICY_WORKTREE_GIT_PATH_MAX_UNITS,
            }
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn existing_legacy_target_is_adopted() {
        let home =
            std::env::temp_dir().join(format!("agend-checkout-path-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let source = "/tmp/project/repo";
        let legacy = home
            .join("worktrees")
            .join(legacy_mangled("reviewer", source));
        std::fs::create_dir_all(&legacy).unwrap();
        let target = resolve_worktree_target(&home, "reviewer", source).unwrap();
        assert!(target.legacy);
        assert_eq!(target.path, legacy);
        assert_eq!(target.mangled, legacy_mangled("reviewer", source));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn legacy_component_over_bounded_cap_is_adopted_when_total_fits() {
        assert_policy_constants_are_unchanged();
        let home = PathBuf::from("/tmp/agend-checkout-legacy-component");
        let source = format!("/tmp/{}", "legacy-segment/".repeat(9));
        let legacy_name = legacy_mangled("reviewer", &source);
        let legacy = home.join("worktrees").join(&legacy_name);
        let component_units = path_units(Path::new(&legacy_name));
        let total_units = path_units(&legacy.join(".git"));
        assert!(component_units > POLICY_WORKTREE_COMPONENT_MAX_UNITS);
        assert!(total_units <= POLICY_WORKTREE_GIT_PATH_MAX_UNITS);
        std::fs::create_dir_all(&legacy).unwrap();
        let target = resolve_worktree_target(&home, "reviewer", &source).unwrap();
        assert!(target.legacy);
        assert_eq!(target.path, legacy);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn existing_legacy_journal_is_adopted() {
        let home = std::env::temp_dir().join(format!(
            "agend-checkout-path-legacy-journal-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let source = "/tmp/project/repo";
        let mangled = legacy_mangled("reviewer", source);
        let journal = super::super::checkout_txn::Journal::prepared(
            "nonce",
            home.join("worktrees").join(&mangled).display().to_string(),
            source,
            "main",
            false,
            "2026-01-01T00:00:00Z",
        );
        journal.save(&home, &mangled).unwrap();
        let target = resolve_worktree_target(&home, "reviewer", source).unwrap();
        assert!(target.legacy);
        assert_eq!(target.mangled, mangled);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn legacy_and_bounded_targets_conflict_fail_closed() {
        let home = std::env::temp_dir().join(format!(
            "agend-checkout-path-conflict-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let source = "/tmp/project/repo";
        let legacy = home
            .join("worktrees")
            .join(legacy_mangled("reviewer", source));
        let bounded = home
            .join("worktrees")
            .join(bounded_mangled("reviewer", source));
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&bounded).unwrap();
        assert_eq!(
            resolve_worktree_target(&home, "reviewer", source),
            Err(WorktreeTargetError::IdentityConflict)
        );
        std::fs::remove_dir_all(home).unwrap();
    }
}
