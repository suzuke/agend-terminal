//! #1642: the shared sync→async bridge for channel runtimes — single source for
//! the `block_on_value` helper that telegram (#1474) and discord (#1476)
//! previously shipped as byte-identical copies (the copy-paste-the-bug class:
//! discord inherited telegram's nested-runtime panic AND later its fix).
//!
//! `telegram_runtime()` / `discord_runtime()` are shared `current_thread`
//! runtimes, so calling `block_on` on one from within *any* tokio runtime
//! context panics with "Cannot start a runtime from within a runtime" (surfaced
//! by the teloxide 0.17 / reqwest 0.12 upgrade, #1293). The guard below detects
//! a current runtime and runs the future on a dedicated scoped thread with its
//! own *fresh* runtime, which is never nested. The non-nested fast path (the
//! shared runtime) is unchanged.
//!
//! HARD RULE (see CLAUDE.md): every value-returning shared-runtime `block_on`
//! in a channel module MUST go through here. Enforced by
//! `tests/block_on_runtime_guard_invariant.rs`, which additionally pins that
//! THIS helper keeps its `Handle::try_current` + `thread::scope` guard.

use std::future::Future;
use tokio::runtime::Runtime;

/// Run `fut` to completion on `runtime`, safe even when already inside a tokio
/// runtime. `runtime` is the shared per-channel `current_thread` runtime;
/// `label` names the channel for panic messages (e.g. `"telegram"`).
///
/// When a runtime is already current, the future runs on a fresh scoped-thread
/// runtime (never nested); otherwise it uses `runtime` directly. Behaviour is
/// byte-for-byte the same as the per-channel copies this replaced, modulo the
/// `label` in the (cold-path) panic strings.
pub(crate) fn block_on_value<F>(runtime: &Runtime, label: &str, fut: F) -> F::Output
where
    F: Future + Send,
    F::Output: Send,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|s| {
            s.spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap_or_else(|e| panic!("{label} nested runtime build failed: {e}"))
                    .block_on(fut)
            })
            .join()
            .unwrap_or_else(|_| panic!("{label} nested block_on thread panicked"))
        })
    } else {
        runtime.block_on(fut)
    }
}
