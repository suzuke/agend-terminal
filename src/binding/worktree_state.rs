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
/// down. A binding that is PRESENT but cannot be confidently understood is `Uncertain`
/// and also fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorktreeBindingState {
    /// A binding maps this exact worktree path — adopt/keep it.
    Bound,
    /// No binding is present for this path — safe to reconcile/remove.
    Unbound,
    /// A binding is PRESENT but cannot be confidently understood — a read error, corrupt
    /// JSON, a FUTURE schema this daemon can't parse, or a record with no usable `worktree`
    /// field — so it MIGHT target this path. Authority uncertain: fail closed (never remove).
    Uncertain,
}

/// Scan `runtime/*/binding.json` for a binding whose `worktree` equals `worktree_path`.
///
/// A definitive match is [`Bound`](WorktreeBindingState::Bound). A genuinely ABSENT
/// binding (no `binding.json`, or a NotFound runtime area) does not block removal. But a
/// binding that is PRESENT yet cannot be confidently understood — a read error, corrupt
/// JSON, a schema NEWER than this daemon supports (`parse_binding_guarded` → `None`), or a
/// record missing a usable `worktree` field — is [`Uncertain`](WorktreeBindingState::Uncertain):
/// a DESTRUCTIVE retention site must never reclaim a worktree a newer daemon may legitimately
/// own just because this daemon can't parse the binding ("future ≠ absent"; see
/// [`super::parse_binding_guarded`] / [`super::present_including_future`]). Only a clean scan —
/// no present-but-unreadable binding AND no match — is [`Unbound`](WorktreeBindingState::Unbound).
pub(crate) fn worktree_binding_state(home: &Path, worktree_path: &Path) -> WorktreeBindingState {
    let rt = crate::paths::runtime_dir(home);
    let entries = match std::fs::read_dir(&rt) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return WorktreeBindingState::Unbound,
        Err(_) => return WorktreeBindingState::Uncertain,
    };
    let target = worktree_path.to_string_lossy();
    // A present-but-unreadable binding could be THIS worktree's owner; remember we saw one
    // and fail closed (Uncertain) at the end unless a definitive Bound match turns up first.
    let mut uncertain = false;
    for entry in entries {
        let Ok(entry) = entry else {
            return WorktreeBindingState::Uncertain;
        };
        let bp = entry.path().join("binding.json");
        match std::fs::read_to_string(&bp) {
            // Corrupt JSON or a newer-than-supported schema ⇒ `None` ⇒ cannot rule out
            // that it targets us. A well-formed binding for a DIFFERENT worktree IS ruled
            // out (skip); one with no usable `worktree` field is uncertain.
            Ok(c) => match parse_binding_guarded(&c) {
                None => uncertain = true,
                Some(v) => match v.get("worktree").and_then(|w| w.as_str()) {
                    Some(w) if w == target.as_ref() => return WorktreeBindingState::Bound,
                    Some(_) => {}
                    None => uncertain = true,
                },
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return WorktreeBindingState::Uncertain,
        }
    }
    if uncertain {
        WorktreeBindingState::Uncertain
    } else {
        WorktreeBindingState::Unbound
    }
}

pub(crate) fn refresh_cached(home: &Path, agent: &str, value: serde_json::Value) {
    if let Ok(mut map) = super::binding_index().write() {
        map.insert(super::index_key(home, agent), value);
    }
}
