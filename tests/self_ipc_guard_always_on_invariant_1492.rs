//! #1492-L2 invariant (decision d-20260531171228817178-0): the
//! lock-across-self-IPC guard + its depth counters in `src/sync_audit.rs` MUST
//! be ALWAYS-ON — never `#[cfg(debug_assertions)]`-gated again.
//!
//! Pre-L2 the counters + `assert_no_registry_lock_for_self_ipc` compiled to a
//! release no-op, so a RELEASE daemon had ZERO protection against the
//! #1492/#1535 lock-across-self-IPC deadlock class (it would freeze). L2 made
//! them always-on and fail-fast (`Err`). Re-introducing a
//! `#[cfg(debug_assertions)]` gate would silently restore that blind spot — so
//! this RED fails CI if the gate ever comes back, and also pins the fail-fast
//! contract (the guard returns `anyhow::Result`, not `()`).

use std::path::PathBuf;

#[test]
fn self_ipc_guard_and_counters_are_always_on_1492() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/sync_audit.rs");
    let src = std::fs::read_to_string(&path).expect("read src/sync_audit.rs");

    // (1) No debug-gating ATTRIBUTE may compile out the self-IPC mechanism. The
    // file legitimately uses the `cfg!(debug_assertions)` MACRO for the separate
    // tier-audit (`cfg!(` form, not `#[cfg(`), so this catches only a
    // re-introduced compile-out of the counters/guard.
    let offending: Vec<String> = src
        .lines()
        .enumerate()
        .filter(|(_, l)| {
            let t = l.trim_start();
            t.starts_with("#[cfg(debug_assertions)]")
                || t.starts_with("#[cfg(not(debug_assertions))]")
        })
        .map(|(i, l)| format!("  {}: {}", i + 1, l.trim()))
        .collect();
    assert!(
        offending.is_empty(),
        "#1492-L2: src/sync_audit.rs must NOT `#[cfg(debug_assertions)]`-gate the \
         self-IPC depth counters / guard — that restores the release no-op blind \
         spot the daemon froze on. Offending attribute line(s):\n{}",
        offending.join("\n")
    );

    // (2) The guard must be fail-fast: return a `Result` (Err on violation), not
    // panic-or-noop. Pins the L2 contract at the signature level.
    assert!(
        src.contains(
            "pub fn assert_no_registry_lock_for_self_ipc(ctx: &str) -> anyhow::Result<()>"
        ),
        "#1492-L2: `assert_no_registry_lock_for_self_ipc` must return \
         `anyhow::Result<()>` (fail-fast Err on violation), not `()`."
    );
}
