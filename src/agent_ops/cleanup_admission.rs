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
}

fn has_dotdot(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn paths_overlap(a: &Path, b: &Path) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
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
    let Some(snapshot) = snapshot else {
        return CleanupAdmission::Preserve {
            reason: "fleet snapshot unavailable — cleanup ownership is unproven".to_string(),
        };
    };
    if !snapshot.instances.contains_key(victim) {
        return CleanupAdmission::Preserve {
            reason: format!("victim '{victim}' is absent from the fleet snapshot"),
        };
    }
    if candidate.as_os_str().is_empty() || has_dotdot(candidate) {
        return CleanupAdmission::Preserve {
            reason: format!(
                "candidate {} is empty or contains '..'",
                candidate.display()
            ),
        };
    }
    let candidate_canonical = match dunce::canonicalize(candidate) {
        Ok(path) => path,
        Err(error) => {
            return CleanupAdmission::Preserve {
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
            return CleanupAdmission::Preserve {
                reason: format!("survivor '{name}' has no resolvable working directory"),
            };
        };
        if has_dotdot(&survivor_path) {
            return CleanupAdmission::Preserve {
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
                return CleanupAdmission::Preserve {
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
            return CleanupAdmission::Preserve {
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
        return CleanupAdmission::Preserve {
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
