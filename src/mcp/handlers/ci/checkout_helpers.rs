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
