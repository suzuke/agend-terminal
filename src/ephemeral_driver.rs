//! #1967 Phase-1 PR3b: the one-shot ephemeral-worker DRIVER (Route B).
//!
//! After [`crate::ephemeral_tracking::spawn_and_track`] finalizes a headless
//! opencode/claude worker, it launches a background driver thread (this module) that
//! runs exactly ONE prompt turn to a recorded result:
//!
//!   wait-ready → inject prompt → turn-end (Idle-debounce) → capture → oracle → record
//!
//! The driver is decoupled from the worker's [`crate::agent::EphemeralPtyHandle`]
//! (the reaper owns that in `LIVE_CHILDREN`): it holds only Arc clones of the PTY
//! writer + core (inside its [`crate::agent::InjectTarget`]), so the reap sweep can
//! terminate the worker process independently. On completion the driver marks the
//! worker `done`, and the next reap sweep terminates the now-idle process + frees the
//! cap slot.
//!
//! ## Empirical constants (PR3b 1a confirm-first smoke, lead-vetted — DO NOT change
//! without re-smoking on a real backend)
//! - **Poll `get_state()` ONLY, NEVER `state.tick()`.** The ephemeral read loop
//!   ([`crate::agent`] `ephemeral_pty_read_loop`) never ticks, so the 30 s
//!   `LATCHED_STATE_EXPIRY` Thinking→Idle decay is disabled and no phantom timer-Idle
//!   fires. Calling `tick()` here would re-enable that decay → a false turn-end on a
//!   quiet >30 s stretch. So the driver reads state, never advances the clock.
//! - **Idle-debounce = 3 s of held Idle** before declaring the turn done. opencode
//!   renders the Thinking marker CONTINUOUSLY while streaming (0 ms mid-turn Idle lull
//!   observed on a fast model); 3 s cheaply covers a slower model's inter-render gaps.
//!   A mid-turn Idle BLIP shorter than the window can never end the turn (the detector
//!   resets the held-Idle clock when it sees work resume).
//! - **Success oracle = terminal `Idle` ∧ ¬error-class ([`AgentState::is_error`]) ∧
//!   transcript GREW.** NOT `has_productive_output()` — that is false on a text-only
//!   success (opencode's productive markers match only tool-use glyphs).
//! - **Transcript "grew" = an ANSWER appeared after the prompt echo** ([`transcript_grew`]):
//!   locate the injected-prompt echo in the (chrome-stripped) transcript and require
//!   non-blank content after it. This is MONOTONIC + term-size-independent + per-backend-
//!   free — it does not depend on screen density. (A raw nonblank-COUNT delta only works
//!   for ALT-SCREEN TUIs whose banner persists in both snapshots, e.g. claude/opencode
//!   `max_scroll=0`; a SCROLLING-TUI backend would false-negative — #2408 lead vet.) The
//!   count-delta is kept only as the echo-scrolled-off FALLBACK. The `result_summary`
//!   (cosmetic) drops the trailing footer via a PATTERN-based strip ([`is_trailing_chrome`]),
//!   not a per-backend magic row count, so it survives a backend bumping its footer.
//!
//! Scope: opencode (Slice-1) + claude (Slice-2) — each §5-smoke-validated. The gate
//! ([`crate::ephemeral_tracking`] `driver_supported`) admits exactly this set; other
//! backends are later slices. Both proven to render a continuous work marker so the
//! Idle-debounce isn't fooled by a mid-turn idle. Durable telemetry (fleet_events
//! L1/L2/L3 + task_events) is PR4; the result lands on the worker row for now.

use crate::agent::InjectTarget;
use crate::state::AgentState;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How often the driver samples `get_state()`. Tight enough to catch opencode's
/// continuous-Thinking turn boundaries, loose enough to be near-free.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Held-Idle window (ms) before declaring the turn done — the lead-vetted
/// conservative 3 s (covers a slow model's inter-render gaps; a fast model's 0 ms lull
/// is well inside it).
const TURN_DEBOUNCE_MS: u64 = 3_000;

