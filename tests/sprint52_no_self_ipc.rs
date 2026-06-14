//! Sprint 52 Invariant 2 — No supervisor/router self-IPC.
//!
//! Scans src/daemon/router.rs and src/daemon/supervisor.rs for forbidden
//! patterns that would re-introduce the Sprint 49 deadlock root cause.

#[test]
fn router_does_not_call_daemon_api() {
    let src = include_str!("../src/daemon/router.rs");
    for (i, line) in src.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with("///") {
            continue;
        }
        assert!(
            !line.contains("api::call("),
            "router.rs line {} contains forbidden api::call: {line}",
            i + 1
        );
        assert!(
            !line.contains("crate::api::call("),
            "router.rs line {} contains forbidden crate::api::call: {line}",
            i + 1
        );
    }
}

#[test]
fn supervisor_does_not_call_daemon_api() {
    let src = include_str!("../src/daemon/supervisor.rs");
    for (i, line) in src.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with("///") {
            continue;
        }
        assert!(
            !line.contains("api::call("),
            "supervisor.rs line {} contains forbidden api::call: {line}",
            i + 1
        );
        assert!(
            !line.contains("crate::api::call("),
            "supervisor.rs line {} contains forbidden crate::api::call: {line}",
            i + 1
        );
    }
}

#[test]
fn router_allows_direct_pty_and_heartbeat() {
    // Positive regression pin: the router keeps the direct-dispatch paths it
    // is ALLOWED to use (the no-self-ipc rule forbids self-targeting, not
    // these). The previous version discarded every `contains(...)` into `_`
    // and only asserted `!src.is_empty()` — vacuous, since `include_str!` of a
    // non-empty file can never be empty (it would not compile). Assert the
    // direct paths are actually present so their removal is caught.
    let src = include_str!("../src/daemon/router.rs");
    assert!(
        src.contains("heartbeat_pair"),
        "router.rs must keep the direct heartbeat_pair path"
    );
    assert!(
        src.contains("channel::send_from_agent"),
        "router.rs must keep the direct channel::send_from_agent path"
    );
}
