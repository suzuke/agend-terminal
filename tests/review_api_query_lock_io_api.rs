#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Review-repro static invariant (scope: list snapshot).
//!
//! Finding: "list snapshot holds the global registry mutex across per-agent
//! blocking disk I/O."
//!
//! `list_snapshot` (src/agent_ops.rs — extracted from the former
//! api::handlers::query::list_response by #2454 S3) takes the tier-1
//! registry lock (`agent::lock_registry`) and must DROP it before any
//! per-agent `pending_for_instance` disk I/O.
//!
//! The runtime "lock held across IO" cannot be driven without a timing
//! race, so this is a SOURCE-SCANNING invariant (the codebase's first-class
//! method for held-lock invariants, mirroring tests/core_mutex_invariant.rs
//! and tests/anti_pattern_invariant.rs). It asserts that within
//! `list_snapshot`, NO `pending_for_instance` call appears between the
//! `lock_registry` acquisition and the matching `drop(reg)`.

use std::path::PathBuf;

fn list_snapshot_source() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("agent_ops.rs")
}

/// Extract the body lines of `list_snapshot` (from its `fn` signature to the
/// next top-level `fn ` at the same indentation). Returns 1-based line
/// numbers paired with the line text, comment/doc lines stripped.
fn list_snapshot_body(src: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut in_fn = false;
    for (i, raw) in src.lines().enumerate() {
        let trimmed = raw.trim_start();
        if !in_fn {
            if raw.starts_with("pub(crate) fn list_snapshot(") {
                in_fn = true;
                out.push((i + 1, raw.to_string()));
            }
            continue;
        }
        // Stop at the start of the NEXT top-level function definition.
        if (trimmed.starts_with("pub(crate) fn ")
            || trimmed.starts_with("pub fn ")
            || trimmed.starts_with("fn "))
            && !trimmed.starts_with("fn list_snapshot(")
            && (!raw.starts_with(' ') || raw.starts_with("pub"))
        {
            break;
        }
        if trimmed.starts_with("//") || trimmed.starts_with('*') {
            continue;
        }
        out.push((i + 1, raw.to_string()));
    }
    out
}

#[test]
fn list_response_does_not_hold_registry_lock_across_dispatch_idle_io_api() {
    let path = list_snapshot_source();
    let src = std::fs::read_to_string(&path).expect("read agent_ops.rs");
    let body = list_snapshot_body(&src);
    assert!(
        !body.is_empty(),
        "could not locate list_snapshot in {}",
        path.display()
    );

    let lock_idx = body
        .iter()
        .position(|(_, l)| l.contains("lock_registry("))
        .expect("list_snapshot must acquire the registry lock");
    let drop_idx = body
        .iter()
        .skip(lock_idx)
        .position(|(_, l)| l.contains("drop(reg)"))
        .map(|p| p + lock_idx)
        .expect("list_snapshot must release the registry lock with drop(reg)");

    let mut offenders = Vec::new();
    for (line_no, line) in &body[lock_idx..=drop_idx] {
        if line.contains("pending_for_instance") || line.contains("list_pending") {
            offenders.push(format!("agent_ops.rs:{line_no}: {}", line.trim()));
        }
    }

    assert!(
        offenders.is_empty(),
        "list_snapshot calls dispatch_idle disk I/O WHILE holding the tier-1 \
         registry lock (between lock_registry and drop(reg)). This blocks \
         every other API handler, the supervisor tick, crash-respawn, \
         hang-detection, and the TUI render path under disk contention. \
         Snapshot the per-agent fields under the lock, drop(reg), and only \
         THEN call pending_for_instance:\n{}",
        offenders.join("\n")
    );
}

/// #2454 S3 r2 RED: an indented/nested `fn list_snapshot(` decoy must NOT
/// be selected by the extraction helper.  The current `trim_start`-based
/// discovery accepts indented lines, so a nested decoy produces a
/// non-empty body — this test fails until the helper requires column-0.
#[test]
fn indented_list_snapshot_decoy_must_not_be_selected() {
    let decoy = "\
        fn other() {\n\
            fn list_snapshot(home: &Path) -> Value {\n\
                lock_registry(r);\n\
                drop(reg);\n\
            }\n\
        }\n";
    let body = list_snapshot_body(decoy);
    assert!(
        body.is_empty(),
        "an indented/nested list_snapshot must not be selected; \
         got {} lines: {:?}",
        body.len(),
        body
    );
}
