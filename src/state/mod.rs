//! Agent state detection via PTY output pattern matching.
//!
//! Detection runs against the current **vterm screen text** (caller supplies
//! it via `feed()`), not an accumulated byte buffer. Pattern hits therefore
//! reflect what the user would currently see on screen, so dismissing an
//! interactive prompt (e.g. codex update menu) drops the matching text from
//! the grid and the next `feed()` re-evaluates to the underlying Idle state
//! without stale-buffer lag.
//!
//! Hysteresis: error states instant, active 2s, passive 5s.
//!
//! Hash-based dedup in `feed()`: if the screen text is identical to the
//! previous snapshot, we skip both the silence-timer bump and pattern
//! detection. This keeps invisible terminal chatter (cursor blinks, etc.)
//! from resetting timers used by hang/awaiting detection.

use crate::backend::Backend;
use serde::Serialize;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

/// Agent runtime state, ordered by priority (highest last).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum AgentState {
    Starting,
    Hang,
    /// Startup stalled on unexpected interactive prompt (e.g. codex update menu).
    /// Entered when Starting + stdout silent > threshold. Operator reply is
    /// routed as raw PTY keystrokes (not inbox-wrapped) via INJECT_RAW.
    AwaitingOperator,
    Idle,
    ToolUse,
    Thinking,
    /// Startup stalled on a backend-specific modal that blocks normal use
    /// (e.g. codex `Update available!` menu). Distinct from
    /// `PermissionPrompt` so operators can tell "CLI waiting for an OK on
    /// an update menu" from "CLI asking whether to Allow a tool invocation".
    /// Higher than `Thinking` because real work cannot progress until the
    /// modal is dismissed; lower than `PermissionPrompt` because formal
    /// authorization flows take precedence when both match.
    InteractivePrompt,
    PermissionPrompt,
    /// Phase A Piece-1: git rebase/merge/cherry-pick produced a
    /// conflict that blocks further work until the agent resolves it
    /// (or the operator intervenes). Distinct from `PermissionPrompt`
    /// because the resolution path is Read/Edit/Bash inside the
    /// worktree, not a yes/no dialog. The daemon's
    /// `conflict_notify` module observes the transition into this
    /// state and emits a structured kind=update message to the bound
    /// agent (op type + conflicted file paths + branch + base +
    /// next_steps hint), plus a 30min escalation to operator if the
    /// state persists.
    GitConflict,
    ContextFull,
    RateLimit,
    /// Anthropic server-side temporary throttle — distinct from user usage
    /// limit. Auto-retry with exponential backoff is safe.
    ServerRateLimit,
    UsageLimit,
    AuthError,
    ApiError,
    /// #1634: the backend's configured model is rejected by the provider
    /// (discontinued / unsupported / typo'd model id). Distinct from `AuthError`
    /// (credentials are fine) and `ApiError` (transient) because it is a
    /// PERMANENT config fault: the agent errors every turn until the operator
    /// changes the model. HIGH_FP (see `is_high_fp_state`) — requires the #919
    /// red anchor so an agent merely discussing the error wording can't latch it.
    ModelUnsupported,
    Crashed,
    Restarting,
}

impl AgentState {
    /// GO-NARROW 6 states that trigger orchestrator notify on transition.
    /// Per Sprint 43 §13 #1 operator decision.
    pub fn is_notify_error_class(self) -> bool {
        matches!(
            self,
            Self::UsageLimit
                | Self::RateLimit
                | Self::Hang
                | Self::Crashed
                | Self::AuthError
                | Self::PermissionPrompt
        )
    }

    /// Priority: higher = more urgent. Error states > prompts > active > passive.
    pub fn priority(self) -> u8 {
        match self {
            Self::Starting => 0,
            Self::Hang => 1,
            Self::AwaitingOperator => 2,
            // (priority 3 was `Ready`, collapsed into `Idle` — gap is harmless;
            // priorities are an ordering, not a contiguous index.)
            Self::Idle => 4,
            Self::ToolUse => 5,
            Self::Thinking => 6,
            Self::InteractivePrompt => 7,
            Self::PermissionPrompt => 8,
            // Phase A Piece-1: GitConflict shares priority 8 with
            // PermissionPrompt — both block work and require external
            // intervention (agent action for the conflict; operator
            // for the permission). Priority is for display ordering;
            // first-match wins in `detect()` is the actual gate.
            Self::GitConflict => 8,
            Self::ContextFull => 9,
            Self::RateLimit => 10,
            Self::ServerRateLimit => 10,
            Self::UsageLimit => 11,
            Self::AuthError => 12,
            Self::ApiError => 13,
            // #1634: error class (instant transition, no hysteresis); shares the
            // ApiError tier — display ordering only, first-match in `detect()`
            // is the real gate.
            Self::ModelUnsupported => 13,
            Self::Crashed => 14,
            Self::Restarting => 15,
        }
    }

    /// Is this an error state (instant transition, no hysteresis)?
    pub fn is_error(self) -> bool {
        self.priority() >= Self::ContextFull.priority()
    }

    /// Is this agent unavailable (restarting or crashed)?
    pub fn is_unavailable(self) -> bool {
        matches!(self, Self::Crashed | Self::Restarting)
    }

    /// States where operator reply text should bypass the inbox and reach the
    /// PTY as raw keystrokes — i.e. the agent is showing an interactive modal
    /// (startup stall or pattern-matched InteractivePrompt), not a
    /// free-form conversation prompt.
    pub fn wants_raw_keystrokes(self) -> bool {
        matches!(self, Self::AwaitingOperator | Self::InteractivePrompt)
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Hang => "hang",
            Self::AwaitingOperator => "awaiting_operator",
            Self::Idle => "idle",
            Self::ToolUse => "tool_use",
            Self::Thinking => "thinking",
            Self::InteractivePrompt => "interactive_prompt",
            Self::PermissionPrompt => "permission",
            Self::GitConflict => "git_conflict",
            Self::ContextFull => "context_full",
            Self::RateLimit => "rate_limit",
            Self::ServerRateLimit => "server_rate_limit",
            Self::UsageLimit => "usage_limit",
            Self::ModelUnsupported => "model_unsupported",
            Self::AuthError => "auth_error",
            Self::ApiError => "api_error",
            Self::Crashed => "crashed",
            Self::Restarting => "restarting",
        }
    }
}

pub(crate) mod patterns;

use crate::vterm::CellFg;
use patterns::is_generic_startup_prompt;
pub use patterns::{classify_pty_output, StatePatterns};

/// #2033: gate inputs for the "recovered from blocked state" Telegram notice,
/// captured when a blocked episode (InteractivePrompt / AwaitingOperator) ends.
/// The supervisor emits the notice only when it is actionable — the operator was
/// actually told about the block AND it lasted long enough that they might be
/// reacting (#2008 actionable-or-silent). A self-resolving / never-notified block
/// produces a `notice_sent=false` (or sub-threshold) episode → log-only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoveryEpisode {
    /// How long the episode stayed in a blocked state.
    pub block_duration: Duration,
    /// Whether a Telegram blocked (`Stall`) notice was forwarded for this episode.
    pub notice_sent: bool,
}

pub struct StateTracker {
    pub current: AgentState,
    pub(crate) since: Instant,
    pub last_output: Instant,
    /// F9 (#685 sub-task 4): `Some(t)` only when `infer_productivity()` returned
    /// a `Productive` signal (heartbeat refresh or structural marker match) at
    /// `t`. Bare screen change does NOT set this — unlike `last_output`. Read by
    /// the daemon supervisor and passed to `check_hang` as `silent_productive`
    /// for the dual-path Hung detection. See `docs/F9-PRODUCTIVE-OUTPUT-GATE.md`
    /// §F9.1 architecture and §F9.3 dual-path decision table.
    ///
    /// `None` until the FIRST productive signal — distinguishing "never produced"
    /// (a fresh tracker / just-spawned agent) from "recently produced". The
    /// recovery gates (`recovered_within`: #1795 retry, #badge re-latch) treat
    /// `None` as NOT recovered, so a creation stamp can no longer be misread as
    /// recovery. Silence readers (`productive_silence`) fall back to `created_at`,
    /// preserving the pre-Option behavior.
    pub last_productive_output: Option<Instant>,
    /// Silence baseline for an agent that has not yet produced
    /// (`last_productive_output == None`): the tracker's creation instant.
    pub(crate) created_at: Instant,
    /// #685 PR-2: hash of the matched-marker substring on the most-recent
    /// productive refresh. Used to suppress re-firing
    /// `last_productive_output = now()` when the same marker text remains
    /// visible across screen-change ticks (e.g. stale "Saved to /tmp/foo.txt"
    /// stays in viewport while a spinner cycles below). Same defense-in-
    /// depth class as #1005 ToolUse oscillation guard. Cleared on
    /// non-matching feed so a genuine future Productive signal re-fires.
    last_productive_marker_hash: Option<u64>,
    /// #1450: hash of the last emitted anchor-suppress WARN's
    /// (state, matched, line_context). The HIGH_FP red-anchor gate is
    /// level-triggered — it re-evaluates on every `feed()`, so a backend that
    /// statically displays a phrase matching a HIGH_FP pattern but never renders
    /// red (e.g. an opencode pane showing the source identifier
    /// `"ContextOverflow")`) re-logged the suppression on every tick, flooding
    /// the daemon log (14k+ identical lines/incident). Dedup on this hash so the
    /// WARN fires once per distinct suppression, not once per render.
    last_anchor_suppress_hash: Option<u64>,
    /// Hash of the last screen text fed to `feed()`. `None` before the first
    /// call. Used to skip re-detection when the screen hasn't changed —
    /// crucial for not resetting `last_output` on cursor-blink noise.
    last_screen_hash: Option<u64>,
    patterns: Option<&'static StatePatterns>,
    /// Context% telemetry: compiled `BackendProfile::context_pattern`
    /// (capture group 1 = the backend's self-reported context usage percent).
    /// `None` for backends that display no usable percent.
    context_regex: Option<regex::Regex>,
    /// Latest pattern-extracted context% reading + its capture instant. Kept
    /// (not cleared) when the pattern stops matching — a narrow pane can
    /// truncate the statusline mid-session; the timestamp lets consumers
    /// judge staleness instead of treating "can't read" as "safe".
    context_pct: Option<(f32, Instant)>,
    /// Set to true the moment we enter `InteractivePrompt`; cleared by
    /// `take_interactive_prompt_notice()` once the supervisor has forwarded a
    /// Telegram notice. This deduplicates per-entry: re-entry (e.g. dismissed
    /// then triggered again) re-arms it, but repeated supervisor ticks while
    /// still in the same InteractivePrompt won't re-spam.
    interactive_prompt_pending_notice: bool,
    /// Set to true the moment we leave a blocked state (InteractivePrompt /
    /// AwaitingOperator) to a non-blocked state; cleared by
    /// `take_recovery_notice()` once the supervisor has forwarded a Telegram
    /// "ready again" notice. Pairs with `interactive_prompt_pending_notice`
    /// so operators get symmetrical enter/exit signals.
    interactive_recovery_pending_notice: bool,
    /// #2033: when the CURRENT blocked episode began (first entry into a
    /// `wants_raw_keystrokes` state from a non-blocked one). `None` outside a
    /// blocked episode. Spans intra-block transitions (InteractivePrompt →
    /// AwaitingOperator), so it measures the FULL operator-facing block duration.
    blocked_since: Option<Instant>,
    /// #2033: whether a Telegram blocked notice (the `Stall` action) was actually
    /// emitted for the current blocked episode. The recovery-notice gate keys on
    /// this — a block the operator was never told about must not produce a
    /// non-actionable "recovered" notice (#2008 actionable-or-silent). Reset on
    /// each fresh blocked-episode entry; the supervisor sets it via
    /// [`Self::mark_blocked_notice_sent`] when it forwards the Stall.
    blocked_notice_sent: bool,
    /// #2033: the just-ended blocked episode's gate inputs, captured on the
    /// leaving-block transition and consumed by `take_recovery_notice()`. Carries
    /// the block duration + whether it was notified, so the supervisor can decide
    /// actionable-or-silent without reaching back into per-episode state that the
    /// recovery tick (a later tick) would already have cleared.
    recovery_episode: Option<RecoveryEpisode>,
    /// Last MCP heartbeat instant. Updated by supervisor tick from metadata.
    /// `None` before first heartbeat. Used by `gate_on_heartbeat` to suppress
    /// false-positive `PermissionPrompt` when the agent is alive (A5 fix).
    last_heartbeat: Option<Instant>,
    /// Sprint 27: behavioral probe config for shadow-mode telemetry.
    behavioral_config: Option<crate::behavioral::BehavioralConfig>,
    /// F9 (#685 sub-task 4): productive-output config for the dual-path
    /// supplement to silence-based Hung detection. Per-backend markers +
    /// heartbeat-as-productive toggle. See
    /// `docs/F9-PRODUCTIVE-OUTPUT-GATE.md` §F9.2 productive-signal design.
    productivity_config: Option<crate::behavioral::ProductivityConfig>,
    /// Instance name for telemetry logging.
    instance_name: String,
    /// Backend name for telemetry logging.
    backend_name: String,
    /// #919/#1450: backend's opt-in to the HIGH_FP color anchor. Cached at
    /// construction from `Backend::should_anchor_on_red()`. When false
    /// (Shell/Raw — no uniform color convention), the anchor gate fails
    /// open (pre-#919 behavior: a HIGH_FP pattern match fires the
    /// transition unconditionally).
    anchor_on_red: bool,
    /// #1947: the backend's input-line prompt markers, cached from
    /// `BackendProfile::input_line_markers` at construction (legacy/profile-None
    /// backends → empty = exclusion unavailable). The content anchor uses these
    /// to reject error patterns matched on operator-typed / quoted input lines.
    input_line_markers: &'static [&'static str],
    /// #1005 Phase A2: most-recent priority-up transition target +
    /// timestamp. Set on every successful priority-up in `transition()`.
    /// Cleared (set to None) on explicit Idle / lower-priority drops
    /// that complete the natural state cycle.
    ///
    /// The oscillation guard reads this to detect the
    /// `Active(X) → Lower(Y) [<5s] → Active(X)` bounce pattern that
    /// makes `LATCHED_STATE_EXPIRY` (30s) unreachable. When the same
    /// active state is re-entered within `oscillation_guard_window()`
    /// (default 30s, env-tunable) AND the lower state was held for
    /// less than `OSCILLATION_LOWER_HOLD_THRESHOLD` (5s), the
    /// transition is suppressed and the tracker stays in the lower
    /// state — letting the natural latched-expiry path eventually fire.
    last_priority_up_into: Option<(AgentState, Instant)>,
    /// #1527: transitions recorded at the moment `current` actually changes
    /// (via `record_set`), drained + logged by the supervisor. This replaces
    /// the supervisor's prev/new-at-tick comparison, which silently missed
    /// every transition that completed async in the read-loop thread (the
    /// feed → `transition` path) between two supervisor ticks — i.e. nearly
    /// all of them, including the error states. Bounded (drop-oldest) so a
    /// stalled drainer can't grow it without limit.
    pending_transitions: Vec<TransitionRecord>,
    /// #1527: count of transitions dropped because `pending_transitions` hit
    /// its cap before the supervisor drained it. Surfaced in a warn at drain
    /// so a wedged drainer is visible rather than silently lossy.
    dropped_transition_count: u64,
    /// #1808-probe0-phantom (instrumentation-only): signature of the LAST
    /// ServerRateLimit detection's error line — `(error_line_hash,
    /// dist_from_bottom)`. Persists ACROSS non-SRL/Idle states (deliberately NOT
    /// cleared) so a same-error re-match after the agent recovered to Idle (the
    /// cross-Idle phantom loop in cheerc's #1808 Evidence 2) is still detected.
    /// Read/written only for the phantom probe log; never affects transitions.
    last_srl_match_sig: Option<(u64, usize)>,
    /// #1955: when the current UsageLimit episode auto-releases. Anchored on
    /// the banner's own unlock hint parsed at latch time ("resets 4am" /
    /// "try again at 15:14", assumed daemon-local TZ — Claude prints the
    /// user's own zone); [`Self::USAGE_LIMIT_EXPIRY`] when unparseable.
    usage_limit_release_at: Option<Instant>,
    /// #1955: signature (bottom-most banner line hash + dist-from-bottom) of
    /// a RELEASED UsageLimit episode. The banner is level-triggered and can
    /// sit in the visible tail forever on an idle pane, so without this the
    /// next feed would simply re-latch what the expiry just released. A
    /// genuinely-new limit hit renders fresh at the bottom (different
    /// position, usually a different reset time) → different sig → latches.
    usage_limit_expired_sig: Option<(u64, usize)>,
    /// #1808-probe0-phantom (instrumentation-only): how many CONSECUTIVE feed
    /// ticks the same SRL error re-matched with NO intervening non-SRL state
    /// (the in-place clock-tick re-scan). Resets on a different signature or a
    /// cross-cycle refire. Distinct from `cross_cycle` (see the probe site).
    srl_consecutive_rematch: u32,
    /// #1808-probe0-phantom (instrumentation-only): set true whenever a feed
    /// lands a NON-ServerRateLimit state (esp. Idle); cleared on an SRL
    /// detection. Lets the probe distinguish a cross-Idle refire (same error
    /// re-grabbed AFTER the agent recovered) from a same-state continuous
    /// re-scan. Telemetry-only.
    non_srl_since_last_srl: bool,
    /// #2100/#2115 fire-once latch for the `#1562`/`#1808` unclassified-throttle
    /// side-log. The feed-level hash-dedup is DELIBERATELY bypassed for a
    /// throttle-hint screen ([`Self::apply_hash_dedup_gate`]), so without this a
    /// STATIC unclassified-throttle pane appends a JSONL record (and re-fires the
    /// `#1808-srl-detect-miss` WARN) on every PTY read. Keyed on the colored tail
    /// hash, which is stable across the cursor/clock re-renders that flip the
    /// full-screen hash; cleared when the pane leaves the throttle-miss shape so a
    /// genuine recurrence re-logs once.
    last_unclassified_throttle_sig: Option<u64>,
    /// #2086 dedup latch: `line_hash` of the stuck-SRL error line for which the
    /// `#2086-srl-keep-latched` WARN last fired. A genuinely stuck SRL has a
    /// working spinner ticking below it that flips the screen hash every feed, so
    /// without this the WARN re-fires every tick for the whole (~26 min) stuck
    /// duration (the #1450 flood class). Keyed on the error LINE hash only (not
    /// the full `srl_match_signature`) so a spinner whose glyph byte-length
    /// changes — shifting `dist_from_bottom` — doesn't defeat the dedup. Behavior
    /// unchanged — only the WARN emission is deduped.
    last_srl_keep_latched_sig: Option<u64>,
    /// #1808-probe0-phantom dedup latch: `(line_hash, cross_cycle)` of the SRL
    /// error line for which the `#1808-probe0-phantom` WARN last fired. An
    /// in-place static SRL keeps the same error line while a clock-tick redraw
    /// flips the screen hash, so the WARN otherwise re-fires every feed. Keyed on
    /// the line hash (see `last_srl_keep_latched_sig`) PLUS the `kind` discriminant
    /// (`cross_cycle` true = `cross_cycle_refire`, false = `consecutive_rematch`)
    /// so the SAME stale sig transitioning between the two refire kinds re-logs
    /// once per kind. Cleared on genuine recovery (CR-2026-06-14 t-43) so a 2nd
    /// SRL incident on the same line after recovery re-logs once; the `#1809`
    /// cross-cycle→Idle behavioral fix stays OUTSIDE this dedup.
    last_srl_phantom_warn_sig: Option<(u64, bool)>,
    /// #1523 Phase 0 (shadow): fire-once latch for the turn-completion sentinel
    /// telemetry — `hash` of the last side-logged sentinel observation, so a
    /// static frame holding the token isn't re-logged every feed (mirrors the
    /// retired `last_hardwrap_miss_sig`). Telemetry-only; never gates behavior.
    last_turn_sentinel_sig: Option<u64>,
}

