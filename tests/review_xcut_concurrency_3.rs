#![allow(clippy::unwrap_used, clippy::expect_used)]

//! xcut-concurrency F3 (resource-leak): in
//! `src/daemon/ci_watch/poller.rs`, `check_ci_watches_with_provider` (driven
//! synchronously from the per-tick `CiWatchPollHandler::run`, which returns
//! immediately) does a fire-and-forget `shared_ci_runtime().spawn(async ...)`
//! ONCE PER REPO PER POLL CYCLE onto the 2-worker shared CI runtime. The
//! JoinHandles are dropped and there is NO guard ensuring the previous cycle's
//! tasks completed (or are bounded) before the next tick spawns more. Under a
//! network stall where `gh`/HTTP back up, each tick can enqueue new repo tasks
//! faster than the 2 workers drain them, growing the detached-future backlog
//! without limit.
//!
//! Correct behavior: bound the detached spawns — a per-repo in-flight marker
//! (skip spawning while the prior cycle's task for that repo is still running)
//! or a `tokio::sync::Semaphore` sized to the worker count. This test asserts
//! poller.rs carries at least one such concurrency-bound marker. Red now (none
//! present); green once a bound is added.

use std::path::PathBuf;

/// Any of these in poller.rs indicates the detached-spawn backlog is bounded.
/// A future fix may use a Semaphore, an in-flight set keyed by repo, or a
/// try_acquire gate — accept the common shapes so the test pins the INVARIANT
/// ("the unbounded fire-and-forget is now bounded"), not one specific fix.
const GUARD_MARKERS: &[&str] = &[
    "Semaphore",
    "try_acquire",
    "in_flight",
    "inflight",
    "InFlight",
    "IN_FLIGHT",
];

#[test]
#[ignore = "xcut-concurrency F3: red until fix; remove #[ignore] after fix to confirm"]
fn ci_poller_bounds_detached_spawn_with_inflight_guard_xcut_concurrency() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/daemon/ci_watch/poller.rs");
    let text = std::fs::read_to_string(&path)
        .expect("xcut-concurrency F3: src/daemon/ci_watch/poller.rs must exist");

    // Sanity: the fire-and-forget detached spawn we are guarding must still be
    // here (if the spawn shape changes entirely this scan should be revisited).
    assert!(
        text.contains("shared_ci_runtime().spawn"),
        "xcut-concurrency F3: expected the fire-and-forget `shared_ci_runtime().spawn(...)` in \
         poller.rs — the unbounded detached-spawn site this invariant guards. If it moved, \
         re-anchor this test."
    );

    // Count code (non-comment) lines as evidence of a real guard, not prose.
    let guarded = text.lines().any(|line| {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') {
            return false;
        }
        GUARD_MARKERS.iter().any(|m| line.contains(m))
    });

    assert!(
        guarded,
        "xcut-concurrency F3: the per-repo per-tick `shared_ci_runtime().spawn(...)` is \
         fire-and-forget with NO inflight/dedupe/semaphore bound. A stalled provider lets each \
         tick enqueue new repo tasks faster than the 2 workers drain them, growing the \
         detached-task backlog without limit. Bound it with a per-repo in-flight marker (skip \
         spawning while the prior cycle's task for that repo is still running) or a \
         tokio::sync::Semaphore sized to the worker count. None of {:?} found in poller.rs.",
        GUARD_MARKERS
    );
}
