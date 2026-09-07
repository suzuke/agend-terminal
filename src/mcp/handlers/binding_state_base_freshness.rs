//! #3546: the two `binding_state` fields that answer "is this worktree standing
//! on the right base?" — split into a sibling file because `binding_state.rs`
//! was already at the 750-LOC MCP-handler ceiling (the Sprint 54/55 pattern used
//! by `binding_state_ci_watches.rs` and `binding_state_target_identity.rs`).

use serde_json::Value;
use std::path::Path;

/// The provision-time fact, read back from the signed binding document.
///
/// Absent means false: every binding written before this field existed is a
/// binding that made no claim, and must not read as an alarm.
pub(super) fn flag_from_binding(binding: &Value) -> bool {
    binding
        .get("base_from_stale_view")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// #3546: how many commits the worktree's HEAD is behind the LOCAL view of the
/// default branch — `None` when that cannot be established.
///
/// Deliberately measured against `refs/remotes/<remote>/<default>` as it is on
/// disk, with NO fetch: a health query must not do network I/O, and inventing a
/// number would be worse than admitting we don't have one. That gives the field
/// an asymmetric meaning worth stating plainly, because the bug this exists for
/// is precisely a stale local view:
///
///   * a NON-ZERO count proves the base is behind;
///   * ZERO does NOT prove it is current — the local ref may itself be stale.
///
/// The `base_from_stale_view` flag beside it is what covers the second case: it
/// records, at provision time, that the ref could not be refreshed at all.
pub(super) fn base_behind_default_by(worktree: &Path, source_repo: &Path) -> Option<u64> {
    let default = crate::git_helpers::default_branch(source_repo);
    if default.is_empty() {
        return None;
    }
    let range = format!("HEAD..origin/{default}");
    let out = crate::git_helpers::git_bypass(worktree, &["rev-list", "--count", &range]).ok()?;
    if !out.status.success() {
        // No such ref (never fetched, renamed default, a fixture without a
        // remote) — unknown, not zero.
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok()
}