/// #1527: one recorded `current` transition, captured at the mutation site so
/// the timestamp is the real transition time (not the later drain time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TransitionRecord {
    pub from: AgentState,
    pub to: AgentState,
    pub ts: String,
}

const MARKER_SCAN_TAIL_LINES: usize = 5;

/// ServerRateLimit recovery window — the single source of truth shared by the
/// detection-side `#badge-recovery` re-latch gate (here) and the supervisor's
/// `#1795` retry-inject gate (`supervisor` imports this as `RECOVERY_SILENCE`).
///
/// If the agent produced PRODUCTIVE output (a `last_productive_output` bump —
/// marker/heartbeat-gated, NOT the error re-render) within this window, it has
/// recovered: the badge must not re-latch ServerRateLimit and the retry must not
/// inject `continue`. This is the robust, position-INDEPENDENT recovery signal
/// (same insight as the #1775/#1792 silence clock): the stale "Server is
/// temporarily limiting" line re-latches ServerRateLimit in the tail and
/// `working_state_below` (#1769) can't see a marker BELOW the most-recent error
/// line, so the agent flickers Thinking↔ServerRateLimit and the Idle-only #1713
/// clear never fires — but `last_productive_output` stays fresh throughout,
/// breaking the loop. 45s comfortably exceeds the 10s tick and the generation
/// gaps between an agent's outputs while still firing fast for a truly wedged
/// throttle (which produces nothing). Tunable 30–60s.
pub(crate) const SERVER_RATE_LIMIT_RECOVERY_SILENCE: Duration = Duration::from_secs(45);

/// #1518: bottom-N bound for HIGH_FP error re-judging. Detection is
/// level-triggered (re-judged every feed), so an error string lingering
/// anywhere in the viewport keeps re-firing the error state — a retry storm
/// even after the agent has visually moved on. Bounding HIGH_FP re-matching to
/// the bottom `ERROR_TAIL_SCAN_LINES` rows lets an error scroll out of the live
/// tail and recover naturally (non-timer).
///
/// Value chosen from fixture evidence: across every canonical error recording
/// in `tests/fixtures/state-replay/` (claude/gemini/kiro/opencode rate-limit,
/// throttle, 429, usage-limit), a *fresh* error marker sits at depth 5–6 rows
/// from the bottom (max 6). 15 clears that by >2× — generous headroom for
/// multi-line / wrapped error bodies not in the fixtures — while still
/// suppressing an error that newer content has pushed into the top portion of a
/// typical 24–50 row viewport. Deliberately conservative: bias is toward NOT
/// dropping a real error. Distinct from `MARKER_SCAN_TAIL_LINES` (a tighter
/// structural-marker scan) — do not collapse the two.
const ERROR_TAIL_SCAN_LINES: usize = 15;

/// #2089: window for the hard-wrap throttle fallback's flatten. A long SRL error
/// on a narrow (~24-col) pane wraps across ~7-9 rows; with the spinner + input
/// box below, its `⏺ API Error:` indicator prefix lands well above
/// `ERROR_TAIL_SCAN_LINES`. This wider span captures the wrapped indicator
/// (still only the visible grid, never scrollback) so `throttle_indicator_adjacent`
/// can validate the real throttle; the char-proximity guard keeps it FP-safe.
const HARD_WRAP_TAIL_LINES: usize = 40;

fn recent_screen_tail(screen_text: &str, n: usize) -> String {
    #[cfg(test)]
    RECENT_SCREEN_TAIL_CALLS.with(|c| c.set(c.get() + 1));
    let lines: Vec<&str> = screen_text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

fn tail_recent_lines(tail: &str, n: usize) -> String {
    let lines: Vec<&str> = tail.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

// Per-test-thread tail-scan counter. The libtest harness runs each `#[test]` on
// its OWN thread, so a thread-local (vs the former process-global AtomicUsize)
// isolates the count per test: a parallel `cargo test` run no longer leaks a
// sibling test's `recent_screen_tail` calls into another test's assertion (the
// `changed_frames_..._bound_tail_scans` flake — green single-threaded, red under
// CI's parallel Check). The `reset()` each test runs at its start also keeps it
// correct under `--test-threads=1` (one thread reused across tests). `#[cfg(test)]`
// only — no prod footprint (the increment in `recent_screen_tail` is gated too).
#[cfg(test)]
thread_local! {
    static RECENT_SCREEN_TAIL_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_recent_screen_tail_call_count() {
    RECENT_SCREEN_TAIL_CALLS.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn recent_screen_tail_call_count() -> usize {
    RECENT_SCREEN_TAIL_CALLS.with(|c| c.get())
}

/// Context% telemetry: rows scanned from the bottom of the screen for the
/// backend's `context_pattern`. Sized from live pane geometry: the Claude
/// statusline renders 2 rows from the bottom; kiro's `◔ N%` footer sits up to
/// 5 rows from the bottom (footer + blank + input hint + blank + key help).
/// Deliberately MUCH narrower than `ERROR_TAIL_SCAN_LINES` — conversation
/// text (where agents discuss context%) must stay out of scope (prose-FP).
const CONTEXT_SCAN_ROWS: usize = 6;

/// Context% telemetry: readings older than this are dropped (not trusted) by
/// `resolved_context` — a narrow pane can truncate the statusline for the
/// rest of a session, and "can't read" must not freeze a stale percent.
const CONTEXT_FRESH: Duration = Duration::from_secs(600);

/// #1005 Phase A2: window inside which a `Lower→Active(X)→Lower→Active(X)`
/// bounce is treated as oscillation. Default 30s — matches
/// `StateTracker::LATCHED_STATE_EXPIRY` so the guard's protection
/// covers the same horizon as the latched-state expiry it backstops.
///
/// Fixed const 30s (#env-cleanup: was env-overridable via
/// `AGEND_OSCILLATION_GUARD_WINDOW_SECS`; demoted to YAGNI for single-user
/// deploys — re-add an override later if a real need appears).
fn oscillation_guard_window() -> Duration {
    const DEFAULT_SECS: u64 = 30;
    Duration::from_secs(DEFAULT_SECS)
}

/// HIGH_FP states: FP-prone error states whose markers appear in dialectic prose
/// / JSON dumps, so they get the extra FP defenses (the #1518 position gate + the
/// #1769 working-marker override + an anchor gate). The ANCHOR gate splits by
/// [`requires_red_anchor`] — RateLimit/ServerRateLimit now use a content anchor
/// (`in_error_line_excluding_input`), only ContextFull/ModelUnsupported still require red. This
/// predicate stays the full set because position + working-marker apply to all
/// four regardless of which anchor regime they use.
/// Per #919 spike + dev-2 cross-audit:
/// - ServerRateLimit / RateLimit: server-side throttle alternations
///   include `api_error|timeout_error|overloaded_error` etc which
///   appear in dialectic prose / JSON dumps.
/// - ContextFull: `context.*(full|limit)` second alternation is a
///   common English phrase.
fn is_high_fp_state(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::ServerRateLimit
            | AgentState::RateLimit
            | AgentState::ContextFull
            // #1634: the model-unsupported wordings (`invalid_request_error`,
            // `model is not supported`) appear verbatim in dialectic prose / when
            // an agent works on THIS detection code, so require the red anchor.
            // Critical here: ModelUnsupported never auto-clears and suppresses
            // hang-check, so a FP would silently disable a healthy agent — the
            // anchor is the FP boundary.
            | AgentState::ModelUnsupported
    )
}

/// t-coloranchor-remove-ratelimit (operator-approved after the corpus gate):
/// which HIGH_FP states still require the #1450 RED-SGR anchor vs the content
/// anchor (`in_error_line_excluding_input`).
///
/// - **`true`** — ContextFull, ModelUnsupported: keep RED. ContextFull has no
///   fixture corpus to prove a content path; ModelUnsupported never auto-clears
///   AND suppresses hang-check, so a verbatim-quote FP would silently disable a
///   healthy agent (the anchor is the FP boundary, #1634).
/// - **`false`** — RateLimit, ServerRateLimit: content anchor instead. The corpus
///   gate proved content+position+working-marker preserves detection (5/5 prose
///   suppressed, FN-covered incl. kiro via #1789), and the residual verbatim-quote
///   FP self-corrects because both states auto-clear + are retry-driven.
fn requires_red_anchor(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::ContextFull | AgentState::ModelUnsupported
    )
}

fn hash_screen(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// #1523 Phase 0: is the turn-completion sentinel shadow enabled? Default-OFF
/// (matches the codebase env-flag idiom, e.g. `AGEND_PRODUCTIVE_GATE`). When
/// off, neither the instruction directive nor the telemetry is active, so a
/// default fleet sees ZERO behaviour change (the fail-open invariant).
pub(crate) fn turn_sentinel_shadow_enabled() -> bool {
    std::env::var("AGEND_TURN_SENTINEL_SHADOW")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// #1523 Phase 0: the per-agent sentinel nonce, derived deterministically from
/// the agent name. Deriving (rather than persisting a random value) lets the
/// instruction writer and the daemon detector compute the SAME token
/// independently — no plumbed/persisted state, so it survives restart with no
/// staleness surface (cf. the in-mem-reset-on-restart bug class). It is an
/// attribution tag, not a secret; per-turn freshness comes from the detector's
/// dedup latch, not from the nonce.
pub(crate) fn turn_sentinel_nonce(name: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    // Low 32 bits as fixed-width hex → an 8-char nonce → 29-char token
    // (`=====AGEND-DONE:xxxxxxxx=====`), comfortably under the #2090 ~35-char
    // hard-wrap threshold so the marker stays on one line.
    format!("{:08x}", hasher.finish() & 0xffff_ffff)
}

/// #1523 Phase 0: turn-completion token delimiters. `=====` (not `<…>`): the
/// #2243 DuDuClaw lesson is that angle-bracket markers can be mangled by an
/// agent's own markdown/HTML rendering, which would conflate "agent did not
/// emit" with "delimiter was rewritten" and poison the Phase-0 emit/compliance
/// data (r2 #2297). The prefix is shared by the detector fast-path so it can't
/// drift from the token.
const TURN_SENTINEL_PREFIX: &str = "=====AGEND-DONE:";
const TURN_SENTINEL_SUFFIX: &str = "=====";

/// #1523 Phase 0: the exact in-band token for an agent's turn-completion
/// sentinel. The single source of truth — the instruction directive embeds this
/// exact string and the shadow detector scans for it, so the two never drift.
pub(crate) fn turn_sentinel_token(name: &str) -> String {
    format!(
        "{TURN_SENTINEL_PREFIX}{}{TURN_SENTINEL_SUFFIX}",
        turn_sentinel_nonce(name)
    )
}

/// #1523 Phase 0: pure classification of a screen `tail` against an agent's
/// turn-completion `token`. Separated from the I/O method so the echo /
/// source-view FP heuristics (the sharpest risk — the instruction file itself
/// carries the token) are unit-testable without env or home redirection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TurnSentinelObs {
    /// The agent's token is present anywhere in the tail (raw or de-wrapped).
    token_seen: bool,
    /// The token sits on the final non-empty line — the shape of a genuine
    /// turn-end emission (vs. embedded in earlier output).
    on_last_line: bool,
    /// Looks like an instruction-echo / source-view rather than a real emit:
    /// directive prose co-occurs, or the token is not the last line.
    suspected_echo: bool,
    /// Coarse leak proxy: token embedded as content (not a clean final-line
    /// emit) and not obviously a directive echo — e.g. a file being viewed.
    leak_signal: bool,
}

fn observe_turn_sentinel(tail: &str, token: &str) -> TurnSentinelObs {
    // Also scan a de-wrapped (newline-stripped) copy so a hard-wrapped token
    // (split mid-string across rows) still matches.
    let dewrapped: String = tail.chars().filter(|c| *c != '\n').collect();
    let token_seen = tail.contains(token) || dewrapped.contains(token);
    if !token_seen {
        return TurnSentinelObs {
            token_seen: false,
            on_last_line: false,
            suspected_echo: false,
            leak_signal: false,
        };
    }
    let on_last_line = tail
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .is_some_and(|l| l.contains(token));
    // The directive that injects the token uses these exact phrases; their
    // presence means the instruction file / system prompt is on screen.
    let directive_echo = tail.contains("Turn-completion signal")
        || tail.contains("never persisted content")
        || tail.contains("print this exact marker");
    let suspected_echo = directive_echo || !on_last_line;
    let leak_signal = !on_last_line && !directive_echo;
    TurnSentinelObs {
        token_seen,
        on_last_line,
        suspected_echo,
        leak_signal,
    }
}

/// #1955: the (bottom-most) screen line containing `matched` — the UsageLimit
/// banner line, so the unlock hint that follows the matched phrase on the same
/// line ("… · resets 4am (Asia/Taipei)") can be parsed.
fn line_containing<'a>(screen_text: &'a str, matched: &str) -> Option<&'a str> {
    let pos = screen_text.rfind(matched)?;
    let start = screen_text[..pos].rfind('\n').map_or(0, |i| i + 1);
    let end = screen_text[pos..]
        .find('\n')
        .map_or(screen_text.len(), |i| pos + i);
    Some(&screen_text[start..end])
}

/// #1955: duration until the banner's own unlock hint. Live forms: claude
/// `resets 4am (Asia/Taipei)` / `resets at 11:59pm`, codex `try again at
/// 15:14`. The banner prints the USER's timezone, which matches the daemon's
/// local clock on a single-operator host — so the target is resolved against
/// local time (next occurrence of that wall-clock time). A bare hour with no
/// am/pm and no `:MM` is too ambiguous to anchor on → `None` (the caller
/// falls back to the conservative window). Result capped at 24h — the hint
/// only encodes a time-of-day; a multi-day window self-corrects on the next
/// genuine re-latch (fresh banner → fresh anchor).
fn parse_usage_limit_release(line: &str) -> Option<Duration> {
    use std::sync::OnceLock;
    static UNLOCK: OnceLock<regex::Regex> = OnceLock::new();
    let re = UNLOCK.get_or_init(|| {
        #[allow(clippy::unwrap_used)] // const pattern — compile failure is a code bug
        regex::Regex::new(
            r"(?i)\b(?:resets?|try again)(?:\s+at)?\s+(\d{1,2})(?::(\d{2}))?\s*(am|pm)?",
        )
        .unwrap()
    });
    let caps = re.captures(line)?;
    let mut hour: u32 = caps[1].parse().ok()?;
    let minute: u32 = caps
        .get(2)
        .map(|m| m.as_str().parse().ok())
        .unwrap_or(Some(0))?;
    let ampm = caps.get(3).map(|m| m.as_str().to_ascii_lowercase());
    if ampm.is_none() && caps.get(2).is_none() {
        return None; // bare "resets 4" — ambiguous
    }
    match ampm.as_deref() {
        Some("pm") if hour < 12 => hour += 12,
        Some("am") if hour == 12 => hour = 0,
        _ => {}
    }
    if hour > 23 || minute > 59 {
        return None;
    }
    let now = chrono::Local::now().naive_local();
    let target_today = now.date().and_hms_opt(hour, minute, 0)?;
    let target = if target_today > now {
        target_today
    } else {
        target_today + chrono::Duration::days(1)
    };
    let until = (target - now).to_std().ok()?;
    Some(until.min(Duration::from_secs(24 * 3600)))
}

/// #1808-probe0-phantom (instrumentation-only): a stable signature of the
/// bottom-most occurrence of an SRL error line — `(line_hash, dist_from_bottom)`.
/// `dist_from_bottom` is the byte distance from the error line's start to the end
/// of `screen_text`; an error that has not physically moved keeps the same
/// distance across the cursor-blink / clock-tick re-renders that flip the screen
/// hash (the phantom trigger). Used only to compare consecutive SRL detections in
/// the phantom probe — never affects classification.
fn srl_match_signature(screen_text: &str, matched: &str) -> (u64, usize) {
    let pos = screen_text.rfind(matched).unwrap_or(0);
    let line_start = screen_text[..pos].rfind('\n').map_or(0, |i| i + 1);
    let line_end = screen_text[pos..]
        .find('\n')
        .map_or(screen_text.len(), |i| pos + i);
    let line_hash = hash_screen(&screen_text[line_start..line_end]);
    let dist_from_bottom = screen_text.len().saturating_sub(line_start);
    (line_hash, dist_from_bottom)
}

/// #1450: char-span (start, end) in `screen_text` for the byte occurrence of
/// `matched` starting at `byte_off`, clamped to `fg.len()`. The `fg` mask is
/// aligned 1:1 with `screen_text.chars()`, so the phrase's char-start index
/// is `screen_text[..byte_off].chars().count()`.
fn char_span(screen_text: &str, byte_off: usize, matched: &str, fg_len: usize) -> (usize, usize) {
    let start = screen_text[..byte_off.min(screen_text.len())]
        .chars()
        .count();
    let end = start.saturating_add(matched.chars().count()).min(fg_len);
    (start, end)
}

/// #1450: does ANY on-screen occurrence of `matched` render with a red cell?
///
/// Scans every byte occurrence of the phrase (not just the regex's first
/// match) and checks its rendered foreground span in `fg`. Scanning all
/// occurrences preserves the #919 "red anywhere" semantics: a real red error
/// still fires even when a plain (injected / user-typed) copy of the same
/// phrase sits earlier in the screen text. "Any red cell in the span"
/// mirrors the looseness of the old ±200-byte proximity check while being
/// more precise — it looks only at the phrase's own cells.
fn matched_span_has_red(screen_text: &str, matched: &str, fg: &[CellFg]) -> bool {
    if matched.is_empty() {
        return false;
    }
    let mut search = 0;
    while let Some(rel) = screen_text[search..].find(matched) {
        let byte_off = search + rel;
        let (start, end) = char_span(screen_text, byte_off, matched, fg.len());
        if fg
            .get(start..end)
            .is_some_and(|span| span.iter().any(|c| c.is_red()))
        {
            return true;
        }
        search = byte_off + matched.len();
    }
    false
}

/// #1518: is the matched marker still visible in the live bottom-`n` rows? A
/// HIGH_FP error phrase that has scrolled up past the tail (pushed there by the
/// agent's post-recovery output) is stale — it must NOT keep re-firing the error
/// transition every feed. Returns true iff `matched` occurs within the last `n`
/// rows of `screen_text` (where the agent's current activity is); a copy that
/// only survives in the scrolled-up region returns false → caller suppresses.
fn matched_span_in_recent_tail(screen_text: &str, matched: &str, n: usize) -> bool {
    if matched.is_empty() {
        return false;
    }
    // Trim trailing blank rows so "bottom-N" tracks the last N lines of actual
    // CONTENT (where the agent's cursor/activity is), not the blank padding the
    // emulator leaves below the cursor on a not-yet-full screen — otherwise a
    // single error line on an near-empty screen would look "scrolled out".
    recent_screen_tail(screen_text.trim_end(), n).contains(matched)
}

/// #1450: the `fg` span of the FIRST on-screen occurrence of `matched`, for
/// the suppress-path WARN diagnostic. Empty if not found / out of range.
fn first_occurrence_span(screen_text: &str, matched: &str, fg: &[CellFg]) -> Vec<CellFg> {
    let Some(byte_off) = screen_text.find(matched) else {
        return Vec::new();
    };
    let (start, end) = char_span(screen_text, byte_off, matched, fg.len());
    fg.get(start..end)
        .map(<[CellFg]>::to_vec)
        .unwrap_or_default()
}

/// #1562 self-capture instrument.
///
/// Distinctive, **low-FP** server-throttle / transient-error phrases drawn from
/// the `ServerRateLimit` pattern set (`patterns.rs`). Used by the diagnostic
/// side-log to detect "a known throttle phrase is on screen but the classifier
/// did NOT land on a retryable state" — the exact in-the-wild miss #1562 is
/// chasing. Deliberately a SUBSET of the regex alternations: only multi-word,
/// prose-unlikely phrases are listed (bare `overloaded` / `429` / `api_error`
/// are omitted because they're the HIGH_FP tokens that fire on dialectic prose,
/// which would make this instrument noisy). Cheap `str::contains` only.
const THROTTLE_DIAG_PHRASES: &[&str] = &[
    "temporarily limiting requests",
    "Overloaded errors",
    "overloaded_error",
    "Rate limited. Quick retry",
    "rate_limit_error",
    "API rate limited",
    "API Error: 5",
    "API Error: Request rejected (429)",
    "hit a rate limit",
];

/// #SRL-phase2: cheap raw single-token pre-filter for the hash-dedup blind-spot
/// bypass. A settled (static) stuck-SRL pane has an unchanged screen hash, so
/// `feed_with_fg`'s dedup would skip detection forever and it never recovers. We
/// only override the skip when one of these distinctive tokens is on screen —
/// and they are chosen to survive Ink WORD-wrap (each is a single token that sits
/// intact on one wrapped row), so a hard-wrapped SRL line still trips the
/// pre-filter even though the full phrase is split across rows. Kept tight to
/// avoid re-detecting every static idle pane.
const THROTTLE_HINT_TOKENS: &[&str] = &[
    "limiting",
    "Overloaded",
    "overloaded_error",
    "rate_limit_error",
    "RESOURCE_EXHAUSTED",
    "429",
    "Rate limited",
    // #1955: usage-limit banners — a static stuck-UsageLimit pane must also
    // re-evaluate so the release anchor / expired-sig suppression can run
    // (UsageLimit is deliberately NOT in `already_throttle`, so re-detection
    // proceeds). Substrings cover the claude trio ("You've hit your …
    // limit"), codex ("hit your usage limit"), kiro ("you have reached the
    // limit" / ServiceQuotaExceeded), and the credit-balance forms.
    "hit your",
    "reached the limit",
    "ServiceQuotaExceeded",
    "Credit balance",
    "credit_balance_too_low",
];

pub(crate) fn screen_has_throttle_hint(screen_text: &str) -> bool {
    THROTTLE_HINT_TOKENS.iter().any(|t| screen_text.contains(t))
}

/// #1562: rows of the live tail captured into the diagnostic record. Matches
/// the `ERROR_TAIL_SCAN_LINES` horizon so the captured context lines up with
/// the bottom-N window the error gates actually look at.
const UNCLASSIFIED_TAIL_LINES: usize = ERROR_TAIL_SCAN_LINES;

/// #1562: states for which a server-throttle phrase IS the expected
/// classification (the auto-retry path already handles them). When the
/// classifier lands on one of these, the throttle phrase is correctly
/// recognized → nothing to diagnose → no side-log (keeps the instrument
/// low-noise). Anything else + a throttle phrase = the miss we want captured.
fn is_throttle_retryable_state(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::ServerRateLimit
            | AgentState::RateLimit
            | AgentState::ApiError
            | AgentState::ContextFull
    )
}

