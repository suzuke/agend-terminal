//! Sprint 55 P0-C — `dispatch_should_skip_auto_bind` helper tests.
//!
//! Located in this sibling file (loaded via `#[path]` from comms.rs) to
//! keep src/mcp/handlers/comms.rs under the file_size_invariant 700 LOC
//! ceiling. Same module layout pattern as the
//! `instance_state::lifecycle` split.

use super::dispatch_should_skip_auto_bind;
use serde_json::json;

#[test]
fn skip_auto_bind_when_bind_false() {
    let args = json!({"bind": false, "branch": "feat/x"});
    assert!(dispatch_should_skip_auto_bind(&args));
}

#[test]
fn proceed_auto_bind_when_bind_true() {
    let args = json!({"bind": true, "branch": "feat/x"});
    assert!(!dispatch_should_skip_auto_bind(&args));
}

#[test]
fn proceed_auto_bind_when_bind_absent() {
    // Backward-compat: 50+ existing dispatch sites omit `bind`; must
    // continue to auto-bind exactly as pre-P0-C.
    let args = json!({"branch": "feat/x"});
    assert!(!dispatch_should_skip_auto_bind(&args));
}

/// #1024 (closes #1002 ROOT 2): source-pin asserting `handle_send`
/// forwards the `reviewed_head` field to the API SEND params dict.
/// Pre-fix the field was silently dropped at the MCP boundary,
/// breaking the `auto_release::is_verdict_message` predicate's
/// `reviewed_head.is_some()` gate and stranding every MCP-send
/// verdict (zero `#1002 record_verdict` traces across all logs).
///
/// File-level positive pin (cross-platform-safe; same pattern as
/// `app::tests::flush_idle_notifications_wired_to_submit_aware_inject`
/// from #982 RC2). If a future refactor moves the params dict,
/// update this assertion alongside.
#[test]
fn handle_send_forwards_reviewed_head_to_api_params() {
    let source = std::fs::read_to_string("src/mcp/handlers/comms.rs")
        .or_else(|_| std::fs::read_to_string("agend-terminal/src/mcp/handlers/comms.rs"))
        .expect("source file must be readable from test cwd");
    assert!(
        source.contains("\"reviewed_head\": args[\"reviewed_head\"].as_str()"),
        "handle_send params dict must forward `reviewed_head` (#1024 / #1002 ROOT 2 fix). \
         Without this, MCP-send verdicts silently drop the field at the boundary and \
         record_verdict never fires."
    );
}
