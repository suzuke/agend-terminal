//! Review-repro test (scope: api) for the request_dedup module.
//!
//! Finding: "Dedup cache has no entry-count ceiling; zero-byte
//! (Errored/Oversized/in-flight) entries grow unbounded between 10-min
//! sweeps."
//!
//! `evict_to_fit` only runs when `total_bytes > total_cap` AND it
//! explicitly skips every entry whose `response_bytes == 0`. Oversized
//! (and Errored) terminal entries are stored with `response_bytes = 0`,
//! contribute nothing to `total_bytes`, and therefore can NEVER trigger or
//! be removed by `evict_to_fit`. The only count bound is `sweep_expired`,
//! which only drops entries past the 10-minute TTL. So a stream of unique
//! request_ids whose responses are oversized accumulates an UNBOUNDED
//! number of HashMap entries for the full TTL window.
//!
//! This test drives the CURRENT public entry point (`dispatch`) with many
//! distinct ids, each returning an over-cap response, and asserts the cache
//! enforces a sane entry-count ceiling. It is RED today (len == N, no cap)
//! and GREEN once an explicit MAX_ENTRIES ceiling evicts oldest terminal
//! entries regardless of `response_bytes`.

use super::{DedupCache, TOTAL_CAP_BYTES, TTL, WAITER_CAP};
use serde_json::json;
use std::time::Duration;

#[test]
#[ignore = "dedup-entry-count-ceiling: red until fix; remove #[ignore] after fix to confirm"]
fn zero_byte_oversized_entries_are_count_bounded_api() {
    // per_entry_cap = 10 bytes → every response below is "oversized" and
    // gets stored as a zero-byte `Oversized` terminal entry that
    // `evict_to_fit` can never see (it skips response_bytes == 0) and that
    // does not contribute to `total_bytes` (so the byte ceiling never
    // trips either).
    let cache = DedupCache::with_caps(TTL, 10, TOTAL_CAP_BYTES, WAITER_CAP);

    // N is far above any reasonable entry-count ceiling but small enough to
    // run quickly. No time passes and `sweep_expired` is never called, so
    // the ONLY thing that could bound `len()` is an explicit count cap.
    const N: usize = 20_000;
    for i in 0..N {
        let id = format!("oversized-{i}");
        let resp = cache.dispatch(Some(&id), Duration::from_secs(5), || {
            // Encodes to well over the 10-byte per-entry cap → Oversized.
            json!({"big": "xxxxxxxxxxxxxxxxxxxx"})
        });
        // Sanity: the original requester still gets the full response; the
        // cache policy never truncates the wire payload.
        assert_eq!(
            resp["big"].as_str().map(str::len),
            Some(20),
            "S1 must still receive its full (oversized) response"
        );
    }

    let len = cache.len();
    assert!(
        len < N,
        "dedup cache grew to {len} zero-byte entries with no count ceiling — \
         oversized/errored entries (response_bytes == 0) are invisible to \
         evict_to_fit and only TTL sweep bounds the map; a count cap must \
         evict oldest terminal entries regardless of response_bytes"
    );
}