/// #1562: a minimal SGR escape for a rendered [`CellFg`]. Reconstructs enough
/// ANSI to make the captured tail re-renderable in a terminal so an operator
/// can SEE whether the throttle line was red — the color-anchor hypothesis is
/// the whole point of #1562's capture. Exact non-red hues are lossy (the vterm
/// classifier collapses all reds into `CellFg::Red`), but the red/not-red
/// signal — the only thing the anchor predicate keys on — is preserved exactly.
fn sgr_for(c: CellFg) -> &'static str {
    match c {
        CellFg::Red => "\x1b[31m",
        CellFg::Default | CellFg::Named => "\x1b[39m",
        // Indexed / Rgb are non-red (the classifier already mapped reds to
        // `Red`); a generic "other color" marker is enough for the diagnostic.
        CellFg::Indexed(_) | CellFg::Rgb(_, _, _) => "\x1b[39m",
    }
}

/// #1562: reconstruct the last `n` lines of `screen_text` WITH ANSI color, using
/// the per-cell `fg` mask (aligned 1:1 with `screen_text.chars()`, see
/// `char_span`). The result is a colored, re-renderable tail for the side-log.
/// When `fg` is empty (text-only callers) every cell maps to Default → the tail
/// is captured without color, which is correct (no color was supplied).
fn ansi_colored_tail(screen_text: &str, fg: &[CellFg], n: usize) -> String {
    // Pair each char with its rendered fg, splitting on newlines (the '\n'
    // itself carries no cell).
    let mut lines: Vec<Vec<(char, CellFg)>> = vec![Vec::new()];
    for (i, ch) in screen_text.chars().enumerate() {
        if ch == '\n' {
            lines.push(Vec::new());
        } else {
            let color = fg.get(i).copied().unwrap_or(CellFg::Default);
            lines
                .last_mut()
                .expect("non-empty by construction")
                .push((ch, color));
        }
    }
    let start = lines.len().saturating_sub(n);
    let mut out = String::new();
    for line in &lines[start..] {
        let mut cur: Option<CellFg> = None;
        for &(ch, color) in line {
            if cur != Some(color) {
                out.push_str(sgr_for(color));
                cur = Some(color);
            }
            out.push(ch);
        }
        out.push_str("\x1b[0m");
        out.push('\n');
    }
    out
}

/// #1562: the pure decision behind the self-capture instrument — no IO, no env.
///
/// Returns `Some(raw_tail)` (ANSI-colored, last [`UNCLASSIFIED_TAIL_LINES`]
/// rows) iff ALL hold:
/// 1. a known throttle phrase ([`THROTTLE_DIAG_PHRASES`]) is on screen,
/// 2. `current` is NOT a retryable throttle state
///    ([`is_throttle_retryable_state`]) — i.e. the classifier MISSED it, and
/// 3. the phrase is in the LIVE bottom-N tail (not just scrolled-up scrollback).
///
/// `None` otherwise. The order is chosen so the common no-phrase case
/// fast-rejects on a single allocation-free `str::contains` scan.
/// Returns `Some((raw_tail, wrap_split))`. `wrap_split` is true when the phrase
/// was found ONLY after whitespace-flattening — i.e. it is LINE-WRAPPED across
/// rows in `screen_text`. Since the detection feed already de-wraps alacritty
/// soft-wraps (#1808 Phase 1), a phrase still wrap-split here means the wrap is
/// a hard `\n` (Ink-emitted layout) the de-wrap cannot merge → the signal that
/// the Phase 2 flattened-tail fallback is required for this backend's render.
fn unclassified_throttle_tail(
    current: AgentState,
    screen_text: &str,
    fg: &[CellFg],
    tail: &str,
) -> Option<(String, bool)> {
    // Fast reject (no allocation on the common path): no known throttle phrase
    // contiguously on screen. #1808 — the original `contains`-only check was
    // BLIND to a line-wrapped phrase (the exact reason the live narrow-pane SRL
    // miss captured nothing), so on a contiguous miss also try a
    // whitespace-flattened view (every whitespace run incl. `\n` → one space) so
    // a wrapped "temporarily\nlimiting\nrequests" still matches.
    let raw_hit = THROTTLE_DIAG_PHRASES
        .iter()
        .copied()
        .find(|p| screen_text.contains(p));
    let phrase = match raw_hit {
        Some(p) => p,
        None => {
            let flat = screen_text.split_whitespace().collect::<Vec<_>>().join(" ");
            THROTTLE_DIAG_PHRASES
                .iter()
                .copied()
                .find(|p| flat.contains(p))?
        }
    };
    let wrap_split = raw_hit.is_none();
    // Classifier landed on a retryable state → throttle phrase was correctly
    // recognized (auto-retry handles it). Nothing to diagnose → no noise.
    if is_throttle_retryable_state(current) {
        return None;
    }
    // Require the phrase in the LIVE bottom-N tail — a copy that only survives
    // in scrolled-up scrollback is stale, not a current miss. Check the
    // flattened tail too so a wrapped live error still qualifies.
    let tail = tail_recent_lines(tail, UNCLASSIFIED_TAIL_LINES);
    let tail_flat = tail.split_whitespace().collect::<Vec<_>>().join(" ");
    if !tail.contains(phrase) && !tail_flat.contains(phrase) {
        return None;
    }
    Some((
        ansi_colored_tail(screen_text, fg, UNCLASSIFIED_TAIL_LINES),
        wrap_split,
    ))
}

/// #SRL-phase2: hard-wrap detection fallback. Ink word-wraps a long
/// SRL/RateLimit error line into multiple rows joined by REAL `\n` (not an
/// alacritty soft-wrap), so the single-line `detect_with_match` regex on the raw
/// screen can't match it — the exact live narrow-pane miss that left the agent
/// SRL-stuck with 0 detection. Flatten the bottom-N tail (every whitespace run,
/// incl. `\n`, → one space) and re-detect there.
///
/// Guards (scoped TIGHT — this runs only when raw detection already missed):
/// - **auto-clear throttle states ONLY** (`ServerRateLimit` / `RateLimit`).
///   NEVER `ModelUnsupported` / `ContextFull`: those don't auto-clear and
///   suppress hang-check, so a flatten-FP would silently wedge a healthy agent.
/// - **a PROXIMITY-scoped error indicator** — the prose-FP guard. The flattened
///   tail has NO `\n`, so `in_error_line` (which line-scopes via `\n`) would
///   degenerate to "an indicator ANYWHERE in the bottom-N tail" — a distant
///   unrelated `API Error:` on a different row would false-pass (reviewer-2,
///   #1857). Instead require the indicator within [`THROTTLE_INDICATOR_PROXIMITY`]
///   chars BEFORE the throttle match: a legit Ink hard-wrap renders the indicator
///   (`API Error:`) immediately before the throttle phrase, so it falls inside
///   the window; a stray indicator from an unrelated earlier row is far away and
///   rejected. The residual verbatim-quote FP (a pane literally rendering
///   `API Error: <throttle>` adjacent) is ACCEPTED — same as the raw-path policy:
///   these states auto-clear and are retry-driven, so a one-off mis-latch
///   self-corrects.
fn flattened_throttle_detect(
    patterns: &crate::state::patterns::StatePatterns,
    screen_text: &str,
    input_line_markers: &[&str],
) -> Option<AgentState> {
    // #2090 P1: SRL/RateLimit caller of the shared `flattened_guarded_detect`
    // skeleton. The SRL-specific config is (a) the `HARD_WRAP_TAIL_LINES` flatten
    // window, (b) the {ServerRateLimit, RateLimit} accept-set, and (c) the
    // `throttle_indicator_adjacent` proximity/anchor guard. Byte-identical to the
    // pre-extraction inline body — SRL is the SOLE caller in P1; P2 adds further
    // patterns (UsageLimit / ContextFull) each with their own accept-set + guard.
    flattened_guarded_detect(
        patterns,
        screen_text,
        input_line_markers,
        HARD_WRAP_TAIL_LINES,
        |state| matches!(state, AgentState::ServerRateLimit | AgentState::RateLimit),
        throttle_indicator_adjacent,
    )
}

