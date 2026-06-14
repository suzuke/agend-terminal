#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Review-repro static invariant (scope: api).
//!
//! Finding: "Pre-auth read timeout is set on a different fd than the one
//! the handshake reads from."
//!
//! In `handle_session` (src/api/mod.rs) `reader` is built from `cloned` (a
//! `try_clone()`/dup of the socket) and `writer` is the original `stream`.
//! The 5s slow-loris pre-auth timeout is applied with
//! `writer.set_read_timeout(Some(Duration::from_secs(5)))`, but
//! `server_handshake_ndjson` READS from `reader` (the cloned fd). The
//! timeout only happens to bound the handshake read because `dup()` shares
//! one underlying file description and `SO_RCVTIMEO` is socket-scoped — an
//! accidental coupling. Any future change giving the cloned fd an
//! independent timeout (or a platform where dup'd handles don't share the
//! option) silently removes the slow-loris protection comment #680
//! promises.
//!
//! This is a SOURCE-SCANNING invariant: the read timeout for the pre-auth
//! handshake must be set on the SAME fd that is read (the `reader`'s inner
//! stream, e.g. `reader.get_ref().set_read_timeout(...)`), NOT on `writer`.
//!
//! RED now: the pre-auth `from_secs(5)` timeout is applied via
//! `writer.set_read_timeout(...)`. GREEN after the fix sets it on the read
//! fd.

use std::path::PathBuf;

fn api_mod_rs() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("api")
        .join("mod.rs")
}

#[test]
#[ignore = "preauth-timeout-on-write-fd-not-read-fd: red until fix; remove #[ignore] after fix to confirm"]
fn preauth_read_timeout_is_set_on_the_read_fd_api() {
    let path = api_mod_rs();
    let src = std::fs::read_to_string(&path).expect("read api/mod.rs");

    // The bug: the 5-second pre-auth slow-loris timeout is applied to
    // `writer` (the original stream / write fd), while the handshake reads
    // from `reader` (the cloned fd). Locate any `writer.set_read_timeout`
    // call that arms a 5-second timeout — that is the misplaced pre-auth
    // guard.
    let mut offenders = Vec::new();
    for (i, raw) in src.lines().enumerate() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') {
            continue; // skip comments/docs
        }
        let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
        // `writer.set_read_timeout(Some(...from_secs(5)...))`
        if compact.contains("writer.set_read_timeout(Some(") && compact.contains("from_secs(5)") {
            offenders.push(format!("api/mod.rs:{}: {}", i + 1, raw.trim()));
        }
    }

    assert!(
        offenders.is_empty(),
        "the 5s pre-auth slow-loris read timeout is armed on `writer` (the \
         write fd) but `server_handshake_ndjson` reads from `reader` (the \
         cloned fd). The timeout only bounds the read by accident (dup() \
         shares SO_RCVTIMEO). Set it on the fd that is actually read — e.g. \
         `reader.get_ref().set_read_timeout(...)` — so the slow-loris guard \
         survives any future independent-timeout change:\n{}",
        offenders.join("\n")
    );
}
