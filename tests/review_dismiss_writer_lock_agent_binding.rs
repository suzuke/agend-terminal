//! review-repro (scope: agent-binding) — static-invariant guard for the
//! dismiss-thread unbounded-write finding.
//!
//! Finding: the auto-dismiss thread (`src/agent/dismiss.rs`) acquires the
//! shared `pty_writer` lock DIRECTLY (`let mut w = writer.lock();`) and does
//! raw `write_all`/`flush` with NO timeout while holding it. Every OTHER write
//! path routes through `write_with_timeout`, which bounds the write on a
//! spawned worker so a non-draining PTY can't pin the lock forever. If a hung
//! agent stops draining its PTY input buffer (exactly when a dialog-dismiss
//! fires), the dismiss thread's `write_all` blocks indefinitely while holding
//! `pty_writer.lock()`, wedging ALL future injects to that agent until daemon
//! restart.
//!
//! Driving the actual indefinite block would hang the test, so this is a
//! SOURCE-SCANNING invariant (mirrors `tests/core_mutex_invariant.rs`): the
//! dismiss thread must NOT grab the raw writer lock for an unbounded write.
//! The fix routes the (tiny) dismiss keystrokes through `write_with_timeout`
//! (or a bounded write), so `writer.lock()` no longer appears in dismiss.rs.
//!
//! GREEN on current code: the raw `writer.lock()` acquisitions for the
//! unbounded `write_all` are gone (the fix routes through a bounded write), so
//! this runs un-ignored as a live regression guard.

use std::path::PathBuf;

#[test]
fn dismiss_thread_has_no_raw_unbounded_writer_lock_agent_binding() {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/agent/dismiss.rs");
    let text = std::fs::read_to_string(&file).expect("read src/agent/dismiss.rs");

    let mut violations = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        // Skip comment / doc lines that merely mention the pattern in prose.
        if t.starts_with("//") || t.starts_with('*') {
            continue;
        }
        // The dismiss thread grabs the raw shared writer lock to do an
        // unbounded `write_all`. `write_with_timeout` (the bounded path) does
        // its `.lock()` on a thread-local clone inside its own worker, never
        // here. The `test_writer()` helper uses `Mutex::new(...)`, not
        // `writer.lock()`, so it does not match.
        if line.contains("writer.lock()") {
            violations.push(format!("{}: {}", i + 1, line.trim()));
        }
    }

    assert!(
        violations.is_empty(),
        "src/agent/dismiss.rs grabs the raw shared `writer.lock()` for an \
         UNBOUNDED `write_all` in the dismiss thread — a non-draining PTY pins \
         the lock forever and wedges all future injects. Route the dismiss \
         keystrokes through `write_with_timeout` (bounded) instead:\n{}",
        violations.join("\n")
    );
}
