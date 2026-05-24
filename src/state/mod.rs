//! Agent state detection via PTY output pattern matching.
//!
//! Detection runs against the current **vterm screen text** (caller supplies
//! it via `feed()`), not an accumulated byte buffer. Pattern hits therefore
//! reflect what the user would currently see on screen, so dismissing an
//! interactive prompt (e.g. codex update menu) drops the matching text from
//! the grid and the next `feed()` re-evaluates to the underlying Ready state
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
use std::collections::VecDeque;
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
    Ready,
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
            Self::Ready => 3,
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
            Self::Ready => "ready",
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
            Self::AuthError => "auth_error",
            Self::ApiError => "api_error",
            Self::Crashed => "crashed",
            Self::Restarting => "restarting",
        }
    }
}

pub(crate) mod anchor;
pub(crate) mod patterns;

pub(crate) use anchor::{has_red_ansi_anchor, RawChunk};
use anchor::{RAW_CHUNK_MAX, RAW_RING_CHUNKS};
use patterns::is_generic_startup_prompt;
pub use patterns::{classify_pty_output, StatePatterns};

pub struct StateTracker {
    pub current: AgentState,
    pub(crate) since: Instant,
    pub last_output: Instant,
    /// F9 (#685 sub-task 4): bumped only when `infer_productivity()` returns
    /// a `Productive` signal (heartbeat refresh or structural marker match).
    /// Bare screen change does NOT bump this — unlike `last_output`. Read by
    /// the daemon supervisor and passed to `check_hang` as `silent_productive`
    /// for the dual-path Hung detection. See `docs/F9-PRODUCTIVE-OUTPUT-GATE.md`
    /// §F9.1 architecture and §F9.3 dual-path decision table.
    pub last_productive_output: Instant,
    /// #685 PR-2: hash of the matched-marker substring on the most-recent
    /// productive refresh. Used to suppress re-firing
    /// `last_productive_output = now()` when the same marker text remains
    /// visible across screen-change ticks (e.g. stale "Saved to /tmp/foo.txt"
    /// stays in viewport while a spinner cycles below). Same defense-in-
    /// depth class as #1005 ToolUse oscillation guard. Cleared on
    /// non-matching feed so a genuine future Productive signal re-fires.
    last_productive_marker_hash: Option<u64>,
    /// Hash of the last screen text fed to `feed()`. `None` before the first
    /// call. Used to skip re-detection when the screen hasn't changed —
    /// crucial for not resetting `last_output` on cursor-blink noise.
    last_screen_hash: Option<u64>,
    patterns: Option<&'static StatePatterns>,
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
    /// #919: ring buffer of recent RAW PTY chunks (with ANSI escapes)
    /// used to anchor HIGH_FP state-detection patterns against red SGR
    /// presence. Bounded at `RAW_RING_CHUNKS` entries × `RAW_CHUNK_MAX`
    /// bytes per entry. Pushed in `feed_raw`; consumed by
    /// `has_red_ansi_anchor`. See `#919` design memo for rationale.
    raw_ring: VecDeque<RawChunk>,
    /// #919: backend's opt-in to anchor on red SGR. Cached at
    /// construction from `Backend::should_anchor_on_red()`. When
    /// false, the anchor gate fails open (pre-#919 behavior).
    anchor_on_red: bool,
    /// #1005 Phase A2: most-recent priority-up transition target +
    /// timestamp. Set on every successful priority-up in `transition()`.
    /// Cleared (set to None) on explicit Ready / lower-priority drops
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
}

const MARKER_SCAN_TAIL_LINES: usize = 5;

