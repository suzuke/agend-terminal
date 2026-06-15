//! Review-repro (scope: daemon-supervisor) — router retain-block appends PTY
//! bytes to the mirror buffer without the cap and without an active/reply check.
//!
//! FINDING (low / correctness): in `src/daemon/router.rs::run_loop`, after the
//! main per-agent loop (which clears the buffer when `reply_to_channel` is None
//! and applies `apply_mirror_cap` when active), the trailing
//! `buffers.retain(|_, buf| match buf.rx.try_recv() { ... })` does an extra
//! `try_recv` purely to detect channel disconnection but ALSO pushes that chunk
//! into `buf.buffer` unconditionally — it does not consult
//! `heartbeat_pair`/`reply_to_channel` and does not call `apply_mirror_cap`. So a
//! chunk consumed here when mirroring is inactive is silently mixed into the
//! buffer (only cleared on the next main-loop iteration), and an oversized chunk
//! consumed here when active bypasses the `2*MAX_MIRROR_LEN` cap until the next
//! push.
//!
//! METHOD: static_invariant (source-scan). `run_loop` is a private,
//! infinite-looping fn over private `AgentBuffer` state with no extracted seam
//! for the retain probe, and the existing `retain_preserves_received_data` unit
//! test actually PINS the current (buggy) ungated-push behavior on a COPY of the
//! closure — so a behavioral test of a copy would not catch the prod regression.
//! We therefore scan the REAL `run_loop` body (bounded to `fn run_loop(` ..
//! next top-level `fn `, to exclude the test-module copy of the same closure)
//! and assert its `buffers.retain` probe does NOT do a bare, ungated
//! `buf.buffer.push_str(` — the disconnection probe must be a pure liveness
//! check (or route consumed bytes through the same reply_to_channel-gated +
//! apply_mirror_cap path the main loop uses).
//!
//! RED now: `run_loop`'s retain closure contains `buf.buffer.push_str(&text);`
//! with no gating → assertion fails.
//! GREEN after fix: the probe no longer blindly pushes into `buf.buffer` (it
//! either does not consume the payload, or runs it through the gated path).

use std::path::PathBuf;

fn router_src() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("daemon")
        .join("router.rs");
    std::fs::read_to_string(&path).expect("read src/daemon/router.rs")
}

/// Body of the prod `run_loop` fn — from `fn run_loop(` to the next top-level
/// `fn ` (excludes the `#[cfg(test)] mod tests` copy of the same retain closure).
fn run_loop_body(src: &str) -> &str {
    let start = src
        .find("fn run_loop(")
        .expect("fn run_loop( anchor missing in router.rs — re-point this test");
    let rest = &src[start..];
    // First top-level fn after run_loop is `fn try_dispatch_mirror`. Bound there.
    let end = rest[1..].find("\nfn ").map(|i| i + 1).unwrap_or(rest.len());
    &rest[..end]
}

#[test]
fn router_retain_probe_does_not_ungated_push_into_mirror_buffer_daemon_supervisor() {
    let src = router_src();
    let body = run_loop_body(&src);

    // Sanity: we bounded the real run_loop (it has the retain liveness probe and
    // the gated main-loop push), not the test copy.
    assert!(
        body.contains("buffers.retain("),
        "run_loop body does not contain `buffers.retain(` — anchors drifted, re-point"
    );
    assert!(
        body.contains("reply_to_channel"),
        "run_loop body does not reference `reply_to_channel` — the gating the probe \
         must respect — anchors drifted, re-point"
    );

    // Isolate the retain closure region: from `buffers.retain(` to the next
    // statement after the closure (`if !had_activity`), so the gated main-loop
    // push (inside the `for (name, buf)` loop ABOVE) is not counted.
    let retain_start = body
        .find("buffers.retain(")
        .expect("buffers.retain( anchor missing");
    let retain_region = &body[retain_start..];
    let retain_end = retain_region
        .find("if !had_activity")
        .unwrap_or(retain_region.len());
    let retain_block = &retain_region[..retain_end];

    assert!(
        !retain_block.contains("buf.buffer.push_str("),
        "router run_loop's trailing `buffers.retain` disconnection-probe pushes \
         consumed PTY bytes into `buf.buffer` UNGATED: it ignores \
         `reply_to_channel`/`heartbeat_pair` (mixing stray bytes into the mirror \
         buffer when mirroring is inactive) and skips `apply_mirror_cap` (an \
         oversized chunk bypasses the 2*MAX_MIRROR_LEN cap). Make it a pure \
         liveness probe — detect disconnection without consuming payload, or run \
         any consumed chunk through the same reply_to_channel-gated + \
         apply_mirror_cap path the main per-agent loop uses."
    );
}
