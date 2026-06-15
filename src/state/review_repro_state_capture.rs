//! Verification/reproduction tests for the state-capture review batch.
//!
//! Each `#[ignore]`d test encodes the CORRECT post-fix behavior so it is RED
//! against the current buggy code and GREEN once the fix lands. Remove the
//! `#[ignore]` after the corresponding fix to confirm.
//!
//! Attached as an in-module submodule of `crate::state` so it can drive the
//! private `StateTracker` fields/methods (`context_regex`, `instance_name`,
//! `scan_context_pct`, the post-classify `capture_unclassified_throttle` side
//! log) directly — these are not part of the thin `lib`/`main` surface.

use super::*;
use crate::backend::Backend;
use crate::vterm::VTerm;

// ── shared harness (self-contained; mirrors src/state/tests.rs) ─────────────

/// Push raw PTY bytes through the REAL `vterm → tail_lines_with_fg →
/// feed_with_fg` production seam (so the color anchor + dedup behave exactly
/// as in production). Mirrors `tests::drive`.
fn drive(vt: &mut VTerm, st: &mut StateTracker, bytes: &[u8]) {
    vt.process(bytes);
    let rows = vt.rows() as usize;
    let (screen, fg) = vt.tail_lines_with_fg(rows);
    st.feed_with_fg(&screen, &fg);
}

/// Capture EVERY tracing event (any target, TRACE+) emitted while `f` runs.
/// The state-detection WARNs use a custom `target: "state_detection"` that the
/// default `tracing_test` filter drops, so we install an unfiltered subscriber
/// for the closure (mirrors `tests::capture_all_logs`).
fn capture_all_logs<F: FnOnce()>(f: F) -> String {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    #[derive(Clone)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("capture buf mutex")
                .extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Buf {
            self.clone()
        }
    }
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sub = tracing_subscriber::fmt()
        .with_writer(Buf(buf.clone()))
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::subscriber::with_default(sub, f);
    let bytes = buf.lock().expect("capture buf mutex").clone();
    String::from_utf8(bytes).expect("capture buf is utf8")
}

const RED_16: &str = "\x1b[31m";
const SGR_RESET: &str = "\x1b[0m";
/// Full Claude-Code SRL line; its `Server is temporarily limiting requests`
/// substring matches the ServerRateLimit regex.
const SRL_LINE: &str = "API Error: Server is temporarily limiting requests (not your usage limit)";

// ── Finding #1 — unbounded growth of unclassified_errors.jsonl ──────────────
//
// `capture_unclassified_throttle` has NO per-record dedup latch; it relies on
// the feed-level hash-dedup, which is DELIBERATELY bypassed when a throttle
// hint is on screen and the tracker is not already throttle-latched. A pane
// that statically DISPLAYS a throttle phrase while the classifier lands on a
// non-retryable state therefore appends a FULL-screen JSONL record on every
// PTY read of the SAME screen — unbounded file growth.

