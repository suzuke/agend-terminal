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

// #1024 (closes #1002 ROOT 2): the reviewed_head-forwarding regression is now a
// BEHAVIORAL test — `send_envelope::tests::reviewed_head_from_args_reaches_send_params_1024`
// (plus the fixed-gap fallback pin `to_inbox_message_carries_full_directive_set_fixed_gap_1024_1833`)
// — replacing this brittle source-text grep, which broke on the smells#2
// SendEnvelope refactor though the behavior was preserved (source-grep tests
// are themselves a flagged de2eb8 smell / Pattern A).
