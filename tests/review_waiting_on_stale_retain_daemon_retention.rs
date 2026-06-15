//! Repro (daemon-retention batch): `WaitingOnStaleTracker.last_alerted_at`
//! grows unbounded — there is no `retain_active`/prune path, and the per-tick
//! `WaitingOnStaleHandler::run` never prunes. By contrast the sibling
//! `ConflictNotifyTracker` was explicitly given `retain_active` for exactly this
//! #1923 leak class and is driven each tick from its handler. Over a long-lived
//! daemon with churning/redeployed agents the waiting_on map accumulates one
//! permanent entry per agent that ever went stale, and a same-name redeploy
//! inherits a stale dedup timestamp that can false-suppress a real alert.
//!
//! Source-scanning invariant (mirrors tests/core_mutex_invariant.rs): assert
//! that (1) `src/daemon/waiting_on_stale.rs` DEFINES a `retain_active` method,
//! and (2) `src/daemon/per_tick/supervisor_trackers.rs` WIRES it into the
//! `WaitingOnStaleHandler` (a `retain_active(` call). RED now (neither present),
//! GREEN once the prune is added + driven per-tick.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

fn read_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// A non-comment line that contains `needle`. Comment/doc lines (which merely
/// MENTION the pattern, e.g. the module's mirror-pattern prose) are skipped so
/// they don't false-satisfy the invariant.
fn has_code_line_with(src: &str, needle: &str) -> bool {
    src.lines().any(|line| {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') {
            return false;
        }
        line.contains(needle)
    })
}

#[test]
fn waiting_on_stale_has_retain_active_and_is_wired_daemon_retention() {
    let tracker_src = read_src("src/daemon/waiting_on_stale.rs");
    let handler_src = read_src("src/daemon/per_tick/supervisor_trackers.rs");

    // (1) The tracker must DEFINE a prune method (mirroring
    //     ConflictNotifyTracker::retain_active). A bare `fn retain_active`
    //     definition in a non-comment line.
    let defines_retain = has_code_line_with(&tracker_src, "fn retain_active");

    // (2) The per-tick handler must CALL it (drive the prune each tick with the
    //     live registry set), exactly as ConflictNotifyHandler::run does.
    //     Scope the check to the WaitingOnStaleHandler region so a stray
    //     ConflictNotifyHandler `retain_active(` call cannot satisfy it.
    let wired = {
        let after = handler_src
            .split_once("struct WaitingOnStaleHandler")
            .map(|(_, rest)| rest)
            .unwrap_or("");
        // Bound the window at the NEXT handler struct so we only inspect the
        // WaitingOnStaleHandler block.
        let window = after
            .split_once("struct ConflictNotifyHandler")
            .map(|(head, _)| head)
            .unwrap_or(after);
        window.contains("retain_active(")
    };

    assert!(
        defines_retain && wired,
        "daemon-retention: WaitingOnStaleTracker must gain a `retain_active` prune \
         (mirroring ConflictNotifyTracker::retain_active for the #1923 leak class) \
         AND WaitingOnStaleHandler::run must call it each tick. \
         defines_retain_active={defines_retain}, handler_wires_retain_active={wired}. \
         Without it last_alerted_at grows one permanent entry per ever-stale agent \
         and a same-name redeploy inherits a stale dedup timestamp."
    );
}
