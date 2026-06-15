#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Review-repro interim guard (scope: api) — redesign_required.
//!
//! Finding: "Dedup cache keys only on request_id, allowing a replayed id to
//! return a stale response for a different operation."
//!
//! The dispatch path (src/api/mod.rs) reads `request_id` straight off the
//! untrusted envelope and `request_dedup::DedupCache::dispatch` keys the
//! cache PURELY on that string with NO binding to method/params. If two
//! logically-different requests reuse the same `request_id` within the
//! 10-minute TTL (a bridge bug, or a malicious agent over the mcp_tool
//! transport deliberately reusing an id), the second request returns the
//! FIRST request's cached response without executing — e.g. `send` id=X
//! then `mcp_tool/delete_instance` id=X returns the cached `send` result
//! and the delete never runs.
//!
//! WHY redesign_required: a behavioral test of the FIX cannot be written
//! against the CURRENT code. `DedupCache::dispatch(request_id, wait_timeout,
//! handler)` has NO parameter through which a (method, params) fingerprint
//! could be supplied, and `Entry` has no field to store one. Verifying that
//! "a mismatched fingerprint re-executes instead of replaying" therefore
//! requires a SIGNATURE CHANGE (add a fingerprint argument to `dispatch`
//! and a `fingerprint` field to `Entry`, then compare on hit). Until that
//! seam exists, the bug cannot be exercised without referencing a
//! not-yet-existing API.
//!
//! Interim guard (per the user's "all code is testable" principle): a
//! SOURCE-SCANNING test asserting the fix's seam is present — the dedup
//! module must carry a request fingerprint on its cache entry. RED now (no
//! `fingerprint` anywhere in request_dedup.rs), GREEN once the redesign adds
//! it.
//!
//! redesign_note: Bind each cache entry to a cheap fingerprint of (method,
//! params) — store it on `Entry`, thread it through `dispatch` /
//! `check_or_register` / `finalize`, and on a Cached/InProgress hit verify
//! the incoming request's fingerprint matches; on mismatch skip dedup
//! (treat as fresh) or return an explicit error rather than replaying an
//! unrelated response. This preserves true-retry idempotency while
//! preventing cross-operation replay.

use std::path::PathBuf;

fn request_dedup_rs() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("api")
        .join("request_dedup.rs")
}

#[test]
fn dedup_entry_carries_a_request_fingerprint_api() {
    let path = request_dedup_rs();
    let src = std::fs::read_to_string(&path).expect("read request_dedup.rs");

    // The fix introduces a per-entry request fingerprint so a replayed
    // request_id bound to a DIFFERENT (method, params) is not served the
    // original response. Until that seam exists the module has no notion of
    // a fingerprint at all.
    let has_fingerprint = src
        .lines()
        .any(|l| l.to_ascii_lowercase().contains("fingerprint"));

    assert!(
        has_fingerprint,
        "request_dedup keys the cache purely on request_id with NO binding \
         to (method, params): a replayed id returns a stale response for a \
         DIFFERENT operation (e.g. send id=X then delete_instance id=X \
         replays the send result and the delete never runs). The fix must \
         bind each Entry to a (method, params) fingerprint and verify it on \
         a Cached/InProgress hit. No `fingerprint` seam exists yet in \
         {}.",
        path.display()
    );
}
