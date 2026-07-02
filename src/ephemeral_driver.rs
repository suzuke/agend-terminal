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
//! - **Poll `get_state()` ONLY, NEVER the general `state.tick()`.** The ephemeral
//!   read loop ([`crate::agent`] `ephemeral_pty_read_loop`) never ticks, so calling
//!   the full `tick()` here would blanket-enable the 30 s `LATCHED_STATE_EXPIRY`
//!   decay for every backend → a false turn-end on a quiet >30 s stretch. #2524 P3a
//!   PR-2 (decision `d-20260702075424735619-11`) adds a NARROWER, throttled
//!   exception — [`StateTracker::expire_stale_latch_if_due`] — called in Phase 0 for
//!   every backend (nothing is in-progress yet, so an early unblock costs nothing)
//!   and in Phase 2 for codex ONLY (which alone has the `observed_status` veto below
//!   as a safety net against that same false-turn-end risk — decision
//!   `d-20260702081055743388-12`). See the Phase 0/Phase 2 comments in `run_turn` for
//!   the full per-phase rationale — do NOT collapse this asymmetry into one
//!   unconditional call without re-deriving that veto for every other backend first.
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
//! Scope: opencode (Slice-1) + claude (Slice-2) + codex (#2524 P3a PR-2) — each
//! §5-smoke-validated. The gate ([`crate::ephemeral_tracking`] `driver_supported`)
//! admits exactly this set; other backends are later slices. opencode/claude are
//! proven to render a continuous work marker so the Idle-debounce isn't fooled by a
//! mid-turn idle — their turn-end reads raw `get_state()` UNCHANGED. codex is
//! different: its raw screen goes idle for several seconds mid-turn
//! (SHADOW-OBSERVER-QUANT-CODEX-2413.md — longer than [`TURN_DEBOUNCE_MS`]), so its
//! turn-end detection additionally consults `core.observed_status` (the Shadow
//! Observer's Stream-authority-corrected status, populated by the codex rollout tail —
//! #2524 P3a PR-1) and VETOES a raw-Idle sample when Stream evidence says the turn is
//! still active ([`effective_turn_state`]). This is a per-backend veto, not a plane
//! swap: opencode/claude never consult `observed_status` (zero behavior change), and
//! codex still falls back to raw `get_state()` once Stream evidence goes stale or
//! agrees with Idle. Durable telemetry (fleet_events L1/L2/L3 + task_events) is PR4;
//! the result lands on the worker row for now.

