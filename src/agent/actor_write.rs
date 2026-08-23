//! #3315 B3: the platform shim for the registered-writer (actor) write path.
//!
//! `mod write_actor` is `#[cfg(unix)]` — it owns a raw PTY fd — so the write
//! path cannot name it directly and still compile on Windows. #3314 removed the
//! two-arm `try_actor_write` abstraction and called `write_actor::write_guarded`
//! inline; CI run 32560269942 failed the Windows `Check` job on exactly that.
//!
//! This module is deliberately NOT cfg-gated: it exists on every platform and
//! resolves the difference internally, so `write_with_timeout_guarded` has one
//! ungated call site. It lives in its own file rather than in `mod.rs` because
//! `mod.rs` is at its grandfathered anti-monolith ceiling (see
//! `tests/src_file_size_invariant.rs`); it may shrink, never grow.
//!
//! Pinned by `tests/write_actor_platform_shim_invariant.rs`.

use super::PtyWriter;
use crate::agent::dev_modal::WriteBarrier;

pub(super) fn record_successful_write(
    writer: &PtyWriter,
    result: std::io::Result<()>,
) -> std::io::Result<()> {
    if result.is_ok() {
        crate::agent::dev_modal::note_pty_write(writer);
    }
    result
}

/// `Some(Ok/Err)` -> `writer` is registered with `write_actor`, use its result
/// directly. `None` -> not registered (Windows; synthetic/test writer) -- the
/// caller falls through to its thread-per-write mechanism.
#[cfg(unix)]
pub(super) fn try_actor_write_guarded(
    writer: &PtyWriter,
    data: &[u8],
    barrier: Option<WriteBarrier>,
) -> Option<std::io::Result<()>> {
    super::write_actor::write_guarded(writer, data.to_vec(), super::PTY_WRITE_TIMEOUT, barrier)
}

#[cfg(not(unix))]
pub(super) fn try_actor_write_guarded(
    _writer: &PtyWriter,
    _data: &[u8],
    _barrier: Option<WriteBarrier>,
) -> Option<std::io::Result<()>> {
    None
}
