//! Sprint 23 P0 — F6 heartbeat coordination via lock-around-pair.
//!
//! Closes Sprint 20 DAEMON.md §1 F6 (High, race): supervisor + main loop
//! tick both read agent metadata independently; an MCP heartbeat write
//! interleaved between the supervisor's two reads (`last_heartbeat` then
//! `waiting_on_since`) produced inconsistent observations — supervisor saw
//! "stale heartbeat with fresh waiting_on_since" → spurious stale-decay
//! firing on a wait the operator just set.
//!
//! ## Design (per Sprint 23 P0 dispatch d-20260427064xxx)
//!
//! Per-instance `Arc<Mutex<HeartbeatPair>>` registry. Both pair fields
//! (`heartbeat_at_ms`, `waiting_on_since_ms`) live behind the same lock
//! so any reader sees a consistent snapshot at lock acquisition time.
//!
//! ## Why lock-around-pair (not AtomicU64 split)
//!
//! Per dev-reviewer-2 threat model (m-20260427064xxx synthesis): the
//! fleet's threat is correctness-corruption (prompt-injection, capability
//! bypass), NOT DoS. Atomic per-field exposes inconsistent-pair window
//! (interleaved load/store between two atomic ops). Lock fits the actual
//! threat model. Pattern transfer: PR #233 F7 used `save_metadata_batch`
//! for the disk-side equivalent; this lock is the in-memory analogue.
//!
//! ## Lock-ordering invariant
//!
//! See `docs/DAEMON-LOCK-ORDERING.md` (Sprint 23 P0 deliverable). Summary:
//! `heartbeat_pair` lock is **leaf-level** — no other daemon lock may be
//! acquired while holding it. Specifically:
//!   - `agent_registry` lock MUST be released before acquiring pair lock
//!   - `core` lock (per-agent) MUST be released before acquiring pair lock
//!   - `configs` lock MUST be released before acquiring pair lock
//!
//! Violations risk deadlocks under concurrent supervisor tick + MCP
//! handler load.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// Paired in-memory heartbeat state — readers see consistent snapshot at
/// lock acquisition time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeartbeatPair {
    /// Last heartbeat timestamp, epoch ms. `0` = never recorded (agent
    /// just spawned, or pre-Sprint 23 backfill not applied).
    pub heartbeat_at_ms: u64,
    /// When the agent's current `waiting_on` started, epoch ms. `None` =
    /// not waiting (cleared OR never set).
    pub waiting_on_since_ms: Option<u64>,
}

/// Per-instance lock registry. Keys are agent names (per
/// `agent::validate_name`); values are the pair locks. Entries are created
/// lazily on first access via [`pair_for`].
fn registry() -> &'static Mutex<HashMap<String, Arc<Mutex<HeartbeatPair>>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<Mutex<HeartbeatPair>>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Get (or lazily create) the pair lock for `name`. Subsequent calls with
/// the same name return the same `Arc` so writers and readers share the
/// same `Mutex`.
pub fn pair_for(name: &str) -> Arc<Mutex<HeartbeatPair>> {
    let mut map = crate::sync::lock_poisoned(registry(), "heartbeat_pair_registry");
    map.entry(name.to_string()).or_default().clone()
}

/// Helper: load the pair as a snapshot. Acquires lock briefly, copies the
/// `Copy` struct, releases. Use when the caller only needs a read view.
pub fn snapshot_for(name: &str) -> HeartbeatPair {
    let pair = pair_for(name);
    let g = crate::sync::lock_poisoned(&pair, "heartbeat_pair");
    *g
}

/// Update the pair atomically. Acquires lock, applies `f`, releases.
/// Callers that also persist to disk MUST call `save_metadata_batch`
/// AFTER this fn returns (lock-ordering rule: pair lock is leaf-level;
/// disk I/O happens outside the lock).
pub fn update_with<F>(name: &str, f: F)
where
    F: FnOnce(&mut HeartbeatPair),
{
    let pair = pair_for(name);
    let mut g = crate::sync::lock_poisoned(&pair, "heartbeat_pair");
    f(&mut g);
}

/// Current epoch ms — convenience for callers updating `heartbeat_at_ms`.
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn pair_for_returns_same_arc_for_same_name() {
        let a = pair_for("test_same_arc_agent");
        let b = pair_for("test_same_arc_agent");
        assert!(
            Arc::ptr_eq(&a, &b),
            "pair_for must return the same Arc for the same name — Arc::ptr_eq false"
        );
    }

    #[test]
    fn pair_for_returns_distinct_arcs_for_distinct_names() {
        let a = pair_for("test_distinct_a");
        let b = pair_for("test_distinct_b");
        assert!(
            !Arc::ptr_eq(&a, &b),
            "pair_for must return distinct Arcs for distinct names"
        );
    }

    #[test]
    fn update_with_persists_changes_visible_to_subsequent_snapshot() {
        update_with("test_update_persist", |p| {
            p.heartbeat_at_ms = 12345;
            p.waiting_on_since_ms = Some(67890);
        });
        let snap = snapshot_for("test_update_persist");
        assert_eq!(snap.heartbeat_at_ms, 12345);
        assert_eq!(snap.waiting_on_since_ms, Some(67890));
    }

    /// F6 race regression: concurrent reader + writer must NEVER observe
    /// an inconsistent pair (heartbeat updated but waiting_on_since not
    /// yet, or vice versa). Writer flips both atomically; reader checks
    /// the invariant after each read; if any read shows half-applied
    /// state, the test panics.
    #[test]
    fn pair_lock_prevents_torn_read_under_concurrent_writers() {
        // Invariant: when heartbeat_at_ms is even, waiting_on_since_ms is
        // Some(heartbeat_at_ms / 2); when odd, waiting_on_since_ms is None.
        // Writer flips between (even, Some) and (odd, None) repeatedly.
        // Without lock: reader can see (even, None) or (odd, Some) → torn.
        // With lock: reader always sees a consistent (even, Some) or
        // (odd, None) pair.
        let writers_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writers_done_w = Arc::clone(&writers_done);

        let writer = thread::spawn(move || {
            for i in 0u64..5_000 {
                if i.is_multiple_of(2) {
                    update_with("test_torn_read", |p| {
                        p.heartbeat_at_ms = i;
                        p.waiting_on_since_ms = Some(i / 2);
                    });
                } else {
                    update_with("test_torn_read", |p| {
                        p.heartbeat_at_ms = i;
                        p.waiting_on_since_ms = None;
                    });
                }
            }
            writers_done_w.store(true, std::sync::atomic::Ordering::Relaxed);
        });

        let reader = thread::spawn(move || {
            while !writers_done.load(std::sync::atomic::Ordering::Relaxed) {
                let snap = snapshot_for("test_torn_read");
                if snap.heartbeat_at_ms == 0 {
                    // Initial state before writer has run; skip.
                    continue;
                }
                let expected_since = if snap.heartbeat_at_ms.is_multiple_of(2) {
                    Some(snap.heartbeat_at_ms / 2)
                } else {
                    None
                };
                assert_eq!(
                    snap.waiting_on_since_ms, expected_since,
                    "torn read detected — heartbeat_at_ms={} waiting_on_since_ms={:?} (expected {:?})",
                    snap.heartbeat_at_ms, snap.waiting_on_since_ms, expected_since
                );
            }
        });

        writer.join().expect("writer thread joined");
        reader
            .join()
            .expect("reader thread joined — no torn read panic");
    }
}
