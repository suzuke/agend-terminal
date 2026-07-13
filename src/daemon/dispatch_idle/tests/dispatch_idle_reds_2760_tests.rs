//! #2760 (codex ruling m-…-1154) — dispatch-idle task-liveness gating REDs, homed
//! in a sibling `*_tests.rs` loaded via `#[path]` from the inline `mod tests` so
//! `dispatch_idle/mod.rs` stays under the anti-monolith LOC ceiling (the
//! `src_file_size_invariant` established split pattern). As a submodule of `mod
//! tests`, `use super::*` inherits both the inline test helpers (`tmp_home` /
//! `write_pending_at`) and the production items under test (`scan_and_emit` /
//! `list_pending` / `DispatchStatus`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

/// RED: a NON-canonical correlation (a query / synthetic dispatch id that was NEVER
/// a board task) must FAIL-OPEN and still fire the idle nag. Pre-fix
/// `task_still_live` treated ANY non-empty NotFound correlation as orphan-dead
/// (`Some(false)`) and swept it silent — the app-e2e / query-dispatch
/// over-suppression regression. The typed `TaskId::parse_canonical` gate now applies
/// liveness only to real task ids.
#[test]
fn non_canonical_correlation_fails_open_and_fires_2760() {
    let home = tmp_home("noncanon-fires");
    let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
    // "corr-query-7" is not `t-<digits>-<digits>[-<digits>]` → not a task id.
    let id = write_pending_at(
        &home,
        "alpha",
        "beta",
        Some("corr-query-7"),
        "task",
        600,
        issued,
    );
    scan_and_emit(&home);
    let inbox = crate::inbox::drain(&home, "alpha");
    assert!(
        inbox
            .iter()
            .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
        "a non-canonical (non-task) correlation must fail-open and fire the idle nag: {inbox:?}"
    );
    assert_eq!(
        list_pending(&home)
            .iter()
            .find(|p| p.dispatch_id == id)
            .map(|p| p.status),
        Some(DispatchStatus::Exceeded),
        "non-task sidecar flips pending→exceeded (fired), never swept as orphan-dead"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// RED: the flip side — a CANONICAL task id present on NO board is a definitively
/// orphaned dispatch → orphan-dead → the sidecar is swept silently, no nag. Pins
/// that the parser gate STILL enforces the frozen NotFound=orphan-dead policy for
/// real task ids.
#[test]
fn canonical_absent_task_is_orphan_dead_and_swept_2760() {
    let home = tmp_home("canon-orphan");
    let issued = chrono::Utc::now() - chrono::Duration::seconds(700);
    // A well-formed canonical id that no board holds → strict route NotFound.
    let did = write_pending_at(
        &home,
        "alpha",
        "beta",
        Some("t-20260101000000000000-1-1"),
        "task",
        600,
        issued,
    );
    scan_and_emit(&home);
    let inbox = crate::inbox::drain(&home, "alpha");
    assert!(
        !inbox
            .iter()
            .any(|m| m.kind.as_deref() == Some("dispatch_idle_threshold_exceeded")),
        "a canonical task id absent from every board is orphan-dead — no idle nag: {inbox:?}"
    );
    assert!(
        list_pending(&home).iter().all(|d| d.dispatch_id != did),
        "the orphan-dead sidecar must be swept"
    );
    std::fs::remove_dir_all(&home).ok();
}