#[test]
fn unclassified_throttle_static_screen_logs_once_not_per_tick() {
    // Isolate the on-disk sidecar to a throwaway HOME so we don't touch the
    // operator's real $AGEND_HOME/unclassified_errors.jsonl.
    let home = std::env::temp_dir().join(format!(
        "agend_state_capture_f1_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&home).expect("create temp home");
    std::env::set_var("AGEND_HOME", &home);
    let path = home.join("unclassified_errors.jsonl");
    let _ = std::fs::remove_file(&path);

    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    t.current = AgentState::Idle;
    t.instance_name = "f1".into();

    // A throttle DIAG phrase ("Overloaded errors") sits in prose — it carries
    // the throttle HINT token "Overloaded" (so the hash-dedup bypass fires) but
    // is NOT an error-line shape, so the classifier stays on a non-retryable
    // state. The screen is byte-identical across feeds → identical hash → the
    // ONLY reason it re-enters the feed body is the throttle-hint bypass.
    let screen = "Assistant: I should explain. Overloaded errors are transient; just retry.\n❯ ";
    for _ in 0..6 {
        t.feed(screen);
    }

    let contents = std::fs::read_to_string(&path).unwrap_or_default();
    let records = contents.lines().filter(|l| !l.trim().is_empty()).count();
    let _ = std::fs::remove_file(&path);

    assert!(
        records <= 1,
        "#1 resource-leak: a STATIC unclassified-throttle screen fed 6× appended \
         {records} JSONL records to unclassified_errors.jsonl (one per tick via the \
         hash-dedup throttle-hint bypass). After the per-signature fire-once latch \
         it must log at most once per distinct screen."
    );
}

// ── Finding #2 — #2086-srl-keep-latched WARN re-fires every feed ────────────
//
// `apply_working_marker_override` emits the #2086 WARN with NO dedup whenever a
// genuinely-stuck SRL has a working spinner rendered below it and no recent
// productive output. Detection is level-triggered and the screen hash flips on
// each spinner/clock tick, so the WARN re-fires every feed for the entire
// (potentially ~26 min) stuck duration — the same flood class fixed elsewhere
// with `last_anchor_suppress_hash`.

#[test]
fn srl_keep_latched_warn_dedups_across_spinner_ticks() {
    let mut vt = VTerm::new(120, 24);
    let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
    st.instance_name = "f2".into();
    // No recent productive output → recovered=false → the #2086 keep-latched
    // branch (not the recovery branch).

    // Same RED SRL error on the top row, a Thinking "Stewing…" spinner BELOW it
    // (a working_state_below marker) that ticks each frame — the exact stuck
    // rate-limited retry-spinner shape #2086 keeps latched. Single-digit seconds
    // keep every frame the same byte length (stable srl signature) while the
    // glyph change flips the screen hash so detection re-runs.
    let spinners = ['✻', '✢', '✶', '✳', '✽', '·', '*', '✻'];
    let logs = capture_all_logs(|| {
        for (i, sp) in spinners.iter().enumerate() {
            let bytes = format!(
                "\x1b[2J\x1b[H{RED_16}{SRL_LINE}{SGR_RESET}\r\n{sp} Stewing\u{2026} ({i}s)\r\n"
            );
            drive(&mut vt, &mut st, bytes.as_bytes());
        }
    });

    assert_eq!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "#2 precondition: the stuck SRL must remain latched across the ticks"
    );
    let warns = logs.matches("#2086-srl-keep-latched").count();
    assert!(
        warns <= 1,
        "#2 maintainability: the #2086-srl-keep-latched WARN fired {warns} times for ONE \
         stuck SRL across {} spinner ticks (per-tick flood reproducing the #1450 \
         14k-lines/incident class). After a dedup latch keyed on srl_match_signature it \
         must fire at most once per distinct stuck-error signature.",
        spinners.len()
    );
}

// ── Finding #4 — scan_context_pct panics on a capture-group-less pattern ─────
//
// `scan_context_pct` indexes `caps[1]` on a regex compiled from the per-backend
// `context_pattern`. A pattern that matches but has NO capture group 1 panics
// the PTY read loop (Index panic — not caught by clippy's unwrap_used lint).
// The fix (`caps.get(1)`) degrades a missing group to "no reading".

#[test]
fn scan_context_pct_no_capture_group_does_not_panic() {
    let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
    // A context_pattern that MATCHES the statusline but has no capture group 1
    // (a plausible future backend profile). `caps[1]` is out of bounds here.
    t.context_regex = Some(regex::Regex::new(r"\d+%").expect("compile group-less pattern"));

    let screen = "Status: 42% context used\n\u{276f} ";
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        t.scan_context_pct(screen);
    }));

    assert!(
        result.is_ok(),
        "#4 error-handling: scan_context_pct PANICKED on a context_pattern with no \
         capture group 1 (caps[1] index out of bounds) — this would crash the PTY read \
         loop on the first matching frame. Use caps.get(1) so a missing group degrades \
         to 'no reading'."
    );
}

// ── Finding #5 — #1808-probe0-phantom consecutive-rematch WARN floods ────────
//
// `apply_srl_phantom_gate` increments `srl_consecutive_rematch` on each
// same-signature re-detection and WARNs whenever it would latch with no recent
// productive output. For an in-place static SRL (same line_hash +
// dist_from_bottom across clock-tick redraws) the counter stays > 0 and the
// WARN fires on every feed for the whole throttle duration with no per-signature
// dedup.

#[test]
fn srl_phantom_consecutive_rematch_warn_dedups_on_static_throttle() {
    let mut vt = VTerm::new(120, 24);
    let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
    st.instance_name = "f5".into();

    // Same RED SRL error line every tick, NO working marker below (so it latches
    // normally → would_latch=true, current stays ServerRateLimit). The waiting
    // glyph below changes (same byte length) so the screen hash flips but the
    // srl signature (line_hash, dist_from_bottom) is identical →
    // srl_consecutive_rematch keeps incrementing.
    let spinners = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];
    let logs = capture_all_logs(|| {
        for sp in &spinners {
            let bytes = format!("\x1b[2J\x1b[H{RED_16}{SRL_LINE}{SGR_RESET}\r\n{sp} waiting\r\n");
            drive(&mut vt, &mut st, bytes.as_bytes());
        }
    });

    assert_eq!(
        st.get_state(),
        AgentState::ServerRateLimit,
        "#5 precondition: the static SRL must remain latched across the ticks"
    );
    let warns = logs.matches("#1808-probe0-phantom").count();
    assert!(
        warns <= 1,
        "#5 maintainability: the #1808-probe0-phantom consecutive-rematch WARN fired \
         {warns} times for ONE in-place static SRL across {} ticks (per-tick flood). \
         After deduping on last_srl_match_sig / emitting only on a signature transition \
         it must fire at most once for a static long throttle.",
        spinners.len()
    );
}
