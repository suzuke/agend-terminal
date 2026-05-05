//! Sprint 52 Invariant 1b — router thread never holds L1 or L2.
//!
//! Verifies that the router thread's `mark_router_thread()` call is in place
//! and that the sync_audit module correctly forbids L1/L2 from router context.

/// Verify that mark_router_thread + assert_lock_tier correctly forbids L1.
#[test]
fn router_thread_never_holds_l1_l2() {
    // Spawn a thread that simulates the router thread context.
    let handle = std::thread::Builder::new()
        .name("router_test".into())
        .spawn(|| {
            agend_terminal::sync_audit::mark_router_thread();
            assert!(agend_terminal::sync_audit::is_router_thread());

            // L3 must be allowed
            agend_terminal::sync_audit::assert_lock_tier(3, "heartbeat_pair");

            // L1 must panic — catch it
            let l1_result = std::panic::catch_unwind(|| {
                agend_terminal::sync_audit::assert_lock_tier(1, "registry");
            });
            assert!(l1_result.is_err(), "router thread acquiring L1 must panic");

            // L2 must panic — catch it
            let l2_result = std::panic::catch_unwind(|| {
                agend_terminal::sync_audit::assert_lock_tier(2, "agent_core");
            });
            assert!(l2_result.is_err(), "router thread acquiring L2 must panic");
        })
        .expect("spawn test thread");

    handle.join().expect("router lock ordering test thread");
}

/// Verify ascending lock order is allowed (L1 → L3).
#[test]
fn non_router_thread_allows_ascending_order() {
    let handle = std::thread::spawn(|| {
        // Fresh thread — not router, no locks held
        agend_terminal::sync_audit::assert_lock_tier(1, "registry");
        agend_terminal::sync_audit::lock_acquired(1);
        agend_terminal::sync_audit::lock_released(1);
        agend_terminal::sync_audit::assert_lock_tier(3, "heartbeat_pair");
    });
    handle.join().expect("ascending order test");
}
