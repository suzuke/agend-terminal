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

// ── #1492: lock-across-self-IPC deadlock detection (always-on) ───────────
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
// the self-IPC vectors refuse (return `Err`) if it's nonzero.
//
// #1492-L2 (decision d-20260531171228817178-0): the depth counters + the
// self-IPC guard are now ALWAYS-ON (previously debug-only, cfg-gated, with
// release no-ops). The counters are pure thread-local `Cell` bumps (no atomics,
// no contention) on control-plane locks, so the steady-state cost is
// negligible; in exchange a lock-across-self-IPC in a RELEASE daemon now
// fail-fasts to a logged `Err` instead of freezing the whole daemon. Pairs with
// the structural collect→drop→emit invariant (#1530) and the #1571 timeout
// backstop as defense-in-depth.

thread_local! {
    /// How many registry locks this thread currently holds (0 = none).
    static REGISTRY_LOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Record that this thread acquired the registry lock.
pub fn registry_lock_entered() {
    REGISTRY_LOCK_DEPTH.with(|c| c.set(c.get().saturating_add(1)));
}

/// Record that this thread released the registry lock.
pub fn registry_lock_exited() {
    REGISTRY_LOCK_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
}

// ── #1535: per-agent core-lock depth tracking + CoreMutex ────────────────
//
// The #1492 guard above only tracks the REGISTRY lock. But the supervisor
// per-agent loop holds the `AgentCore` mutex (the registry lock is already
// released — handles are Arc-cloned out first) while some reactions self-IPC
// (`api::call(INJECT)` → orchestrator). That core-held self-IPC ALSO deadlocks
// — the loopback handler locks the registry AND the target agent's core — yet
// was invisible to the registry-only guard (#1530 surfaced it; #1535 closes
// it). [`CoreMutex`] makes EVERY core lock bump `CORE_LOCK_DEPTH` so the same
// self-IPC assert covers the core-held case too.

thread_local! {
    /// How many `AgentCore` locks this thread currently holds (0 = none).
    /// A single total-depth counter is sufficient: the rule is "no self-IPC
    /// while holding ANY core lock", so per-agent identity is irrelevant.
    static CORE_LOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Record that this thread acquired a core lock.
fn core_lock_entered() {
    CORE_LOCK_DEPTH.with(|c| c.set(c.get().saturating_add(1)));
}

/// Record that this thread released a core lock.
fn core_lock_exited() {
    CORE_LOCK_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
}

// ── #1629: fs4 advisory-lock (flock) depth tracking ──────────────────────
//
// The #1492/#1535 guard tracks the registry + core locks. A THIRD tier — `fs4`
// advisory file locks (flocks) acquired via [`crate::store::acquire_file_lock`]
// — was invisible to it: holding a flock across a self-IPC (loopback `api::call`
// / `enqueue_with_idle_hint`) is the same lock-while-blocking deadlock class as
// #1617, yet bumped neither counter — so every flock-while-blocking bug
// (#1617/#1342/#1340/#1624) had to be caught by manual review. The
// [`crate::store::FileFlockGuard`] returned by `acquire_file_lock` bumps
// `FLOCK_DEPTH` for its lifetime so the same self-IPC assert covers the flock
// tier too.
//
// The 2 daemon-singleton `.daemon.lock` raw `fs4::try_lock` sites
// (`bootstrap::acquire_daemon_lock`, `daemon::run`) deliberately bypass
// `acquire_file_lock` and MUST NOT bump: they hold for the daemon's whole life,
// which would pin the depth > 0 and false-trip every self-IPC.

thread_local! {
    /// How many `acquire_file_lock` flocks this thread currently holds (0 = none).
    /// A single total-depth counter suffices: the rule is "no self-IPC while
    /// holding ANY flock", so per-path identity is irrelevant.
    static FLOCK_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Record that this thread acquired an `acquire_file_lock` flock.
pub fn flock_entered() {
    FLOCK_DEPTH.with(|c| c.set(c.get().saturating_add(1)));
}

/// Record that this thread released an `acquire_file_lock` flock.
pub fn flock_exited() {
    FLOCK_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
}

/// #1886: this thread's current `acquire_file_lock` flock nesting depth. Used by
/// the leaf-lock invariant test to assert a `with_json_state` RMW closure runs at
/// depth 1 (the helper's own lock, no nested file-flock). Test-only — used from
/// the bin-side `store` test tree, so the lib-test build sees no caller.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn flock_depth() -> u32 {
    FLOCK_DEPTH.with(|c| c.get())
}

/// #1535: a `parking_lot::Mutex` wrapper for the per-agent `AgentCore` that
/// tracks lock depth via [`CORE_LOCK_DEPTH`]. `.lock()` is API-compatible with
/// `parking_lot::Mutex::lock` (the returned [`CoreGuard`] `Deref`s to `T`), so
/// the ~50 `.core.lock()` call sites are unchanged. The guard decrements the
/// depth on drop. Release builds: the depth calls compile to no-ops, so this is
/// a zero-overhead newtype.
///
/// This is the SOLE constructor for an `AgentCore` mutex — enforced by
/// `tests/core_mutex_invariant.rs` so a future bare `Mutex<AgentCore>` cannot
/// silently reintroduce the #1492 core-lock blind spot.
pub struct CoreMutex<T> {
    inner: parking_lot::Mutex<T>,
}

impl<T> CoreMutex<T> {
    pub fn new(value: T) -> Self {
        Self {
            inner: parking_lot::Mutex::new(value),
        }
    }

    /// Lock the core, bumping [`CORE_LOCK_DEPTH`] for the guard's lifetime.
    pub fn lock(&self) -> CoreGuard<'_, T> {
        let guard = self.inner.lock();
        core_lock_entered();
        CoreGuard { guard }
    }
}

/// RAII guard from [`CoreMutex::lock`]; decrements [`CORE_LOCK_DEPTH`] on drop.
pub struct CoreGuard<'a, T> {
    guard: parking_lot::MutexGuard<'a, T>,
}

