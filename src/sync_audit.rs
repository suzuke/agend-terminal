//! Sprint 52 Invariant 1 — Lock ordering audit.
//!
//! Thread-local tier tracking to detect lock ordering violations at runtime.
//! Router thread is forbidden from acquiring L1 (registry) or L2 (agent_core).
//!
//! Behavior:
//! - `cargo test`: panics on violation (fails the test).
//! - Release with `AGEND_LOCK_AUDIT=1`: logs error, no panic.
//! - Default release: macros compile to `()` — zero overhead.
//!
//! #941: extended with a global `REGISTRY_HOLDER` slot for the
//! [`crate::agent::lock_registry_tracked`] wrapper — feeds the
//! periodic thread-dump observability handler. Gated by
//! `AGEND_DAEMON_THREAD_DUMP_SECS` (parsed once via [`thread_dump_enabled`]
//! into a `OnceLock<bool>`; cannot be live-toggled after daemon start).

use std::cell::Cell;
use std::sync::OnceLock;
use std::time::Instant;

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
#[allow(dead_code)] // macro-only consumer in production builds; used by tests
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
#[allow(dead_code)] // consumed by `lock_tier_assert!` macro + test paths
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
#[allow(dead_code)] // consumed by test paths; release builds don't pair release w/ acquire
pub fn lock_released(tier: u8) {
    if !cfg!(debug_assertions) && std::env::var("AGEND_LOCK_AUDIT").is_err() {
        return;
    }
    let current = CURRENT_TIER.get();
    if current == tier {
        CURRENT_TIER.set(0); // simplified: reset to 0 on release
    }
}

// ── #1492: lock-across-self-IPC deadlock detection (debug-only) ──────────
//
// The morning cron deadlock (#1479/#1483 neighborhood) was: a thread held the
// registry lock and then made a self-IPC call (`api::call` over the loopback
// socket, reachable via `enqueue_with_idle_hint`). The loopback API handler
// needs the SAME registry lock → both sides wait forever. The integration
// "restart smoke test" (#1481) only catches it after a real restart, which is
// flaky and slow.
//
// This makes the bug catchable by ANY unit test that exercises the bad path:
// a thread-local depth counter is bumped while the registry lock is held, and
// the self-IPC vectors panic (debug builds only) if it's nonzero. Release
// builds compile every entry/exit/assert to a no-op — zero overhead.

#[cfg(debug_assertions)]
thread_local! {
    /// How many registry locks this thread currently holds (0 = none).
    static REGISTRY_LOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Record that this thread acquired the registry lock. Debug-only; release no-op.
#[cfg(debug_assertions)]
pub fn registry_lock_entered() {
    REGISTRY_LOCK_DEPTH.with(|c| c.set(c.get().saturating_add(1)));
}
/// Release-build no-op (zero overhead).
#[cfg(not(debug_assertions))]
#[inline(always)]
pub fn registry_lock_entered() {}

/// Record that this thread released the registry lock. Debug-only; release no-op.
#[cfg(debug_assertions)]
pub fn registry_lock_exited() {
    REGISTRY_LOCK_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
}
/// Release-build no-op (zero overhead).
#[cfg(not(debug_assertions))]
#[inline(always)]
pub fn registry_lock_exited() {}

/// Panic (debug builds only) if a self-IPC vector is entered while this thread
/// holds the registry lock — that ordering deadlocks the daemon (#1492). `ctx`
/// names the vector for the panic message. Release builds: no-op, zero cost.
#[cfg(debug_assertions)]
pub fn assert_no_registry_lock_for_self_ipc(ctx: &str) {
    let depth = REGISTRY_LOCK_DEPTH.with(|c| c.get());
    if depth > 0 {
        panic!(
            "#1492 lock-across-self-IPC deadlock risk: `{ctx}` was called while this thread holds \
             the registry lock (depth={depth}). The loopback API handler needs the same lock, so \
             this self-IPC would deadlock the daemon. Drop the registry guard BEFORE the call \
             (see commit 6f1403d and docs/DAEMON-LOCK-ORDERING.md)."
        );
    }
}
/// Release-build no-op (zero overhead).
#[cfg(not(debug_assertions))]
#[inline(always)]
pub fn assert_no_registry_lock_for_self_ipc(_ctx: &str) {}

/// Convenience macro for lock tier assertion + acquisition tracking.
#[macro_export]
macro_rules! lock_tier_assert {
    ($tier:expr, $name:expr) => {
        $crate::sync_audit::assert_lock_tier($tier, $name);
        $crate::sync_audit::lock_acquired($tier);
    };
}

// ── #941: registry-lock holder tracking for thread-dump observability ──
//
// `REGISTRY_HOLDER` is updated by [`crate::agent::lock_registry_tracked`]
// on acquire and cleared by the returned `RegistryGuard`'s `Drop`. The
// periodic thread-dump handler in `daemon::per_tick::thread_dump` reads
// it via [`current_registry_holder`].
//
// **Wrapper-only blind spot** (documented in PR body): ~30 bare
// `reg.lock()` call sites bypass this tracker. Operator interpreting
// `registry_holder=None` in a dump MUST NOT conclude "no wedge" —
// non-handler sites are not visible here. The dump's load-bearing value
// is for the per-tick handler wedge case (#932 RCA H1 hypothesis).

#[derive(Debug, Clone)]
pub struct HolderInfo {
    pub thread_name: String,
    pub acquired_at: Instant,
    pub site_label: &'static str,
}

static REGISTRY_HOLDER: parking_lot::Mutex<Option<HolderInfo>> = parking_lot::Mutex::new(None);

/// Cached env-var check — parsed once on first call into a `OnceLock<bool>`.
/// Operator must restart the daemon to change the gate; live toggling is
/// explicitly not supported (cost: per-call atomic load only after init).
pub fn thread_dump_enabled() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("AGEND_DAEMON_THREAD_DUMP_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(|n| n > 0)
            .unwrap_or(false)
    })
}

