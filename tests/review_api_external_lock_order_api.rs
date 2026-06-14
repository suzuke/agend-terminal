#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Review-repro static invariant (scope: api).
//!
//! Finding: "handle_register_external is the only site holding
//! registry+external locks simultaneously, creating an undocumented nested
//! lock order."
//!
//! `handle_register_external` (src/api/handlers/external.rs) acquires the
//! registry lock (`reg = agent::lock_registry`) and then, while STILL
//! holding it, acquires the external lock (`ext = agent::lock_external`),
//! holding BOTH until the explicit `drop(reg)` / `drop(ext)` at the end.
//! Every other handler takes these two locks sequentially with a release
//! between them. This is the sole place that nests external INSIDE
//! registry; it does not deadlock today only because no path takes
//! external-then-registry while holding, but the nesting is undocumented
//! and the registry lock is even held across the `fleet::resolve_uuid` disk
//! read — a future external-then-registry holder would deadlock against it.
//!
//! This is a SOURCE-SCANNING invariant (the codebase's first-class method
//! for lock-ordering invariants, mirroring tests/core_mutex_invariant.rs).
//! It asserts that within `handle_register_external`, the registry lock is
//! RELEASED (`drop(reg)`) BEFORE the external lock is acquired
//! (`lock_external`) — i.e. the two locks are never held simultaneously.
//!
//! RED now: `lock_external(` appears before any `drop(reg)`. GREEN after
//! the fix checks the registry for a managed-name collision under `reg`,
//! drops `reg`, and only THEN acquires `ext`.

use std::path::PathBuf;

fn external_rs() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("api")
        .join("handlers")
        .join("external.rs")
}

/// Return 1-based (line_no, line) pairs for the body of
/// `handle_register_external`, comment/doc lines stripped, up to the next
/// top-level `fn`.
fn handle_register_external_body(src: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut in_fn = false;
    for (i, raw) in src.lines().enumerate() {
        let trimmed = raw.trim_start();
        if !in_fn {
            if trimmed.starts_with("pub(crate) fn handle_register_external(")
                || trimmed.starts_with("fn handle_register_external(")
                || trimmed.starts_with("pub fn handle_register_external(")
            {
                in_fn = true;
                out.push((i + 1, raw.to_string()));
            }
            continue;
        }
        // The next top-level fn (column-0) ends this function.
        if !raw.starts_with(' ')
            && (trimmed.starts_with("fn ")
                || trimmed.starts_with("pub fn ")
                || trimmed.starts_with("pub(crate) fn "))
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
#[ignore = "register-external-nests-external-lock-inside-registry-lock: red until fix; remove #[ignore] after fix to confirm"]
fn register_external_releases_registry_lock_before_taking_external_api() {
    let path = external_rs();
    let src = std::fs::read_to_string(&path).expect("read external.rs");
    let body = handle_register_external_body(&src);
    assert!(
        !body.is_empty(),
        "could not locate handle_register_external in {}",
        path.display()
    );

    let reg_lock_idx = body
        .iter()
        .position(|(_, l)| l.contains("lock_registry("))
        .expect("handle_register_external must acquire the registry lock");
    let ext_lock_idx = body
        .iter()
        .position(|(_, l)| l.contains("lock_external("))
        .expect("handle_register_external must acquire the external lock");

    // Is there a `drop(reg)` strictly between acquiring the registry lock
    // and acquiring the external lock? If so the locks are NOT held
    // simultaneously (the fix). If not, external is nested inside registry
    // (the bug).
    let drop_reg_between = body[reg_lock_idx..ext_lock_idx]
        .iter()
        .any(|(_, l)| l.contains("drop(reg)"));

    assert!(
        drop_reg_between,
        "handle_register_external acquires the external lock at line {} while \
         still holding the registry lock acquired at line {} — the ONLY site \
         that nests external INSIDE registry, and it even holds the registry \
         lock across the fleet::resolve_uuid disk read. Release the registry \
         lock (drop(reg)) BEFORE taking the external lock so no reverse-order \
         (external-then-registry) holder can ever deadlock against it.",
        body[ext_lock_idx].0, body[reg_lock_idx].0,
    );
}
