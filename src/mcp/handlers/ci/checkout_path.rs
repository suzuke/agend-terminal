use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Git for Windows passes `<worktree>/.git` through child-process `GIT_DIR`.
/// Its `mingw_getenv` path is measured in UTF-8 bytes; keep it below the
/// observed PATH_MAX-40 safety ceiling. Non-Windows Git keeps its prior
/// unbounded total-path behavior.
#[cfg_attr(not(windows), allow(dead_code))]
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
    PathBudget {
        path_kind: &'static str,
        path: String,
        path_units: usize,
        max_units: usize,
    },
    IdentityConflict {
        legacy_path: String,
        bounded_path: Option<String>,
    },
    #[cfg_attr(not(windows), allow(dead_code))]
    GitCommonDirUnavailable { source_path: String },
}

impl WorktreeTargetError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::PathBudget { .. } => "worktree_path_budget",
            Self::IdentityConflict { .. } => "worktree_identity_conflict",
            Self::GitCommonDirUnavailable { .. } => "worktree_git_common_dir_unavailable",
        }
    }

    pub(crate) fn into_response(self) -> Value {
        let code = self.code();
        match self {
            Self::PathBudget {
                path_kind,
                path,
                path_units,
                max_units,
            } => json!({
                "error": "checkout worktree path exceeds the safe Git path budget",
                "code": code,
                "stage": "preflight",
                "violated_path_kind": path_kind,
                "violated_path": path,
                "path_units": path_units,
                "max_path_units": max_units,
            }),
            Self::IdentityConflict {
                legacy_path,
                bounded_path,
            } => json!({
                "error": "legacy and bounded checkout worktree identities both exist",
                "code": code,
                "stage": "preflight",
                "legacy_path": legacy_path,
                "bounded_path": bounded_path,
            }),
            Self::GitCommonDirUnavailable { source_path } => json!({
                "error": "checkout source has no resolvable Git common directory",
                "code": code,
                "stage": "preflight",
                "source_path": source_path,
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
    let common_dir = super::source_resolve::repo_common_dir(Path::new(source_path));
    resolve_worktree_target_with_common_dir(home, instance_name, source_path, common_dir.as_deref())
}

fn resolve_worktree_target_with_common_dir(
    home: &Path,
    instance_name: &str,
    source_path: &str,
    git_common_dir: Option<&Path>,
) -> Result<WorktreeTarget, WorktreeTargetError> {
    #[cfg(windows)]
    if git_common_dir.is_none() {
        return Err(WorktreeTargetError::GitCommonDirUnavailable {
            source_path: source_path.to_string(),
        });
    }
    let legacy_mangled = legacy_mangled(instance_name, source_path);
    let legacy_path = home.join("worktrees").join(&legacy_mangled);
    let bounded_mangled = bounded_mangled(instance_name, source_path);
    let bounded_path = home.join("worktrees").join(&bounded_mangled);
    let legacy_present = legacy_identity_matches(
        home,
        &legacy_path,
        &legacy_mangled,
        instance_name,
        source_path,
        git_common_dir,
    )?;
    let bounded_present = bounded_path.exists() || legacy_journal_exists(home, &bounded_mangled);

    if legacy_present && bounded_present && !same_existing_path(&legacy_path, &bounded_path) {
        return Err(WorktreeTargetError::IdentityConflict {
            legacy_path: legacy_path.display().to_string(),
            bounded_path: Some(bounded_path.display().to_string()),
        });
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
    validate_target_budget(&target.path, &target.mangled, target.legacy, git_common_dir)?;
    Ok(target)
}

fn validate_target_budget(
    path: &Path,
    mangled: &str,
    legacy: bool,
    git_common_dir: Option<&Path>,
) -> Result<(), WorktreeTargetError> {
    #[cfg(not(windows))]
    let _ = (path, git_common_dir);
    let component_units = path_units(Path::new(mangled));
    if !legacy && component_units > WORKTREE_COMPONENT_MAX_UNITS {
        return Err(WorktreeTargetError::PathBudget {
            path_kind: "worktree_component",
            path: mangled.to_string(),
            path_units: component_units,
            max_units: WORKTREE_COMPONENT_MAX_UNITS,
        });
    }

    #[cfg(windows)]
    {
        let worktree_git = path.join(".git");
        if path_units(&worktree_git) > WORKTREE_GIT_PATH_MAX_UNITS {
            return Err(WorktreeTargetError::PathBudget {
                path_kind: "worktree_git_dir",
                path: worktree_git.display().to_string(),
                path_units: path_units(&worktree_git),
                max_units: WORKTREE_GIT_PATH_MAX_UNITS,
            });
        }
        if let Some(git_common_dir) = git_common_dir {
            let admin = git_common_dir.join("worktrees").join(mangled);
            if path_units(&admin) > WORKTREE_GIT_PATH_MAX_UNITS {
                return Err(WorktreeTargetError::PathBudget {
                    path_kind: "git_admin_worktree_dir",
                    path: admin.display().to_string(),
                    path_units: path_units(&admin),
                    max_units: WORKTREE_GIT_PATH_MAX_UNITS,
                });
            }
        }
    }
    Ok(())
}

fn legacy_mangled(instance_name: &str, source_path: &str) -> String {
    format!(
        "{instance_name}-{}",
        source_path.replace(['/', '\\', ':'], "_").replace('~', "")
    )
}

pub(crate) fn bounded_mangled(instance_name: &str, source_path: &str) -> String {
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

fn legacy_identity_matches(
    home: &Path,
    legacy_path: &Path,
    mangled: &str,
    instance_name: &str,
    source_path: &str,
    git_common_dir: Option<&Path>,
) -> Result<bool, WorktreeTargetError> {
    let path_present = legacy_path.exists();
    let mut source_verified = false;

    if path_present {
        match legacy_marker_identity(legacy_path) {
            Ok(Some((agent, marker_source))) => {
                if agent != instance_name {
                    return Err(identity_conflict(legacy_path));
                }
                if let Some(marker_source) = marker_source {
                    source_verified = true;
                    if !source_identity_matches(&marker_source, source_path) {
                        return Err(identity_conflict(legacy_path));
                    }
                }
            }
            Ok(None) => {}
            Err(()) => return Err(identity_conflict(legacy_path)),
        }
        match legacy_git_common_dir(legacy_path) {
            Ok(Some(legacy_common_dir)) => {
                source_verified = true;
                if !git_common_dir.is_some_and(|common_dir| {
                    source_identity_paths_match(&legacy_common_dir, common_dir)
                }) {
                    return Err(identity_conflict(legacy_path));
                }
            }
            Ok(None) => {}
            Err(()) => return Err(identity_conflict(legacy_path)),
        }
    }

    let journal_verified = legacy_journal_identity_matches(home, mangled, source_path)?;
    if path_present && !source_verified && !journal_verified {
        return Err(identity_conflict(legacy_path));
    }
    Ok(path_present || journal_verified)
}

fn identity_conflict(legacy_path: &Path) -> WorktreeTargetError {
    WorktreeTargetError::IdentityConflict {
        legacy_path: legacy_path.display().to_string(),
        bounded_path: None,
    }
}

fn legacy_marker_identity(path: &Path) -> Result<Option<(String, Option<String>)>, ()> {
    let marker = path.join(crate::worktree_pool::MANAGED_MARKER);
    if !marker.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(marker).map_err(|_| ())?;
    let agent = content
        .lines()
        .find_map(|line| line.strip_prefix("agent="))
        .ok_or(())?
        .trim()
        .to_string();
    let source = content
        .lines()
        .find_map(|line| line.strip_prefix("source_repo="))
        .map(str::trim)
        .map(str::to_string);
    Ok(Some((agent, source)))
}

fn legacy_git_common_dir(path: &Path) -> Result<Option<PathBuf>, ()> {
    let git_file = path.join(".git");
    if !git_file.exists() {
        return Ok(None);
    }
    super::source_resolve::repo_common_dir(path)
        .ok_or(())
        .map(Some)
}

fn legacy_journal_identity_matches(
    home: &Path,
    mangled: &str,
    source_path: &str,
) -> Result<bool, WorktreeTargetError> {
    let key = super::checkout_txn::journal_key(home, mangled);
    let keys = if key == mangled {
        vec![mangled.to_string()]
    } else {
        vec![mangled.to_string(), key]
    };
    let mut found = false;
    for key in keys {
        let journal_path = super::checkout_txn::journal_path(home, &key);
        if !journal_path.exists() {
            continue;
        }
        found = true;
        match super::checkout_txn::load_typed(home, &key) {
            super::checkout_txn::JournalLoad::Loaded(journal)
                if source_identity_matches(&journal.source_repo, source_path) => {}
            _ => return Err(identity_conflict(&home.join("worktrees").join(mangled))),
        }
    }
    Ok(found)
}

fn source_identity_matches(actual: &str, expected: &str) -> bool {
    source_identity_paths_match(Path::new(actual), Path::new(expected))
}

fn source_identity_paths_match(actual: &Path, expected: &Path) -> bool {
    match (
        std::fs::canonicalize(actual),
        std::fs::canonicalize(expected),
    ) {
        (Ok(actual), Ok(expected)) => actual == expected,
        _ => actual == expected,
    }
}

fn same_existing_path(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn path_units(path: &Path) -> usize {
    path.to_string_lossy().len()
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

    fn write_matching_legacy_journal(home: &Path, mangled: &str, source: &str) {
        let journal = super::super::checkout_txn::Journal::prepared(
            "nonce",
            home.join("worktrees").join(mangled).display().to_string(),
            source,
            "main",
            false,
            "2026-01-01T00:00:00Z",
        );
        journal.save(home, mangled).unwrap();
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

    #[cfg(windows)]
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
                path_kind: "worktree_git_dir",
                path: candidate.display().to_string(),
                path_units: candidate_units,
                max_units: POLICY_WORKTREE_GIT_PATH_MAX_UNITS,
            }
        );
        std::fs::remove_dir_all(home).ok();
    }

    #[cfg(windows)]
    #[test]
    fn overlong_legacy_target_returns_typed_budget_error() {
        assert_policy_constants_are_unchanged();
        let home = PathBuf::from(format!(
            "/tmp/agend-checkout-path-legacy-budget-{}/{}",
            std::process::id(),
            "long-home/".repeat(30)
        ));
        let source = "/tmp/repo";
        let legacy_name = legacy_mangled("reviewer", source);
        let legacy = home.join("worktrees").join(&legacy_name);
        let candidate_units = path_units(&legacy.join(".git"));
        assert!(
            candidate_units > POLICY_WORKTREE_GIT_PATH_MAX_UNITS,
            "fixture must measure an over-budget legacy target: {candidate_units}"
        );
        std::fs::create_dir_all(&legacy).unwrap();
        write_matching_legacy_journal(&home, &legacy_name, source);
        let err = resolve_worktree_target(&home, "reviewer", source).unwrap_err();
        assert_eq!(
            err,
            WorktreeTargetError::PathBudget {
                path_kind: "worktree_git_dir",
                path: legacy.join(".git").display().to_string(),
                path_units: candidate_units,
                max_units: POLICY_WORKTREE_GIT_PATH_MAX_UNITS,
            }
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_preserves_unbounded_total_path_behavior() {
        let home = PathBuf::from(format!(
            "/tmp/agend-checkout-path-non-windows-{}/{}",
            std::process::id(),
            "long-home/".repeat(30)
        ));
        let target = resolve_worktree_target(&home, "reviewer", "/tmp/repo").unwrap();
        assert!(!target.legacy);
        assert!(path_units(&target.path.join(".git")) > POLICY_WORKTREE_GIT_PATH_MAX_UNITS);
        std::fs::remove_dir_all(home).ok();
    }

    #[cfg(windows)]
    #[test]
    fn windows_budget_counts_git_visible_utf8_bytes() {
        let path = PathBuf::from("é".repeat(111));
        assert!(path.to_string_lossy().encode_utf16().count() <= 220);
        assert!(path_units(&path) > 220);
    }

    #[cfg(windows)]
    #[test]
    fn windows_budget_checks_git_admin_worktree_path() {
        let home = PathBuf::from(r"C:\w");
        let source = r"C:\repo";
        let common = PathBuf::from(format!(r"C:\{}", "c".repeat(210)));
        let target = home
            .join("worktrees")
            .join(bounded_mangled("reviewer", source));
        let target_git = target.join(".git");
        let admin = common
            .join("worktrees")
            .join(bounded_mangled("reviewer", source));
        assert!(path_units(&target_git) <= WORKTREE_GIT_PATH_MAX_UNITS);
        assert!(path_units(&admin) > WORKTREE_GIT_PATH_MAX_UNITS);

        let err = resolve_worktree_target_with_common_dir(&home, "reviewer", source, Some(&common))
            .unwrap_err();
        assert_eq!(
            err,
            WorktreeTargetError::PathBudget {
                path_kind: "git_admin_worktree_dir",
                path: admin.display().to_string(),
                path_units: path_units(&admin),
                max_units: WORKTREE_GIT_PATH_MAX_UNITS,
            }
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_missing_git_common_dir_fails_closed() {
        let home = PathBuf::from(r"C:\w");
        let err = resolve_worktree_target_with_common_dir(
            &home,
            "reviewer",
            r"C:\not-a-git-worktree",
            None,
        )
        .unwrap_err();
        assert_eq!(
            err,
            WorktreeTargetError::GitCommonDirUnavailable {
                source_path: r"C:\not-a-git-worktree".to_string(),
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn separate_git_dir_legacy_target_is_adopted_by_git_common_identity() {
        let base = std::env::temp_dir().join(format!(
            "agend-checkout-path-separate-git-dir-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let source = base.join("source");
        let common = base.join("source-common.git");
        std::fs::create_dir_all(&base).unwrap();
        let init = std::process::Command::new("git")
            .args([
                "init",
                "--separate-git-dir",
                common.to_str().unwrap(),
                source.to_str().unwrap(),
            ])
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        assert!(init.status.success(), "git init failed: {:?}", init);
        std::fs::write(source.join("README"), "separate git dir").unwrap();
        let add = std::process::Command::new("git")
            .args(["add", "README"])
            .current_dir(&source)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        assert!(add.status.success(), "git add failed: {:?}", add);
        let commit = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=AgEnD Test",
                "-c",
                "user.email=agend@example.invalid",
                "commit",
                "-m",
                "initial",
            ])
            .current_dir(&source)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        assert!(commit.status.success(), "git commit failed: {:?}", commit);

        let home = base.join("home");
        let source_path = source.display().to_string();
        let legacy_name = legacy_mangled("reviewer", &source_path);
        let legacy = home.join("worktrees").join(&legacy_name);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let add_worktree = std::process::Command::new("git")
            .args([
                "worktree",
                "add",
                "--detach",
                legacy.to_str().unwrap(),
                "HEAD",
            ])
            .current_dir(&source)
            .env("AGEND_GIT_BYPASS", "1")
            .output()
            .unwrap();
        assert!(
            add_worktree.status.success(),
            "git worktree add failed: {:?}",
            add_worktree
        );

        let target = resolve_worktree_target(&home, "reviewer", &source_path).unwrap();
        assert!(target.legacy);
        assert_eq!(target.path, legacy);
        std::fs::remove_dir_all(base).unwrap();
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
        write_matching_legacy_journal(&home, &legacy_mangled("reviewer", source), source);
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
        write_matching_legacy_journal(&home, &legacy_name, &source);
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
        write_matching_legacy_journal(&home, &mangled, source);
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
        write_matching_legacy_journal(&home, &legacy_mangled("reviewer", source), source);
        assert!(matches!(
            resolve_worktree_target(&home, "reviewer", source),
            Err(WorktreeTargetError::IdentityConflict { .. })
        ));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn mismatched_legacy_evidence_fails_closed() {
        let home = std::env::temp_dir().join(format!(
            "agend-checkout-path-mismatched-legacy-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        let original_source = "/tmp/a/b/repo";
        let requested_source = "/tmp/a_b/repo";
        let legacy_name = legacy_mangled("reviewer", original_source);
        assert_eq!(legacy_name, legacy_mangled("reviewer", requested_source));
        let legacy = home.join("worktrees").join(&legacy_name);
        std::fs::create_dir_all(&legacy).unwrap();
        write_matching_legacy_journal(&home, &legacy_name, original_source);
        assert!(matches!(
            resolve_worktree_target(&home, "reviewer", requested_source),
            Err(WorktreeTargetError::IdentityConflict { .. })
        ));
        std::fs::remove_dir_all(home).unwrap();
    }
}