/// Update `REGISTRY_HOLDER` with the current thread's identity + site
/// label. Called by `lock_registry_tracked` immediately AFTER the
/// `reg.lock()` returns. No-op when [`thread_dump_enabled`] returns
/// false (default).
pub fn set_registry_holder(site: &'static str) {
    if !thread_dump_enabled() {
        return;
    }
    let info = HolderInfo {
        thread_name: std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string(),
        acquired_at: Instant::now(),
        site_label: site,
    };
    *REGISTRY_HOLDER.lock() = Some(info);
}

/// Clear `REGISTRY_HOLDER`. Called by `RegistryGuard::drop` AFTER the
/// underlying parking_lot MutexGuard is implicitly dropped (so the
/// observability slot is freed strictly after the real lock is freed).
pub fn clear_registry_holder() {
    if !thread_dump_enabled() {
        return;
    }
    *REGISTRY_HOLDER.lock() = None;
}

/// Snapshot the current holder for the periodic dump handler. Cloned
/// because the dump handler runs on a different thread than the holder.
pub fn current_registry_holder() -> Option<HolderInfo> {
    REGISTRY_HOLDER.lock().clone()
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

    // ── #1492: lock-across-self-IPC deadlock detection ──
    // (Mechanism-level here; the real `lock_registry` guard wiring is tested
    // in `agent::mod` tests — `agent` is bin-only, not in this shared lib.)

    /// No registry lock held → the self-IPC assert is a no-op (no panic). Holds
    /// in both debug and release (release is a no-op unconditionally).
    #[test]
    fn self_ipc_assert_is_noop_without_lock() {
        assert_no_registry_lock_for_self_ipc("test-vector");
    }

    /// While the depth flag is set (as `lock_registry` does on acquire), a
    /// self-IPC vector trips the assert. Debug-only — the detection compiles
    /// out in release, so the panic only exists there.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "lock-across-self-IPC")]
    fn self_ipc_assert_panics_while_lock_depth_nonzero() {
        registry_lock_entered();
        assert_no_registry_lock_for_self_ipc("api::call");
    }

    /// The correct pattern (release the lock BEFORE self-IPC — the 6f1403d fix
    /// shape, modeled here by a balanced enter/exit) must NOT trip the assert.
    #[cfg(debug_assertions)]
    #[test]
    fn self_ipc_assert_ok_after_balanced_enter_exit() {
        registry_lock_entered();
        registry_lock_exited();
        assert_no_registry_lock_for_self_ipc("api::call");
    }
}