/// #2090 P1: the reusable narrow-pane hard-wrap flatten-rematch skeleton,
/// extracted from the SRL path (#2089) so P2 can add per-pattern callers.
///
/// A long error/status line on a NARROW pane is word-wrapped by the app across
/// many rows, so the single-line `detect_with_match` over the live grid misses
/// it. This flattens the bottom `tail_lines` rows — dropping input/user lines
/// FIRST (#1947: flattening destroys the line structure the input-line
/// exclusion needs) then space-joining — to recover the wrapped logical line,
/// re-runs `detect_with_match`, and gates the result on:
///   - `accept`: the detected state is one this caller rescues (a per-pattern
///     allow-set — keeps an unrelated state the flatten happened to surface out);
///   - `anchor`: a caller-supplied proximity/anchor guard `(flat, matched)` that
///     is the `\n`-less replacement for line-scoped exclusion (flattening
///     removed the `\n`s the raw path's line-scoping relied on), rejecting a
///     prose / distant-indicator false positive.
///
/// `tail_lines` is widened past `ERROR_TAIL_SCAN_LINES` for SRL because a
/// hard-wrapped indicator prefix can sit many rows above the throttle phrase;
/// it only widens the visible-grid LINE span (never scrollback), and the
/// char-proximity `anchor` still rejects a distant unrelated indicator.
fn flattened_guarded_detect(
    patterns: &crate::state::patterns::StatePatterns,
    screen_text: &str,
    input_line_markers: &[&str],
    tail_lines: usize,
    accept: impl Fn(AgentState) -> bool,
    anchor: impl Fn(&str, &str) -> bool,
) -> Option<AgentState> {
    let tail = recent_screen_tail(screen_text, tail_lines);
    let flat = tail
        .lines()
        .filter(|line| !crate::state::patterns::is_input_line(line, input_line_markers))
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ");
    let (state, matched) = patterns.detect_with_match(&flat)?;
    if !accept(state) {
        return None;
    }
    if !anchor(&flat, matched) {
        return None;
    }
    Some(state)
}

/// Chars before a throttle match within which a real error indicator must sit for
/// the [`flattened_throttle_detect`] prose-FP guard. A hard-wrapped SRL renders
/// the `API Error:` prefix directly before the throttle phrase (≈10–30 chars); 80
/// leaves slack for a short row prefix while staying tight enough to exclude an
/// unrelated indicator on a different (flattened-away) row.
const THROTTLE_INDICATOR_PROXIMITY: usize = 80;

/// #SRL-phase2 (reviewer-2 #1857 fix): require an error indicator within
/// `THROTTLE_INDICATOR_PROXIMITY` chars BEFORE `matched` in the flattened tail —
/// the `\n`-less replacement for `in_error_line_excluding_input`'s line-scoping. The window spans
/// `[match_start - K, match_end]` so an indicator that IS the match
/// (`API Error: 5xx`) or sits just before it (`API Error: Server is temporarily
/// limiting …`) passes, while a distant unrelated indicator does not.
fn throttle_indicator_adjacent(flat: &str, matched: &str) -> bool {
    let Some(start) = flat.find(matched) else {
        return false;
    };
    let end = start + matched.len();
    let raw_win_start = start.saturating_sub(THROTTLE_INDICATOR_PROXIMITY);
    // Snap to a char boundary at/after the raw start so slicing never panics on
    // multi-byte content. `start` is always a boundary (it's a `find` result).
    let win_start = (raw_win_start..=start)
        .find(|&i| flat.is_char_boundary(i))
        .unwrap_or(start);
    crate::state::patterns::line_has_error_indicator(&flat[win_start..end])
}

/// Chars around a UsageLimit match within which the banner's STRUCTURAL markers
/// must sit for the [`flattened_guarded_detect`] prose-FP guard. The Claude
/// weekly/session-limit banner renders inside a `⎿` box-draw block whose phrase is
/// immediately followed by a `· resets <time>` reset stamp; after the hard-wrap
/// flatten the box-draw lands a few tokens before the phrase and the reset stamp a
/// few after, so 40 chars each side covers a short row prefix while staying tight.
const USAGELIMIT_BANNER_PROXIMITY: usize = 40;

/// #2090 P2: the `\n`-less line-scope replacement for UsageLimit (the analogue of
/// SRL's [`throttle_indicator_adjacent`]). A real hard-wrapped usage-limit banner
/// carries BOTH a `⎿` box-draw prefix BEFORE the phrase and a `resets` stamp AFTER
/// it, within proximity. A prose verbatim quote of the phrase (an agent pasting
/// "You've hit your weekly limit" into chat) has neither, so it is rejected — the
/// residual FP (agent quotes the FULL banner incl. box-draw + reset stamp while
/// itself idle) is vanishingly rare. Conservative by design: a banner variant
/// without a `resets` stamp (e.g. the credit-balance form) is NOT rescued here —
/// the raw non-wrapped path still catches it, and the #2090 shadow only observed
/// the weekly/session `· resets` banners hard-wrapping.
fn usagelimit_banner_adjacent(flat: &str, matched: &str) -> bool {
    let Some(start) = flat.find(matched) else {
        return false;
    };
    let end = start + matched.len();
    // `⎿` box-draw block prefix within proximity BEFORE the phrase.
    let raw_before = start.saturating_sub(USAGELIMIT_BANNER_PROXIMITY);
    let before_start = (raw_before..=start)
        .find(|&i| flat.is_char_boundary(i))
        .unwrap_or(start);
    let has_box = flat[before_start..start].contains('⎿');
    // `resets` reset stamp within proximity AFTER the phrase.
    let raw_after = (end + USAGELIMIT_BANNER_PROXIMITY).min(flat.len());
    let after_end = (end..=raw_after)
        .rev()
        .find(|&i| flat.is_char_boundary(i))
        .unwrap_or(end);
    let has_reset = flat[end..after_end].contains("resets");
    has_box && has_reset
}

/// #1562: best-effort append of one JSON record as a single `line\n` to `path`,
/// creating parent dirs / the file as needed. Returns `Err` for the caller to
/// log; never panics. The single small `write_all` relies on `O_APPEND`
/// atomicity so concurrent appenders don't interleave within a record.
/// `pub(crate)` so the shadow-telemetry family (e.g. `daemon::recovery_shadow`)
/// shares the one append helper.
pub(crate) fn append_jsonl(
    path: &std::path::Path,
    record: &serde_json::Value,
) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())
}

impl StateTracker {
    /// Max time a self-expiring active state (Thinking / ToolUse) may stay
    /// latched when the screen keeps updating but no pattern matches on it.
    /// See `maybe_expire_latched_state` for rationale.
    ///
    /// F39: expiry effectiveness depends on `since` actually elapsing —
    /// Scenario C oscillation can keep `since` recent. See
    /// `docs/HUNG-STATE-TRANSITIONS.md §F39.2`. #1005 Phase A2 closes
    /// this gap via the oscillation guard at `transition()`.
    /// #1527: max buffered transitions before drop-oldest kicks in. The
    /// supervisor drains every tick (~10s) and a healthy agent produces far
    /// fewer than this per tick; the cap only bounds memory if the drainer
    /// stalls.
    const PENDING_TRANSITIONS_CAP: usize = 256;
    const LATCHED_STATE_EXPIRY: Duration = Duration::from_secs(30);

    /// #1005 Phase A2: minimum hold in the lower-priority state before
    /// a priority-up back into the previous active state is allowed.
    /// If the lower-state was held less than this, the priority-up is
    /// treated as part of an oscillation cycle and suppressed.
    ///
    /// Chosen at 5s: matches `min_hold` for passive states (5s for
    /// Idle), so legitimate "user briefly idle then activity"
    /// transitions still go through.
    const OSCILLATION_LOWER_HOLD_THRESHOLD: Duration = Duration::from_secs(5);

    /// Max time `InteractivePrompt` / `PermissionPrompt` may stay latched
    /// after its trigger pattern stops matching. Longer than
    /// LATCHED_STATE_EXPIRY because operators legitimately take a while
    /// to respond to a dialog, but bounded so a prompt dismissed
    /// out-of-band (screen hash unchanged after dismissal ⇒ no re-detect)
    /// eventually recovers to Idle instead of staying stuck — the
    /// operator-reported `dev-reviewer 卡在互動 prompt` false positive.
    const INTERACTIVE_EXPIRY: Duration = Duration::from_secs(120);

    /// Max time `RateLimit` may stay latched. Real rate limits typically
    /// clear in seconds to minutes; 5 min covers aggressive throttling
    /// while preventing hours-long false positives (PR #319 incident).
    const RATE_LIMIT_EXPIRY: Duration = Duration::from_secs(300);

    /// #1955: conservative UsageLimit release window when the banner carries
    /// no parseable unlock hint ("resets 4am" / "try again at 15:14"). Long
    /// enough to not flap on a real limit, short enough that an idle agent is
    /// never stranded for days (the `general` incident): after release the
    /// agent degrades to Idle; if it is dispatched while genuinely still
    /// limited, the attempt re-renders a FRESH banner at the bottom → a new
    /// signature → re-latch with a re-parsed anchor (self-correcting).
    const USAGE_LIMIT_EXPIRY: Duration = Duration::from_secs(30 * 60);

    /// If the last MCP heartbeat is within this window, the agent is
    /// considered alive and `PermissionPrompt` detection is suppressed.
    const HEARTBEAT_FRESH_WINDOW: Duration = Duration::from_secs(120);

    pub fn new(backend: Option<&Backend>) -> Self {
        // Backends without a state pattern catalog (Shell, Raw) skip the
        // `Starting → Idle` handshake. Without this they sat in
        // `Starting` forever — `detect()` can.t possibly fire Idle
        // without any patterns — and the silence-based
        // `check_awaiting_operator` then flagged every idle shell as
        // "stuck on interactive prompt" after 30s of normal quiet at
        // its own prompt. Managed backends still start in `Starting` so
        // Idle is declared.
        // #1580: every backend sources its initial state from the co-located
        // profile (empty_profile=Idle for Shell/Raw, agy/kiro/...=Starting). An
        // unmanaged tracker (`backend == None`) starts Idle.
        let initial_state = match backend {
            Some(b) => crate::backend_profile::profile(b).initial_state,
            None => AgentState::Idle,
        };
        Self {
            current: initial_state,
            since: Instant::now(),
            last_output: Instant::now(),
            last_productive_output: None,
            created_at: Instant::now(),
            last_productive_marker_hash: None,
            last_anchor_suppress_hash: None,
            last_screen_hash: None,
            patterns: backend.map(StatePatterns::for_backend),
            context_regex: backend
                .map(crate::backend_profile::profile)
                .and_then(|p| p.context_pattern)
                .and_then(|p| regex::Regex::new(p).ok()),
            context_pct: None,
            interactive_prompt_pending_notice: false,
            interactive_recovery_pending_notice: false,
            blocked_since: None,
            blocked_notice_sent: false,
            recovery_episode: None,
            last_heartbeat: None,
            behavioral_config: backend.map(crate::behavioral::config_for),
            productivity_config: backend.map(crate::behavioral::config_for_productivity),
            instance_name: String::new(),
            backend_name: backend.map(|b| b.name().to_string()).unwrap_or_default(),
            // #919/#1450: backend opt-in for the color anchor. Defaults true
            // for known TUI backends (Claude/Codex/Gemini/OpenCode/
            // KiroCli), false for Shell/Raw — see
            // `Backend::should_anchor_on_red`.
            anchor_on_red: backend.is_some_and(|b| b.should_anchor_on_red()),
            input_line_markers: backend
                .map(crate::backend_profile::profile)
                .map(|p| p.input_line_markers)
                .unwrap_or(&[]),
            // #1005 A2: oscillation guard starts unarmed — first
            // legitimate priority-up records into it; subsequent
            // priority-ups within the window check against it.
            last_priority_up_into: None,
            // #1527: transition audit buffer starts empty.
            pending_transitions: Vec::new(),
            dropped_transition_count: 0,
            // #1808-probe0-phantom: no SRL seen yet.
            last_srl_match_sig: None,
            usage_limit_release_at: None,
            usage_limit_expired_sig: None,
            srl_consecutive_rematch: 0,
            non_srl_since_last_srl: false,
            last_unclassified_throttle_sig: None,
            last_srl_keep_latched_sig: None,
            last_srl_phantom_warn_sig: None,
            last_turn_sentinel_sig: None,
        }
    }

    /// Construct a tracker for a NAMED agent — the production path. The name is
    /// required for per-agent telemetry: in particular the #1523 turn-completion
    /// sentinel derives this agent's token from its name (see
    /// [`turn_sentinel_token`]), so an unnamed tracker would compute the wrong
    /// token and the shadow log would never fire. Prefer this over `new` +
    /// `set_instance_name` at every real spawn site so the name can't be
    /// forgotten (r6 #2297: the inline `StateTracker::new` left it empty in prod).
    pub fn for_agent(backend: Option<&Backend>, name: &str) -> Self {
        let mut t = Self::new(backend);
        t.set_instance_name(name);
        t
    }

    /// Set instance name for per-agent telemetry logging. Prefer [`for_agent`]
    /// at construction; this stays public for the supervisor/tests that need to
    /// (re)name an already-built tracker.
    pub fn set_instance_name(&mut self, name: &str) {
        self.instance_name = name.to_string();
    }

    /// Returns true if behavioral config is populated (managed backends).
    #[allow(dead_code)] // Used by behavioral::tests::state_tracker_has_behavioral_config
    pub fn has_behavioral_config(&self) -> bool {
        self.behavioral_config.is_some()
    }

    /// Returns true at most once per entry into `InteractivePrompt`. The
    /// supervisor calls this each tick; it returns true only on the first
    /// tick after a fresh transition into the state so Telegram only gets
    /// one notice per prompt, not one per tick.
    pub fn take_interactive_prompt_notice(&mut self) -> bool {
        if self.interactive_prompt_pending_notice {
            self.interactive_prompt_pending_notice = false;
            true
        } else {
            false
        }
    }

    /// Returns `Some(episode)` at most once per recovery from a blocked state
    /// (InteractivePrompt / AwaitingOperator → non-blocked). The supervisor
    /// calls this each tick; it returns `Some` only on the first tick after the
    /// recovery transition so Telegram sees one "ready again" decision, not one
    /// per tick. #2033: the returned [`RecoveryEpisode`] carries the gate inputs
    /// (was-notified + duration) so the supervisor can pick actionable-or-silent.
    /// A defensive default episode (`notice_sent=false`) is returned if the flag
    /// armed without a captured episode → the gate treats it as non-actionable.
    ///
    /// KNOWN LIMITATION (#2033, reviewer-2 non-blocking): the per-episode state
    /// (`blocked_since` / `blocked_notice_sent`) is in-memory, so a daemon restart
    /// mid-block loses it. If the agent then recovers BEFORE the supervisor
    /// re-detects + re-notifies the block, its recovery episode defaults to
    /// `notice_sent=false` and the all-clear is suppressed — one missed recovery
    /// notice per restart-straddling block. Deliberately the safe direction
    /// (err-silent over false-notify); persisting the episode across restarts is
    /// not worth the complexity for this narrow window.
    pub fn take_recovery_notice(&mut self) -> Option<RecoveryEpisode> {
        if self.interactive_recovery_pending_notice {
            self.interactive_recovery_pending_notice = false;
            Some(self.recovery_episode.take().unwrap_or_default())
        } else {
            None
        }
    }

    /// #2033: record that a Telegram blocked (`Stall`) notice was forwarded for
    /// the current blocked episode. Called by the supervisor at the moment it
    /// commits to the notice, so the paired recovery notice knows the operator
    /// was actually told. No-op if not currently in a blocked episode.
    pub fn mark_blocked_notice_sent(&mut self) {
        self.blocked_notice_sent = true;
    }

