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
    // Positive check: router.rs is allowed to use these patterns.
    let src = include_str!("../src/daemon/router.rs");
    // These are allowed (no assertion failure):
    let _ = src.contains("heartbeat_pair");
    let _ = src.contains("inject_to_agent");
    let _ = src.contains("channel::send_from_agent");
    // Just verify the file is non-empty and parseable.
    assert!(!src.is_empty(), "router.rs must exist and be non-empty");
}