/// Max wait for the worker to reach its first `Idle` (ready) after spawn (capped by
/// the worker's wall-TTL). opencode reaches ready in ~2.6 s, claude in ~11 s; this is a
/// generous cap for both.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Logical lines to dump from the vterm tail for the body measure + result capture.
/// The worker's vterm is 50 rows; 80 covers the visible screen with margin for
/// dewrapping (which merges wrapped rows into fewer logical lines).
const CAPTURE_LINES: usize = 80;

/// Cap on the persisted `result_summary` so a runaway transcript can't bloat the JSON
/// sidecar. Coarse — PR4 routes the full transcript to durable telemetry.
const MAX_SUMMARY_BYTES: usize = 8_192;

/// Everything the driver thread needs. Built in `spawn_and_track` (which holds the
/// handle) and moved into the thread; the [`InjectTarget`] carries Arc clones of the
/// PTY writer + core, so the driver is independent of the handle the reaper owns.
pub(crate) struct DriverConfig {
    pub home: PathBuf,
    pub worker_id: String,
    pub prompt: String,
    /// Inject capability + the worker's `core` (polled for state/vterm). Built via
    /// [`InjectTarget::from_ephemeral`].
    pub inject_target: InjectTarget,
    /// The worker's wall-TTL (the reap sweep's cost guard); the driver's hard ceiling.
    pub wall_ttl: Duration,
}

/// Launch the one-shot driver thread for a freshly-finalized worker.
pub(crate) fn spawn_driver(cfg: DriverConfig) {
    // fire-and-forget: drives the one-shot turn off the MCP `ephemeral spawn` handler,
    // which returns worker_id+pid IMMEDIATELY (the PR1/2 async contract). The thread is
    // self-bounding — it exits when the turn ends, an error class latches, an inject
    // fails, or the wall-TTL lapses — and the reap sweep terminates the worker PROCESS
    // independently, so no graceful JoinHandle is kept: a daemon shutdown simply drops
    // the worker (its `reap_on_boot` sweeps any orphan on the next boot).
    std::thread::spawn(move || run_turn(cfg));
}

