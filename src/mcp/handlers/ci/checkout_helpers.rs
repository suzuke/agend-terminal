//! #2755 R3 provisioning helpers extracted from `checkout.rs` to keep that handler
//! under the MCP-handler LOC ceiling (the same split pattern as `source_resolve.rs`):
//! the post-rollback response mapping and the marker content-durability fsync.

use super::checkout_txn::RollbackOutcome;
use serde_json::{json, Value};
use std::path::Path;

/// #2755 R3 (root + independent review): map a post-`git worktree add`
/// [`RollbackOutcome`] to the checkout error response, reporting the ACTUAL cleanup
/// state. `Removed` → the historical "worktree rolled back" text. `RollbackPending`
/// → a STRUCTURED pending state (`code: "rollback_pending"`, `rollback_pending: true`)
/// that NEVER claims the worktree was rolled back — the remove failed (Windows
/// open-handle / transient FS) and the worktree survives for the recovery sweep.
/// `intent_durable=false` (the retained-intent journal save ALSO failed) is surfaced
/// for intervention. The original failure `code`/`stage` are preserved
/// (`failed_code`/`stage`) so machine consumers keep the root cause. Pure —
/// unit-tested cross-platform.
pub(super) fn rollback_response(
    outcome: RollbackOutcome,
    reason: &str,
    code: &str,
    stage: &str,
    branch: &str,
) -> Value {
    match outcome {
        RollbackOutcome::Removed => json!({
            "error": format!("{reason}, worktree rolled back"),
            "code": code,
            "stage": stage,
            "branch": branch,
        }),
        RollbackOutcome::RollbackPending { intent_durable } => json!({
            "error": format!(
                "{reason}; worktree REMOVE FAILED — rollback pending, recovery sweep will retry{}",
                if intent_durable {
                    ""
                } else {
                    " (retained-intent journal save ALSO failed — operator intervention needed)"
                }
            ),
            "code": "rollback_pending",
            "rollback_pending": true,
            "intent_durable": intent_durable,
            "failed_code": code,
            "stage": stage,
            "branch": branch,
        }),
    }
}

/// #2755 R3 (independent P1.4): fsync the `.agend-managed` marker file's CONTENTS
/// durable — `std::fs::write` + a parent-dir fsync makes the DIRENT durable but not
/// the bytes, so a crash/power loss could leave a durable journal phase (or Committed
/// success) with an empty/torn marker. Open + `sync_all()` and OBSERVE the result; a
/// failure aborts the transaction fail-closed. A `cfg(test)` thread-local seam forces
/// the sync error so the crash/durability rollback path is testable cross-platform.
///
/// The handle is opened for WRITE (not read-only): on Windows `sync_all` maps to
/// `FlushFileBuffers`, which requires `GENERIC_WRITE` and returns ACCESS_DENIED on a
/// read-only handle (`File::open`). `write(true)` (no truncate) preserves the bytes and
/// yields a flushable handle on every platform.
pub(super) fn sync_marker_contents(path: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    if FAIL_MARKER_SYNC.with(std::cell::Cell::get) {
        return Err(std::io::Error::other(
            "test seam: forced marker sync_all failure",
        ));
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .sync_all()
}

#[cfg(test)]
thread_local! {
    static FAIL_MARKER_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test-only: arm/disarm the [`sync_marker_contents`] failure seam (current thread).
#[cfg(test)]
pub(super) fn set_fail_marker_sync(fail: bool) {
    FAIL_MARKER_SYNC.with(|c| c.set(fail));
}

/// #6: optional exact-head precondition — BEFORE any branch creation so a
/// mismatch returns a structured error with zero mutation.
pub(super) fn validate_expected_head(
    args: &Value,
    source_path: &str,
    branch: &str,
) -> Option<Value> {
    let expected = args["expected_head"].as_str()?;
    let is_full_hex = (expected.len() == 40 || expected.len() == 64)
        && expected.chars().all(|c| c.is_ascii_hexdigit());
    if !is_full_hex {
        return Some(json!({
            "error": format!("expected_head must be a full 40/64-hex SHA, got '{expected}'"),
            "code": "invalid_expected_head",
        }));
    }
    let src = Path::new(source_path);
    let verify = crate::git_helpers::git_cmd(
        src,
        &["rev-parse", "--verify", &format!("{expected}^{{commit}}")],
    );
    if verify.is_err() {
        return Some(json!({
            "error": format!(
                "expected_head {expected} does not exist as a commit in the repository"
            ),
            "code": "expected_head_mismatch",
            "expected_head": expected,
            "actual_head": "",
        }));
    }
    let branch_ref = format!("refs/heads/{branch}");
    let branch_exists = crate::git_helpers::git_cmd(src, &["rev-parse", "--verify", &branch_ref]);
    let actual = if let Ok(sha) = branch_exists {
        sha.trim().to_string()
    } else {
        let default_base = format!("origin/{}", crate::git_helpers::default_branch(src));
        let from_ref = args["from_ref"].as_str().unwrap_or(&default_base);
        crate::git_helpers::git_cmd(src, &["rev-parse", from_ref])
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    if !actual.eq_ignore_ascii_case(expected) {
        return Some(json!({
            "error": format!("expected_head {expected} does not match branch HEAD {actual}"),
            "code": "expected_head_mismatch",
            "expected_head": expected,
            "actual_head": actual,
        }));
    }
    None
}
