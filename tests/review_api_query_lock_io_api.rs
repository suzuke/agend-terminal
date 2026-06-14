#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Review-repro static invariant (scope: api).
//!
//! Finding: "handle_list holds the global registry mutex across per-agent
//! blocking disk I/O."
//!
//! `handle_list` (src/api/handlers/query.rs) takes the tier-1 registry lock
//! (`agent::lock_registry`) and then, INSIDE the `.map()` over
//! `reg.values()`, calls
//! `crate::daemon::dispatch_idle::pending_for_instance(ctx.home, name)` for
//! every managed agent while the lock is STILL held. `pending_for_instance`
//! → `list_pending(home)` does a full `read_dir` plus a `read_to_string` +
//! `serde_json::from_str` per `.json` sidecar — once per agent. So a LIST
//! with N agents performs N directory scans + N*M file reads/parses
//! entirely under the registry lock, blocking ~30 daemon per-tick handlers,
//! the supervisor tick, crash-respawn, hang-detection, and the TUI render
//! path that all need the same lock.
//!
//! The runtime "lock held across IO" cannot be driven without a timing
//! race, so this is a SOURCE-SCANNING invariant (the codebase's first-class
//! method for held-lock invariants, mirroring tests/core_mutex_invariant.rs
//! and tests/anti_pattern_invariant.rs). It asserts that within
//! `handle_list`, NO `pending_for_instance` call appears between the
//! `lock_registry` acquisition and the matching `drop(reg)`.
//!
//! RED now: `pending_for_instance` is called inside the `.map()` closure
//! before `drop(reg)`. GREEN after the fix snapshots the per-agent fields
//! under the lock, drops `reg`, and only THEN calls `pending_for_instance`
//! (or calls `list_pending` once up front and filters in memory).

use std::path::PathBuf;

fn query_rs() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("api")
        .join("handlers")
        .join("query.rs")
}

/// Extract the body lines of `handle_list` (from its `fn` signature to the
/// next top-level `fn ` at the same indentation). Returns 1-based line
/// numbers paired with the line text, comment/doc lines stripped.
fn handle_list_body(src: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut in_fn = false;
    for (i, raw) in src.lines().enumerate() {
        let trimmed = raw.trim_start();
        if !in_fn {
            if trimmed.starts_with("pub(crate) fn handle_list(")
                || trimmed.starts_with("fn handle_list(")
                || trimmed.starts_with("pub fn handle_list(")
            {
                in_fn = true;
                out.push((i + 1, raw.to_string()));
            }
            continue;
        }
        // Stop at the start of the NEXT top-level function definition.
        if trimmed.starts_with("pub(crate) fn ")
            || trimmed.starts_with("pub fn ")
            || (trimmed.starts_with("fn ") && !raw.starts_with(' '))
        {
            // A new top-level fn at column 0 ends handle_list.
            if !raw.starts_with(' ') || raw.starts_with("pub") {
                break;
            }
        }
        // Strip comment / doc lines so a comment mentioning the pattern
        // can't trip the scan.
        if trimmed.starts_with("//") || trimmed.starts_with('*') {
            continue;
        }
        out.push((i + 1, raw.to_string()));
    }
    out
}

#[test]
#[ignore = "list-registry-lock-held-across-pending_for_instance-io: red until fix; remove #[ignore] after fix to confirm"]
fn handle_list_does_not_hold_registry_lock_across_dispatch_idle_io_api() {
    let path = query_rs();
    let src = std::fs::read_to_string(&path).expect("read query.rs");
    let body = handle_list_body(&src);
    assert!(
        !body.is_empty(),
        "could not locate handle_list in {}",
        path.display()
    );

    // Find the registry-lock acquisition and the matching drop within the
    // function body.
    let lock_idx = body
        .iter()
        .position(|(_, l)| l.contains("lock_registry("))
        .expect("handle_list must acquire the registry lock");
    let drop_idx = body
        .iter()
        .skip(lock_idx)
        .position(|(_, l)| l.contains("drop(reg)"))
        .map(|p| p + lock_idx)
        .expect("handle_list must release the registry lock with drop(reg)");

    // Any pending_for_instance (or list_pending) call BETWEEN the lock and
    // the drop is the bug: per-agent blocking disk I/O under the tier-1
    // registry lock.
    let mut offenders = Vec::new();
    for (line_no, line) in &body[lock_idx..=drop_idx] {
        if line.contains("pending_for_instance") || line.contains("list_pending") {
            offenders.push(format!("query.rs:{line_no}: {}", line.trim()));
        }
    }

    assert!(
        offenders.is_empty(),
        "handle_list calls dispatch_idle disk I/O WHILE holding the tier-1 \
         registry lock (between lock_registry and drop(reg)). This blocks \
         every other API handler, the supervisor tick, crash-respawn, \
         hang-detection, and the TUI render path under disk contention. \
         Snapshot the per-agent fields under the lock, drop(reg), and only \
         THEN call pending_for_instance:\n{}",
        offenders.join("\n")
    );
}
