//! review-repro (scope: agent-binding) — verification/reproduction tests for
//! confirmed code-review findings whose items are private to `src/agent/mod.rs`.
//!
//! These encode the CORRECT post-fix behavior and are GREEN on current code
//! (the cited bugs are closed); they run un-ignored as live regression guards.

use super::{classify_exit, resolve_instance, ExitKind};

/// Finding: `resolve_instance` returns a fresh random UUID for a fleet
/// instance whose fleet.yaml entry has no parseable `id`
/// (`InstanceId::default()` is `Uuid::new_v4()`, NOT a stable nil). Two calls
/// for the SAME id-less instance therefore yield DIFFERENT ids, silently
/// breaking every id-keyed correlation (message from/to routing, task-event
/// emitter id, dedup/threading/audit). The function's contract is a STABLE
/// identity.
///
/// Correct behavior (either acceptable fix): two resolutions of the same
/// id-less instance MUST agree — return the same deterministic id on both
/// calls, OR fail-fast on both (a dedicated error). The one thing that must
/// NOT happen is two DIFFERENT non-deterministic ids.
///
/// RED now: the two `Ok` ids differ (random per call). GREEN after fix:
/// either both `Ok` and equal, or both `Err`.
#[test]
#[serial_test::serial]
fn resolve_instance_idless_is_deterministic_agent_binding() {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let uniq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-review-resolve-idless-{}-{}",
        std::process::id(),
        uniq
    ));
    std::fs::create_dir_all(&dir).expect("create temp home");

    // `id: legacy-id` is non-null but unparseable as a UUID, so:
    //  - `backfill_ids` leaves it untouched (it only fills NULL/absent ids), and
    //  - `resolve_instance`'s `InstanceId::parse` returns None →
    //    `.unwrap_or_default()` → a fresh random UUID on EVERY call.
    let yaml = "instances:\n  idless-agent:\n    id: legacy-id\n    command: /bin/bash\n";
    std::fs::write(dir.join("fleet.yaml"), yaml).expect("write fleet.yaml");

    let r1 = resolve_instance(&dir, "idless-agent");
    let r2 = resolve_instance(&dir, "idless-agent");
    std::fs::remove_dir_all(&dir).ok();

    match (r1, r2) {
        (Ok((id1, _)), Ok((id2, _))) => assert_eq!(
            id1,
            id2,
            "resolve_instance must return a STABLE id for an id-less instance — \
             got two different random UUIDs ({} vs {}), which breaks every \
             id-keyed correlation (routing/dedup/audit)",
            id1.full(),
            id2.full()
        ),
        (Err(_), Err(_)) => { /* fail-fast on both is an acceptable fix */ }
        (a, b) => panic!(
            "resolve_instance must be deterministic for an id-less instance: \
             one call succeeded and the other did not (a_ok={}, b_ok={})",
            a.is_ok(),
            b.is_ok()
        ),
    }
}

/// Finding: `wait_for_process_exit` returns `None` when the child never
/// reports an exit code within the 2s poll window; `classify_exit(None)` then
/// returns `ExitKind::Crash`, which drives a Crash respawn. But this same path
/// is reached when the daemon force-kills (sweeps) a wedged process tree — a
/// daemon-induced teardown is then mis-classified as a respawnable crash.
///
/// Correct behavior: a never-observed exit (the `None` case) must NOT be
/// classified as a respawnable `Crash`; it should be treated as a `SignalKill`
/// (the suggestion: "classified as SignalKill rather than respawned as a
/// crash").
///
/// RED now: `classify_exit(None)` is `Crash`. GREEN after fix: `SignalKill`.
#[test]
fn classify_exit_none_is_not_respawnable_crash_agent_binding() {
    assert!(
        !matches!(classify_exit(None), ExitKind::Crash),
        "classify_exit(None) (process never reaped / daemon-swept) must NOT be a \
         respawnable Crash — a daemon-induced kill should classify as SignalKill"
    );
    assert!(
        matches!(classify_exit(None), ExitKind::SignalKill),
        "classify_exit(None) should be SignalKill (daemon-induced teardown), \
         not a respawn-triggering Crash"
    );
}
