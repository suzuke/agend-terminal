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
    assert_eq!(
        warns,
        1,
        "#5 maintainability: the #1808-probe0-phantom consecutive-rematch WARN fired \
         {warns} times for ONE in-place static SRL across {} ticks — it must fire \
         EXACTLY once (the first re-match WARNs; subsequent ticks dedup on the \
         (line_hash, kind) latch). 0 = the latch swallowed the genuine first WARN; \
         >1 = per-tick flood.",
        spinners.len()
    );
}

// ── CR-2026-06-14 t-43 — SRL phantom WARN latch is SET-ONLY ──────────────────
//
// `last_srl_phantom_warn_sig` (and `last_srl_keep_latched_sig`) are set once and
// NEVER reset — unlike `last_unclassified_throttle_sig`, which clears when the
// pane leaves the throttle-miss shape so a recurrence re-logs once. Consequently
// a SECOND, genuinely-distinct SRL incident on the SAME error line — separated
// by a real recovery (productive output) — is silently suppressed (WARN
// swallowed), losing the telemetry for the new stuck episode. The fix clears the
// latches at the `current != SRL` recovery point, GATED on `recovered_within`
// (genuine productive recovery) so an active cross-cycle phantom override — which
// also lands a non-SRL state but with no productive output — keeps its per-tick
// dedup.

/// A RED SRL screen (error line on top, a `waiting` spinner below with NO working
/// marker → latches normally). `sp` flips the glyph so the screen hash differs
/// from the preceding feed while the `srl_match_signature` (line_hash,
/// dist_from_bottom) stays identical.
fn srl_screen(sp: char) -> Vec<u8> {
    format!("\x1b[2J\x1b[H{RED_16}{SRL_LINE}{SGR_RESET}\r\n{sp} waiting\r\n").into_bytes()
}

/// Feed one SRL frame. `recovered` toggles `last_productive_output` to drive the
/// #badge-recovery path: with recent productive output the SRL yields to Idle
/// (genuine recovery — `current` leaves ServerRateLimit, `non_srl_since_last_srl`
/// is set); with none, the stale error re-matches with `!recovered_now`. The
/// sticky SRL latch never yields to a bare idle prompt, so productive output is
/// the ONLY deterministic lever to land a non-SRL state from an SRL screen.
fn feed_srl(vt: &mut VTerm, st: &mut StateTracker, sp: char, recovered: bool) {
    st.last_productive_output = if recovered {
        Some(Instant::now())
    } else {
        None
    };
    // Age `since` past the 2s active-state min-hold so the priority-DOWN
    // SRL→Idle transition (#badge-recovery / #1809 cross-cycle override) is not
    // rejected by `transition`'s hysteresis — in production the SRL is held far
    // longer than the hold before recovery. Mirrors src/state/tests.rs.
    st.since = Instant::now() - Duration::from_secs(3);
    drive(vt, st, &srl_screen(sp));
}

#[test]
fn srl_phantom_warn_relogs_after_genuine_recovery() {
    let mut vt = VTerm::new(120, 24);
    let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
    st.instance_name = "t43-relog".into();

    let logs = capture_all_logs(|| {
        // Incident 1: latch the SRL, recover once (→ Idle, arms cross_cycle), then
        // the stale error re-matches with no recent productive output →
        // cross_cycle refire → phantom WARN #1 (latches the dedup sig).
        feed_srl(&mut vt, &mut st, '⠋', false); // latch (first detect, no warn)
        feed_srl(&mut vt, &mut st, '⠙', true); // #badge-recovery → Idle, non_srl=true
        feed_srl(&mut vt, &mut st, '⠹', false); // cross_cycle → WARN #1

        // GENUINE recovery again: productive output lands Idle → the recovery point
        // (`current != SRL` AND recovered_within) CLEARS the fire-once latch.
        feed_srl(&mut vt, &mut st, '⠸', true); // recovery → resets the latch

        // Incident 2: same error line re-grabbed, no recent productive output →
        // cross_cycle refire → the WARN must fire a 2ND time (latch was cleared).
        feed_srl(&mut vt, &mut st, '⠼', false); // cross_cycle → WARN #2
    });

    let warns = logs.matches("#1808-probe0-phantom").count();
    assert_eq!(
        warns, 2,
        "t-43: the #1808-probe0-phantom WARN fired {warns} times — a 2nd SRL incident \
         on the same error line AFTER a genuine recovery must re-log once (expected 2). \
         1 = the SET-ONLY latch suppressed the second incident's WARN (the bug)."
    );
}

#[test]
fn srl_phantom_warn_dedups_across_cross_cycle_loop_without_recovery() {
    // Guard for the t-43 fix: the recovery-gated reset must NOT clear the latch on
    // a cross_cycle phantom override (which lands Idle every tick but with NO
    // productive output). Were the reset ungated (`current != SRL` alone), each
    // override-Idle feed would clear the latch → the cross_cycle WARN would re-fire
    // on every stale re-grab = the #1808 flood. The `recovered_within` gate holds
    // the dedup → the whole no-recovery loop WARNs exactly once.
    let mut vt = VTerm::new(120, 24);
    let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
    st.instance_name = "t43-noflood".into();

    let logs = capture_all_logs(|| {
        feed_srl(&mut vt, &mut st, '⠋', false); // latch
        feed_srl(&mut vt, &mut st, '⠙', true); // recover once → Idle, arms cross_cycle
                                               // Sustained phantom loop: stale error re-grabbed every tick, NEVER recovers
                                               // → each is a cross_cycle override to Idle with recovered=false.
        for sp in ['⠹', '⠸', '⠼', '⠴', '⠦', '⠧'] {
            feed_srl(&mut vt, &mut st, sp, false);
        }
    });

    let warns = logs.matches("#1808-probe0-phantom").count();
    assert_eq!(
        warns, 1,
        "t-43 guard: a cross_cycle phantom loop with NO recovery WARNed {warns} times \
         — it must dedup to EXACTLY once. >1 means the recovery gate leaked and the \
         latch reset on a phantom (non-recovered) Idle override (the #1808 flood)."
    );
}
