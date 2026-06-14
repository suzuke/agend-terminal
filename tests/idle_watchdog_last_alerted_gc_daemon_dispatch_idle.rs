//! Resource-leak repro (daemon-dispatch-idle batch): `idle_watchdog`'s long-lived
//! `last_alerted` HashMap (`AntiStallTracker`-style per-tracker map) accumulates one
//! `("dev", agent)` / `("fleet", "*")` entry per distinct agent name ever seen and
//! is NEVER pruned. `scan_dev_vantage` / `scan_fleet_vantage` only ever `insert`;
//! nothing removes entries for agents that have been deleted / redeployed.
//!
//! The sibling `anti_stall.rs` already solved exactly this: after each scan it calls
//! `last_emitted.retain(...)` to GC dedup entries for tasks no longer `InProgress`.
//! `idle_watchdog` has no equivalent `retain`, so its map grows unbounded over a
//! long-running daemon with churning instance names.
//!
//! METHOD: static_invariant (source-scan), mirroring `tests/core_mutex_invariant.rs`
//! and the established `last_emitted.retain` GC pattern next door. The leak is a
//! slow unbounded growth and the map is a private per-tracker field. The FIX SHAPE
//! is an anti_stall-style `retain` GC over `last_alerted`, or a delete-hook
//! companion that prunes the map. We assert GC discipline is present in prod source.
//!
//! RED now: idle_watchdog.rs prod source contains ZERO `last_alerted.retain(` (no
//! GC) → assertion fails.
//! GREEN after fix: a `last_alerted.retain(...)` call (drop keys whose agent is no
//! longer tracked) lands in `scan_and_emit` / a vantage scan, mirroring anti_stall.

use std::path::PathBuf;

#[test]
#[ignore = "daemon-dispatch-idle idle-watchdog-last-alerted-leak: red until fix; remove #[ignore] after fix to confirm"]
fn idle_watchdog_last_alerted_map_is_gc_pruned_daemon_dispatch_idle() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("daemon")
        .join("idle_watchdog.rs");
    let src = std::fs::read_to_string(&path).expect("read idle_watchdog.rs");

    // Slice off the test module so test fixtures don't satisfy the scan.
    let cfg_test = ["#[cfg(", "test)]"].concat();
    let prod = match src.find(&cfg_test) {
        Some(i) => &src[..i],
        None => &src[..],
    };

    // Sanity: the map exists and is inserted into (the thing that must be GC'd).
    let insert_needle = ["last_alerted", ".insert("].concat();
    assert!(
        prod.contains(&insert_needle),
        "idle_watchdog must still insert into last_alerted — guard anchor missing, \
         re-point this test"
    );

    // The fix: a retain-based GC over last_alerted, mirroring anti_stall's
    // `last_emitted.retain(...)` so stale agent keys are dropped instead of leaked.
    let retain_needle = ["last_alerted", ".retain("].concat();
    assert!(
        prod.contains(&retain_needle),
        "idle_watchdog's last_alerted HashMap is never pruned — it grows one stale \
         entry per distinct agent name ever seen (deleted/redeployed agents linger \
         forever). Add a GC mirroring anti_stall.rs's `last_emitted.retain(...)`: \
         retain only keys whose agent is still tracked (e.g. `\"*\"` or present in \
         enumerate_agent_activity / the fleet), or prune via an \
         idle_watchdog::remove_agent companion on the agent-delete hook."
    );
}