    /// If detected state is `PermissionPrompt` but a fresh heartbeat exists,
    /// override to `Thinking` — the agent is alive and the PTY pattern is a
    /// false positive (A5 fix, design §4.3).
    fn gate_on_heartbeat(&self, detected: AgentState) -> AgentState {
        if detected == AgentState::PermissionPrompt && self.is_heartbeat_fresh() {
            AgentState::Thinking
        } else {
            detected
        }
    }

    pub(crate) fn is_heartbeat_fresh(&self) -> bool {
        self.last_heartbeat
            .is_some_and(|t| t.elapsed() < Self::HEARTBEAT_FRESH_WINDOW)
    }

    /// Update heartbeat from an externally computed age (supervisor tick
    /// reads metadata file timestamp, computes duration since).
    pub fn update_heartbeat(&mut self, age: Duration) {
        self.last_heartbeat = Some(Instant::now().checked_sub(age).unwrap_or_else(Instant::now));
    }

    /// Context% telemetry: extract the backend's self-reported context usage
    /// from the BOTTOM status rows of a changed screen. Scoped to the last
    /// [`CONTEXT_SCAN_ROWS`] rows (the statusline region) — agents routinely
    /// DISCUSS context% in conversation text, so scanning the wider error-tail
    /// window would false-positive on prose. A stale frame can leave an old
    /// statusline copy above the live one, so the LAST matching row wins. No
    /// match (e.g. a narrow pane truncating the statusline) keeps the previous
    /// reading — its timestamp lets consumers judge staleness.
    fn scan_context_pct(&mut self, screen_text: &str) {
        let Some(re) = &self.context_regex else {
            return;
        };
        let tail = recent_screen_tail(screen_text, CONTEXT_SCAN_ROWS);
        for line in tail.lines().rev() {
            // #1246: a backend `context_pattern` may match without a capture
            // group 1 (a plausible future profile); `caps[1]` would Index-panic
            // the PTY read loop on the first matching frame. `caps.get(1)`
            // degrades a missing group to "no reading" (keeps the previous one).
            if let Some(pct) = re
                .captures(line)
                .and_then(|caps| caps.get(1))
                .and_then(|m| m.as_str().parse::<f32>().ok())
            {
                self.context_pct = Some((pct.clamp(0.0, 100.0), Instant::now()));
                return;
            }
        }
    }

    /// Resolved context usage as `(percent, source)` — PATTERN ONLY (the
    /// agent's own statusline). Readings older than [`CONTEXT_FRESH`] are
    /// dropped rather than trusted — `None` = honestly unknown, no alert.
    ///
    /// #1945-disable (operator decision, 2026-06-10): the transcript-estimate
    /// fallback ("transcript" source) is DISABLED — its first live minute
    /// produced a triple false 100% alert (window misjudge: transcript model
    /// ids carry no `[1m]` suffix → 1M sessions resolved against 200k). The
    /// corrected estimator + its root-cause record live on in
    /// `token_cost::estimate_context_pct` (tested, uncalled); re-enable ONLY
    /// after validating its readings against statusline ground truth.
    pub fn resolved_context(&self) -> Option<(f32, &'static str)> {
        if let Some((pct, at)) = self.context_pct {
            if at.elapsed() < CONTEXT_FRESH {
                return Some((pct, "pattern"));
            }
        }
        None
    }

    /// Feed the current vterm screen text (ANSI already resolved by the
    /// terminal emulator — caller passes plain text rows).
    ///
    /// If the screen is identical to the previous snapshot (same hash) we
    /// skip: no silence-timer bump, no re-detection. This lets invisible
    /// terminal chatter (cursor blinks, bell, etc.) pass through without
    /// masking hang/awaiting detection.
    ///
    /// When the screen does change, `last_output` is bumped and pattern
    /// detection runs against the full screen text. Because we always feed
    /// the current grid (not an accumulation), dismissed prompts drop out of
    /// detection on the next call — no stale-buffer lag.
    ///
    /// When detection returns `None` on a changed screen we fall through to
    /// `maybe_expire_latched_state`, which drops long-held active states back
    /// to Idle so the tracker cannot get stuck if a marker pattern briefly
    /// disappears without the Idle pattern re-matching.
    ///
    /// Heartbeat gate (A5 fix): after pattern detection, if the detected
    /// state is `PermissionPrompt` but a fresh MCP heartbeat exists, the
    /// detection is overridden to `Thinking` — the agent is alive and the
    /// PTY pattern is a false positive.
    ///
    /// Text-only entry point: delegates to [`feed_with_fg`] with an empty
    /// color mask, which fails the HIGH_FP color anchor *open* (no color
    /// info ⇒ no suppression). Production drives [`feed_with_fg`] directly
    /// with the vterm cell-color mask; tests and non-managed callers use
    /// this.
    #[allow(dead_code)] // text-only test seam; production uses feed_with_fg
    pub fn feed(&mut self, screen_text: &str) {
        self.feed_with_fg(screen_text, &[]);
    }

    /// #1450: feed the current vterm screen text together with the
    /// per-character foreground color mask (`fg`, aligned 1:1 with
    /// `screen_text.chars()` — see [`crate::vterm::VTerm::tail_lines_with_fg`]).
    ///
    /// HIGH_FP patterns (ServerRateLimit / RateLimit / ContextFull) require
    /// the matched phrase to be rendered red before the transition fires.
    /// This replaces the #919 raw-byte SGR ring: instead of scanning raw PTY
    /// bytes for one of four hard-coded 16-color escapes (which missed
    /// truecolor and broke on Ink redraw fragmentation — #1450 RCA), we read
    /// the color straight off the resolved grid cells, where alacritty has
    /// already normalized every SGR encoding (16 / 256 / truecolor).
    ///
    /// Fail-open conditions (HIGH_FP transition fires WITHOUT a red check):
    /// `anchor_on_red == false` (Shell/Raw) OR `fg` is empty (text-only
    /// callers / cold paths) — matching pre-#919 unconditional behavior.
    pub fn feed_with_fg(&mut self, screen_text: &str, fg: &[CellFg]) -> bool {
        // Gate 1 — hash-dedup (skip unchanged frames; throttle-hint override).
        // On a MISS the post-dedup pipeline runs (see `feed_after_dedup`).
        let hash = hash_screen(screen_text);
        if self.apply_hash_dedup_gate(screen_text, hash) {
            return false;
        }
        self.feed_after_dedup(screen_text, fg);
        true
    }

    /// #perf-R1: hot-path entry. Runs the cheap text-only hash-dedup gate FIRST
    /// and builds the fg colour mask (via `build_fg`) ONLY on a dedup MISS, so an
    /// unchanged redraw (Ink/spinner re-emitting an identical frame) skips the
    /// O(rows*cols) `classify_fg` pass + per-row allocations entirely — all while
    /// holding the contended per-agent core lock.
    ///
    /// Behaviourally identical to `feed_with_fg(screen_text, &build_fg())`: the
    /// gate's dedup decision is byte-identical because `screen_text` MUST be the
    /// SAME de-wrapped text `feed_with_fg` would hash — callers pass
    /// [`crate::vterm::VTerm::tail_lines_dewrapped`], whose text equals
    /// `tail_lines_with_fg().0`. `build_fg` then yields the colour mask aligned
    /// 1:1 with that text (same grid, same de-wrap ⇒ `tail_lines_with_fg().1`).
    pub fn feed_with_lazy_fg(
        &mut self,
        screen_text: &str,
        build_fg: impl FnOnce() -> Vec<CellFg>,
    ) -> bool {
        let hash = hash_screen(screen_text);
        if self.apply_hash_dedup_gate(screen_text, hash) {
            return false;
        }
        let fg = build_fg();
        self.feed_after_dedup(screen_text, &fg);
        true
    }

    /// Shared post-dedup body of [`feed_with_fg`] / [`feed_with_lazy_fg`] —
    /// order-critical, read top-to-bottom:
    ///   2. classify + transition  — anchor/position suppression → landing
    ///                               pipeline → heartbeat gate → transition
    ///   3. post-classify instrumentation (zero behavior): unclassified-throttle
    ///      capture, shadow telemetry, F9 productive-output detection
    ///
    /// The caller MUST have already passed the hash-dedup gate (gate 1) for this
    /// `screen_text`; entering here re-runs detection unconditionally.
    fn feed_after_dedup(&mut self, screen_text: &str, fg: &[CellFg]) {
        // Sprint 27 shadow-mode: capture silence duration BEFORE updating
        // last_output, so we measure time since previous feed (not current).
        let silence_since_last_feed = self.last_output.elapsed();

        self.last_output = Instant::now();

        if let Some(patterns) = self.patterns {
            // #1450: detect_with_match returns the matched substring so we
            // can locate its rendered grid cells and check their foreground
            // color. For HIGH_FP states, require at least one red cell across
            // some on-screen occurrence of the phrase. Gate fail-open when
            // `anchor_on_red` is false (Shell/Raw backends) OR no color mask
            // was supplied (text-only callers).
            match patterns.detect_with_match(screen_text) {
                Some((detected, matched))
                    if !is_high_fp_state(detected)
                        && !matches!(detected, AgentState::UsageLimit)
                        // Each rescue has its OWN wrap-surviving cheap pre-filter:
                        // THROTTLE_HINT_TOKENS are single words for SRL, but the
                        // usage-limit hint must survive a hard-wrap — the multi-word
                        // "hit your" splits across narrow-pane rows, so gate the
                        // UsageLimit rescue on the single word `resets` (the reset
                        // stamp the banner + `usagelimit_banner_adjacent` both carry).
                        && ((screen_has_throttle_hint(screen_text)
                            && self.try_hard_wrap_throttle(patterns, screen_text))
                            || (screen_text.contains("resets")
                                && self.try_hard_wrap_usagelimit(patterns, screen_text))) =>
                {
                    // #2089/#2090: `detect_with_match` landed a BENIGN state
                    // (Idle/Thinking — the ❯ prompt / a spinner) because a long
                    // SRL error OR usage-limit banner word-wrapped across a narrow
                    // pane and the single-line regex missed it. The cheap
                    // throttle/quota-hint pre-filter (THROTTLE_HINT_TOKENS covers
                    // both) gates the cost; `try_hard_wrap_throttle` then (#2090 P2)
                    // `try_hard_wrap_usagelimit` re-detected it on the flattened
                    // tail under its own structural guard and transitioned
                    // (overriding the idle-prompt chrome). `matched` is unused here.
                    let _ = matched;
                }
                Some((detected, matched)) => {
                    let high_fp = is_high_fp_state(detected);
                    let usage_limit = matches!(detected, AgentState::UsageLimit);
                    // Suppression gates — run BEFORE the landing pipeline. A
                    // detection that fails the anchor gate OR has scrolled out of
                    // the live tail (position gate) is treated as no-match and
                    // falls through to structural fallback / latch maintenance.
                    let anchor_fail = self.apply_anchor_gate(
                        detected,
                        matched,
                        screen_text,
                        fg,
                        high_fp,
                        usage_limit,
                    );
                    let stale_position =
                        self.apply_position_gate(matched, screen_text, high_fp, usage_limit);
                    if anchor_fail || stale_position {
                        self.handle_suppressed_detection(
                            detected,
                            matched,
                            screen_text,
                            fg,
                            anchor_fail,
                        );
                    } else {
                        // Landing pipeline — order-critical, encoded here in ONE
                        // place. Each gate may override the previous `landed`:
                        //   1. working-marker override (#1768/#1777/#badge-recovery)
                        //   2. SRL phantom gate (#1808/#1809)    — cross-cycle → Idle
                        //   3. UsageLimit lifecycle gate (#1955) — past release → Idle
                        // The heartbeat gate then has the final say before transition.
                        let mut landed = self.apply_working_marker_override(
                            patterns,
                            detected,
                            matched,
                            screen_text,
                            high_fp,
                        );
                        landed =
                            self.apply_srl_phantom_gate(detected, matched, screen_text, landed);
                        landed = self.apply_usage_limit_lifecycle_gate(
                            detected,
                            matched,
                            screen_text,
                            landed,
                        );
                        let gated = self.gate_on_heartbeat(landed);
                        self.transition(gated);
                    }
                }
                None => self.handle_no_raw_match(patterns, screen_text),
            }

            // #1808-probe0-phantom (instrumentation-only): record that the tracker
            // passed through a NON-ServerRateLimit landed state since the last SRL
            // detection, so a later same-error re-match is flagged as a cross-Idle
            // phantom refire (not a same-state continuous re-scan). The SRL branch
            // above clears this on detection; here we (re)set it whenever the landed
            // state is anything else. Telemetry-only — does not affect classification.
            if !matches!(self.current, AgentState::ServerRateLimit) {
                self.non_srl_since_last_srl = true;
                // CR-2026-06-14 t-43: the SRL fire-once WARN latches
                // (`last_srl_keep_latched_sig` #2086 / `last_srl_phantom_warn_sig`
                // #1808) are SET-ONLY — unlike `last_unclassified_throttle_sig`,
                // which clears when the pane leaves the throttle-miss shape so a
                // recurrence re-logs once. Mirror that here: on a GENUINE recovery
                // (recent productive output) drop both latches so a 2nd SRL
                // incident on the SAME error line — after the agent really
                // recovered — re-logs once instead of being silently suppressed.
                // Gated on `recovered_within`, NOT on `current != SRL` alone: an
                // active #1809 cross-cycle phantom override ALSO lands a non-SRL
                // state here, but with no productive output (`recovered=false`, the
                // very gate that produced the override), so it keeps its per-tick
                // dedup — clearing unconditionally would re-introduce the
                // #1808/#2086 WARN flood the latches exist to prevent.
                if self.recovered_within(SERVER_RATE_LIMIT_RECOVERY_SILENCE) {
                    self.last_srl_keep_latched_sig = None;
                    self.last_srl_phantom_warn_sig = None;
                }
            }
        }

        // #1562 self-capture instrument: pure-additive diagnostic. If a known
        // server-throttle phrase is on screen but the classifier did NOT land
        // on a retryable state, side-log the colored tail so the in-the-wild
        // miss can be diagnosed. Zero behavior change (runs AFTER classify,
        // never touches `self.current`/retry).
        let recent_tail = recent_screen_tail(screen_text, HARD_WRAP_TAIL_LINES);
        self.capture_unclassified_throttle(screen_text, fg, &recent_tail);

        // #1523 Phase 0: in-band turn-completion sentinel shadow telemetry.
        // Default-OFF (flag-gated); when on, side-logs this agent's token sighting
        // for measurement. Runs AFTER classify, never touches `self.current`.
        self.capture_turn_sentinel_shadow(screen_text, &recent_tail);

        // Instrumentation 2 — Sprint 27 shadow-mode behavioral telemetry.
        self.record_shadow_telemetry(silence_since_last_feed);

        // F9 (#685 sub-task 4): productive-output detection (zero behavior).
        self.detect_productive_output(&recent_tail);
    }

    /// Gate 1 — hash-dedup. Returns `true` when this frame is a duplicate
    /// that should skip the whole feed.
    ///
    /// #SRL-phase2 hash-dedup blind spot: a SETTLED (static) stuck-SRL pane
    /// has an UNCHANGED hash, so this dedup early-return would skip detection
    /// forever and the agent never recovers (no further feed ever re-runs the
    /// SRL gates). Override the skip ONLY when the pane carries a throttle hint
    /// AND we are not already latched on a throttle — then fall through so the
    /// hard-wrap fallback (None arm) can latch it → auto-retry fires. The cheap
    /// raw-token pre-filter keeps the common static-idle pane on the fast skip
    /// path.
    fn apply_hash_dedup_gate(&mut self, screen_text: &str, hash: u64) -> bool {
        if self.last_screen_hash == Some(hash) {
            let already_throttle = matches!(
                self.current,
                AgentState::ServerRateLimit | AgentState::RateLimit
            );
            if already_throttle || !screen_has_throttle_hint(screen_text) {
                return true;
            }
            // Re-detect the static throttle pane. Do NOT re-stamp the hash
            // (it's unchanged); once we latch, `already_throttle` short-circuits
            // subsequent identical frames.
            false
        } else {
            self.last_screen_hash = Some(hash);
            self.scan_context_pct(screen_text);
            false
        }
    }

    /// Instrumentation — Sprint 27 shadow-mode: log behavioral signal alongside
    /// regex state. Zero state change — telemetry only. Phase 2 (Sprint 28+)
    /// promotes behavioral to tiebreaker/primary.
    fn record_shadow_telemetry(&self, silence_since_last_feed: Duration) {
        if let Some(ref config) = self.behavioral_config {
            let signal = crate::behavioral::infer_from_silence(config, silence_since_last_feed);
            crate::behavioral::log_shadow_telemetry(
                &self.instance_name,
                &self.backend_name,
                self.current.display_name(),
                signal,
            );
            // Sprint 27 PR-B: accumulate divergence stats for dashboard
            crate::behavioral::record_divergence(
                &self.backend_name,
                signal,
                self.current.display_name(),
            );
        }
    }

