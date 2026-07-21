//! Typed pre-delete admission for instance working-directory cleanup.
//!
//! The decision is made from the immutable FleetConfig snapshot captured before
//! `full_delete_instance` removes the victim entry.  A survivor that resolves to
//! an overlapping canonical path preserves the directory completely; only an
//! unshared exact default may be removed recursively, while an unshared external
//! directory keeps the existing agend-file scrub semantics.

use crate::fleet::FleetConfig;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) enum CleanupAdmission {
    RemoveOwned { canonical: PathBuf },
    ScrubExclusive { canonical: PathBuf },
    Preserve { reason: String },
    NoOp { reason: String },
    Refuse { reason: String },
}

fn has_dotdot(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn paths_overlap(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}

fn path_entry_present(candidate: &Path) -> Result<bool, std::io::Error> {
    match std::fs::symlink_metadata(candidate) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn raw_working_directory(snapshot: &FleetConfig, home: &Path, name: &str) -> Option<PathBuf> {
    let instance = snapshot.instances.get(name)?;
    let path = instance
        .working_directory
        .as_deref()
        .map(crate::fleet::resolve::expand_tilde_path)
        .unwrap_or_else(|| crate::paths::workspace_dir(home).join(name));
    (!path.as_os_str().is_empty()).then_some(path)
}

/// Derive the only path mutation a full delete may perform.
pub(crate) fn derive(
    snapshot: Option<&FleetConfig>,
    home: &Path,
    victim: &str,
    candidate: &Path,
) -> CleanupAdmission {
    let candidate_present = match path_entry_present(candidate) {
        Ok(present) => present,
        Err(error) => {
            return CleanupAdmission::Refuse {
                reason: format!(
                    "candidate {} cannot inspect metadata: {error}",
                    candidate.display()
                ),
            };
        }
    };
    let Some(snapshot) = snapshot else {
        if !candidate_present {
            return CleanupAdmission::NoOp {
                reason: "fleet snapshot unavailable but candidate is already absent".to_string(),
            };
        }
        return CleanupAdmission::Refuse {
            reason: "fleet snapshot unavailable — cleanup ownership is unproven".to_string(),
        };
    };
    if !snapshot.instances.contains_key(victim) {
        if !candidate_present {
            return CleanupAdmission::NoOp {
                reason: format!("victim '{victim}' is absent and candidate is already absent"),
            };
        }
        return CleanupAdmission::Refuse {
            reason: format!("victim '{victim}' is absent from the fleet snapshot"),
        };
    }
    if candidate.as_os_str().is_empty() || has_dotdot(candidate) {
        return CleanupAdmission::Refuse {
            reason: format!(
                "candidate {} is empty or contains '..'",
                candidate.display()
            ),
        };
    }
    let candidate_canonical = match dunce::canonicalize(candidate) {
        Ok(path) => path,
        Err(error) => {
            if error.kind() == std::io::ErrorKind::NotFound && !candidate_present {
                return CleanupAdmission::NoOp {
                    reason: format!("candidate {} is already absent", candidate.display()),
                };
            }
            return CleanupAdmission::Refuse {
                reason: format!(
                    "candidate {} cannot canonicalize: {error}",
                    candidate.display()
                ),
            };
        }
    };

    for name in snapshot.instances.keys() {
        if name == victim {
            continue;
        }
        let Some(survivor_path) = raw_working_directory(snapshot, home, name) else {
            return CleanupAdmission::Refuse {
                reason: format!("survivor '{name}' has no resolvable working directory"),
            };
        };
        if has_dotdot(&survivor_path) {
            return CleanupAdmission::Refuse {
                reason: format!("survivor '{name}' working directory contains '..'"),
            };
        }
        match dunce::canonicalize(&survivor_path) {
            Ok(survivor_canonical) if paths_overlap(&candidate_canonical, &survivor_canonical) => {
                return CleanupAdmission::Preserve {
                    reason: format!(
                        "survivor '{name}' overlaps candidate {}",
                        survivor_canonical.display()
                    ),
                };
            }
            Ok(_) => {}
            Err(error) => {
                return CleanupAdmission::Refuse {
                    reason: format!(
                        "survivor '{name}' working directory {} is ambiguous: {error}",
                        survivor_path.display()
                    ),
                };
            }
        }
    }

    let workspace_root = crate::paths::workspace_dir(home);
    let workspace_canonical = match dunce::canonicalize(&workspace_root) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return CleanupAdmission::ScrubExclusive {
                canonical: candidate_canonical,
            };
        }
        Err(error) => {
            return CleanupAdmission::Refuse {
                reason: format!("workspace root cannot canonicalize: {error}"),
            };
        }
    };
    let owned_default = workspace_canonical.join(victim);
    if candidate_canonical == owned_default {
        return CleanupAdmission::RemoveOwned {
            canonical: candidate_canonical,
        };
    }
    if candidate_canonical.starts_with(&workspace_canonical) {
        return CleanupAdmission::Refuse {
            reason: format!(
                "candidate {} is under workspace root but is not victim default {}",
                candidate_canonical.display(),
                owned_default.display()
            ),
        };
    }
    CleanupAdmission::ScrubExclusive {
        canonical: candidate_canonical,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn test_dir(label: &str) -> PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "agend-test-2876-{label}-{}-{id}",
            std::process::id(),
        ));
        let _ = std::fs::remove_dir_all(&base);
        base
    }

    fn parse_fleet(yaml: &str) -> crate::fleet::FleetConfig {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    /// #2876 RED: an unreachable survivor whose deepest existing ancestor is
    /// provably disjoint from the candidate must NOT block deletion.
    /// Currently returns `Refuse` because `dunce::canonicalize` fails on the
    /// missing path and the error arm unconditionally refuses.
    #[test]
    fn unrelated_unreachable_survivor_permits_owned_victim_cleanup_2876() {
        let base = test_dir("unrelated");
        let home = base.join("home");
        let victim_dir = crate::paths::workspace_dir(&home).join("victim");
        std::fs::create_dir_all(&victim_dir).unwrap();
        // Separate existing root for the survivor — clearly disjoint.
        let other_root = base.join("other-root");
        std::fs::create_dir_all(&other_root).unwrap();
        let survivor_missing = other_root.join("nas-mount").join("data");

        let config = parse_fleet(&format!(
            "instances:\n  victim:\n    backend: claude\n  survivor:\n    backend: claude\n    working_directory: {}\n",
            survivor_missing.display()
        ));

        let result = derive(Some(&config), &home, "victim", &victim_dir);
        assert!(
            matches!(result, CleanupAdmission::RemoveOwned { .. }),
            "#2876: unrelated unreachable survivor must not block owned-victim \
             cleanup, got: {result:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// #2876 safety pair: a missing survivor whose deepest existing ancestor
    /// overlaps the candidate must STILL refuse — the fail-closed invariant
    /// from #2764 is preserved for ambiguous paths.
    #[test]
    fn overlapping_unreachable_survivor_still_refuses_2876() {
        let base = test_dir("overlap");
        let home = base.join("home");
        let victim_dir = crate::paths::workspace_dir(&home).join("victim");
        std::fs::create_dir_all(&victim_dir).unwrap();
        // Survivor nested under victim — deepest ancestor IS the victim dir.
        let survivor_missing = victim_dir.join("nested-missing").join("deep");

        let config = parse_fleet(&format!(
            "instances:\n  victim:\n    backend: claude\n  survivor:\n    backend: claude\n    working_directory: {}\n",
            survivor_missing.display()
        ));

        let result = derive(Some(&config), &home, "victim", &victim_dir);
        assert!(
            matches!(
                result,
                CleanupAdmission::Refuse { .. } | CleanupAdmission::Preserve { .. }
            ),
            "#2876 safety: overlapping unreachable survivor must still block \
             cleanup, got: {result:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