use crate::agent::InjectTarget;
use crate::backend::Backend;
// RED (#2524 P3a PR-2): `effective_turn_state` doesn't consult these yet — the
// GREEN commit's real veto body uses both. Only referenced by tests until then.
#[allow(unused_imports)]
use crate::daemon::shadow::evidence::Authority;
#[allow(unused_imports)]
use crate::daemon::shadow::reducer::{ObservedState, ObservedStatus};
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
    /// #2524 P3a PR-2: which backend this worker runs, so turn-end detection can branch
    /// (codex consults `observed_status`; every other backend is unaffected — see
    /// [`effective_turn_state`]).
    pub backend: Backend,
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
        backend,
    } = cfg;
    let start = Instant::now();
    let wall_ttl_ms = wall_ttl.as_millis() as u64;

    // Phase 0 — wait for the worker to reach its first Idle (ready). Poll get_state()
    // ONLY (never the general tick()) — but DO run the throttled latch-expiry check,
    // for ALL backends here (#2524 P3a PR-2, decision `d-20260702075424735619-11`):
    // without it, a screen-classifier state that mis-latches Active on the ready
    // screen and then goes fully static (no further pty bytes ⇒ no `detect()`
    // re-evaluation either) can NEVER self-heal, and Phase 0 would wait out the full
    // `READY_TIMEOUT` on an already-ready worker. Safe for EVERY backend here
    // specifically because nothing is "in progress" yet to falsely cut short — the
    // worst case is Phase 0 unblocking slightly early, never a truncated turn.
    let ready_cap_ms = (READY_TIMEOUT.as_millis() as u64).min(wall_ttl_ms);
    loop {
        let now_ms = start.elapsed().as_millis() as u64;
        let state = {
            let mut c = inject_target.core.lock();
            c.state.expire_stale_latch_if_due(now_ms);
            c.state.get_state()
        };
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
    // the wall-TTL. Poll get_state() ONLY (never the general tick()).
    //
    // #2524 P3a PR-2 (decision d-20260702081055743388-12): the Phase 0 latch-expiry
    // check does NOT extend here for opencode/claude — DELIBERATELY, not an oversight.
    // `expire_stale_latch_if_due` transitions on ELAPSED TIME alone, without
    // re-evaluating the current screen (`record_set`'s same-state early-return never
    // refreshes `since` on a repeat Active match, however often the screen re-renders
    // — see `StateTracker::record_set`). Calling it here for a backend with no
    // turn-end-authority fallback would re-derive EXACTLY the risk this module's own
    // top doc already flags ("Calling `tick()` here would re-enable that decay → a
    // false turn-end on a quiet >30s stretch"): a genuinely-still-running turn past
    // 30s continuously classified Active could get force-flipped to Idle and read as
    // a real TurnEnded 3s later. opencode/claude are §5-smoke-validated on the
    // OPPOSITE premise — a real turn-end always produces a final render burst (new
    // pty bytes ⇒ `detect()` naturally re-evaluates and transitions), so they have
    // no latent ready-style mis-latch to fix here, and adding the check would be a
    // new, un-smoked risk for no benefit. codex is different ONLY because it has the
    // `observed_status` veto below as a safety net: even if `expire_stale_latch_if_due`
    // wrongly forces Idle on a still-active codex turn, fresh Stream-authority
    // evidence overrides it back to `Active` before `TurnEndDetector` ever sees it
    // (see `codex_expiry_forced_idle_is_vetoed_back_to_active` below) — a protection
    // opencode/claude do not have. Do NOT "simplify" this to an unconditional call for
    // all backends without re-adding that veto first.
    let mut detector = TurnEndDetector::new(TURN_DEBOUNCE_MS);
    let stop_reason = loop {
        let now_ms = start.elapsed().as_millis() as u64;
        if now_ms >= wall_ttl_ms {
            break "wall-TTL elapsed before turn end";
        }
        let (raw, observed) = {
            let mut c = inject_target.core.lock();
            if backend == Backend::Codex {
                c.state.expire_stale_latch_if_due(now_ms);
            }
            (c.state.get_state(), c.observed_status.clone())
        };
        let state = effective_turn_state(&backend, raw, observed.as_ref());
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

/// #2524 P3a PR-2: the state actually fed to [`TurnEndDetector`] for this poll sample.
/// PURE (no lock, no I/O) — takes the raw screen state and the worker's current
/// `observed_status` already read under one `core.lock()` by the caller — so it's
/// unit-testable with synthetic inputs (see the `codex_survives_quant_2413_*` test,
/// fed the REAL SHADOW-OBSERVER-QUANT-CODEX-2413.md trace).
///
/// opencode/claude: always the raw state, unchanged — zero behavior change, per the
/// §5-smoke precedent (this function is a no-op for them).
///
/// codex: a raw `Idle` sample is VETOED (treated as still `Active`) when
/// `observed_status` carries FRESH `Stream`-authority evidence that says otherwise —
/// SHADOW-OBSERVER-QUANT-CODEX-2413.md shows codex's raw screen false-idling for
/// several seconds mid-turn (longer than [`TURN_DEBOUNCE_MS`]) while the rollout-tail
/// Stream evidence correctly holds `Responding`. Once Stream evidence goes stale or
/// itself agrees with Idle (the reducer falls back to `Screen` authority — the real
/// turn has ended), the raw state passes through unchanged, so the debounce still
/// applies normally at the REAL end. A non-`Idle` raw sample is never touched — the
/// veto only ever suppresses a false idle, never invents one.
///
/// RED (#2524 P3a PR-2): not yet vetoing anything — always the raw state, for every
/// backend. The GREEN commit adds the codex Stream-authority veto.
fn effective_turn_state(
    _backend: &Backend,
    raw: AgentState,
    _observed: Option<&ObservedStatus>,
) -> AgentState {
    raw
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

    /// #2524 P3a PR-2: codex's bottom statusline, verbatim off a real isolated smoke
    /// (`gpt-5.5 medium · /private/var/…/backend-data/ephemeral/eph-53468-0`) —
    /// dropped structurally (path after " · "), not by the model name.
    #[test]
    fn strip_trailing_chrome_drops_codex_footer() {
        let dump = "• pong\n\n  gpt-5.5 medium · /private/var/folders/wn/T/agend-ephemeral-53468-codex-e2e-0/backend-data/ephemeral/eph-53468-0";
        let body = strip_trailing_chrome(dump);
        assert!(body.contains("pong"), "body answer kept");
        assert!(!body.contains("gpt-5.5 medium"), "codex statusline dropped");
    }

    /// A body line that happens to contain " · " but NOT followed by an absolute
    /// path must NOT be mistaken for the codex statusline.
    #[test]
    fn is_trailing_chrome_requires_a_path_after_the_middle_dot() {
        assert!(!is_trailing_chrome("today's agenda · groceries, laundry"));
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

    // ──────────────────── effective_turn_state (#2524 P3a PR-2) ────────────────────

    fn observed(state: ObservedState, authority: Authority) -> ObservedStatus {
        ObservedStatus {
            state,
            confidence: crate::daemon::shadow::evidence::Confidence::Strong,
            authority,
            evidence: Vec::new(),
            since_ms: 0,
        }
    }

    /// opencode/claude MUST be a no-op regardless of raw state or observed_status —
    /// the veto is codex-only (zero behavior change for the §5-smoke-validated
    /// backends, per the module doc).
    #[test]
    fn effective_turn_state_is_noop_for_opencode_and_claude() {
        for backend in [Backend::OpenCode, Backend::ClaudeCode] {
            for raw in [AgentState::Idle, AgentState::Active] {
                assert_eq!(
                    effective_turn_state(&backend, raw, None),
                    raw,
                    "{backend:?}/{raw:?} with no observed_status must pass through"
                );
                assert_eq!(
                    effective_turn_state(
                        &backend,
                        raw,
                        Some(&observed(ObservedState::Responding, Authority::Stream))
                    ),
                    raw,
                    "{backend:?}/{raw:?} must ignore observed_status entirely"
                );
            }
        }
    }

    /// codex: a non-Idle raw sample is never touched — the veto only ever suppresses
    /// a false Idle, it never invents activity out of an already-active sample.
    #[test]
    fn effective_turn_state_codex_leaves_non_idle_raw_alone() {
        assert_eq!(
            effective_turn_state(&Backend::Codex, AgentState::Active, None),
            AgentState::Active
        );
        assert_eq!(
            effective_turn_state(
                &Backend::Codex,
                AgentState::Active,
                Some(&observed(ObservedState::Idle, Authority::Screen))
            ),
            AgentState::Active
        );
    }

    /// codex: raw Idle + no observed_status yet (e.g. the shadow-observer tick hasn't
    /// run) → passes through unchanged. Never invents a veto from nothing.
    #[test]
    fn effective_turn_state_codex_no_observed_status_passes_through() {
        assert_eq!(
            effective_turn_state(&Backend::Codex, AgentState::Idle, None),
            AgentState::Idle
        );
    }

    /// codex: raw Idle + observed_status ALSO Idle (any authority) → the two planes
    /// agree, no veto — the debounce proceeds normally at a REAL idle.
    #[test]
    fn effective_turn_state_codex_agreeing_idle_passes_through() {
        assert_eq!(
            effective_turn_state(
                &Backend::Codex,
                AgentState::Idle,
                Some(&observed(ObservedState::Idle, Authority::Stream))
            ),
            AgentState::Idle
        );
        assert_eq!(
            effective_turn_state(
                &Backend::Codex,
                AgentState::Idle,
                Some(&observed(ObservedState::Idle, Authority::Screen))
            ),
            AgentState::Idle
        );
    }

    /// codex: raw Idle + observed_status says active but via `Screen` authority (not
    /// `Stream`) → NOT vetoed. `Screen` authority is the same plane as the raw sample
    /// itself (no independent correction), so there is nothing to veto WITH.
    #[test]
    fn effective_turn_state_codex_screen_authority_does_not_veto() {
        assert_eq!(
            effective_turn_state(
                &Backend::Codex,
                AgentState::Idle,
                Some(&observed(ObservedState::Responding, Authority::Screen))
            ),
            AgentState::Idle
        );
    }

    /// codex: raw Idle + FRESH Stream-authority evidence saying the turn is still
    /// active (Responding/Thinking/ToolUse/Active — anything that doesn't coarsen to
    /// Idle) → VETOED to `Active`. This is the exact false-idle shape
    /// SHADOW-OBSERVER-QUANT-CODEX-2413.md documents on a real turn.
    #[test]
    fn effective_turn_state_codex_stream_active_vetoes_false_idle() {
        for state in [
            ObservedState::Responding,
            ObservedState::Thinking,
            ObservedState::ToolUse,
            ObservedState::Active,
        ] {
            assert_eq!(
                effective_turn_state(
                    &Backend::Codex,
                    AgentState::Idle,
                    Some(&observed(state, Authority::Stream))
                ),
                AgentState::Active,
                "{state:?}/Stream must veto a raw-Idle sample"
            );
        }
    }

    /// #2524 P3a PR-2 — THE fixture-replay pin: the REAL raw/observed/authority trace
    /// from SHADOW-OBSERVER-QUANT-CODEX-2413.md (§"Result" table, verbatim timestamps
    /// converted to relative ms from the first sample), fed through
    /// `effective_turn_state` into a REAL `TurnEndDetector`. Proves the false-idle
    /// window (11:38:58–11:39:04, raw=Idle/observed=Responding/Stream — ~6 s, LONGER
    /// than the 3 s debounce) does not end the turn, and the REAL end (11:39:05,
    /// raw=Idle/observed=Idle/Screen — Stream evidence went stale, reducer fell back
    /// to Screen) does, once held for the debounce. No real codex binary, no sleep —
    /// pure data replay.
    #[test]
    fn codex_survives_quant_2413_false_idle_trace() {
        let trace: &[(u64, AgentState, ObservedState, Authority)] = &[
            // 11:38:41–56: continuous work, Stream authority throughout (11 samples in
            // the doc; 4 representative ones suffice to establish Working).
            (
                0,
                AgentState::Active,
                ObservedState::Responding,
                Authority::Stream,
            ),
            (
                5_000,
                AgentState::Active,
                ObservedState::Responding,
                Authority::Stream,
            ),
            (
                10_000,
                AgentState::Active,
                ObservedState::Responding,
                Authority::Stream,
            ),
            (
                15_000,
                AgentState::Active,
                ObservedState::Responding,
                Authority::Stream,
            ),
            // 11:38:58–11:39:04: raw screen false-idles for ~6s; Stream evidence still
            // says Responding — this is the window a naive raw-state feed would
            // wrongly end the turn in (3s debounce < 6s window).
            (
                17_000,
                AgentState::Idle,
                ObservedState::Responding,
                Authority::Stream,
            ),
            (
                18_000,
                AgentState::Idle,
                ObservedState::Responding,
                Authority::Stream,
            ),
            (
                20_000,
                AgentState::Idle,
                ObservedState::Responding,
                Authority::Stream,
            ),
            (
                21_000,
                AgentState::Idle,
                ObservedState::Responding,
                Authority::Stream,
            ),
            (
                23_000,
                AgentState::Idle,
                ObservedState::Responding,
                Authority::Stream,
            ),
            // 11:39:05: the turn REALLY ends — no fresh Stream evidence, reducer falls
            // back to Screen authority, agreeing with raw Idle.
            (
                24_000,
                AgentState::Idle,
                ObservedState::Idle,
                Authority::Screen,
            ),
            // Hold the real Idle for the debounce window so TurnEnded actually fires.
            (
                27_000,
                AgentState::Idle,
                ObservedState::Idle,
                Authority::Screen,
            ),
        ];

        let mut detector = TurnEndDetector::new(TURN_DEBOUNCE_MS);
        let mut turn_ended_at: Option<u64> = None;
        for &(now_ms, raw, obs_state, authority) in trace {
            let observed_status = observed(obs_state, authority);
            let effective = effective_turn_state(&Backend::Codex, raw, Some(&observed_status));
            if detector.observe(effective, now_ms) == TurnDecision::TurnEnded {
                turn_ended_at.get_or_insert(now_ms);
            }
        }
        assert_eq!(
            turn_ended_at,
            Some(27_000),
            "must not false-fire during the 17s-23s false-idle window; must fire only \
             once the REAL end (24s) has held the debounce"
        );
    }

    /// #2524 P3a PR-2 (decision `d-20260702081055743388-12`) — THE decisive
    /// interaction pin between the two codex-only Phase 2 safety nets:
    /// `expire_stale_latch_if_due` transitions on ELAPSED TIME alone (never
    /// re-reading the actual screen), so it CAN wrongly force a genuinely-still-
    /// running codex turn's raw state to `Idle` after 30s of no state CHANGE (even
    /// if the screen keeps re-rendering the SAME classification — `record_set`'s
    /// same-state early-return never refreshes `since`). This is exactly the risk
    /// the decision flagged. It's safe ONLY because `effective_turn_state`'s
    /// `observed_status` veto catches it: fresh Stream-authority evidence saying the
    /// turn is still `Responding` (the QUANT-CODEX-2413.md shape) overrides the
    /// wrongly-forced `Idle` back to `Active` BEFORE `TurnEndDetector` ever sees it —
    /// so the two mechanisms together never produce a false `TurnEnded`. This is
    /// exactly why Phase 2's `expire_stale_latch_if_due` call is codex-ONLY (see
    /// `run_turn`'s Phase 2 comment): opencode/claude have no such veto, so the same
    /// risk would NOT be caught for them.
    #[test]
    fn codex_expiry_forced_idle_is_vetoed_back_to_active() {
        let mut tracker = crate::state::StateTracker::new(Some(&Backend::Codex));
        // A genuinely-still-running codex turn, classified `Active` for >30s straight
        // (a slow model — real, not stale).
        tracker.current = AgentState::Active;
        tracker.since = std::time::Instant::now() - std::time::Duration::from_secs(31);

        // The throttle is due → `expire_stale_latch_if_due` blindly force-transitions
        // to `Idle` without re-checking the screen. This alone WOULD be wrong.
        tracker.expire_stale_latch_if_due(1_000);
        let raw = tracker.get_state();
        assert_eq!(
            raw,
            AgentState::Idle,
            "expiry force-transitions on elapsed time alone, with no screen re-check"
        );

        // Fresh Stream-authority evidence says the turn is genuinely still active —
        // the veto must override the wrongly-forced Idle before TurnEndDetector sees it.
        let observed_status = observed(ObservedState::Responding, Authority::Stream);
        let effective = effective_turn_state(&Backend::Codex, raw, Some(&observed_status));
        assert_eq!(
            effective,
            AgentState::Active,
            "the observed_status veto must override the wrongly-forced Idle"
        );

        // Feed the ALREADY-Working detector the effective (vetoed) state — must NOT
        // end the turn.
        let mut detector = TurnEndDetector::new(TURN_DEBOUNCE_MS);
        assert_eq!(
            detector.observe(AgentState::Active, 0),
            TurnDecision::Continue
        ); // enter Working
        assert_eq!(
            detector.observe(effective, 1_000),
            TurnDecision::Continue,
            "the vetoed-back-to-Active sample must not end the turn"
        );
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
