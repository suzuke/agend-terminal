//! Verification / reproduction tests for the `mcp-core-surface` review batch.
//!
//! Each `#[test]` encodes the CORRECT expected behavior of an MCP
//! instance-handler and PASSES (green) on current code (the cited fixes have
//! landed); they run un-ignored as live regression guards.
//!
//! Placement mirrors the sibling `instance_964_tests.rs`: this submodule of
//! `crate::mcp::handlers` reaches the handlers via `super::` re-exports and the
//! `pub(in crate::mcp::handlers)` test seam `spawn_single_instance_impl`.
//!
//! All daemon-bound handlers are driven against a FRESH temp `home` with no
//! `run/` dir, so `crate::api::call` resolves no active daemon and returns the
//! deterministic "no active daemon" / "API unavailable" error — this is the
//! current (buggy) terminal state these tests assert AGAINST. After the fix,
//! the boundary validation short-circuits BEFORE that RPC.

use super::instance_state::spawn::spawn_single_instance_impl;
use serde_json::{json, Value};
use std::path::Path;

/// Unique, isolated temp $AGEND_HOME seeded with an empty fleet.yaml. No `run/`
/// dir is created, so `crate::api::call` finds no active daemon for this home.
fn tmp_home(slug: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let id = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-review-mcp-core-surface-{}-{}-{}",
        slug,
        std::process::id(),
        id
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp home dir");
    std::fs::write(crate::fleet::fleet_yaml_path(&dir), "instances: {}\n")
        .expect("seed empty fleet.yaml");
    dir
}

fn error_str(v: &Value) -> String {
    v.get("error")
        .and_then(|e| e.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Finding 1 (high/security): `create_instance(branch="main")` must be rejected
/// by the E4.5 protected-branch guard — the same invariant `worktree_pool::lease`
/// and `bind_self` already enforce. Today the spawn path only calls
/// `validate_branch`, which permits "main"/"master", so the protected branch is
/// accepted and a worktree on `main` would be created.
///
/// We drive the real test seam `spawn_single_instance_impl` with a stub SPAWN
/// fn (no daemon needed). With branch="main" the current code completes
/// "successfully" (returns `{"name":...,"backend":...}` with NO `error` key).
/// After the fix it must return an `error` mentioning the protected branch.
#[test]
fn create_instance_branch_main_rejected_e4_5_mcp_core_surface() {
    let home = tmp_home("f1-branch-main");

    // Stub SPAWN: would succeed if the code ever reached it. The point is that
    // for a protected branch the handler must error BEFORE this runs.
    let spawn_fn = |_h: &Path, _req: &Value| -> anyhow::Result<Value> {
        Ok(json!({ "ok": true, "result": { "topic_id": 1_i64 } }))
    };

    let result = spawn_single_instance_impl(
        &home,
        "lead",
        &json!({ "name": "f1-main-instance", "backend": "claude", "branch": "main" }),
        &spawn_fn,
    );

    assert!(
        result.get("error").is_some(),
        "E4.5: create_instance(branch=\"main\") must be REJECTED (protected branch \
         cannot back an agent worktree); got success response: {result}"
    );
    let err = error_str(&result).to_lowercase();
    assert!(
        err.contains("main") || err.contains("protected") || err.contains("e4.5"),
        "E4.5 rejection error must reference the protected branch 'main'; got: {result}"
    );

    let _ = std::fs::remove_dir_all(&home);
}

/// Finding 2 (high/security): the team-mode branch of `handle_create_instance`
/// forwards `args["team"]` into the CREATE_TEAM RPC WITHOUT `validate_name`,
/// so a traversal team name like "../../tmp/evil" reaches the daemon and
/// becomes member names + workspace dirs outside the workspace root.
///
/// With no active daemon the unvalidated name currently produces an
/// "API unavailable" RPC error (proving the bad name reached the RPC layer).
/// After the fix, `validate_name_or_err!(team_name)` rejects it at the MCP
/// boundary with a "... invalid characters ..." error BEFORE the RPC.
#[test]
fn create_instance_team_name_traversal_rejected_mcp_core_surface() {
    let home = tmp_home("f2-team-traversal");

    let result = super::instance::handle_create_instance(
        &home,
        &json!({ "team": "../../tmp/evil-mcp-core-surface", "count": 1 }),
        "lead",
    );

    let err = error_str(&result).to_lowercase();
    assert!(
        err.contains("invalid"),
        "a traversal team name must be REJECTED by validate_name at the MCP \
         boundary (expected an 'invalid characters' error), NOT forwarded to \
         the CREATE_TEAM RPC; got: {result}"
    );

    let _ = std::fs::remove_dir_all(&home);
}

/// Finding 3 (medium/resource-leak): team-mode `count` is read as u64 and cast
/// to usize with NO upper bound, then used to size `vec![backend; count]`. An
/// absurd count would request a huge allocation (OOM/abort DoS). We use a
/// safe-but-oversized count (10000 > any sane cap) so the test harness does NOT
/// abort; the bug is that this oversized count is NOT rejected and instead
/// flows straight to the CREATE_TEAM RPC.
///
/// Current behavior (no daemon): the oversized vec is built and the handler
/// returns an "API unavailable" RPC error — proving no boundary cap exists.
/// After the fix (e.g. reject count>64), an oversized count returns a cap error
/// BEFORE the RPC, so the error contains neither "API unavailable" nor
/// "no active daemon".
#[test]
fn create_instance_team_count_capped_mcp_core_surface() {
    let home = tmp_home("f3-count-cap");

    let result = super::instance::handle_create_instance(
        &home,
        &json!({ "team": "cap-team-mcp-core-surface", "count": 10000 }),
        "lead",
    );

    let err = error_str(&result).to_lowercase();
    assert!(
        result.get("error").is_some(),
        "an oversized team count must be REJECTED with a cap error; got: {result}"
    );
    assert!(
        !err.contains("api unavailable") && !err.contains("no active daemon"),
        "oversized count must be rejected at the MCP boundary BEFORE the \
         CREATE_TEAM RPC (a sane cap, e.g. count<=64), not reach the daemon \
         call; got an RPC-layer error: {result}"
    );

    let _ = std::fs::remove_dir_all(&home);
}

/// Finding 4 (info/correctness): `handle_clear_blocked_reason` accepts
/// `args["instance"]` and forwards it to CLEAR_BLOCKED_REASON without
/// `validate_name`, unlike its siblings `handle_interrupt` /
/// `handle_pane_snapshot` in the same file.
///
/// With no daemon the unvalidated malformed name reaches the RPC and yields a
/// "no active daemon" error (NOT a validation error). After adding
/// `validate_name_or_err!(instance)`, the malformed name is rejected at the
/// boundary with a "... invalid characters ..." error.
#[test]
fn clear_blocked_reason_validates_instance_name_mcp_core_surface() {
    let home = tmp_home("f4-clear-blocked");

    let result = super::instance::handle_clear_blocked_reason(
        &home,
        &json!({ "instance": "../evil-mcp-core-surface" }),
    );

    let err = error_str(&result).to_lowercase();
    assert!(
        err.contains("invalid"),
        "a malformed instance name must be REJECTED by validate_name at the MCP \
         boundary (expected an 'invalid characters' error, mirroring \
         handle_interrupt / handle_pane_snapshot), NOT forwarded to the \
         CLEAR_BLOCKED_REASON RPC; got: {result}"
    );

    let _ = std::fs::remove_dir_all(&home);
}
