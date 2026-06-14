#![allow(clippy::unwrap_used, clippy::expect_used)]

//! xcut-concurrency F1 (#813 follow-up / CLAUDE.md "no raw shared-runtime
//! block_on" hard rule): `check_pr_mergeable_blocking` in
//! `src/daemon/ci_watch/provider.rs` calls
//! `super::poller::shared_ci_runtime().block_on(...)` directly. The existing
//! `tests/block_on_runtime_guard_invariant.rs` treats this as "guarded" only
//! because a `std::thread::scope` happens to sit in the enclosing fn — but the
//! scoped thread STILL `block_on`s the *shared* CI runtime, which is exactly the
//! copy-paste bug `channel::shared_async::block_on_value` centralized away (it
//! builds a FRESH current-thread runtime on the scoped thread instead).
//!
//! Correct behavior: provider.rs must not perform a raw
//! `shared_ci_runtime().block_on` on the long-lived shared accessor at all; it
//! must route through a nested-safe bridge that runs the future on a fresh
//! (non-shared) runtime. This source-scan fails (red) while the raw
//! `shared_ci_runtime().block_on` is still present, and passes (green) once the
//! call is moved off the shared accessor.

use std::path::PathBuf;

/// The forbidden pattern: a `block_on` issued directly against the shared CI
/// runtime accessor. A fresh, locally-built runtime is `rt.block_on(...)` and
/// never matches this needle, so the post-fix bridge is exempt.
const NEEDLE: &str = "shared_ci_runtime().block_on";

#[test]
#[ignore = "xcut-concurrency F1: red until fix; remove #[ignore] after fix to confirm"]
fn ci_mergeable_blocking_has_no_raw_shared_runtime_block_on_xcut_concurrency() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/daemon/ci_watch/provider.rs");
    let text = std::fs::read_to_string(&path)
        .expect("xcut-concurrency F1: src/daemon/ci_watch/provider.rs must exist");

    let mut violations = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        // Skip comment/doc lines that merely mention the pattern in prose.
        if t.starts_with("//") || t.starts_with('*') {
            continue;
        }
        if line.contains(NEEDLE) {
            violations.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
        }
    }

    assert!(
        violations.is_empty(),
        "xcut-concurrency F1: raw `shared_ci_runtime().block_on` on the long-lived shared CI \
         runtime accessor found in provider.rs. CLAUDE.md forbids a sync->async bridge from \
         block_on'ing a shared `*_runtime()` accessor; route through a nested-safe bridge that \
         runs the future on a FRESH (non-shared) runtime (mirror \
         channel::shared_async::block_on_value, or build a local current_thread runtime like \
         quickstart.rs::verify_bot_is_admin):\n{}",
        violations.join("\n")
    );
}