impl<T> std::ops::Deref for CoreGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> std::ops::DerefMut for CoreGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for CoreGuard<'_, T> {
    fn drop(&mut self) {
        core_lock_exited();
    }
}

/// Return `Err` if a self-IPC vector is entered while this thread holds the
/// registry lock (#1492), any per-agent core lock (#1535), OR any `fs4` flock
/// acquired via [`crate::store::acquire_file_lock`] (#1629) — each ordering
/// deadlocks the daemon (the loopback API handler needs the registry lock and
/// may lock the target agent's core; a held flock stalls any thread contending
/// it). `ctx` names the vector (logged + carried in the error).
///
/// #1492-L2 (decision d-20260531171228817178-0): ALWAYS-ON, fail-fast `Err`
/// (was debug-only `panic!` + release no-op). On violation it logs at ERROR
/// with the named site + lock depths, then returns `Err` so the self-IPC
/// entrypoint refuses the deadlocking call and the daemon stays LIVE — instead
/// of freezing (the old release no-op gave zero release protection). The two
/// production vectors (`api::call`, `enqueue_with_idle_hint`) already return
/// `anyhow::Result`, so they propagate via `?` with no signature change. The
/// `#[must_use]` `Result` makes ignoring the refusal a compile error at any new
/// call site.
pub fn assert_no_registry_lock_for_self_ipc(ctx: &str) -> anyhow::Result<()> {
    let reg = REGISTRY_LOCK_DEPTH.with(|c| c.get());
    let core = CORE_LOCK_DEPTH.with(|c| c.get());
    let flock = FLOCK_DEPTH.with(|c| c.get());
    if reg > 0 || core > 0 || flock > 0 {
        tracing::error!(
            ctx,
            registry_depth = reg,
            core_depth = core,
            flock_depth = flock,
            "#1492/#1535/#1629 lock-across-self-IPC deadlock risk — refusing self-IPC (returning Err)"
        );
        anyhow::bail!(
            "#1492/#1535/#1629 lock-across-self-IPC deadlock risk: `{ctx}` was called while this thread \
             holds locks (registry depth={reg}, core depth={core}, flock depth={flock}). The loopback API \
             handler needs the registry lock and may lock the target agent's core, and an fs4 flock held \
             across this self-IPC stalls any thread contending it — so this would deadlock the daemon. \
             Drop the guard(s) BEFORE the call (collect under lock → drop → emit; see #1530, \
             docs/DAEMON-LOCK-ORDERING.md)."
        );
    }
    Ok(())
}

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

/// Cached parse of `AGEND_DAEMON_THREAD_DUMP_SECS` → interval seconds
/// (`0` = disabled). Parsed once on first call into a `OnceLock<u64>`; operator
/// must restart the daemon to change it (live toggling is not supported).
///
/// #t-23: single source for BOTH the registry-holder tracking gate
/// ([`thread_dump_enabled`]) and `ThreadDumpHandler`'s emit interval — these
/// previously parsed the env var independently.
pub fn thread_dump_interval_secs() -> u64 {
    static CACHE: OnceLock<u64> = OnceLock::new();
    *CACHE.get_or_init(|| {
        parse_thread_dump_interval(std::env::var("AGEND_DAEMON_THREAD_DUMP_SECS").ok())
    })
}

/// Pure parse of the raw `AGEND_DAEMON_THREAD_DUMP_SECS` value → interval
/// seconds (`0` = disabled / unset / unparseable). Split out from the cached
/// accessor above so tests can exercise the unset→0 / set→N mapping
/// deterministically without depending on the process-global `OnceLock`,
/// which is seeded once-per-process and cannot be reset by `remove_var`.
pub fn parse_thread_dump_interval(raw: Option<String>) -> u64 {
    raw.and_then(|s| s.parse::<u64>().ok()).unwrap_or(0)
}

/// Whether thread-dump observability is enabled — derived from
/// [`thread_dump_interval_secs`] (`> 0`).
pub fn thread_dump_enabled() -> bool {
    thread_dump_interval_secs() > 0
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

    /// No registry lock held → the guard returns `Ok` (allows the self-IPC).
    /// #1492-L2: always-on now (runs in release too, not just debug).
    #[test]
    fn self_ipc_guard_ok_without_lock() {
        assert!(assert_no_registry_lock_for_self_ipc("test-vector").is_ok());
    }

    /// #1492-L2: while the depth flag is set (as `lock_registry` does on
    /// acquire), the self-IPC guard refuses with `Err` (was a debug-only
    /// `panic!` pre-L2; now an always-on fail-fast `Err`). Balance the
    /// enter/exit so the always-on thread-local counter doesn't leak into the
    /// next test sharing this worker thread.
    #[test]
    fn self_ipc_guard_errs_while_lock_depth_nonzero() {
        registry_lock_entered();
        let result = assert_no_registry_lock_for_self_ipc("api::call");
        registry_lock_exited();
        assert!(
            result.is_err(),
            "guard must refuse self-IPC while a lock is held"
        );
    }

    /// The correct pattern (release the lock BEFORE self-IPC — the 6f1403d fix
    /// shape, modeled here by a balanced enter/exit) must NOT trip the guard.
    /// #1492-L2: always-on now (runs in release too).
    #[test]
    fn self_ipc_guard_ok_after_balanced_enter_exit() {
        registry_lock_entered();
        registry_lock_exited();
        assert!(assert_no_registry_lock_for_self_ipc("api::call").is_ok());
    }
}