    /// F9 (#685 sub-task 4): productive-output detection. Bumps
    /// `last_productive_output` only on a Productive signal. The bump affects
    /// nothing about Hung classification directly — the daemon supervisor reads
    /// `last_productive_output.elapsed()` and passes it to `check_hang` as the
    /// dual-path signal. Activation of the new classification branch is gated on
    /// `AGEND_PRODUCTIVE_GATE=1` in `check_hang` (shadow-mode default). See
    /// `docs/F9-PRODUCTIVE-OUTPUT-GATE.md` §F9.5.
    ///
    /// #685 PR-2 (reviewer #1009 / #1005 same-class flag): scan ONLY the recent
    /// tail (last MARKER_SCAN_TAIL_LINES rows) — historical completion markers
    /// visible in scrollback (e.g. `Saved to /tmp/foo.txt` left at row 5 from a
    /// 5-min-old write) MUST NOT keep refreshing `last_productive_output` on
    /// every cursor-blink tick.
    ///
    /// #685 PR-2 RC1 (reviewer #1013 verdict): dedup hash scope narrowed from
    /// "entire recent tail" → "matched marker substring". Pre-RC1 a stale marker
    /// that stayed visible in the tail while an adjacent spinner ticked produced
    /// a DIFFERENT tail hash on every tick — the dedup-hash never matched,
    /// `last_productive_output` re-fired despite no new productive evidence.
    /// Hashing the matched substring directly captures evidence identity, not
    /// surrounding-context noise.
    fn detect_productive_output(&mut self, recent_tail: &str) {
        if let Some(ref pconfig) = self.productivity_config {
            let heartbeat_age = self
                .last_heartbeat
                .map(|t| t.elapsed())
                .unwrap_or_else(|| Duration::from_secs(u32::MAX as u64));
            let recent_tail = tail_recent_lines(recent_tail, MARKER_SCAN_TAIL_LINES);
            let (signal, matched_substr) = crate::behavioral::infer_productivity_with_match(
                pconfig,
                &recent_tail,
                heartbeat_age,
            );
            match (&signal, matched_substr.as_deref()) {
                (
                    crate::behavioral::ProductivitySignal::Productive {
                        source: crate::behavioral::ProductivitySource::Marker(_),
                    },
                    Some(matched),
                ) => {
                    // Marker source: dedup against the matched substring text.
                    // Same substring across feeds = same evidence, suppress
                    // refresh — even when adjacent content (spinner ticks,
                    // status line edits) changes around it.
                    let marker_hash = hash_screen(matched);
                    if self.last_productive_marker_hash != Some(marker_hash) {
                        self.last_productive_output = Some(Instant::now());
                        self.last_productive_marker_hash = Some(marker_hash);
                    }
                }
                (
                    crate::behavioral::ProductivitySignal::Productive {
                        source: crate::behavioral::ProductivitySource::Heartbeat,
                    },
                    _,
                ) => {
                    // Heartbeat source is timestamp-driven — each fresh
                    // heartbeat IS new evidence. Always refresh; reset
                    // marker-hash so a subsequent Marker re-fires.
                    self.last_productive_output = Some(Instant::now());
                    self.last_productive_marker_hash = None;
                }
                _ => {
                    // No marker visible in the recent tail → clear the dedup
                    // hash so a fresh-after-silence marker re-fires the refresh.
                    self.last_productive_marker_hash = None;
                }
            }
            crate::behavioral::log_productivity_telemetry(
                &self.instance_name,
                &self.backend_name,
                self.current.display_name(),
                &signal,
            );
        }
    }

    /// Gate 2a — anchor. Returns `true` when the detection should be SUPPRESSED
    /// (treated as no-match). Two regimes (t-coloranchor-remove-ratelimit,
    /// operator-approved after the corpus gate's go/no-go):
    ///
    /// - **`requires_red_anchor`** {ContextFull, ModelUnsupported}: keep the
    ///   #1450 RED anchor — a marker needs ≥1 red rendered cell, else it's prose
    ///   not a state. ContextFull has no corpus to prove a content path;
    ///   ModelUnsupported never auto-clears AND suppresses hang-check, so a
    ///   verbatim-quote FP would silently disable a healthy agent (cost too high).
    /// - **content-anchor** {RateLimit, ServerRateLimit}: the marker must sit on
    ///   an error-line-shaped line (`in_error_line_excluding_input`) —
    ///   corpus-proven safe (5/5 prose suppressed via pattern-narrow + #1518
    ///   position + #1769 working-marker; FN-covered incl. kiro
    ///   `ThrottlingException` via #1789). NO red required, so a real fault
    ///   rendered in DEFAULT/grey (codex/gemini net errors) latches — the old
    ///   #1757 net-error red-exemption is now the GENERAL rule, not a special
    ///   case. The residual verbatim-quote FP (an agent pasting a real error line
    ///   into the live tail with no working-marker below) is ACCEPTED for these
    ///   two: both auto-clear and are retry-driven, so a one-off mis-latch
    ///   self-corrects (unlike ModelUnsupported).
    ///
    /// #1947: the content anchor additionally rejects matches sitting on the
    /// backend's INPUT line (or an echoed / submitted user-message line) —
    /// operator-typed / quoted error strings are prose, not CLI error output
    /// (operator-reproduced live FP, 2026-06-10).
    ///
    /// #1955 self-poisoning (UsageLimit arm): an agent QUOTING the banner
    /// ("You've hit your weekly limit" in an RCA / dispatch) latched itself.
    /// Input-line exclusion is the right-sized anchor here — the REAL banner
    /// carries no error indicator (`⎿ You've hit your weekly limit · resets 4am`),
    /// so the error-line content anchor would false-negative it, and its
    /// rendering isn't reliably red. Prose mentions in agent OUTPUT are bounded by
    /// the position gate + the #1777 working-marker override + the #1955 release
    /// anchor.
    fn apply_anchor_gate(
        &self,
        detected: AgentState,
        matched: &str,
        screen_text: &str,
        fg: &[CellFg],
        high_fp: bool,
        usage_limit: bool,
    ) -> bool {
        if requires_red_anchor(detected) {
            self.anchor_on_red && !fg.is_empty() && !matched_span_has_red(screen_text, matched, fg)
        } else if high_fp {
            !crate::state::patterns::in_error_line_excluding_input(
                screen_text,
                matched,
                self.input_line_markers,
            )
        } else if usage_limit {
            !crate::state::patterns::any_match_off_input_lines(
                screen_text,
                matched,
                self.input_line_markers,
            )
        } else {
            false
        }
    }

    /// Gate 2b — position. Returns `true` when a HIGH_FP / UsageLimit marker has
    /// scrolled out of the live bottom-N rows (e.g. an ApiError / ServerRateLimit
    /// line pushed up by the post-recovery `continue` output) and is therefore
    /// stale — the agent has moved on, so it must NOT keep re-firing the error
    /// transition (the level-triggered re-match that drove the retry storm).
    /// Scoped to HIGH_FP/error states ONLY: Idle and modal/interactive prompts
    /// keep full-screen scanning, because a modal can legitimately sit above the
    /// tail. #1955: UsageLimit joins the position gate — a banner quote buried in
    /// deep scrollback is discussion, not a live limit (same staleness logic).
    fn apply_position_gate(
        &self,
        matched: &str,
        screen_text: &str,
        high_fp: bool,
        usage_limit: bool,
    ) -> bool {
        (high_fp || usage_limit)
            && !matched_span_in_recent_tail(screen_text, matched, ERROR_TAIL_SCAN_LINES)
    }

    /// Suppression handler — a detection that failed the anchor/position gates is
    /// treated as no detection: log why (anchor → #1450 observability,
    /// otherwise → #1518 stale-position), then fall through to structural
    /// fallback (Starting → InteractivePrompt on a generic startup prompt) or
    /// latch maintenance.
    fn handle_suppressed_detection(
        &mut self,
        detected: AgentState,
        matched: &str,
        screen_text: &str,
        fg: &[CellFg],
        anchor_fail: bool,
    ) {
        if anchor_fail {
            self.log_anchor_suppress(detected, matched, screen_text, fg);
        } else {
            tracing::debug!(
                target: "state_detection",
                agent = %self.instance_name,
                state = ?detected,
                tail_rows = ERROR_TAIL_SCAN_LINES,
                "#1518: HIGH_FP marker scrolled out of the live tail — suppressing stale error transition"
            );
        }
        if matches!(self.current, AgentState::Starting) && is_generic_startup_prompt(screen_text) {
            self.transition(AgentState::InteractivePrompt);
        } else {
            self.maybe_expire_latched_state();
        }
    }

    /// No-raw-match handler (the `detect_with_match` → `None` arm).
    ///
    /// #SRL-phase2: hard-wrap fallback. Raw detection missed — which is exactly
    /// what an Ink-hard-wrapped SRL/RateLimit line looks like (the phrase split
    /// across rows by real `\n`). Retry on the flattened bottom-N tail before the
    /// structural/expire path. Land via the SAME recovery gate as the raw SRL
    /// path: recent productive output ⇒ recovered ⇒ Idle; else latch the throttle
    /// so auto-retry fires. (working_state_below can't be located in the wrapped
    /// raw text, so a hard-wrapped throttle with a working marker below is not
    /// overridden here — accepted; `recovered_within` still releases a
    /// genuinely-recovered pane.)
    /// #SRL-phase2 / #2089: rescue a hard-wrapped throttle error that the
    /// single-line `detect_with_match` missed (narrow-pane word-wrap). Runs the
    /// flattened-tail re-detect; if it finds a throttle, transitions to it (via
    /// the same recovered→Idle gate as the raw SRL path) and returns `true`.
    /// Called from BOTH the no-raw-match arm AND the benign-detection arm (#2089:
    /// a wrapped SRL co-existing with the idle `❯` prompt makes `detect_with_match`
    /// return `Idle`, so the `None` arm alone was unreachable).
    fn try_hard_wrap_throttle(
        &mut self,
        patterns: &'static StatePatterns,
        screen_text: &str,
    ) -> bool {
        let Some(throttle) =
            flattened_throttle_detect(patterns, screen_text, self.input_line_markers)
        else {
            return false;
        };
        let landed = if matches!(throttle, AgentState::ServerRateLimit)
            && self.recovered_within(SERVER_RATE_LIMIT_RECOVERY_SILENCE)
        {
            AgentState::Idle
        } else {
            throttle
        };
        let gated = self.gate_on_heartbeat(landed);
        self.transition(gated);
        true
    }

    /// #2090 P2: rescue a hard-wrapped UsageLimit banner that single-line
    /// `detect_with_match` missed (narrow-pane word-wrap landed the bottom `❯`
    /// idle prompt instead). Flatten-rematches with a {UsageLimit} accept-set and
    /// the `usagelimit_banner_adjacent` structural guard (box-draw + reset stamp),
    /// then transitions. UsageLimit is a hard quota (no recovered→Idle downgrade
    /// like SRL); `gate_on_heartbeat` is a no-op for it. Returns `true` if rescued.
    /// Sibling of [`try_hard_wrap_throttle`]; called from the SAME two arms.
    fn try_hard_wrap_usagelimit(
        &mut self,
        patterns: &'static StatePatterns,
        screen_text: &str,
    ) -> bool {
        let Some(state) = flattened_guarded_detect(
            patterns,
            screen_text,
            self.input_line_markers,
            HARD_WRAP_TAIL_LINES,
            |s| matches!(s, AgentState::UsageLimit),
            usagelimit_banner_adjacent,
        ) else {
            return false;
        };
        let gated = self.gate_on_heartbeat(state);
        self.transition(gated);
        true
    }

    fn handle_no_raw_match(&mut self, patterns: &'static StatePatterns, screen_text: &str) {
        // #2090: throttle rescue stays UNGATED here (THROTTLE_HINT_TOKENS omits the
        // net-error tokens, so hint-gating would drop a hard-wrapped ECONNRESET);
        // the UsageLimit rescue is gated on the wrap-surviving `resets` stamp (the
        // multi-word quota tokens split across narrow-pane rows) to bound the
        // flatten cost on no-match frames.
        if self.try_hard_wrap_throttle(patterns, screen_text)
            || (screen_text.contains("resets")
                && self.try_hard_wrap_usagelimit(patterns, screen_text))
        {
            // hard-wrapped throttle / usage-limit rescued + transitioned.
        } else if matches!(self.current, AgentState::Starting)
            && is_generic_startup_prompt(screen_text)
        {
            // Starting-only structural fallback: a generic prompt token (y/n,
            // press enter, …) at startup is almost certainly an operator dialog —
            // flag InteractivePrompt immediately instead of waiting on the
            // silence window.
            self.transition(AgentState::InteractivePrompt);
        } else {
            self.maybe_expire_latched_state();
        }
    }

    /// Landing gate 1 — working-marker override. Computes the INITIAL `landed`
    /// state for a non-suppressed HIGH_FP / UsageLimit detection.
    ///
    /// #1768: a HIGH_FP error wins the priority race even after it scrolled up
    /// and the agent RESUMED WORK below it (ServerRateLimit > Thinking by pattern
    /// order), so it keeps re-latching → `clears_server_rate_limit_retry`
    /// (Idle-only, #1713) never fires → supervisor re-injects `continue` into a
    /// working agent (the #1768 retry storm; the latched ServerRateLimit also
    /// doesn't auto-expire). If a Thinking/ToolUse marker is rendered BELOW the
    /// error, the agent recovered → land on that working state instead of the
    /// stale error. A genuinely-stuck error has NO in-flight working marker below
    /// it → `landed` stays `detected` → still latches → auto-retry fires.
    /// Detection-side only; `clears_server_rate_limit_retry` untouched → no #1713
    /// flicker-reset regression.
    ///
    /// #1777 (cheerc, "Sticky UsageLimit"): UsageLimit (prio 11) is the same kind
    /// of sticky error — it outranks Thinking/ToolUse and never auto-expires (no
    /// `maybe_expire_latched_state` arm), so a stale UsageLimit line keeps
    /// re-latching after the agent resumed work below it. Extend the same
    /// recovery override to it. UsageLimit stays OUT of `is_high_fp_state` → the
    /// #1450 red anchor is unchanged; this is the #1768 working-marker override
    /// only.
    ///
    /// #badge-recovery (state-level mirror of #1795): a ServerRateLimit whose
    /// agent produced PRODUCTIVE output within the recovery window has recovered
    /// even though the stale error still matches and `working_state_below` can't
    /// see a marker below the bottom-most (re-injected) error line. Land Idle
    /// instead of re-latching the badge. `recovered_within` is None-safe: a fresh
    /// / just-spawned agent (never produced) is NOT recovery → falls through to
    /// `detected` → latches + nudges normally. Scoped to ServerRateLimit (the
    /// #1795 storm's state); other HIGH_FP / UsageLimit keep re-latching as before.
    ///
    /// #1809-srl-swallow-probe (instrumentation-only, NO behavior change): when a
    /// ServerRateLimit is SWALLOWED (landed != SRL) it never latches → no
    /// auto-retry → the live stuck-agent bug. TWO gates can swallow it; probe
    /// BOTH, for ALL backends — the old #1808-flaw2-probe was scoped to Agy/Kiro
    /// and was therefore BLIND to the live claude SRL incident. Records the raw
    /// `recovered_within` bool + `productive_silent_secs` as INDEPENDENT fields;
    /// `dist_from_bottom` locates the matched error line. Does NOT alter `landed`.
    fn apply_working_marker_override(
        &mut self,
        patterns: &'static StatePatterns,
        detected: AgentState,
        matched: &str,
        screen_text: &str,
        high_fp: bool,
    ) -> AgentState {
        if high_fp || matches!(detected, AgentState::UsageLimit) {
            let is_srl = matches!(detected, AgentState::ServerRateLimit);
            let recovered = is_srl && self.recovered_within(SERVER_RATE_LIMIT_RECOVERY_SILENCE);
            let fallback = if recovered {
                AgentState::Idle
            } else {
                detected
            };
            let working_below = patterns.working_state_below(screen_text, matched);
            if is_srl {
                let productive_silent_secs = self.productive_silence().as_secs();
                let srl_sig = srl_match_signature(screen_text, matched);
                let dist_from_bottom = srl_sig.1;
                match &working_below {
                    // Path A: a working marker renders BELOW the error.
                    Some((win_state, marker)) => {
                        if recovered {
                            // Recent productive output → genuine recovery; the
                            // #1769 working_state_below override lands the working
                            // state (SRL was stale).
                            tracing::info!(
                                target: "state_detection",
                                agent = %self.instance_name,
                                tag = "#1809-srl-swallow-probe",
                                path = "working_state_below",
                                backend = %self.backend_name,
                                working_marker = %marker,
                                landed_state = ?win_state,
                                recovered_within = recovered,
                                productive_silent_secs,
                                dist_from_bottom,
                                "ServerRateLimit yields to working_state_below override (Path A) with recent productive output — genuine recovery"
                            );
                        } else {
                            // #2086 fix: NO recent productive output → the working
                            // marker below the error is claude's STUCK
                            // rate-limited retry spinner ("· Stewing…"), NOT
                            // recovery. Do NOT let it swallow the SRL — keep it
                            // latched so the supervisor's SRL backoff retry fires.
                            // This promotes the #1809-srl-swallow-probe's
                            // `recovered_within` discriminator from log to decision
                            // (the WARN here used to fire on the silent swallow that
                            // stranded the agent for ~26 min, #2086).
                            //
                            // #2086 dedup: a stuck SRL keeps a spinner ticking
                            // below it that flips the screen hash every feed, so
                            // this WARN would re-fire every tick for the whole
                            // stuck duration (the #1450 flood class). Emit only on
                            // a distinct stuck-error signature. Behavior (keeping
                            // the SRL latched, below) is NOT gated by this.
                            if self.last_srl_keep_latched_sig != Some(srl_sig.0) {
                                self.last_srl_keep_latched_sig = Some(srl_sig.0);
                                tracing::warn!(
                                    target: "state_detection",
                                    agent = %self.instance_name,
                                    tag = "#2086-srl-keep-latched",
                                    path = "working_state_below",
                                    backend = %self.backend_name,
                                    working_marker = %marker,
                                    masked_working_state = ?win_state,
                                    recovered_within = recovered,
                                    productive_silent_secs,
                                    dist_from_bottom,
                                    "ServerRateLimit KEPT latched — a working marker below the error with NO recent productive output is a stuck-retry spinner, not recovery (was swallowed pre-#2086)"
                                );
                            }
                        }
                    }
                    // Path B: NO working marker below, but the
                    // `recovered_within`→Idle fallback swallows the SRL. Only
                    // reachable when `recovered` is true (that IS the gate).
                    None if recovered => {
                        tracing::info!(
                            target: "state_detection",
                            agent = %self.instance_name,
                            tag = "#1809-srl-swallow-probe",
                            path = "recovered_within_idle",
                            backend = %self.backend_name,
                            recovered_within = recovered,
                            productive_silent_secs,
                            dist_from_bottom,
                            "ServerRateLimit yields to recovered_within→Idle fallback (Path B) — no working marker below"
                        );
                    }
                    // No working marker + NOT recovered → SRL latches normally
                    // (auto-retry fires); not a swallow, no probe.
                    None => {}
                }
            }
            // #2086: for a non-recovered SRL, a working marker below the error is
            // a stuck-retry spinner — keep the SRL (do NOT swallow). Every other
            // case is unchanged: recovered SRL / non-SRL HIGH_FP / UsageLimit all
            // honor the working_state_below override as before.
            if is_srl && !recovered && working_below.is_some() {
                detected
            } else {
                working_below.map(|(s, _)| s).unwrap_or(fallback)
            }
        } else {
            detected
        }
    }

