//! Sprint 52 Invariant 1 — Lock ordering audit.
//!
//! Thread-local tier tracking to detect lock ordering violations at runtime.
//! Router thread is forbidden from acquiring L1 (registry) or L2 (agent_core).
//!
//! Behavior:
//! - `cargo test`: panics on violation (fails the test).
//! - Release with `AGEND_LOCK_AUDIT=1`: logs error, no panic.
//! - Default release: macros compile to `()` — zero overhead.

use std::cell::Cell;

thread_local! {
    /// Current highest lock tier held by this thread. 0 = no lock held.
    static CURRENT_TIER: Cell<u8> = const { Cell::new(0) };
    /// If true, this thread is the router thread — L1/L2 acquisition is forbidden.
    static IS_ROUTER_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Mark the current thread as the router thread (forbids L1/L2 acquisition).
pub fn mark_router_thread() {
    IS_ROUTER_THREAD.set(true);
}

/// Check if the current thread is marked as the router thread.
pub fn is_router_thread() -> bool {
    IS_ROUTER_THREAD.get()
}

/// Assert that acquiring `tier` is safe given the current thread's state.
/// Panics in test/debug builds; logs in release with AGEND_LOCK_AUDIT=1.
#[inline]
pub fn assert_lock_tier(tier: u8, lock_name: &str) {
    if !cfg!(debug_assertions) && std::env::var("AGEND_LOCK_AUDIT").is_err() {
        return; // zero overhead in default release
    }

    if IS_ROUTER_THREAD.get() && tier <= 2 {
        let msg = format!(
            "LOCK ORDERING VIOLATION: router thread attempted to acquire {lock_name} (tier {tier}) — forbidden (max tier 3)"
        );
        if cfg!(test) || cfg!(debug_assertions) {
            panic!("{msg}");
        } else {
            tracing::error!("{msg}");
        }
    }

    let current = CURRENT_TIER.get();
    if current > 0 && tier <= current {
        let msg = format!(
            "LOCK ORDERING VIOLATION: attempted to acquire {lock_name} (tier {tier}) while holding tier {current}"
        );
        if cfg!(test) || cfg!(debug_assertions) {
            panic!("{msg}");
        } else {
            tracing::error!("{msg}");
        }
    }
}

/// Record that a lock at `tier` has been acquired.
#[inline]
pub fn lock_acquired(tier: u8) {
    if !cfg!(debug_assertions) && std::env::var("AGEND_LOCK_AUDIT").is_err() {
        return;
    }
    let current = CURRENT_TIER.get();
    if tier > current {
        CURRENT_TIER.set(tier);
    }
}

/// Record that a lock at `tier` has been released.
#[inline]
pub fn lock_released(tier: u8) {
    if !cfg!(debug_assertions) && std::env::var("AGEND_LOCK_AUDIT").is_err() {
        return;
    }
    let current = CURRENT_TIER.get();
    if current == tier {
        CURRENT_TIER.set(0); // simplified: reset to 0 on release
    }
}

/// Convenience macro for lock tier assertion + acquisition tracking.
#[macro_export]
macro_rules! lock_tier_assert {
    ($tier:expr, $name:expr) => {
        $crate::sync_audit::assert_lock_tier($tier, $name);
        $crate::sync_audit::lock_acquired($tier);
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_assert_allows_ascending_order() {
        // L1 then L3 is fine
        CURRENT_TIER.set(0);
        IS_ROUTER_THREAD.set(false);
        assert_lock_tier(1, "registry");
        lock_acquired(1);
        lock_released(1);
        assert_lock_tier(3, "heartbeat_pair");
    }

    #[test]
    #[should_panic(expected = "LOCK ORDERING VIOLATION")]
    fn tier_assert_panics_on_descending_order() {
        CURRENT_TIER.set(0);
        IS_ROUTER_THREAD.set(false);
        assert_lock_tier(3, "heartbeat_pair");
        lock_acquired(3);
        // This should panic: acquiring L1 while holding L3
        assert_lock_tier(1, "registry");
    }

    #[test]
    #[should_panic(expected = "router thread")]
    fn router_thread_cannot_acquire_l1() {
        CURRENT_TIER.set(0);
        IS_ROUTER_THREAD.set(true);
        assert_lock_tier(1, "registry");
    }

    #[test]
    #[should_panic(expected = "router thread")]
    fn router_thread_cannot_acquire_l2() {
        CURRENT_TIER.set(0);
        IS_ROUTER_THREAD.set(true);
        assert_lock_tier(2, "agent_core");
    }

    #[test]
    fn router_thread_can_acquire_l3() {
        CURRENT_TIER.set(0);
        IS_ROUTER_THREAD.set(true);
        // L3 is allowed for router thread
        assert_lock_tier(3, "heartbeat_pair");
    }
}
