//! Review-repro test (scope: api) for the request_dedup module.
//!
//! Regression coverage for the former finding that zero-byte request-dedup
//! entries could grow without an entry-count ceiling between 10-min sweeps.
//!
//! `evict_to_fit` only runs when `total_bytes > total_cap` and explicitly
//! skips every entry whose `response_bytes == 0`. Oversized (and Errored)
//! terminal entries are stored with `response_bytes = 0`, contribute nothing
//! to `total_bytes`, and therefore cannot trigger or be removed by
//! `evict_to_fit`; the separate count-cap pass must handle them. InProgress
//! entries must remain protected until their handler completes.
//!
//! This test drives the CURRENT public entry point (`dispatch`) with many
//! distinct ids, each returning an over-cap response, and asserts the cache
//! enforces the production entry-count ceiling. A second control verifies a
//! blocked InProgress request survives the same overflow. Each test has an
//! independent RED when its corresponding production invariant is removed.

use super::{DedupCache, MAX_ENTRIES, TOTAL_CAP_BYTES, TTL, WAITER_CAP};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

#[test]
fn zero_byte_oversized_entries_are_count_bounded_api() {
    // per_entry_cap = 10 bytes → every response below is "oversized" and
    // gets stored as a zero-byte `Oversized` terminal entry that
    // `evict_to_fit` can never see (it skips response_bytes == 0) and that
    // does not contribute to `total_bytes` (so the byte ceiling never
    // trips either).
    let cache = DedupCache::with_caps(TTL, 10, TOTAL_CAP_BYTES, WAITER_CAP);

    // Cross the real production count cap enough times to exercise repeated
    // eviction without paying the old 20,000-entry sort tail. No time passes
    // and `sweep_expired` is never called, so the ONLY thing that can bound
    // `len()` is the production count cap.
    const N: usize = MAX_ENTRIES + 32;
    for i in 0..N {
        let id = format!("oversized-{i}");
        let resp = cache.dispatch(Some(&id), 0, Duration::from_secs(5), || {
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

    assert_eq!(
        cache.len(),
        MAX_ENTRIES,
        "count-cap eviction must retain exactly the production entry bound"
    );
}

#[test]
fn in_progress_entry_survives_count_cap_eviction_api() {
    let cache = Arc::new(DedupCache::with_caps(TTL, 10, TOTAL_CAP_BYTES, WAITER_CAP));
    let executions = Arc::new(AtomicUsize::new(0));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();

    let handler_cache = Arc::clone(&cache);
    let handler_executions = Arc::clone(&executions);
    // fire-and-forget: the test retains and joins this controlled handler thread.
    let handler = thread::spawn(move || {
        handler_cache.dispatch(Some("in-progress"), 0, Duration::from_secs(5), move || {
            handler_executions.fetch_add(1, Ordering::SeqCst);
            entered_tx.send(()).expect("in-progress handler observer");
            release_rx.recv().expect("release in-progress handler");
            json!({"x": 1})
        })
    });

    entered_rx
        .recv()
        .expect("in-progress handler must be registered before overflow");

    const N: usize = MAX_ENTRIES + 32;
    for i in 0..N {
        let id = format!("oversized-in-progress-{i}");
        cache.dispatch(
            Some(&id),
            0,
            Duration::from_secs(5),
            || json!({"big": "xxxxxxxxxxxxxxxxxxxx"}),
        );
    }

    assert_eq!(
        cache.len(),
        MAX_ENTRIES,
        "count-cap eviction must retain the blocked InProgress entry while bounding terminals"
    );

    release_tx.send(()).expect("release in-progress handler");
    let first = handler.join().expect("in-progress handler thread");

    let retry_executions = Arc::new(AtomicUsize::new(0));
    let retry_counter = Arc::clone(&retry_executions);
    let retry = cache.dispatch(Some("in-progress"), 0, Duration::from_secs(5), move || {
        retry_counter.fetch_add(1, Ordering::SeqCst);
        json!({"unexpected": "re-executed"})
    });

    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "the blocked handler must execute exactly once"
    );
    assert_eq!(
        retry_executions.load(Ordering::SeqCst),
        0,
        "retry must observe the completed InProgress entry rather than re-execute"
    );
    assert_eq!(retry, first, "retry must return the original response");
    assert_eq!(cache.len(), MAX_ENTRIES);
}