    /// Landing gate 2 — SRL phantom re-match (#1808-probe0-phantom + #1809 fix).
    ///
    /// cheerc Evidence 2 — a stale SRL error stuck in the bottom-N tail keeps
    /// re-matching after the agent recovered (the screen hash flips when the CLI
    /// clock ticks → feed re-scans → re-grabs the SAME old error → re-latch →
    /// blind inject). Signature the matched error line `(line_hash,
    /// dist_from_bottom)` and compare to the previous SRL detection —
    ///   • same sig, NO intervening non-SRL state  → in-place clock-tick re-scan
    ///     → `srl_consecutive_rematch++`;
    ///   • same sig, intervening Idle/non-SRL state → `cross_cycle` refire (the
    ///     agent recovered, then the OLD error was re-grabbed) = cheerc's exact
    ///     cross-Idle loop.
    /// `last_srl_match_sig` PERSISTS across Idle so the cross-cycle case survives.
    /// WARN when the SRL would win (`landed == SRL` → latch → inject) AND no
    /// recent productive output (`!recovered`) AND it is a re-match. The #1809
    /// fix then OVERRIDES `landed` to Idle for the `cross_cycle` sub-case only
    /// (the unambiguous phantom) — the `consecutive_rematch` (still-SRL) case
    /// stays telemetry-only so a genuine long throttle keeps retrying. Not
    /// backend-scoped (the phantom is the Claude `Server is temporarily limiting`
    /// re-match — cheerc's `general` agent).
    fn apply_srl_phantom_gate(
        &mut self,
        detected: AgentState,
        matched: &str,
        screen_text: &str,
        mut landed: AgentState,
    ) -> AgentState {
        if matches!(detected, AgentState::ServerRateLimit) {
            let sig = srl_match_signature(screen_text, matched);
            let same_sig = self.last_srl_match_sig == Some(sig);
            let cross_cycle = same_sig && self.non_srl_since_last_srl;
            self.srl_consecutive_rematch = if same_sig && !self.non_srl_since_last_srl {
                self.srl_consecutive_rematch.saturating_add(1)
            } else {
                0
            };
            self.last_srl_match_sig = Some(sig);
            self.non_srl_since_last_srl = false;

            let would_latch = matches!(landed, AgentState::ServerRateLimit);
            let recovered_now = self.recovered_within(SERVER_RATE_LIMIT_RECOVERY_SILENCE);
            if would_latch && !recovered_now && (self.srl_consecutive_rematch > 0 || cross_cycle) {
                // #1808 dedup: an in-place static SRL keeps the same `sig` while a
                // clock-tick redraw flips the screen hash → this WARN would re-fire
                // every feed for the whole throttle (per-tick flood). Emit only on
                // a signature transition. The #1809 cross-cycle→Idle behavioral fix
                // below is intentionally OUTSIDE this dedup.
                if self.last_srl_phantom_warn_sig != Some((sig.0, cross_cycle)) {
                    self.last_srl_phantom_warn_sig = Some((sig.0, cross_cycle));
                    let kind = if cross_cycle {
                        "cross_cycle_refire"
                    } else {
                        "consecutive_rematch"
                    };
                    tracing::warn!(
                        target: "state_detection",
                        agent = %self.instance_name,
                        tag = "#1808-probe0-phantom",
                        kind,
                        consecutive_rematch = self.srl_consecutive_rematch,
                        cross_cycle_refire = cross_cycle,
                        dist_from_bottom = sig.1,
                        productive_silent_secs = self.productive_silence().as_secs(),
                        "phantom re-match: same stale ServerRateLimit error re-detected (would latch → inject) with no recent productive output"
                    );
                }
                // #1809 fix (behavioral): a CROSS-CYCLE phantom — the agent
                // already LEFT ServerRateLimit (passed through a non-SRL landed
                // state) and the SAME stale error line (`same_sig`) was re-grabbed
                // with no recent productive output. This is cheerc's exact loop: a
                // CLI clock-tick flips the screen hash → feed re-scans → re-matches
                // the OLD error → re-latch → the supervisor schedules ANOTHER
                // auto-retry → blind `continue` inject, repeating every ~45s as the
                // recovery window expires. Land Idle instead of re-latching: the
                // genuine error already latched + retried on its FIRST detection; a
                // genuinely-new error has a different signature; genuine productive
                // output sets `recovered_now` (→ #badge-recovery lands Idle anyway,
                // never reaching here). The IN-PLACE `consecutive_rematch` case is
                // deliberately NOT suppressed — a still-SRL agent may be a genuine
                // long throttle that still needs its retry. (Accepted narrow FP: a
                // genuinely-new SRL identical in text AND screen position to the
                // just-processed one, with no intervening productive output, is
                // ignored — see PR body.)
                if cross_cycle {
                    landed = AgentState::Idle;
                }
            }
        }
        landed
    }

    /// Landing gate 3 — UsageLimit episode lifecycle (#1955). The match is
    /// level-triggered and a silent pane never scrolls the banner away (the
    /// `general` incident: stuck for DAYS past the account reset), so the state
    /// machine — not new output — must provide the exit:
    ///  • fresh latch → anchor a release deadline on the banner's own unlock hint
    ///    (conservative fallback);
    ///  • in-episode re-match past the deadline → release to Idle and remember
    ///    the banner's signature;
    ///  • re-match of that SAME released signature → suppress (mirror of the
    ///    #1809 cross-cycle suppression) — a genuinely-new limit renders fresh at
    ///    the bottom → new sig → latches again.
    fn apply_usage_limit_lifecycle_gate(
        &mut self,
        detected: AgentState,
        matched: &str,
        screen_text: &str,
        mut landed: AgentState,
    ) -> AgentState {
        if matches!(detected, AgentState::UsageLimit) && matches!(landed, AgentState::UsageLimit) {
            let sig = srl_match_signature(screen_text, matched);
            if self.usage_limit_expired_sig == Some(sig) {
                landed = AgentState::Idle;
            } else if self.current == AgentState::UsageLimit {
                if self
                    .usage_limit_release_at
                    .is_some_and(|at| Instant::now() >= at)
                {
                    self.usage_limit_expired_sig = Some(sig);
                    self.usage_limit_release_at = None;
                    tracing::info!(
                        target: "state_detection",
                        agent = %self.instance_name,
                        dist_from_bottom = sig.1,
                        "#1955: UsageLimit released — unlock anchor passed; suppressing the stale banner re-latch"
                    );
                    landed = AgentState::Idle;
                }
            } else {
                let release_in = line_containing(screen_text, matched)
                    .and_then(parse_usage_limit_release)
                    .unwrap_or(Self::USAGE_LIMIT_EXPIRY);
                self.usage_limit_release_at = Some(Instant::now() + release_in);
                self.usage_limit_expired_sig = None;
                tracing::info!(
                    target: "state_detection",
                    agent = %self.instance_name,
                    release_in_secs = release_in.as_secs(),
                    "#1955: UsageLimit latched — release anchored on the banner's unlock hint (or conservative fallback)"
                );
            }
        }
        landed
    }

    /// #1450 observability: a HIGH_FP pattern matched but no on-screen
    /// occurrence of the phrase rendered red, so the transition is
    /// suppressed. Logged at WARN (was `debug!` under #919 — invisible in
    /// production, which is why the original break went undiagnosed) with
    /// the actual per-cell foreground of the first occurrence's span. That
    /// lets a future incident distinguish "real red mis-classified" (tune
    /// the predicate) from "genuinely not red" (correct suppression)
    /// straight from the logs — no DEBUG rebuild.
    fn log_anchor_suppress(
        &mut self,
        detected: AgentState,
        matched: &str,
        screen_text: &str,
        fg: &[CellFg],
    ) {
        let line_context = screen_text
            .lines()
            .find(|l| l.contains(matched))
            .unwrap_or(matched);
        // #1450: dedup. The gate is level-triggered (re-runs every feed), so a
        // static on-screen phrase that never renders red would otherwise re-log
        // this WARN on every tick. Emit only when the suppressed
        // (state, matched, line) tuple differs from the last one logged.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        format!("{detected:?}\u{1}{matched}\u{1}{line_context}").hash(&mut hasher);
        let suppress_hash = hasher.finish();
        if self.last_anchor_suppress_hash == Some(suppress_hash) {
            return;
        }
        self.last_anchor_suppress_hash = Some(suppress_hash);
        let span_fg: Vec<CellFg> = first_occurrence_span(screen_text, matched, fg);
        // #1809-trivial: `instance_name` can be empty (tracker built before
        // `set_instance_name`), which rendered a bare `agent=`. Show a placeholder
        // so the log line is greppable by who-is-this.
        let agent_label: &str = if self.instance_name.is_empty() {
            "<unset>"
        } else {
            self.instance_name.as_str()
        };
        tracing::warn!(
            agent = %agent_label,
            backend = %self.backend_name,
            state = ?detected,
            matched = %matched,
            span_fg = ?span_fg,
            line_context = %line_context,
            // #1809-trivial: wording was stale. Post-#1790 the anchor gate has two
            // regimes — a RED-SGR anchor (ContextFull / ModelUnsupported) and a
            // content/error-line anchor (`in_error_line`, for RateLimit /
            // ServerRateLimit). So a suppression here is NOT necessarily "not red";
            // it's "the active anchor predicate did not hold".
            "#1450: HIGH_FP pattern matched but the anchor gate did not hold \
             (red-SGR anchor for ContextFull/ModelUnsupported, or content/error-line \
             anchor for RateLimit/ServerRateLimit) — suppressing transition. If this \
             was a real backend error, span_fg shows the actual colors (predicate may \
             be too strict)."
        );
    }

