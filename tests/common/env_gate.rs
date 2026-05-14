//! Test-only helpers for serializing process-wide env-var mutations.
//!
//! F9 (#685 sub-task 4) introduced the `AGEND_PRODUCTIVE_GATE` env var for
//! activation gating of the productive-output Hung classification path.
//! Tests that exercise either side of the gate (active / inactive) must
//! mutate the env var per-test and tolerate parallel test execution.
//!
//! This helper serialises the mutations via a `OnceLock<Mutex<()>>` so
//! cross-thread leakage between tests is prevented.
//!
//! **Mirror copy**: `src/health.rs::tests::with_f9_gate` is the canonical
//! source for unit tests (which cannot import from `tests/common/`). This
//! integration-test copy must stay in lock-step. Sub-task 5 decision
//! `d-20260514015214320625-1` §1.D acknowledges the limitation and accepts
//! the duplication (~15 LOC) to enable both unit and integration test
//! reuse without exposing a `pub mod test_util` to production.

use std::sync::{Mutex, OnceLock};

/// Run `f` with `AGEND_PRODUCTIVE_GATE` env var set to `"1"` (when `active`)
/// or unset (when `!active`). Restores the prior value on return.
///
/// Tests touching `AGEND_PRODUCTIVE_GATE` must use this helper rather than
/// directly setting the env var, since Rust's `cargo test` runs tests in
/// parallel and env mutations are process-global. The internal `Mutex`
/// serialises wrapped tests across threads.
///
/// `#[allow(dead_code)]` because multiple integration test binaries pull
/// `tests/common/` in via `mod common`; the cargo per-binary dead-code
/// lint flags `with_f9_gate` in test binaries that don't use it. Removing
/// the lint suppression would force every other integration test to
/// either use the helper or omit `mod common`. The helper itself is
/// exercised by `tests/fixture_corpus_measurement.rs` and the unit-test
/// mirror in `src/health.rs::tests`.
#[allow(dead_code)]
pub fn with_f9_gate<R>(active: bool, f: impl FnOnce() -> R) -> R {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let lock = LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let prior = std::env::var("AGEND_PRODUCTIVE_GATE").ok();
    // SAFETY: serialised by the LOCK guard above; this is a test-only
    // helper with documented contract that callers do not spawn threads
    // that read AGEND_PRODUCTIVE_GATE concurrently with this region.
    unsafe {
        if active {
            std::env::set_var("AGEND_PRODUCTIVE_GATE", "1");
        } else {
            std::env::remove_var("AGEND_PRODUCTIVE_GATE");
        }
    }
    let result = f();
    unsafe {
        match prior {
            Some(v) => std::env::set_var("AGEND_PRODUCTIVE_GATE", v),
            None => std::env::remove_var("AGEND_PRODUCTIVE_GATE"),
        }
    }
    result
}
