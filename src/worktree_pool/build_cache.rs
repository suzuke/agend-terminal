//! #3694: deadline-bounded pre-removal cleanup of an ignored `target/` build
//! cache.
//!
//! `git worktree remove --force` deletes the whole worktree directory anyway,
//! but a multi-GB `target/` full of build artifacts — often with files still
//! held open by a running cargo/test process — makes that removal slow enough
//! to blow the release budget. Deleting the disposable, git-ignored cache first
//! keeps the subsequent removal quick.
//!
//! The sweep is **deadline-bounded**: if the budget elapses it leaves the
//! remainder in place (the bounded worktree removal that follows deletes it)
//! and logs a warning, rather than consuming the entire release budget or
//! failing the release. A single locked child is skipped, never fatal.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// #3694: hard cap on time spent pre-deleting an ignored `target/`. Kept well
/// below the 60s `LOCAL_GIT_TIMEOUT` that bounds the subsequent
/// `git worktree remove`, so this sweep can never consume the whole release
/// budget. Normal caches are deleted far inside this window.
pub(crate) const BUILD_CACHE_CLEANUP_BUDGET: Duration = Duration::from_secs(10);

/// Remove an ignored `target/` cache before worktree removal, bounded by
/// [`BUILD_CACHE_CLEANUP_BUDGET`].
///
/// Returns `Err((path, reason))` only for an opaque precondition failure — an
/// unreadable `target/` metadata or a directory that cannot be enumerated at
/// all (matching the old unbounded `remove_dir_all` contract). Budget
/// exhaustion is a **non-fatal skip**: the leftover is deleted by the worktree
/// removal that follows.
pub(crate) fn clean_ignored_build_cache(worktree: &Path) -> Result<(), (PathBuf, String)> {
    clean_ignored_build_cache_with_budget(worktree, BUILD_CACHE_CLEANUP_BUDGET)
}

/// Test seam: [`clean_ignored_build_cache`] with an injectable budget so the
/// bound can be exercised without building a multi-GB fixture.
pub(crate) fn clean_ignored_build_cache_with_budget(
    worktree: &Path,
    budget: Duration,
) -> Result<(), (PathBuf, String)> {
    let target = worktree.join("target");
    let metadata = match std::fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err((target, error.to_string())),
    };
    // A target directory is only a disposable cache when Git confirms the
    // repository ignores it. An unignored target stays part of the ordinary
    // WIP-preservation + worktree-removal transaction.
    if !crate::git_helpers::git_ok(worktree, &["check-ignore", "-q", "--", "target/"]) {
        return Ok(());
    }
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return std::fs::remove_file(&target).map_err(|error| (target, error.to_string()));
    }
    match remove_dir_all_bounded(&target, Instant::now() + budget) {
        Ok(()) => Ok(()),
        Err(BoundedRemoval::Deadline) => {
            tracing::warn!(
                path = %target.display(),
                budget_secs = budget.as_secs(),
                "release: ignored build cache cleanup exceeded its budget — leaving the \
                 remainder for the bounded worktree removal"
            );
            Ok(())
        }
        Err(BoundedRemoval::Io(error)) => Err((target, error)),
    }
}

#[derive(Debug)]
enum BoundedRemoval {
    /// The budget elapsed before the tree was fully removed; the partial tree
    /// is intentionally left in place.
    Deadline,
    /// A top-level directory could not be read at all.
    Io(String),
}

/// Post-order recursive delete that aborts once `deadline` passes. Individual
/// entry errors are non-fatal (a locked child is skipped) so one open file
/// cannot abort the whole sweep.
fn remove_dir_all_bounded(dir: &Path, deadline: Instant) -> Result<(), BoundedRemoval> {
    if Instant::now() >= deadline {
        return Err(BoundedRemoval::Deadline);
    }
    let entries = std::fs::read_dir(dir).map_err(|e| BoundedRemoval::Io(e.to_string()))?;
    for entry in entries.flatten() {
        if Instant::now() >= deadline {
            return Err(BoundedRemoval::Deadline);
        }
        let path = entry.path();
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => remove_dir_all_bounded(&path, deadline)?,
            Ok(_) => {
                let _ = std::fs::remove_file(&path);
            }
            Err(_) => {}
        }
    }
    let _ = std::fs::remove_dir(dir);
    Ok(())
}

#[cfg(test)]
mod tests;
