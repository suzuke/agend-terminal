#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Review-repro static invariant (scope: api) — register-external lock order.
//!
//! Original finding (#2197): `handle_register_external` was the only site that
//! nested the external lock INSIDE the registry lock (registry→external),
//! holding both — even across the `fleet::resolve_uuid` disk read. That
//! undocumented registry→external order would deadlock against any future
//! external→registry holder, so #2197 released the registry BEFORE taking the
//! external lock.
//!
//! t-65 evolution: dropping the registry before `lock_external` made the
//! managed-name check + external insert NON-atomic — a same-name managed agent
//! spawned in the gap slips past the check and gets a colliding external
//! registration. The fix re-checks the managed registry UNDER the external lock
//! and holds both across the insert. To stay deadlock-free that re-nesting MUST
//! use the SAFE order — external FIRST, then registry (external→registry) — the
//! exact inverse of the #2197 bug. An audit of every production `lock_external`
//! site confirmed none holds the registry across it, so external→registry has
//! no AB-BA partner.
//!
//! This SOURCE-SCANNING invariant therefore now asserts the surviving safety
//! property: within the register-external logic fn, the external lock is
//! acquired BEFORE the registry lock (external→registry) — i.e. the registry is
//! NEVER taken before the external lock, so the registry→external nesting #2197
//! removed cannot regress back in.

use std::path::PathBuf;

fn external_rs() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("api")
        .join("handlers")
        .join("external.rs")
}

/// Return 1-based (line_no, line) pairs for the body of the fn whose DEFINITION
/// line contains `fn <fn_name>(`, comment/doc lines stripped, up to the next
/// top-level `fn`. Matches the definition (any visibility) — not call sites or
/// doc mentions.
fn fn_body(src: &str, fn_name: &str) -> Vec<(usize, String)> {
    let needle = format!("fn {fn_name}(");
    let mut out = Vec::new();
    let mut in_fn = false;
    for (i, raw) in src.lines().enumerate() {
        let trimmed = raw.trim_start();
        if !in_fn {
            let is_def = (trimmed.starts_with("fn ")
                || trimmed.starts_with("pub fn ")
                || trimmed.starts_with("pub(crate) fn "))
                && trimmed.contains(&needle);
            if is_def {
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
fn register_external_takes_external_before_registry_api() {
    let path = external_rs();
    let src = std::fs::read_to_string(&path).expect("read external.rs");
    // The lock logic lives in `register_external_with_seam` (the seam'd inner fn
    // `handle_register_external` delegates to); fall back to the public fn if a
    // future change inlines it back.
    let mut body = fn_body(&src, "register_external_with_seam");
    if body.is_empty() {
        body = fn_body(&src, "handle_register_external");
    }
    assert!(
        !body.is_empty(),
        "could not locate the register-external logic fn in {}",
        path.display()
    );

    let ext_lock_idx = body
        .iter()
        .position(|(_, l)| l.contains("lock_external("))
        .expect("register-external must acquire the external lock");
    let reg_lock_idx = body
        .iter()
        .position(|(_, l)| l.contains("lock_registry("))
        .expect("register-external must re-check the managed registry under lock");

    // external→registry: the external lock must be acquired BEFORE the registry
    // lock. If the registry lock appears first, the registry→external nesting
    // that #2197 removed (and that would deadlock against any external→registry
    // holder) has regressed back in.
    assert!(
        ext_lock_idx < reg_lock_idx,
        "register-external acquires the registry lock at line {} BEFORE the \
         external lock at line {} — that is the registry→external nesting #2197 \
         removed; a future external→registry holder would deadlock against it. \
         Take the external lock first, then the registry lock for the managed \
         re-check (external→registry — the audited-safe order; t-65).",
        body[reg_lock_idx].0,
        body[ext_lock_idx].0,
    );
}
