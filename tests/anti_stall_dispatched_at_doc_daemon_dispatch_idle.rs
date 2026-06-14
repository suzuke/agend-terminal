//! Maintainability repro (daemon-dispatch-idle batch): `anti_stall.rs`'s module
//! header and several inline comments describe the stall anchor as `dispatched_at`
//! (e.g. "falls back to `dispatched_at` when no progress sidecar exists", "else
//! dispatched_at"), but the `Task` struct has NO `dispatched_at` field — #807 Item 3
//! renamed it to `started_at` (kept only as a `#[serde(alias = "dispatched_at")]`
//! on `task_events.rs`). `check_stalled` actually anchors on `task.started_at`.
//!
//! The behavior is correct; only the documentation names a field that does not
//! exist, misleading anyone grepping the source for `dispatched_at` and
//! contradicting the actual code.
//!
//! METHOD: static_invariant (source-scan), mirroring `tests/core_mutex_invariant.rs`.
//! We scan ONLY the production slice of `anti_stall.rs` (everything before the
//! `#[cfg(test)]` module) so the test fns that legitimately reference the historical
//! name (`check_stalled_returns_none_when_dispatched_at_missing_and_no_sidecar`,
//! local `let dispatched_at = ...` fixtures, etc.) do not poison the scan.
//!
//! RED now: the prod slice (doc-comment header lines 9/11/20 + the inline comments
//! in `check_stalled`) still contains the stale `dispatched_at` references →
//! assertion fails.
//! GREEN after fix: renaming those references to `started_at` (or explicitly noting
//! `started_at` is the dispatch anchor) removes `dispatched_at` from prod source.

use std::path::PathBuf;

#[test]
#[ignore = "daemon-dispatch-idle anti-stall-dispatched-at-doc: red until fix; remove #[ignore] after fix to confirm"]
fn anti_stall_docs_do_not_reference_nonexistent_dispatched_at_daemon_dispatch_idle() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("daemon")
        .join("anti_stall.rs");
    let src = std::fs::read_to_string(&path).expect("read anti_stall.rs");

    // Scan ONLY the production slice — the `#[cfg(test)]` module legitimately uses
    // `dispatched_at` in test names / local fixtures and must not poison the scan.
    let cfg_test = ["#[cfg(", "test)]"].concat();
    let prod = match src.find(&cfg_test) {
        Some(i) => &src[..i],
        None => &src[..],
    };

    // Needle assembled at runtime so the literal in this comment can't self-match.
    let stale = ["dispatched", "_at"].concat();

    let hits: Vec<(usize, &str)> = prod
        .lines()
        .enumerate()
        .filter(|(_, l)| l.contains(&stale))
        .map(|(i, l)| (i + 1, l.trim()))
        .collect();

    assert!(
        hits.is_empty(),
        "anti_stall.rs prod source references `dispatched_at`, a field that does NOT \
         exist on Task (#807 renamed it to `started_at`; check_stalled anchors on \
         task.started_at). Rename these doc/comment references to `started_at`:\n{}",
        hits.iter()
            .map(|(ln, txt)| format!("  L{ln}: {txt}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
