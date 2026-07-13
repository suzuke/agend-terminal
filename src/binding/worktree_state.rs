//! #2755 R4: binding-adoption query for checkout recovery.
//!
//! Extracted from `binding.rs` so the parent stays under the anti-monolith
//! ceiling (`tests/src_file_size_invariant.rs`); the binding-format coupling
//! (`parse_binding_guarded`, the `worktree` field) stays in the `binding`
//! module rather than leaking into the checkout handlers that consume it.

use super::parse_binding_guarded;
use std::path::Path;

/// #2755 R4: whether ANY agent's binding currently targets `worktree_path`. Checkout
/// recovery MUST never delete a still-BOUND worktree (a crash after `bind_full` but
/// before the Committed journal leaves the binding written yet the journal
/// non-Committed) — the provision effectively succeeded and must be ADOPTED, not torn
/// down. `Uncertain` (a binding dir/file exists but is unreadable) fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorktreeBindingState {
    /// A binding maps this exact worktree path — adopt/keep it.
    Bound,
    /// No binding targets it — safe to reconcile/remove.
    Unbound,
    /// A runtime dir/binding could not be read — authority uncertain, fail closed.
    Uncertain,
}

/// Scan `runtime/*/binding.json` for a binding whose `worktree` equals `worktree_path`.
/// A NotFound runtime area or per-agent binding is `Unbound`; any other read error is
/// `Uncertain` (fail-closed). A corrupt/newer-schema binding that does not match is
/// simply not this worktree's binding.
pub(crate) fn worktree_binding_state(home: &Path, worktree_path: &Path) -> WorktreeBindingState {
    let rt = crate::paths::runtime_dir(home);
    let entries = match std::fs::read_dir(&rt) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return WorktreeBindingState::Unbound,
        Err(_) => return WorktreeBindingState::Uncertain,
    };
    let target = worktree_path.to_string_lossy();
    for entry in entries {
        let Ok(entry) = entry else {
            return WorktreeBindingState::Uncertain;
        };
        let bp = entry.path().join("binding.json");
        match std::fs::read_to_string(&bp) {
            Ok(c) => {
                if parse_binding_guarded(&c)
                    .and_then(|v| {
                        v.get("worktree")
                            .and_then(|w| w.as_str())
                            .map(str::to_string)
                    })
                    .as_deref()
                    == Some(target.as_ref())
                {
                    return WorktreeBindingState::Bound;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return WorktreeBindingState::Uncertain,
        }
    }
    WorktreeBindingState::Unbound
}
