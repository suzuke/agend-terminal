//! The per-agent inbox flock.
//!
//! Every read-modify-write operation on an agent's inbox — `enqueue`, `drain`,
//! `sweep_expired`, and (PR #3495) the keyed idempotent enqueue — serialises on
//! this one lock file, cross-process. It lives beside `storage` rather than
//! inside it so the exactly-once enqueue in `super::idempotent` takes the SAME
//! lock as the ordinary append rather than growing a second spelling of it.

use super::storage::inbox_path_resolved;
use std::path::Path;

/// Acquire a per-agent flock and run `f` with the inbox path.
/// All read-modify-write operations on an agent's inbox (enqueue, drain,
/// sweep_expired) must go through this helper to prevent concurrent races.
pub(crate) fn with_inbox_lock<T>(
    home: &Path,
    name: &str,
    f: impl FnOnce(&Path) -> T,
) -> anyhow::Result<T> {
    let path = inbox_path_resolved(home, name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_extension("jsonl.lock");
    let _lock = crate::store::acquire_file_lock(&lock_path)?;
    Ok(f(&path))
}
