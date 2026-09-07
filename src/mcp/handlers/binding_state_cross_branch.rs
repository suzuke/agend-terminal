//! Cross-branch binding holders for `binding_state`. Pure move out of
//! `binding_state.rs` (#3546) — that file sat exactly at the 750-LOC handler
//! ceiling, so additive fields required an extraction. No logic change.

use std::path::Path;

/// Return list of agent names (other than `exclude_agent`) whose
/// binding currently references `branch`. P0-1.5 enforces uniqueness
/// at bind time — this enumerator surfaces any violation so it's
/// immediately visible via `binding_state`.
pub(super) fn cross_branch_holders_for(
    home: &Path,
    branch: &str,
    exclude_agent: &str,
) -> Vec<String> {
    if branch.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (other, v) in crate::binding::binding_scan_all(home) {
        if other == exclude_agent {
            continue;
        }
        if v["branch"].as_str() == Some(branch) {
            out.push(other);
        }
    }
    out.sort();
    out
}