    /// #1562 self-capture instrument — a pure-additive diagnostic side-log.
    ///
    /// When a known server-throttle / transient-error phrase
    /// ([`THROTTLE_DIAG_PHRASES`]) is visible in the live tail but the
    /// classifier did NOT land on a retryable state
    /// ([`is_throttle_retryable_state`]), append one JSONL record to
    /// `<agend-home>/unclassified_errors.jsonl`:
    /// `{ts, backend, classified_state, raw_tail}`, where `raw_tail` carries
    /// ANSI color (reconstructed from `fg`) so the color-anchor hypothesis
    /// (#1562: did the throttle line render red?) can be checked from the log.
    ///
    /// Invariants:
    /// - **Zero behavior change** — runs after classify, never touches
    ///   `self.current`, retry, or any timer; failures are swallowed.
    /// - **Cheap** — fast-rejects on a `str::contains` scan (no allocation) when
    ///   no throttle phrase is present, which is the overwhelming common case.
    /// - **Low-noise** — fires only on phrase-present + classified-non-retryable
    ///   + phrase-in-live-tail (a scrolled-up scrollback echo is ignored).
    fn capture_unclassified_throttle(&mut self, screen_text: &str, fg: &[CellFg], tail: &str) {
        let Some((raw_tail, wrap_split)) =
            unclassified_throttle_tail(self.current, screen_text, fg, tail)
        else {
            // Left the throttle-miss shape — drop the latch so a later recurrence
            // of the same screen logs once again (#2100/#2115).
            self.last_unclassified_throttle_sig = None;
            return;
        };
        // #2100/#2115 fire-once latch: `apply_hash_dedup_gate` bypasses the
        // feed-level screen hash-dedup for throttle-hint screens, so a STATIC
        // unclassified-throttle pane would otherwise append a record (and re-fire
        // the WARN below) on every PTY read. Key on the colored tail — stable
        // across the cursor/clock re-renders that flip the full-screen hash.
        let sig = hash_screen(&raw_tail);
        if self.last_unclassified_throttle_sig == Some(sig) {
            return;
        }
        self.last_unclassified_throttle_sig = Some(sig);
        // #1808 Phase-1 upstream instrument: a server-throttle phrase is on a
        // LIVE non-retryable screen — the detection miss that left agents stuck.
        // When `wrap_split` (phrase matched only after whitespace-flatten), the
        // de-wrap of soft-wraps did NOT merge it → the wrap is a hard `\n` (Ink
        // layout) → WARN with the FULL escaped screen_text so the next real SRL
        // event is captured verbatim and we can confirm soft-vs-hard wrap before
        // building the Phase 2 fallback. Contiguous misses (wrap_split=false) are
        // the already-understood prose-FP class (correctly suppressed by the
        // line-scoped content anchor) → kept to the JSONL sidecar only, no WARN
        // noise. Both this WARN and the append are bounded to once per distinct
        // screen by the `last_unclassified_throttle_sig` latch above — the
        // feed-level hash-dedup does NOT bound it (it is bypassed for throttle
        // screens, and a spinner/clock tick would churn the screen hash; #2115).
        if wrap_split {
            let escaped = screen_text.escape_debug().to_string();
            tracing::warn!(
                target: "state_detection",
                agent = %self.instance_name,
                backend = %self.backend_name,
                tag = "#1808-srl-detect-miss",
                classified_state = %self.current.display_name(),
                wrap_split,
                screen_text = %escaped,
                "SRL/throttle phrase present only when whitespace-flattened (hard `\\n` line-wrap) but classifier did NOT latch a retryable state — capturing full screen_text for soft-vs-hard wrap diagnosis (#1808 Phase 2 signal)"
            );
        }
        let record = serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "backend": self.backend_name,
            "classified_state": self.current.display_name(),
            "wrap_split": wrap_split,
            "raw_tail": raw_tail,
            "screen_text": screen_text,
        });
        let path = crate::home_dir().join("unclassified_errors.jsonl");
        if let Err(e) = append_jsonl(&path, &record) {
            // Diagnostic must never affect behavior — log and move on.
            tracing::debug!(
                target: "state_detection",
                agent = %self.instance_name,
                error = %e,
                "#1562: failed to append unclassified-throttle diagnostic"
            );
        }
    }

    /// #1523 Phase 0: turn-completion sentinel shadow telemetry.
    ///
    /// When `AGEND_TURN_SENTINEL_SHADOW=1`, hook-less agents are instructed to
    /// print `<<<AGEND-DONE:{nonce}>>>` as the final line when a turn finishes
    /// (see [`turn_sentinel_token`]). This side-logs whether THIS agent's token
    /// is on screen alongside the heuristic classification, so emit-rate /
    /// false-emit (instruction-echo / source-view) / leak can be measured before
    /// any production reliance in Phase 1. The daemon takes NO action on the
    /// signal in Phase 0 — this only ever ADDS corroboration, never subtracts.
    ///
    /// Invariants (mirror the retired `capture_hardwrap_miss_shadow`):
    /// - **Zero behaviour change** — runs after classify, never touches
    ///   `self.current`, retry, or any timer; failures are swallowed. Gated OFF
    ///   by default (flag absent → early return → byte-identical classification).
    /// - **Cheap** — fast-rejects on a `str::contains` for the fixed prefix
    ///   before building the per-agent token or allocating a tail.
    /// - **Fire-once** — a static token-bearing frame logs once via the
    ///   `last_turn_sentinel_sig` latch (the feed-level hash-dedup already drops
    ///   unchanged frames; this guards token frames whose hash churns).
    fn capture_turn_sentinel_shadow(&mut self, screen_text: &str, tail: &str) {
        if !turn_sentinel_shadow_enabled() || self.instance_name.is_empty() {
            return;
        }
        // Cheap fast-path: the marker prefix is fixed; bail before building the
        // per-agent token or allocating a tail if it is nowhere on screen.
        if !screen_text.contains(TURN_SENTINEL_PREFIX) {
            self.last_turn_sentinel_sig = None;
            return;
        }
        let token = turn_sentinel_token(&self.instance_name);
        let obs = observe_turn_sentinel(tail, &token);
        if !obs.token_seen {
            // Some OTHER agent's token (or a malformed marker) is on screen — not
            // ours. Drop the latch and skip (never attribute another agent's emit).
            self.last_turn_sentinel_sig = None;
            return;
        }
        // Consistency: a real emission means the agent just finished → the
        // heuristic should independently read Idle. Disagreement is the
        // corroboration value we want to measure.
        let consistent = matches!(self.current, AgentState::Idle);
        // Fire-once latch keyed on the token-bearing tail + derived flags so a
        // static pane logs once, not every churned-hash frame.
        let mut h = std::collections::hash_map::DefaultHasher::new();
        tail.hash(&mut h);
        obs.on_last_line.hash(&mut h);
        obs.suspected_echo.hash(&mut h);
        let sig = h.finish();
        if self.last_turn_sentinel_sig == Some(sig) {
            return;
        }
        self.last_turn_sentinel_sig = Some(sig);
        let record = serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "backend": self.backend_name,
            "agent": self.instance_name,
            "token_seen": obs.token_seen,
            "on_last_line": obs.on_last_line,
            "existing_state": self.current.display_name(),
            "consistent": consistent,
            "suspected_echo": obs.suspected_echo,
            "leak_signal": obs.leak_signal,
        });
        let path = crate::home_dir().join("turn_sentinel_shadow.jsonl");
        if let Err(e) = append_jsonl(&path, &record) {
            // Diagnostic must never affect behavior — log and move on.
            tracing::debug!(
                target: "state_detection",
                agent = %self.instance_name,
                error = %e,
                "#1523 Phase 0 shadow: failed to append turn-sentinel diagnostic"
            );
        }
    }

    /// Fallback when the screen changed but no pattern matched.
    ///
    /// Active-state markers (Thinking "esc to cancel", ToolUse tool banners)
    /// can stop rendering while the CLI still shows on-screen content that
    /// happens not to match the backend's Idle pattern either — e.g. a
    /// mid-scroll render between the spinner clearing and the prompt
    /// re-appearing. Without a fallback the tracker would stay latched on
    /// the prior active state indefinitely.
    ///
    /// If the current state is a self-expiring active state
    /// (Thinking / ToolUse) and it has been held longer than
    /// `LATCHED_STATE_EXPIRY`, drop to Idle. Everything else is excluded:
    /// InteractivePrompt / PermissionPrompt need explicit operator action,
    /// errors transition instantly on the next matching screen, and
    /// Starting / AwaitingOperator / Hang are driven by their own
    /// supervisors (see `daemon::supervisor`).
    fn maybe_expire_latched_state(&mut self) {
        // F39: scrollback re-detection (Scenarios A/B) preserved by
        // transition() same-state early-return + feed() hash-dedup; Scenario C
        // priority oscillation between Thinking and other states resets `since`
        // per bounce and is the unaddressed bug surface. See
        // docs/HUNG-STATE-TRANSITIONS.md §F39.3.
        // Active states (Thinking / ToolUse) expire on the short window —
        // their trigger patterns (spinners, tool-call banners) commonly
        // stop rendering mid-operation even when the agent is still
        // working, so a brief latch is fine but holding beyond
        // LATCHED_STATE_EXPIRY is almost always stale.
        let short_expiring = matches!(self.current, AgentState::Thinking | AgentState::ToolUse);
        if short_expiring && self.since.elapsed() >= Self::LATCHED_STATE_EXPIRY {
            self.transition(AgentState::Idle);
            return;
        }
        // RateLimit expires on its own 5-minute window. Real rate limits
        // clear in seconds-to-minutes; stuck for hours is a false positive.
        let rate_limit_expiring = matches!(self.current, AgentState::RateLimit);
        if rate_limit_expiring && self.since.elapsed() >= Self::RATE_LIMIT_EXPIRY {
            self.transition(AgentState::Idle);
            return;
        }
        // #1955: UsageLimit expires on its release deadline (anchored on the
        // banner's own unlock hint at latch time, else the conservative
        // window). This arm covers the banner-scrolled-away case (detection
        // returns None); a still-visible banner releases at the detection
        // override instead (the level-triggered re-match never reaches here).
        // A pre-#1955 latch carries no deadline → conservative window from
        // `since`.
        if matches!(self.current, AgentState::UsageLimit) {
            let deadline_passed = self
                .usage_limit_release_at
                .map_or(self.since.elapsed() >= Self::USAGE_LIMIT_EXPIRY, |at| {
                    Instant::now() >= at
                });
            if deadline_passed {
                self.usage_limit_release_at = None;
                self.transition(AgentState::Idle);
                return;
            }
        }
        // Prompt states (InteractivePrompt / PermissionPrompt) expire on
        // the longer window. When the screen goes stable after the
        // operator dismisses the dialog, feed()'s hash-dedup skips
        // `detect()` and the state never re-evaluates — which is how
        // `dev-reviewer` stayed flagged as "卡在互動 prompt" long after
        // the prompt was gone. The 2-minute bound gives a real operator
        // reaction window while still guaranteeing self-recovery.
        let long_expiring = matches!(
            self.current,
            AgentState::InteractivePrompt | AgentState::PermissionPrompt
        );
        if long_expiring && self.since.elapsed() >= Self::INTERACTIVE_EXPIRY {
            self.transition(AgentState::Idle);
        }
    }

    /// Get current state.
    pub fn get_state(&self) -> AgentState {
        self.current
    }

    /// Time since the agent last produced PRODUCTIVE output, for the silence-based
    /// readers (`check_hang` `silent_productive`, the recovery dispatcher, the
    /// snapshot `silent_secs`). When the agent has not produced yet
    /// (`last_productive_output == None`), the baseline is `created_at` — i.e. it
    /// has been productive-silent since it started. This preserves the pre-Option
    /// behavior exactly (the field used to be stamped `now()` at creation).
    /// #1961 phase-2: seconds since the pane CONTENT last changed (raw screen
    /// hash delta — `last_output` is bumped only when `feed`'s screen hash
    /// differs, so cursor-blink/no-op chatter does NOT reset it, but token
    /// streaming / spinner frames / tool output DO). Classification-free: this
    /// is the activity signal that survives a state-detector mis-read (the
    /// #1961 false-fire put a code-writing agent at agent_state=idle).
    pub fn output_silence(&self) -> Duration {
        self.last_output.elapsed()
    }

    pub fn productive_silence(&self) -> Duration {
        self.last_productive_output
            .unwrap_or(self.created_at)
            .elapsed()
    }

    /// Has the agent produced PRODUCTIVE output within `window`? Used by the
    /// recovery gates (#1795 retry inject, #badge re-latch). `None` (never
    /// produced) is NOT recovery — a creation stamp must never be misread as
    /// recovery, and a just-spawned agent that immediately errors must latch +
    /// nudge normally. Only a real, recent productive signal counts.
    pub fn recovered_within(&self, window: Duration) -> bool {
        self.last_productive_output
            .is_some_and(|t| t.elapsed() < window)
    }

    pub fn has_productive_output(&self) -> bool {
        self.last_productive_output.is_some()
    }

    /// Periodic tick — expire stale latched states without requiring new PTY
    /// output. Called from supervisor and app mode tick loops so idle agents
    /// don't stay stuck on ToolUse/Thinking indefinitely.
    pub(crate) fn tick(&mut self) {
        self.maybe_expire_latched_state();
    }

    /// Force state to Restarting (called by reaper on crash).
    pub fn set_restarting(&mut self) {
        self.record_set(AgentState::Restarting); // #1527: also logs the transition
    }

    /// Force state to AwaitingOperator when the agent is stalled waiting on
    /// operator input. Fires from `Starting` (the original startup-stall
    /// fallback) or, post-#1552, from a runtime `PermissionPrompt` /
    /// `InteractivePrompt` (a mid-task approval stall). Other states are left
    /// untouched so a late tick-loop detection can't corrupt a healthy
    /// mid-task agent — the WHEN-to-escalate gating (silence threshold +
    /// position / stability / engagement FP-gates) lives in the supervisor;
    /// this setter just guards the legal source states.
    ///
    /// Once the operator unblocks the stall and the ready pattern matches
    /// fresh screen content, `transition()` lifts the state (Idle prio >
    /// AwaitingOperator prio → higher always wins).
    pub fn set_awaiting_operator(&mut self) {
        if matches!(
            self.current,
            AgentState::Starting | AgentState::PermissionPrompt | AgentState::InteractivePrompt
        ) {
            self.record_set(AgentState::AwaitingOperator); // #1527: also logs
        }
    }

    /// #1527: the SINGLE funnel for every `current` mutation. Records the
    /// transition (bounded, drop-oldest) and updates `current` + `since`. All
    /// five production mutation sites route through this — `transition()`'s
    /// three assignment branches plus the two that bypass `transition()`
    /// entirely (`set_restarting` / `set_awaiting_operator`) — so EVERY state
    /// change is captured at its true source, regardless of which thread
    /// (read-loop `feed` or supervisor `tick`) drives it. Callers must NOT also
    /// assign `current`/`since` (record_set owns both — avoids double-update).
    /// No-op on same-state (won't reset `since` or push a spurious record).
    fn record_set(&mut self, new_state: AgentState) {
        if new_state == self.current {
            return;
        }
        if self.pending_transitions.len() >= Self::PENDING_TRANSITIONS_CAP {
            self.pending_transitions.remove(0); // drop oldest
            self.dropped_transition_count = self.dropped_transition_count.saturating_add(1);
        }
        self.pending_transitions.push(TransitionRecord {
            from: self.current,
            to: new_state,
            ts: chrono::Utc::now().to_rfc3339(),
        });
        // #2033: blocked-episode tracking for the recovery-notice gate. record_set
        // is the SINGLE funnel for every `current` mutation (incl. the
        // `set_awaiting_operator` bypass), so entry/exit are caught here regardless
        // of which path drove the transition.
        let from = self.current;
        if !from.wants_raw_keystrokes() && new_state.wants_raw_keystrokes() {
            // entering a fresh blocked episode
            self.blocked_since = Some(Instant::now());
            self.blocked_notice_sent = false;
        } else if from.wants_raw_keystrokes() && !new_state.wants_raw_keystrokes() {
            // leaving — snapshot the gate inputs for the paired recovery notice
            self.recovery_episode = Some(RecoveryEpisode {
                block_duration: self.blocked_since.map(|t| t.elapsed()).unwrap_or_default(),
                notice_sent: self.blocked_notice_sent,
            });
            self.blocked_since = None;
            self.blocked_notice_sent = false;
        }
        self.current = new_state;
        self.since = Instant::now();
    }

    /// #1527: drain the buffered transitions (FIFO) for the supervisor to log.
    /// Returns the records in occurrence order plus the count dropped to the
    /// cap since the last drain (nonzero ⇒ the drainer fell behind). Clears
    /// both. The supervisor logs each record AFTER dropping the core lock
    /// (file append, no self-IPC → no #1492).
    pub(crate) fn drain_pending_transitions(&mut self) -> (Vec<TransitionRecord>, u64) {
        let dropped = self.dropped_transition_count;
        self.dropped_transition_count = 0;
        (std::mem::take(&mut self.pending_transitions), dropped)
    }

    fn transition(&mut self, new_state: AgentState) {
        if new_state == self.current {
            return;
        }

        let prev = self.current;

        // Error states: instant transition (no hysteresis)
        if new_state.is_error() {
            self.record_set(new_state); // #1527: records + sets current/since
        } else {
            // Higher priority than current: transition if current held > min duration
            let held = self.since.elapsed();
            let min_hold = if self.current.priority() <= AgentState::Idle.priority() {
                Duration::from_secs(5) // Passive states: 5s
            } else {
                Duration::from_secs(2) // Active states: 2s
            };

            // Higher priority always transitions
            if new_state.priority() > self.current.priority() {
                // #1005 Phase A2: oscillation guard. Suppress priority-up
                // back into the SAME self-expiring latched state we just
                // left briefly. This is the
                // `ToolUse(2s) → Idle(2s) → ToolUse(2s)` bounce that
                // keeps `since` recent and blocks `LATCHED_STATE_EXPIRY`
                // (30s) from firing (Scenario C of §F39). Scoped to
                // {Thinking, ToolUse} — the exact set
                // `maybe_expire_latched_state` targets. Operator-driven
                // dialogs (InteractivePrompt / PermissionPrompt) and
                // error states are deliberately OUT of scope: those
                // have legitimate re-entry semantics (operator dismiss
                // then re-prompt) and their own recovery paths.
                let now = Instant::now();
                let guard_applies = matches!(new_state, AgentState::Thinking | AgentState::ToolUse);
                if guard_applies {
                    if let Some((prev_target, prev_at)) = self.last_priority_up_into {
                        let bouncing_to_same = prev_target == new_state;
                        let within_window = now
                            .checked_duration_since(prev_at)
                            .is_some_and(|d| d < oscillation_guard_window());
                        let lower_held_briefly = held < Self::OSCILLATION_LOWER_HOLD_THRESHOLD;
                        if bouncing_to_same && within_window && lower_held_briefly {
                            tracing::debug!(
                                target: "oscillation_guard",
                                agent = %self.instance_name,
                                backend = %self.backend_name,
                                state = ?new_state,
                                lower_held_ms = held.as_millis() as u64,
                                window_age_ms = now
                                    .duration_since(prev_at)
                                    .as_millis() as u64,
                                "#1005 priority-up suppressed: bounce pattern detected"
                            );
                            // Stay in current lower state. Do NOT update
                            // `last_priority_up_into` — the entry that
                            // armed the guard is still the canonical
                            // record for this window.
                            return;
                        }
                    }
                    self.last_priority_up_into = Some((new_state, now));
                }
                self.record_set(new_state); // #1527
            } else if held >= min_hold {
                // Lower priority only after min hold
                self.record_set(new_state); // #1527
            }
        }

        // Arm a one-shot Telegram notice whenever we actually entered
        // InteractivePrompt on this call. Gated by `prev != current` so the
        // no-op path (rejected by hysteresis) doesn't arm.
        if self.current == AgentState::InteractivePrompt && prev != AgentState::InteractivePrompt {
            self.interactive_prompt_pending_notice = true;
        }

        // Symmetric recovery notice: whenever we leave a blocked state
        // (InteractivePrompt / AwaitingOperator) for a non-blocked state,
        // arm a one-shot "ready again" notice. Also gated on the actual
        // transition so hysteresis-rejected calls don't arm.
        if prev.wants_raw_keystrokes()
            && !self.current.wants_raw_keystrokes()
            && prev != self.current
        {
            self.interactive_recovery_pending_notice = true;
        }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
mod review_repro_state_capture;