/// The driver's body: wait-ready → inject → turn-end → capture → oracle → record.
/// ALWAYS records a result (success or a failure reason) so the worker never hangs in
/// `running`/`prompting` until the wall-TTL reap.
fn run_turn(cfg: DriverConfig) {
    let DriverConfig {
        home,
        worker_id,
        prompt,
        inject_target,
        wall_ttl,
    } = cfg;
    let start = Instant::now();
    let wall_ttl_ms = wall_ttl.as_millis() as u64;

    // Phase 0 — wait for the worker to reach its first Idle (ready). Poll get_state()
    // ONLY (never tick()). An error class before ready, or a ready timeout, fails the
    // turn immediately.
    let ready_cap_ms = (READY_TIMEOUT.as_millis() as u64).min(wall_ttl_ms);
    loop {
        let now_ms = start.elapsed().as_millis() as u64;
        let state = inject_target.core.lock().state.get_state();
        if state.is_error() {
            return record_failure(&home, &worker_id, &format!("not ready: {state:?}"));
        }
        if state == AgentState::Idle {
            break; // ready
        }
        if now_ms >= ready_cap_ms {
            return record_failure(&home, &worker_id, "not ready: timed out before Idle");
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    // Baseline non-blank-line count (at ready, before inject) for the "grew" measure.
    let baseline_dump = inject_target
        .core
        .lock()
        .vterm
        .tail_lines_dewrapped(CAPTURE_LINES);
    let baseline_body = nonblank_count(&baseline_dump);

    // Phase 1 — inject the prompt. Mark phase=prompting first so `ephemeral list`
    // reflects the mid-turn state.
    crate::ephemeral_tracking::mark_prompting(&home, &worker_id);
    if let Err(e) = crate::agent::run_ephemeral_inject(&inject_target, prompt.as_bytes()) {
        return record_failure(&home, &worker_id, &format!("inject failed: {e}"));
    }

    // Phase 2 — wait for turn end: Idle held ≥ debounce after work, an error class, or
    // the wall-TTL. Poll get_state() ONLY (never tick()).
    let mut detector = TurnEndDetector::new(TURN_DEBOUNCE_MS);
    let stop_reason = loop {
        let now_ms = start.elapsed().as_millis() as u64;
        if now_ms >= wall_ttl_ms {
            break "wall-TTL elapsed before turn end";
        }
        let state = inject_target.core.lock().state.get_state();
        match detector.observe(state, now_ms) {
            TurnDecision::Continue => std::thread::sleep(POLL_INTERVAL),
            TurnDecision::TurnEnded => break "turn ended (Idle held)",
            TurnDecision::ErrorClass => break "error-class state latched",
        }
    };

    // Phase 3 — capture + oracle. Re-read the terminal state + dump at the end (an
    // error banner can scroll off, so sample near turn-end), then decide success.
    let end_dump = inject_target
        .core
        .lock()
        .vterm
        .tail_lines_dewrapped(CAPTURE_LINES);
    let terminal_state = inject_target.core.lock().state.get_state();
    let grew = transcript_grew(&end_dump, &prompt, baseline_body);
    let success = oracle_success(terminal_state, grew);
    let summary = build_summary(&end_dump, terminal_state, grew, stop_reason);

    tracing::info!(
        target: "ephemeral",
        worker_id = %worker_id,
        success,
        terminal_state = ?terminal_state,
        grew,
        stop_reason,
        "ephemeral driver turn complete"
    );
    crate::ephemeral_tracking::record_result(&home, &worker_id, summary, success);
}

/// Record a pre-turn failure (not-ready / inject error): the worker produced nothing,
/// so success=false with the reason as the summary.
fn record_failure(home: &std::path::Path, worker_id: &str, reason: &str) {
    tracing::warn!(target: "ephemeral", worker_id = %worker_id, reason, "ephemeral driver turn failed");
    crate::ephemeral_tracking::record_result(home, worker_id, format!("driver: {reason}"), false);
}

/// Success oracle (#1967 PR3b finding 2): a turn SUCCEEDED iff the worker is at a
/// terminal `Idle` (NOT a latched error class — [`AgentState::is_error`] covers
/// ContextFull/RateLimit/UsageLimit/AuthError/ApiError/Crashed/…), AND the transcript
/// grew (the worker actually produced output). Deliberately NOT
/// `has_productive_output()` (false on a text-only success).
fn oracle_success(terminal_state: AgentState, transcript_grew: bool) -> bool {
    terminal_state == AgentState::Idle && !terminal_state.is_error() && transcript_grew
}

/// Count non-blank lines in a vterm dump. Used as the FALLBACK "grew" signal (see
/// [`transcript_grew`]): a baseline↔final DELTA. Chrome is NOT excluded — the footer row
/// COUNT is identical at both samples (only its content churns), so it cancels in the
/// delta; counting LINES (not bytes) makes a chrome update invisible.
fn nonblank_count(dump: &str) -> usize {
    dump.lines().filter(|l| !l.trim().is_empty()).count()
}

/// Did the worker produce an ANSWER this turn? PR3b Slice-2 (lead vet) — a MONOTONIC,
/// term-size-independent, per-backend-free signal: locate the injected-prompt echo in
/// the (chrome-stripped) transcript and require non-blank content AFTER it (= the
/// assistant's reply).
///
/// Why not the raw nonblank-COUNT delta: that only works for an ALT-SCREEN TUI whose
/// startup banner persists in BOTH the baseline and final snapshots (claude/opencode —
/// empirically `max_scroll=0`), so `final = banner + answer > baseline = banner`. A
/// SCROLLING-TUI backend (a future slice) would FALSE-NEGATIVE when a dense ready screen
/// scrolls off behind a short answer (`nonblank(final) ≤ nonblank(baseline)`). Anchoring
/// on the prompt echo sidesteps screen density entirely — content after the echo is the
/// answer regardless of how the screen scrolled.
///
/// FALLBACK: if the echo is not found (a long turn scrolled it off the captured window),
/// fall back to the conservative nonblank-COUNT delta (`final > baseline`) so a
/// genuinely-grown transcript whose echo is no longer visible is not wrongly judged empty.
fn transcript_grew(end_dump: &str, prompt: &str, baseline_nonblank: usize) -> bool {
    let body = strip_trailing_chrome(end_dump);
    let body_lines: Vec<&str> = body.lines().collect();
    match locate_prompt_echo(&body_lines, prompt) {
        // Answer = any non-blank line after the echo (trailing chrome already stripped).
        Some(echo_idx) => body_lines[echo_idx + 1..]
            .iter()
            .any(|l| !l.trim().is_empty()),
        // Echo scrolled off → conservative count-delta fallback.
        None => nonblank_count(end_dump) > baseline_nonblank,
    }
}

/// A stable prefix of the prompt to match the echo on — first ~40 chars (or the whole
/// prompt if shorter). A prefix (not the full text) so a TUI that WRAPS or truncates a
/// long echo still anchors on its first rendered line.
fn prompt_echo_needle(prompt: &str) -> &str {
    let trimmed = prompt.trim();
    let end = trimmed
        .char_indices()
        .nth(40)
        .map(|(i, _)| i)
        .unwrap_or(trimmed.len());
    &trimmed[..end]
}

/// Find the transcript line echoing the injected prompt, by matching its prefix
/// ([`prompt_echo_needle`]). Returns the LAST match (the most recent turn's echo, so a
/// prompt that also appears earlier in scrollback doesn't mis-anchor). `None` if the
/// prompt is empty or its echo isn't present (scrolled off / not yet rendered).
fn locate_prompt_echo(lines: &[&str], prompt: &str) -> Option<usize> {
    let needle = prompt_echo_needle(prompt);
    if needle.is_empty() {
        return None;
    }
    lines.iter().rposition(|l| l.contains(needle))
}

/// True if `line` is trailing CHROME (a footer / statusline row), not transcript body.
/// PATTERN-based (PR3b Slice-2 lead vet) rather than a per-backend magic row count, so
/// it survives a backend bumping its footer's row count. Covers opencode + claude
/// footers; an unrecognized footer line simply isn't stripped — purely COSMETIC (the
/// "grew" oracle never uses this; it's chrome-cancelling). Used only to clean the
/// persisted `result_summary`.
fn is_trailing_chrome(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return true; // trailing blank rows
    }
    // A separator / box-drawing-frame-only row (claude's `───` rules + box borders).
    if t.chars().all(|c| {
        matches!(
            c,
            '─' | '━'
                | '│'
                | '┃'
                | '╭'
                | '╮'
                | '╰'
                | '╯'
                | '├'
                | '┤'
                | '┬'
                | '┴'
                | ' '
        )
    }) {
        return true;
    }
    // A lone input-prompt glyph = an EMPTY input box (NOT `❯ <prompt>`, which is body).
    if matches!(t, "❯" | "›" | ">" | "┃") {
        return true;
    }
    // Known backend footer statuslines (claude + opencode).
    let lower = t.to_ascii_lowercase();
    lower.contains("bypass permissions")        // claude: `⏵⏵ bypass permissions …`
        || lower.contains("ctx used")           // claude: `Model: … | Ctx Used: …`
        || lower.starts_with("model:")          // claude statusline
        || lower.starts_with("▣ build")         // opencode completion line
        || lower.contains("esc to interrupt") // any working footer (safety; not at Idle)
}

/// The transcript body = the dump with its trailing chrome rows stripped (pattern-
/// based). Walks from the bottom dropping chrome rows, stops at the first body row.
fn strip_trailing_chrome(dump: &str) -> String {
    let lines: Vec<&str> = dump.lines().collect();
    let keep = lines.len()
        - lines
            .iter()
            .rev()
            .take_while(|l| is_trailing_chrome(l))
            .count();
    lines[..keep].join("\n")
}

/// Build the persisted `result_summary`: the body region (trailing chrome stripped),
/// trimmed of trailing blanks and capped at [`MAX_SUMMARY_BYTES`], prefixed with a
/// one-line verdict so a reader sees the outcome without re-deriving it.
fn build_summary(dump: &str, terminal_state: AgentState, grew: bool, stop_reason: &str) -> String {
    let body = strip_trailing_chrome(dump);
    let body = body.trim_end();
    let header = format!("[{terminal_state:?} grew={grew} stop={stop_reason}]\n");
    let mut out = header;
    out.push_str(body);
    if out.len() > MAX_SUMMARY_BYTES {
        // Truncate on a char boundary (byte slicing a multi-byte char would panic).
        let mut cut = MAX_SUMMARY_BYTES;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str("…[truncated]");
    }
    out
}

/// Pure turn-end detector — the Idle-debounce state machine, time-injected (`now_ms`)
/// so it is unit-testable with a synthetic state sequence (no sleeps, no real clock).
#[derive(Debug)]
struct TurnEndDetector {
    debounce_ms: u64,
    phase: DetectorPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectorPhase {
    /// Post-inject: waiting to SEE the worker leave Idle into work. A brief post-inject
    /// (or pre-turn) Idle here must NOT count as the turn end.
    AwaitingWork,
    /// Saw work; waiting for the worker to return to Idle.
    Working,
    /// Idle seen at `idle_since_ms`; waiting for it to hold ≥ `debounce_ms`.
    Settling { idle_since_ms: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnDecision {
    /// Keep polling.
    Continue,
    /// Idle held ≥ debounce after work — the turn ended cleanly.
    TurnEnded,
    /// An error class latched — stop now (the turn failed).
    ErrorClass,
}

impl TurnEndDetector {
    fn new(debounce_ms: u64) -> Self {
        Self {
            debounce_ms,
            phase: DetectorPhase::AwaitingWork,
        }
    }

    /// Feed one `get_state()` sample at `now_ms` (ms since the driver started).
    fn observe(&mut self, state: AgentState, now_ms: u64) -> TurnDecision {
        if state.is_error() {
            return TurnDecision::ErrorClass;
        }
        match self.phase {
            DetectorPhase::AwaitingWork => {
                // Any non-Idle, non-error state means work has started.
                if state != AgentState::Idle {
                    self.phase = DetectorPhase::Working;
                }
                TurnDecision::Continue
            }
            DetectorPhase::Working => {
                if state == AgentState::Idle {
                    self.phase = DetectorPhase::Settling {
                        idle_since_ms: now_ms,
                    };
                }
                TurnDecision::Continue
            }
            DetectorPhase::Settling { idle_since_ms } => {
                if state == AgentState::Idle {
                    if now_ms.saturating_sub(idle_since_ms) >= self.debounce_ms {
                        TurnDecision::TurnEnded
                    } else {
                        TurnDecision::Continue
                    }
                } else {
                    // A mid-turn Idle BLIP ended (work resumed) — reset the held-Idle
                    // clock so a blip shorter than the window can never end the turn.
                    self.phase = DetectorPhase::Working;
                    TurnDecision::Continue
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ───────────────────────── oracle truth-table ──────────────────────────

    #[test]
    fn oracle_idle_and_grew_is_success() {
        assert!(oracle_success(AgentState::Idle, true));
    }

    #[test]
    fn oracle_idle_but_no_growth_fails() {
        // Text-only-but-empty / no output → NOT a success (the `has_productive_output`
        // false-negative class is avoided by requiring growth, but empty must fail).
        assert!(!oracle_success(AgentState::Idle, false));
    }

    #[test]
    fn oracle_error_class_fails_even_if_grew() {
        // UsageLimit / RateLimit / ApiError / ContextFull / AuthError / Crashed are all
        // is_error() → never a success, regardless of any output that scrolled by.
        for s in [
            AgentState::UsageLimit,
            AgentState::RateLimit,
            AgentState::ServerRateLimit,
            AgentState::ApiError,
            AgentState::ContextFull,
            AgentState::AuthError,
            AgentState::Crashed,
        ] {
            assert!(
                !oracle_success(s, true),
                "{s:?} is an error class → must fail"
            );
        }
    }

    #[test]
    fn oracle_nonidle_nonerror_fails() {
        // Stopped mid-work (e.g. wall-TTL while Thinking) → not a clean terminal Idle.
        assert!(!oracle_success(AgentState::Active, true));
    }

    // ─────────────── grew measure (nonblank delta) + chrome strip ───────────────

    #[test]
    fn nonblank_count_ignores_blank_lines() {
        assert_eq!(nonblank_count("a\n\nb\n   \nc"), 3);
        assert_eq!(nonblank_count(""), 0);
    }

    #[test]
    fn nonblank_count_stable_under_chrome_churn() {
        // The "grew" measure is a baseline↔final DELTA; the footer's row COUNT is
        // constant (only its content — elapsed timer / token count — churns), so a
        // chrome update never changes the count → never misread as growth.
        let before = "prompt echo\nModel: Opus | Ctx Used: 1.0%\n⏵⏵ bypass permissions on";
        let after = "prompt echo\nModel: Opus | Ctx Used: 9.0%\n⏵⏵ bypass permissions on";
        assert_eq!(nonblank_count(before), nonblank_count(after));
    }

    #[test]
    fn nonblank_delta_fallback_detects_growth() {
        // The FALLBACK signal (echo scrolled off): baseline vs final nonblank delta.
        let baseline = "❯ prompt\n─────\n❯\nModel: Opus | Ctx Used: 0%";
        let grown =
            "❯ prompt\n⏺ assistant reply\n⏺ more reply\n─────\n❯\nModel: Opus | Ctx Used: 3%";
        assert!(nonblank_count(grown) > nonblank_count(baseline));
    }

    // ───────────── grew = prompt-echo anchor (#2408 root fix) ─────────────

    /// THE root-fix test (replaces fixup-dev's `grew_false_negative_when_dense_baseline_
    /// exceeds_sparse_end`): a DENSE baseline (huge ready banner) + a SPARSE end (short
    /// answer) — the raw count-delta FALSE-NEGATIVES, but the prompt-echo anchor finds the
    /// answer after the echo → grew=TRUE. This is the scrolling-TUI failure the new signal
    /// fixes.
    #[test]
    fn grew_true_even_when_sparse_end_is_denser_baseline() {
        let prompt = "Summarize the file";
        // Dense baseline: a big ready banner (would be in scrollback for a scrolling TUI).
        let dense_baseline_nonblank = 50;
        // Sparse end: the prompt echo + a one-line answer + chrome. nonblank ≈ 3 < 50, so
        // the count-delta would judge grew=false (the reviewers' false-negative).
        let sparse_end =
            "❯ Summarize the file\n⏺ It is a config file.\n─────\n❯\n  Model: Opus | Ctx Used: 2%";
        assert!(
            nonblank_count(sparse_end) < dense_baseline_nonblank,
            "precondition: this IS the count-delta false-negative shape"
        );
        assert!(
            transcript_grew(sparse_end, prompt, dense_baseline_nonblank),
            "prompt-echo anchor must see the answer after the echo → grew=true"
        );
    }

    /// Echo present but NO content after it (worker produced nothing) → grew=false.
    #[test]
    fn grew_false_when_no_answer_after_echo() {
        let prompt = "do the thing";
        let end = "❯ do the thing\n─────\n❯\n  Model: Opus | Ctx Used: 0%";
        assert!(
            !transcript_grew(end, prompt, 0),
            "only the echo + chrome, no answer → grew=false"
        );
    }

    /// Echo NOT found (scrolled off a long turn) → fall back to the nonblank count-delta.
    #[test]
    fn grew_falls_back_to_count_delta_when_echo_absent() {
        let prompt = "a prompt that scrolled off the captured window";
        // No echo line present; end is denser than baseline → fallback says grew.
        let end = "⏺ line1\n⏺ line2\n⏺ line3\n─────\n❯";
        assert!(
            transcript_grew(end, prompt, 1),
            "fallback: end denser than baseline → grew"
        );
        assert!(
            !transcript_grew(end, prompt, 99),
            "fallback: end NOT denser than a huge baseline → not grew"
        );
    }

    /// The anchor matches a PREFIX (a wrapped/truncated echo still anchors) and uses the
    /// LAST occurrence (a prompt repeated in scrollback doesn't mis-anchor to the old one).
    #[test]
    fn locate_prompt_echo_prefix_and_last_match() {
        let prompt = "Reply with exactly the word: pong and nothing else at all";
        let lines = vec![
            "❯ Reply with exactly the word: pong and nothing el", // wrapped/truncated echo
            "⏺ pong",
        ];
        // Prefix (first ~40 chars) still matches the truncated echo line.
        assert_eq!(locate_prompt_echo(&lines, prompt), Some(0));
        // Last occurrence wins.
        let repeated = vec!["❯ do x", "⏺ ok", "❯ do x", "⏺ done"];
        assert_eq!(locate_prompt_echo(&repeated, "do x"), Some(2));
        // Empty prompt → no anchor.
        assert_eq!(locate_prompt_echo(&lines, ""), None);
    }

    /// Pattern-based chrome strip drops claude's footer (rules / empty `❯` box / Model
    /// statusline / bypass line) but keeps the answer + the `✻ Worked` completion line.
    #[test]
    fn strip_trailing_chrome_drops_claude_footer() {
        let dump = "the answer\n✻ Worked for 6s\n\n──────────\n❯\n──────────\n  Model: Opus 4.8 | Ctx Used: 3.0%\n  ⏵⏵ bypass permissions on (shift+tab to cycle)";
        let body = strip_trailing_chrome(dump);
        assert!(body.contains("the answer"), "body answer kept");
        assert!(body.contains("✻ Worked for 6s"), "completion marker kept");
        assert!(
            !body.contains("bypass permissions"),
            "claude footer dropped"
        );
        assert!(!body.contains("Model:"), "claude statusline dropped");
        assert!(!body.contains('❯'), "empty input box dropped");
    }

    /// A `❯ <prompt>` echo (content after the glyph) is BODY, not chrome — only a LONE
    /// `❯` (empty input box) is stripped.
    #[test]
    fn strip_trailing_chrome_keeps_prompt_echo() {
        let dump = "❯ do the thing\nresult here\n❯";
        let body = strip_trailing_chrome(dump);
        assert!(body.contains("❯ do the thing"), "the prompt echo is body");
        assert!(body.contains("result here"));
        assert!(
            body.ends_with("result here"),
            "the trailing empty ❯ box is stripped"
        );
    }

    #[test]
    fn strip_trailing_chrome_handles_all_chrome_and_empty() {
        // All-chrome / empty dumps return empty, no panic (the `len - take_while` math).
        assert_eq!(strip_trailing_chrome(""), "");
        assert_eq!(strip_trailing_chrome("──────\n❯\n\n"), "");
    }

    // ───────────────────── turn-end detector (debounce) ─────────────────────

    /// A mid-turn Idle BLIP shorter than the debounce window must NOT end the turn;
    /// only a continuously-held Idle ≥ debounce does. Deterministic synthetic timeline.
    #[test]
    fn debounce_ignores_midturn_idle_blip_then_ends_on_held_idle() {
        let mut d = TurnEndDetector::new(TURN_DEBOUNCE_MS);
        // Post-inject lull (Idle) — must not be mistaken for the end.
        assert_eq!(d.observe(AgentState::Idle, 0), TurnDecision::Continue);
        // Work starts.
        assert_eq!(d.observe(AgentState::Active, 250), TurnDecision::Continue);
        assert_eq!(d.observe(AgentState::Active, 500), TurnDecision::Continue);
        // A mid-turn Idle BLIP at 750ms…
        assert_eq!(d.observe(AgentState::Idle, 750), TurnDecision::Continue);
        // …only ~1s of "held" Idle, still under the 3s window — NOT the end…
        assert_eq!(d.observe(AgentState::Idle, 1_750), TurnDecision::Continue);
        // …then work resumes (the blip ends) — held-Idle clock resets.
        assert_eq!(d.observe(AgentState::Active, 2_000), TurnDecision::Continue);
        assert_eq!(d.observe(AgentState::Active, 2_500), TurnDecision::Continue);
        // Real turn end: Idle starts at 3_000 and holds.
        assert_eq!(d.observe(AgentState::Idle, 3_000), TurnDecision::Continue);
        // Held < 3s → still continue (would have wrongly fired if the blip leaked).
        assert_eq!(d.observe(AgentState::Idle, 5_000), TurnDecision::Continue);
        // Held ≥ 3s from idle_since=3_000 → turn ended.
        assert_eq!(d.observe(AgentState::Idle, 6_000), TurnDecision::TurnEnded);
    }

    /// The detector must not fire on the pre-turn Idle if work never started (a fast
    /// model that finished between polls is left to the wall-TTL + the end-oracle).
    #[test]
    fn debounce_does_not_end_while_only_idle_seen() {
        let mut d = TurnEndDetector::new(TURN_DEBOUNCE_MS);
        for t in [0, 1_000, 5_000, 60_000] {
            assert_eq!(
                d.observe(AgentState::Idle, t),
                TurnDecision::Continue,
                "must never end the turn while only Idle has been seen (no work started)"
            );
        }
    }

    /// An error class at any phase short-circuits to ErrorClass.
    #[test]
    fn debounce_short_circuits_on_error_class() {
        let mut d = TurnEndDetector::new(TURN_DEBOUNCE_MS);
        assert_eq!(d.observe(AgentState::Active, 100), TurnDecision::Continue);
        assert_eq!(
            d.observe(AgentState::UsageLimit, 200),
            TurnDecision::ErrorClass
        );
    }

    /// Clean happy path: work → held Idle ≥ debounce → TurnEnded (no blip).
    #[test]
    fn debounce_clean_turn_ends_after_held_idle() {
        let mut d = TurnEndDetector::new(TURN_DEBOUNCE_MS);
        assert_eq!(d.observe(AgentState::Active, 0), TurnDecision::Continue);
        assert_eq!(d.observe(AgentState::Idle, 1_000), TurnDecision::Continue);
        assert_eq!(d.observe(AgentState::Idle, 3_999), TurnDecision::Continue);
        assert_eq!(d.observe(AgentState::Idle, 4_000), TurnDecision::TurnEnded);
    }

    // ─────────────────────────────── summary ───────────────────────────────

    #[test]
    fn summary_drops_chrome_and_prefixes_verdict() {
        let dump =
            "the answer\n─────\n❯\n  Model: Opus | Ctx Used: 3.0%\n  ⏵⏵ bypass permissions on";
        let s = build_summary(dump, AgentState::Idle, true, "turn ended (Idle held)");
        assert!(s.starts_with("[Idle grew=true stop=turn ended (Idle held)]"));
        assert!(s.contains("the answer"));
        assert!(
            !s.contains("bypass permissions") && !s.contains("Model:"),
            "the chrome footer must be dropped from the summary"
        );
    }

    #[test]
    fn summary_caps_runaway_transcript_on_char_boundary() {
        // A many-line body (survives the chrome-tail drop) with MULTIBYTE chars, so the
        // MAX_SUMMARY_BYTES cut can land mid-char — the char-boundary backoff must not
        // panic and must still append the marker.
        let big = "café日本語テスト line\n".repeat(1_000);
        let s = build_summary(&big, AgentState::Idle, true, "r");
        assert!(
            s.len() <= MAX_SUMMARY_BYTES + "…[truncated]".len() + 64,
            "summary must be capped near MAX_SUMMARY_BYTES, got {} bytes",
            s.len()
        );
        assert!(
            s.ends_with("…[truncated]"),
            "a capped summary must carry the marker"
        );
    }
}
