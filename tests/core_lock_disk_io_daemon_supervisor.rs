//! Review-repro (scope: daemon-supervisor) — blocking file IO performed while
//! holding the per-agent core lock in the supervisor tick.
//!
//! FINDING (low / concurrency): inside the
//! `let action: Option<NoticeAction> = { let mut core = core.lock(); ... };`
//! block, the supervisor performs several synchronous filesystem reads/writes
//! while STILL holding the per-agent core mutex:
//!   - `clear_waiting_on_if_stale(home, &name, ...)`  (read_to_string + batch write)
//!   - `idle_expectation_for(home, &name)`            (FleetConfig::load = file read)
//!   - `read_input_submit_timestamps(home, &name)`    (file read)
//!
//! The same core mutex is taken by the PTY read-loop (`feed`) and the
//! render/monitor loops; holding it across multiple per-agent disk reads/writes
//! every 10s tick contends with and can stall live PTY output processing. This
//! contradicts the module's own discipline (the #1530/#1492 self-IPC-out-of-lock
//! pattern immediately below, and the Sprint-23 warning that disk reads under
//! the lock race writers) and the DAEMON-LOCK-ORDERING rule that disk IO stays
//! outside the core lock.
//!
//! METHOD: static_invariant (source-scan). The contention is a timing property
//! of the live `run_loop` tick (an infinite loop over a private `CoreMutex`),
//! and the fix is precisely the restructuring "capture in-memory values under
//! the lock, `drop(core)`, then do disk IO lock-free" — so we scan the REAL
//! lock-block region and assert those three disk-IO call sites are no longer
//! inside it. We bound the region to the lock block: from
//! `let action: Option<NoticeAction> = {` (lock open) to the
//! `// #1530: emit the collected reactions now that the core lock is dropped`
//! comment (which marks where the lock is dropped).
//!
//! RED now: all three calls appear inside the bounded lock-block region.
//! GREEN after fix: they move below `};` (lock dropped) → none remain in-region.

use std::path::PathBuf;

const LOCK_OPEN: &str = "let action: Option<NoticeAction> = {";
const LOCK_DROP_MARKER: &str = "emit the collected reactions now that the core lock is dropped";

fn supervisor_src() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("daemon")
        .join("supervisor.rs");
    std::fs::read_to_string(&path).expect("read src/daemon/supervisor.rs")
}

/// The text of the supervisor tick's per-agent core-lock block — from the lock
/// open to the post-lock emit marker.
fn lock_block_region(src: &str) -> &str {
    let start = src
        .find(LOCK_OPEN)
        .expect("lock-block open anchor `let action: Option<NoticeAction> = {` missing — re-point");
    let rest = &src[start..];
    let end = rest
        .find(LOCK_DROP_MARKER)
        .expect("post-lock emit marker missing — re-point this test");
    &rest[..end]
}

#[test]
#[ignore = "daemon-supervisor disk-io-under-core-lock: red until fix; remove #[ignore] after fix to confirm"]
fn disk_io_not_performed_under_core_lock_daemon_supervisor() {
    let src = supervisor_src();
    let region = lock_block_region(&src);

    // Sanity: we really did bound the lock block (it takes the core lock).
    assert!(
        region.contains("let mut core = core.lock();"),
        "bounded region does not contain `core.lock()` — anchors drifted, re-point"
    );

    // Strip comment lines so the prose mentions of these helpers (the long
    // #1530/#1492 rationale comments) don't false-positive.
    let mut code_only = String::new();
    for line in region.lines() {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') {
            continue;
        }
        code_only.push_str(line);
        code_only.push('\n');
    }

    let mut violations = Vec::new();
    let disk_io_calls = [
        "clear_waiting_on_if_stale(home",
        "idle_expectation_for(home",
        "read_input_submit_timestamps(home",
    ];
    for needle in &disk_io_calls {
        if code_only.contains(needle) {
            violations.push(*needle);
        }
    }

    assert!(
        violations.is_empty(),
        "blocking disk IO is performed while holding the per-agent core lock in \
         the supervisor tick: {violations:?} run inside the \
         `let action = {{ let mut core = core.lock(); ... }}` block. This contends \
         with the PTY read-loop `feed` (same core mutex) every 10s tick. Capture \
         the in-memory values needed (state, since.elapsed(), last_output, vterm \
         tails) under the lock, then `drop(core)` and perform \
         clear_waiting_on_if_stale / idle_expectation_for / \
         read_input_submit_timestamps lock-free — mirroring the post-lock \
         reaction-emit pattern already used immediately below (#1530/#1492)."
    );
}