fn recent_screen_tail(screen_text: &str, n: usize) -> String {
    let lines: Vec<&str> = screen_text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// #1005 Phase A2: window inside which a `Lower→Active(X)→Lower→Active(X)`
/// bounce is treated as oscillation. Default 30s — matches
/// `StateTracker::LATCHED_STATE_EXPIRY` so the guard's protection
/// covers the same horizon as the latched-state expiry it backstops.
///
/// Operator-tunable via `AGEND_OSCILLATION_GUARD_WINDOW_SECS=<N>`.
/// Set to `0` to effectively disable (no bounce ever falls within
/// a zero-duration window).
fn oscillation_guard_window() -> Duration {
    const DEFAULT_SECS: u64 = 30;
    let secs = std::env::var("AGEND_OSCILLATION_GUARD_WINDOW_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SECS);
    Duration::from_secs(secs)
}

/// HIGH_FP states require the red-SGR anchor before transitioning.
/// Per #919 spike + dev-2 cross-audit:
/// - ServerRateLimit / RateLimit: server-side throttle alternations
///   include `api_error|timeout_error|overloaded_error` etc which
///   appear in dialectic prose / JSON dumps.
/// - ContextFull: `context.*(full|limit)` second alternation is a
///   common English phrase.
fn is_high_fp_state(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::ServerRateLimit | AgentState::RateLimit | AgentState::ContextFull
    )
}

fn hash_screen(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
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
    const LATCHED_STATE_EXPIRY: Duration = Duration::from_secs(30);

    /// #1005 Phase A2: minimum hold in the lower-priority state before
    /// a priority-up back into the previous active state is allowed.
    /// If the lower-state was held less than this, the priority-up is
    /// treated as part of an oscillation cycle and suppressed.
    ///
    /// Chosen at 5s: matches `min_hold` for passive states (5s for
    /// Idle/Ready), so legitimate "user briefly idle then activity"
    /// transitions still go through.
    const OSCILLATION_LOWER_HOLD_THRESHOLD: Duration = Duration::from_secs(5);

    /// Max time `InteractivePrompt` / `PermissionPrompt` may stay latched
    /// after its trigger pattern stops matching. Longer than
    /// LATCHED_STATE_EXPIRY because operators legitimately take a while
    /// to respond to a dialog, but bounded so a prompt dismissed
    /// out-of-band (screen hash unchanged after dismissal ⇒ no re-detect)
    /// eventually recovers to Ready instead of staying stuck — the
    /// operator-reported `dev-reviewer 卡在互動 prompt` false positive.
    const INTERACTIVE_EXPIRY: Duration = Duration::from_secs(120);

    /// Max time `RateLimit` may stay latched. Real rate limits typically
    /// clear in seconds to minutes; 5 min covers aggressive throttling
    /// while preventing hours-long false positives (PR #319 incident).
    const RATE_LIMIT_EXPIRY: Duration = Duration::from_secs(300);

    /// If the last MCP heartbeat is within this window, the agent is
    /// considered alive and `PermissionPrompt` detection is suppressed.
    const HEARTBEAT_FRESH_WINDOW: Duration = Duration::from_secs(120);

    pub fn new(backend: Option<&Backend>) -> Self {
        // Backends without a state pattern catalog (Shell, Raw) skip the
        // `Starting → Ready` handshake. Without this they sat in
        // `Starting` forever — `detect()` can't possibly fire Ready
        // without any patterns — and the silence-based
        // `check_awaiting_operator` then flagged every idle shell as
        // "stuck on interactive prompt" after 30s of normal quiet at
        // its own prompt. Managed backends still start in `Starting` so
        // their onboarding / auth dialogs can pattern-match before
        // Ready is declared.
        let initial_state = match backend {
            Some(Backend::Shell | Backend::Raw(_)) | None => AgentState::Ready,
            Some(_) => AgentState::Starting,
        };
        Self {
            current: initial_state,
            since: Instant::now(),
            last_output: Instant::now(),
            last_productive_output: Instant::now(),
            last_productive_marker_hash: None,
            last_screen_hash: None,
            patterns: backend.map(StatePatterns::for_backend),
            interactive_prompt_pending_notice: false,
            interactive_recovery_pending_notice: false,
            last_heartbeat: None,
            behavioral_config: backend.map(crate::behavioral::config_for),
            productivity_config: backend.map(crate::behavioral::config_for_productivity),
            instance_name: String::new(),
            backend_name: backend.map(|b| b.name().to_string()).unwrap_or_default(),
            // #919: per-agent raw-chunk ring buffer for red-SGR anchor.
            // Init empty; populated via `feed_raw` from `on_pty_data`
            // BEFORE `vterm.process` consumes the bytes.
            raw_ring: VecDeque::with_capacity(RAW_RING_CHUNKS),
            // #919: backend opt-in for the anchor gate. Defaults true
            // for known TUI backends (Claude/Codex/Gemini/OpenCode/
            // KiroCli), false for Shell/Raw — see
            // `Backend::should_anchor_on_red`.
            anchor_on_red: backend.is_some_and(|b| b.should_anchor_on_red()),
            // #1005 A2: oscillation guard starts unarmed — first
            // legitimate priority-up records into it; subsequent
            // priority-ups within the window check against it.
            last_priority_up_into: None,
        }
    }

    /// #919: push raw PTY bytes (with ANSI escapes) into the anchor
    /// ring buffer. Called from `agent::on_pty_data` BEFORE
    /// `vterm.process` strips the ANSI. Bounded ring + per-chunk size
    /// cap — see `RAW_RING_CHUNKS` / `RAW_CHUNK_MAX`.
    pub fn feed_raw(&mut self, raw: &[u8]) {
        let truncated: Vec<u8> = if raw.len() > RAW_CHUNK_MAX {
            raw[..RAW_CHUNK_MAX].to_vec()
        } else {
            raw.to_vec()
        };
        if self.raw_ring.len() >= RAW_RING_CHUNKS {
            self.raw_ring.pop_front();
        }
        self.raw_ring.push_back(RawChunk {
            bytes: truncated,
            at: Instant::now(),
        });
    }

    /// #919 test seam: read-only access to the raw ring buffer for
    /// anchor-behavior tests. Not used in production.
    #[cfg(test)]
    pub(crate) fn raw_ring(&self) -> &VecDeque<RawChunk> {
        &self.raw_ring
    }

    /// Set instance name for behavioral telemetry logging.
    #[allow(dead_code)] // Called by daemon supervisor when wiring up agents
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

    /// Returns true at most once per recovery from a blocked state
    /// (InteractivePrompt / AwaitingOperator → non-blocked). The supervisor
    /// calls this each tick; it returns true only on the first tick after the
    /// recovery transition so Telegram sees one "ready again" notice, not
    /// one per tick.
    pub fn take_recovery_notice(&mut self) -> bool {
        if self.interactive_recovery_pending_notice {
            self.interactive_recovery_pending_notice = false;
            true
        } else {
            false
        }
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
    /// to Ready so the tracker cannot get stuck if a marker pattern briefly
    /// disappears without the Ready pattern re-matching.
    ///
    /// Heartbeat gate (A5 fix): after pattern detection, if the detected
    /// state is `PermissionPrompt` but a fresh MCP heartbeat exists, the
    /// detection is overridden to `Thinking` — the agent is alive and the
    /// PTY pattern is a false positive.
    pub fn feed(&mut self, screen_text: &str) {
        let hash = hash_screen(screen_text);
        if self.last_screen_hash == Some(hash) {
            return;
        }
        self.last_screen_hash = Some(hash);

        // Sprint 27 shadow-mode: capture silence duration BEFORE updating
        // last_output, so we measure time since previous feed (not current).
        let silence_since_last_feed = self.last_output.elapsed();

        self.last_output = Instant::now();

        if let Some(patterns) = self.patterns {
            // #919: detect_with_match returns the matched substring so
            // we can anchor it against the raw-chunk ring. For HIGH_FP
            // states, require a red SGR escape within
            // `ANCHOR_WINDOW_BYTES` of the phrase in the recent raw
            // ring (and the chunk must be within `ANCHOR_WINDOW_MS`).
            // Gate fail-open when `anchor_on_red` is false (Shell/Raw
            // backends) OR ring is empty (cold start).
            match patterns.detect_with_match(screen_text) {
                Some((detected, matched)) => {
                    if self.anchor_on_red
                        && is_high_fp_state(detected)
                        && !self.raw_ring.is_empty()
                        && !has_red_ansi_anchor(&self.raw_ring, matched, Instant::now())
                    {
                        tracing::debug!(
                            agent = %self.instance_name,
                            state = ?detected,
                            matched = %matched,
                            "#919: HIGH_FP pattern matched without red-SGR anchor in raw ring — suppressing transition (likely daemon-injected prose)"
                        );
                        // Treat as no detection — fall through to
                        // structural fallback / latch maintenance.
                        if matches!(self.current, AgentState::Starting)
                            && is_generic_startup_prompt(screen_text)
                        {
                            self.transition(AgentState::InteractivePrompt);
                        } else {
                            self.maybe_expire_latched_state();
                        }
                    } else {
                        let gated = self.gate_on_heartbeat(detected);
                        self.transition(gated);
                    }
                }
                None => {
                    // Starting-only structural fallback: if the pattern
                    // catalog didn't recognize anything but the screen
                    // contains a generic prompt token (y/n, press enter,
                    // etc.), this is almost certainly a startup-time
                    // dialog waiting for the operator. Flag it as
                    // InteractivePrompt immediately instead of waiting
                    // on `check_awaiting_operator`'s silence window.
                    if matches!(self.current, AgentState::Starting)
                        && is_generic_startup_prompt(screen_text)
                    {
                        self.transition(AgentState::InteractivePrompt);
                    } else {
                        self.maybe_expire_latched_state();
                    }
                }
            }
        }

        // Sprint 27 shadow-mode: log behavioral signal alongside regex state.
        // Zero state change — telemetry only. Phase 2 (Sprint 28+) promotes
        // behavioral to tiebreaker/primary.
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

        // F9 (#685 sub-task 4): productive-output detection. Bumps
        // `last_productive_output` only on a Productive signal. The bump
        // affects nothing about Hung classification directly — the daemon
        // supervisor reads `last_productive_output.elapsed()` and passes it
        // to `check_hang` as the dual-path signal. Activation of the new
        // classification branch is gated on `AGEND_PRODUCTIVE_GATE=1` in
        // `check_hang` (shadow-mode default). See
        // `docs/F9-PRODUCTIVE-OUTPUT-GATE.md` §F9.5.
        //
        // #685 PR-2 (reviewer #1009 / #1005 same-class flag): scan ONLY
        // the recent tail (last MARKER_SCAN_TAIL_LINES rows) — historical
        // completion markers visible in scrollback (e.g. `Saved to /tmp/
        // foo.txt` left at row 5 from a 5-min-old write) MUST NOT keep
        // refreshing `last_productive_output` on every cursor-blink tick.
        //
        // #685 PR-2 RC1 (reviewer #1013 verdict): dedup hash scope
        // narrowed from "entire recent tail" → "matched marker
        // substring". Pre-RC1 a stale marker that stayed visible in
        // the tail while an adjacent spinner ticked produced a
        // DIFFERENT tail hash on every tick — the dedup-hash never
        // matched, `last_productive_output` re-fired despite no new
        // productive evidence. Hashing the matched substring directly
        // captures evidence identity, not surrounding-context noise.
        if let Some(ref pconfig) = self.productivity_config {
            let heartbeat_age = self
                .last_heartbeat
                .map(|t| t.elapsed())
                .unwrap_or_else(|| Duration::from_secs(u32::MAX as u64));
            let recent_tail = recent_screen_tail(screen_text, MARKER_SCAN_TAIL_LINES);
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
                    // Marker source: dedup against the matched
                    // substring text. Same substring across feeds =
                    // same evidence, suppress refresh — even when
                    // adjacent content (spinner ticks, status line
                    // edits) changes around it.
                    let marker_hash = hash_screen(matched);
                    if self.last_productive_marker_hash != Some(marker_hash) {
                        self.last_productive_output = Instant::now();
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
                    self.last_productive_output = Instant::now();
                    self.last_productive_marker_hash = None;
                }
                _ => {
                    // No marker visible in the recent tail → clear the
                    // dedup hash so a fresh-after-silence marker
                    // re-fires the refresh.
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

    /// Fallback when the screen changed but no pattern matched.
    ///
    /// Active-state markers (Thinking "esc to cancel", ToolUse tool banners)
    /// can stop rendering while the CLI still shows on-screen content that
    /// happens not to match the backend's Ready pattern either — e.g. a
    /// mid-scroll render between the spinner clearing and the prompt
    /// re-appearing. Without a fallback the tracker would stay latched on
    /// the prior active state indefinitely.
    ///
    /// If the current state is a self-expiring active state
    /// (Thinking / ToolUse) and it has been held longer than
    /// `LATCHED_STATE_EXPIRY`, drop to Ready. Everything else is excluded:
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
            self.transition(AgentState::Ready);
            return;
        }
        // RateLimit expires on its own 5-minute window. Real rate limits
        // clear in seconds-to-minutes; stuck for hours is a false positive.
        let rate_limit_expiring = matches!(self.current, AgentState::RateLimit);
        if rate_limit_expiring && self.since.elapsed() >= Self::RATE_LIMIT_EXPIRY {
            self.transition(AgentState::Ready);
            return;
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
            self.transition(AgentState::Ready);
        }
    }

    /// Get current state.
    pub fn get_state(&self) -> AgentState {
        self.current
    }

    /// Periodic tick — expire stale latched states without requiring new PTY
    /// output. Called from supervisor and app mode tick loops so idle agents
    /// don't stay stuck on ToolUse/Thinking indefinitely.
    pub(crate) fn tick(&mut self) {
        self.maybe_expire_latched_state();
    }

    /// Force state to Restarting (called by reaper on crash).
    pub fn set_restarting(&mut self) {
        self.current = AgentState::Restarting;
        self.since = Instant::now();
    }

    /// Force state to AwaitingOperator when startup stalls on an unexpected
    /// interactive prompt. Only fires from `Starting` — Ready-state stalls
    /// are caught by pattern-based detection (see `InteractivePrompt` once
    /// added; until then they're missed, which is acceptable for the
    /// time-based fallback role).
    ///
    /// Once the operator unblocks the stall and the ready pattern matches
    /// fresh screen content, `transition()` lifts the state (Ready prio >
    /// AwaitingOperator prio → higher always wins).
    pub fn set_awaiting_operator(&mut self) {
        if matches!(self.current, AgentState::Starting) {
            self.current = AgentState::AwaitingOperator;
            self.since = Instant::now();
        }
    }

    fn transition(&mut self, new_state: AgentState) {
        if new_state == self.current {
            return;
        }

        let prev = self.current;

        // Error states: instant transition (no hysteresis)
        if new_state.is_error() {
            self.current = new_state;
            self.since = Instant::now();
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
                self.current = new_state;
                self.since = now;
            } else if held >= min_hold {
                // Lower priority only after min hold
                self.current = new_state;
                self.since = Instant::now();
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
