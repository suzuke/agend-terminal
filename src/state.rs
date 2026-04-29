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
use regex::Regex;
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
    ContextFull,
    RateLimit,
    UsageLimit,
    AuthError,
    ApiError,
    Crashed,
    Restarting,
}

impl AgentState {
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
            Self::ContextFull => 9,
            Self::RateLimit => 10,
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
    /// (startup stall or pattern-matched InteractivePrompt like codex's
    /// update menu), not a free-form conversation prompt.
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
            Self::ContextFull => "context_full",
            Self::RateLimit => "rate_limit",
            Self::UsageLimit => "usage_limit",
            Self::AuthError => "auth_error",
            Self::ApiError => "api_error",
            Self::Crashed => "crashed",
            Self::Restarting => "restarting",
        }
    }
}

/// Compiled patterns for one backend.
pub struct StatePatterns {
    /// (state, regex) pairs in priority order (highest priority first).
    patterns: Vec<(AgentState, Regex)>,
}

impl StatePatterns {
    /// Pattern sources: [実測] = verified from real capture, [文件] = from docs/source, [推測] = estimated
    /// Tested versions: Claude v2.1.89, Codex v0.118.0, OpenCode v1.4.0, Gemini v0.37.1
    #[allow(clippy::unwrap_used)] // patterns are const — compile failure is a code bug
    pub fn for_backend(backend: &Backend) -> Self {
        let patterns = match backend {
            // Claude Code v2.1.89
            Backend::ClaudeCode => vec![
                // [docs] Claude Code SDK error handling
                (
                    AgentState::AuthError,
                    r"API key|authentication failed|unauthorized",
                ),
                // [docs] SDK retry logic for 429/overloaded
                // Sprint 31+ #4: word-boundary `429` to avoid false-positive
                // on substrings like "build #4290" / "request id: 4291...".
                (AgentState::RateLimit, r"overloaded|rate.?limit|\b429\b"),
                // [docs] Auto-compaction on context limit
                (
                    AgentState::ContextFull,
                    r"compacting context|context.*(full|limit)",
                ),
                // [measured] Claude 2.1.98 permission dialog renders as an
                // Ink overlay with a distinctive footer — `Esc to cancel ·
                // Tab to amend` — plus a `Do you want to …` question and
                // `1. Yes / 2. Yes, allow all edits during this session /
                // 3. No` options. Observed in tests/fixtures/state-replay/
                // claude-perm.raw at byte ~9216. The previous pattern
                // (`Allow once|Allow always|approve`) did not match any
                // wording in this dialog. The footer line is the most
                // specific anchor; the question prefix and allow-all-edits
                // option cover variations where the footer is scrolled out.
                (
                    AgentState::PermissionPrompt,
                    r"Esc to cancel · Tab to amend|Do you want to |allow all edits during this session|Allow once|Allow always|approve",
                ),
                // [estimated] Ink render during processing
                (AgentState::Thinking, r"Thinking"),
                // [measured] Completion glyph `⏺` (U+23FA RECORD) prefixes
                // tool-name banners like `⏺ Write(...)` in 2.1.98. Previously
                // only `●` (U+25CF) was in the class, so `⏺ Write(...)` lines
                // never matched — see docs/archived/FOLLOWUP-tooluse-pattern-gaps.md.
                // In-flight banners use -ing verbs (`⏺ Listing ...`,
                // `⏺ Reading ...`) rather than the bare tool name shown on
                // the completion banner — covered by the alternation below.
                (
                    AgentState::ToolUse,
                    r"[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏✓●⏺].*(Read|Bash|Edit|Write|Grep|Glob|Listing|Reading|Writing|Searching|Editing)",
                ),
                // [measured] Prompt symbol in idle state
                (AgentState::Idle, r"❯"),
                // [measured] Shown after startup with --dangerously-skip-permissions
                (AgentState::Ready, r"bypass permissions"),
            ],
            // Kiro CLI (version TBD)
            Backend::KiroCli => vec![
                // [docs] Kiro auth error messages
                (
                    AgentState::AuthError,
                    r"Not authenticated|AccessDenied|denied access",
                ),
                // [docs] AWS quota errors
                (
                    AgentState::UsageLimit,
                    r"ServiceQuotaExceeded|InsufficientModelCapacity",
                ),
                // [docs] HTTP 429 handling
                // Sprint 31+ #4: word-boundary `429` per Claude pattern.
                (
                    AgentState::RateLimit,
                    r"Too Many Requests|ThrottlingError|\b429\b",
                ),
                // [docs] Context overflow triggers compaction
                // `/compact` was previously included but matches the slash-
                // command autocomplete menu (kiro lists `/compact` alongside
                // other commands when user types `/`), producing a false
                // ContextFull on any `/` keypress. "compacting context"
                // covers the actual in-progress compaction message.
                (
                    AgentState::ContextFull,
                    r"context window overflow|compacting context",
                ),
                // [docs] Trust-based permission system
                (AgentState::PermissionPrompt, r"Allow this action|y/n/t"),
                // [measured] Kiro 2.0.1 renders tool banners as
                // `● Read .`, `● Write <path>`, etc. — `●` (U+25CF BLACK
                // CIRCLE) + space + capitalized tool verb. Observed in
                // tests/fixtures/state-replay/kiro-tooluse.raw at byte
                // ~40960. Placed above Thinking so the tool banner wins
                // first-match when both a `●` banner and the `Thinking`
                // spinner coexist on the same screen. The legacy internal
                // tool-name alternation (`execute_bash` etc.) is retained
                // for completeness — those strings appear in tool-output
                // stack traces and may surface during errors.
                (
                    AgentState::ToolUse,
                    r"●\s+(Read|Write|Edit|Bash|Grep|Glob|Task|List|Search)\b|execute_bash|fs_read|fs_write",
                ),
                // [measured] Kiro prints the word "Thinking" during generation
                // (captured live 2026-04-20, 23 occurrences in a short session).
                // Earlier pattern "Generating" never matched real output.
                (AgentState::Thinking, r"Thinking"),
                // [measured] Idle prompt
                (
                    AgentState::Idle,
                    r"\d+%\s*$|ask a question or describe a task",
                ),
                // [measured] Trust dialog completion / ready state
                (AgentState::Ready, r"Trust All Tools active|/quit to exit"),
            ],
            // Codex v0.118.0
            Backend::Codex => vec![
                // [docs] Requires OPENAI_API_KEY env
                (AgentState::AuthError, r"OPENAI_API_KEY|api.?key"),
                // [実測 v0.118.0] Quota exhausted message
                (AgentState::UsageLimit, r"hit your usage limit|try again at"),
                // [docs] HTTP 429 handling
                // Sprint 31+ #4: word-boundary `429` per Claude pattern.
                (AgentState::RateLimit, r"rate.?limit|\b429\b"),
                // [docs] Context overflow error
                (AgentState::ContextFull, r"ContextOverflow"),
                // [measured] Codex 0.120.0 renders approval dialogs with
                // a distinctive header (`Would you like to run the
                // following command?`), three numbered options starting
                // with `Yes, proceed` and ending with `No, and tell
                // Codex what to do differently`, plus a footer
                // (`Press enter to confirm or esc to cancel`). Observed
                // in tests/fixtures/state-replay/codex-perm.raw at byte
                // ~68K through dismissal at ~90K. The prior pattern
                // (`Request approval|approve|deny`) never matched any
                // of the wording. `approve|deny` retained for legacy
                // and adjacent docs wording; the new alternations cover
                // the real dialog text. Header + footer are long enough
                // to avoid false positives on narration lines.
                (
                    AgentState::PermissionPrompt,
                    r"Would you like to run the following command\?|Yes, proceed|No, and tell Codex|Press enter to confirm or esc to cancel|Request approval|approve|deny",
                ),
                // [measured] Codex launches into an update-available modal
                // when a newer version is published; the banner blocks the
                // REPL until the operator presses Enter to dismiss or select.
                // Previously this left the agent visibly at Ready (the banner
                // text still matched "OpenAI Codex") while silently stalled.
                (
                    AgentState::InteractivePrompt,
                    r"Update available!|Press enter to continue",
                ),
                // [measured] Codex 0.120.0 renders tool-call blocks as a
                // two-line region — a `•` title line (`• Explored`,
                // `• Edited`, `• Ran`) followed by a `└` continuation
                // line carrying the actual tool call (e.g.
                // `  └ Read README.md`, `  └ Ran apply_patch`). Observed
                // in codex-tooluse.raw at byte ~40960. Placed above
                // Thinking so tool blocks win first-match against the
                // `• Working (...)` spinner that renders concurrently.
                // Bare `•` is intentionally NOT in the pattern — it also
                // prefixes assistant narration lines (`• I'm reading
                // ...`) and would cause false positives. The legacy
                // `apply_patch` substring is retained for completeness.
                (
                    AgentState::ToolUse,
                    r"└\s+(Read|Write|Edit|List|Bash|Search|Apply|Ran)\b|•\s+(Explored|Edited|Ran)\b|apply_patch",
                ),
                // [estimated] Processing state
                (AgentState::Thinking, r"Thinking"),
                // [measured] Prompt symbol + model info in status
                (AgentState::Idle, r"›"),
                // [measured] Version + model display
                (AgentState::Ready, r"OpenAI Codex|gpt-.*left"),
            ],
            // OpenCode v1.4.0
            Backend::OpenCode => vec![
                // [docs] HTTP error handling
                // Sprint 31+ #4: word-boundary `429` per Claude pattern.
                (AgentState::RateLimit, r"rate.?limit|\b429\b"),
                // [docs] Context overflow
                (AgentState::ContextFull, r"ContextOverflow"),
                // [docs] Permission UI
                (
                    AgentState::PermissionPrompt,
                    r"Permission required|Allow once|Allow always",
                ),
                // [measured] OpenCode 1.4.0 prefixes tool banners with
                // `✱` (U+2731 HEAVY ASTERISK, in-flight) or `→` (U+2192,
                // completed) followed by the tool name — e.g.
                // `✱ Glob "README.md" (1 match)` and `→ Read README.md`.
                // Observed in tests/fixtures/state-replay/opencode-tooluse.raw
                // at byte ~30720 / ~61440. Priority above the Thinking
                // pattern so active tool use outranks the generic spinner.
                (
                    AgentState::ToolUse,
                    r"[✱→]\s+(Read|Write|Edit|Glob|Grep|Bash|List|Task)\b",
                ),
                // [measured] OpenCode draws `■⬝⬝⬝⬝⬝⬝⬝  esc interrupt` on
                // its bottom status bar only while a request is in flight;
                // the line disappears the moment streaming completes.
                // Earlier pattern "Working" never matched real output.
                (AgentState::Thinking, r"esc interrupt"),
                // [measured] Update dialog that may block
                (
                    AgentState::PermissionPrompt,
                    r"Update Available|Skip\s+Confirm",
                ),
                // [measured] Input prompt text
                (AgentState::Idle, r"Ask anything"),
                // [measured] Ready state with keybinding hints
                (AgentState::Ready, r"Ask anything|tab agents"),
            ],
            // Gemini CLI v0.37.1
            Backend::Gemini => vec![
                // [docs] OAuth errors from API
                (
                    AgentState::AuthError,
                    r"OAuth not authenticated|OAuth expired|UNAUTHENTICATED|check API key",
                ),
                // [docs] Usage limit messages
                (
                    AgentState::UsageLimit,
                    r"Usage limit reached|Access resets at",
                ),
                // [docs] API resource exhaustion
                // Sprint 31+ #4: word-boundary `429` per Claude pattern.
                (AgentState::RateLimit, r"RESOURCE_EXHAUSTED|\b429\b"),
                // [docs] Token/quota limit
                (AgentState::ContextFull, r"quota.*exceeded|token.*limit"),
                // [docs] Permission select options
                (
                    AgentState::PermissionPrompt,
                    r"Allow once|Allow for this session|suggest changes",
                ),
                // [measured] Gemini 0.38.2 renders completed tool calls as
                // `✓  ReadFile  Cargo.toml` — `✓` (U+2713 CHECK MARK) + a
                // CamelCase tool name + target. Observed in
                // tests/fixtures/state-replay/gemini-tooluse.raw at byte
                // ~143360. Placed above Thinking so the tool banner wins
                // first-match against the concurrent `⠦ Thinking... (esc
                // to cancel)` spinner. The existing `tool.*call|MCP.*tool`
                // alternation is retained for the end-of-session
                // `Tool Calls: 1` summary and MCP-tool surfaces.
                (
                    AgentState::ToolUse,
                    r"✓\s+(ReadFile|WriteFile|ReadManyFiles|Edit|Shell|WebFetch|Glob|GoogleSearch|MemoryTool|ReadFolder)\b|tool.*call|MCP.*tool",
                ),
                // [measured] Gemini's spinner line ("⠦ Thinking... (esc to
                // cancel, Ns)") only renders while a request is in flight and
                // is overwritten in place when streaming completes. Matching
                // the bare word "Thinking" previously latched the state and
                // never released — chat history kept the token visible on
                // screen and detect() kept returning Thinking forever.
                (AgentState::Thinking, r"esc to cancel"),
                // [measured] Input prompt text
                (AgentState::Idle, r"Type your message"),
                // [measured] Full ready prompt + YOLO mode
                (AgentState::Ready, r"Type your message|YOLO"),
            ],
            // Non-preset backends have no state-detection heuristics — pane
            // stays in whatever state the generic output pipeline sets. These
            // variants should never reach here in normal flow (state machine
            // is gated on preset variants today), but keep the match
            // exhaustive so we fail loudly if a caller does route them here.
            Backend::Shell | Backend::Raw(_) => vec![],
        };

        let compiled: Vec<_> = patterns
            .into_iter()
            .map(|(state, pat)| {
                let re = Regex::new(pat)
                    .unwrap_or_else(|e| panic!("BUG: invalid state regex {pat:?}: {e}"));
                (state, re)
            })
            .collect();

        Self { patterns: compiled }
    }

    /// Match against state buffer, return highest-priority matching state.
    pub fn detect(&self, text: &str) -> Option<AgentState> {
        // Patterns are already in priority order (highest first)
        for (state, re) in &self.patterns {
            if re.is_match(text) {
                return Some(*state);
            }
        }
        None
    }
}

/// Classify PTY output into a [`BlockedReason`] for the given backend.
///
/// Returns `None` when the output does not match any known error pattern.
/// Uses simple substring/regex checks aligned with the per-backend patterns
/// in [`StatePatterns::for_backend`].
///
/// Stacking dep: production caller wired in S2-T4 (daemon watchdog).
#[allow(dead_code, clippy::collapsible_match)]
pub fn classify_pty_output(
    backend: &crate::backend::Backend,
    output: &str,
) -> Option<crate::health::BlockedReason> {
    use crate::backend::Backend;
    use crate::health::BlockedReason;

    match backend {
        Backend::ClaudeCode => {
            if regex::Regex::new(r"(?i)credit_balance_too_low")
                .ok()?
                .is_match(output)
            {
                return Some(BlockedReason::QuotaExceeded);
            }
            // Sprint 31+ #4: word-boundary `429` per state-pattern fix.
            if regex::Regex::new(r"(?i)overloaded|rate.?limit|\b429\b")
                .ok()?
                .is_match(output)
            {
                return Some(BlockedReason::RateLimit {
                    retry_after_secs: None,
                });
            }
        }
        Backend::KiroCli => {
            if regex::Regex::new(r"ServiceQuotaExceeded|InsufficientModelCapacity")
                .ok()?
                .is_match(output)
            {
                return Some(BlockedReason::QuotaExceeded);
            }
            if regex::Regex::new(r"Too Many Requests|ThrottlingError|\b429\b")
                .ok()?
                .is_match(output)
            {
                return Some(BlockedReason::RateLimit {
                    retry_after_secs: None,
                });
            }
        }
        Backend::Codex => {
            if regex::Regex::new(r"hit your usage limit|try again at")
                .ok()?
                .is_match(output)
            {
                return Some(BlockedReason::QuotaExceeded);
            }
            if regex::Regex::new(r"(?i)rate.?limit|\b429\b")
                .ok()?
                .is_match(output)
            {
                return Some(BlockedReason::RateLimit {
                    retry_after_secs: None,
                });
            }
        }
        Backend::Gemini => {
            if regex::Regex::new(r"RESOURCE_EXHAUSTED|\b429\b")
                .ok()?
                .is_match(output)
            {
                return Some(BlockedReason::QuotaExceeded);
            }
        }
        _ => {}
    }
    None
}

/// Cheap structural test for a generic startup-time interactive prompt.
///
/// Only called while the agent is still in `Starting` state, so false
/// positives during Thinking/Ready (where model output might legitimately
/// contain strings like `(y/n)` as examples) are avoided by the caller
/// gating on state. The token set is restricted to glyph sequences that
/// effectively never appear outside of a real TUI prompt — broad catches
/// like a trailing `?` or `:` are intentionally excluded because they fire
/// on ordinary prose.
///
/// Complements `check_awaiting_operator` silence detection:
/// - When the prompt text is recognized structurally we transition to
///   `InteractivePrompt` immediately (no waiting on a silence window).
/// - Unknown prompts that happen not to use any of these tokens still fall
///   through to the silence fallback in `daemon::supervisor`.
fn is_generic_startup_prompt(text: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        // Case-insensitive so `(Y/n)` etc. hit the same token set.
        Regex::new(r"(?i)\(y/n\)|\(yes/no\)|\[y/n\]|press\s+(enter|return|any\s+key)")
            .expect("generic startup prompt regex compiles")
    });
    re.is_match(text)
}

/// Tracks state with hysteresis.
pub struct StateTracker {
    pub current: AgentState,
    pub(crate) since: Instant,
    pub last_output: Instant,
    /// Hash of the last screen text fed to `feed()`. `None` before the first
    /// call. Used to skip re-detection when the screen hasn't changed —
    /// crucial for not resetting `last_output` on cursor-blink noise.
    last_screen_hash: Option<u64>,
    patterns: Option<StatePatterns>,
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
    /// Instance name for telemetry logging.
    instance_name: String,
    /// Backend name for telemetry logging.
    backend_name: String,
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
    const LATCHED_STATE_EXPIRY: Duration = Duration::from_secs(30);

    /// Max time `InteractivePrompt` / `PermissionPrompt` may stay latched
    /// after its trigger pattern stops matching. Longer than
    /// LATCHED_STATE_EXPIRY because operators legitimately take a while
    /// to respond to a dialog, but bounded so a prompt dismissed
    /// out-of-band (screen hash unchanged after dismissal ⇒ no re-detect)
    /// eventually recovers to Ready instead of staying stuck — the
    /// operator-reported `dev-reviewer 卡在互動 prompt` false positive.
    const INTERACTIVE_EXPIRY: Duration = Duration::from_secs(120);

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
            last_screen_hash: None,
            patterns: backend.map(StatePatterns::for_backend),
            interactive_prompt_pending_notice: false,
            interactive_recovery_pending_notice: false,
            last_heartbeat: None,
            behavioral_config: backend.map(crate::behavioral::config_for),
            instance_name: String::new(),
            backend_name: backend.map(|b| b.name().to_string()).unwrap_or_default(),
        }
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

        if let Some(ref patterns) = self.patterns {
            match patterns.detect(screen_text) {
                Some(detected) => {
                    let gated = self.gate_on_heartbeat(detected);
                    self.transition(gated);
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
                self.current = new_state;
                self.since = Instant::now();
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
mod tests {
    use super::*;
    use crate::health::HealthTracker;

    /// Create a tracker with a given current state and elapsed time since that state began.
    fn tracker_at(backend: &Backend, state: AgentState, elapsed_secs: u64) -> StateTracker {
        let mut t = StateTracker::new(Some(backend));
        t.current = state;
        t.since = Instant::now() - Duration::from_secs(elapsed_secs);
        t
    }

    // ── False-positive regression pins ──────────────────────────────────
    //
    // Operator reported two misfires:
    //   1. `shell` flagged with "⚠️ shell 靜默 38s，可能卡在互動 prompt" while
    //      sitting at a normal `❯` prompt.
    //   2. `dev-reviewer` (Codex) flagged with "卡在互動 prompt" after
    //      dismissing a transient banner — the state never recovered
    //      because hash-dedup suppressed re-detection.

    // Sprint 31+ #4: rate-limit regex false-positive on benign tokens.
    // Operator m-2 quick fix (Option A): word-boundary the `429` token to
    // avoid matching substrings like "build #4290" / "request id: 4291abc"
    // / "response time: 4290ms". `\b429\b` ensures `429` is a standalone
    // token (preceded + followed by non-word boundary).
    #[test]
    fn rate_limit_regex_does_not_misfire_on_build_number_4290() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
        t.feed("CI: build #4290 succeeded in 12s");
        assert_ne!(
            t.get_state(),
            AgentState::RateLimit,
            "benign build number `4290` must not trigger RateLimit"
        );
    }

    #[test]
    fn rate_limit_regex_does_not_misfire_on_request_id_4291abc() {
        let mut t = tracker_at(&Backend::Codex, AgentState::Idle, 0);
        t.feed("request id: 4291abcdef");
        assert_ne!(
            t.get_state(),
            AgentState::RateLimit,
            "benign request id starting with `4291` must not trigger RateLimit"
        );
    }

    #[test]
    fn rate_limit_regex_still_matches_real_429_token() {
        // Regression guard: word-boundary fix must NOT break the canonical
        // "Error: 429" detection. Same scenario as `error_state_instant_transition`.
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
        t.feed("API error: 429 Too Many Requests");
        assert_eq!(
            t.get_state(),
            AgentState::RateLimit,
            "canonical `429` standalone token must still trigger RateLimit"
        );
    }

    #[test]
    fn shell_backend_starts_in_ready_not_starting() {
        // Regression: Shell/Raw have no state pattern catalog, so `detect()`
        // can never produce Ready; they used to sit in `Starting` forever
        // and `check_awaiting_operator` fired on every idle shell after
        // 30 s. Initial state Ready sidesteps the silence fallback — a
        // truly-stuck shell still gets caught by `check_hang` later.
        let t = StateTracker::new(Some(&Backend::Shell));
        assert_eq!(t.get_state(), AgentState::Ready);
    }

    #[test]
    fn raw_backend_starts_in_ready_not_starting() {
        let t = StateTracker::new(Some(&Backend::Raw("/opt/whatever".to_string())));
        assert_eq!(t.get_state(), AgentState::Ready);
    }

    #[test]
    fn managed_backends_still_start_in_starting() {
        // Keep the handshake for real backends so their
        // onboarding / auth prompts have a chance to pattern-match before
        // we declare Ready.
        for backend in [
            Backend::ClaudeCode,
            Backend::KiroCli,
            Backend::Gemini,
            Backend::Codex,
            Backend::OpenCode,
        ] {
            let t = StateTracker::new(Some(&backend));
            assert_eq!(
                t.get_state(),
                AgentState::Starting,
                "managed backend {backend:?} must still start in Starting"
            );
        }
    }

    #[test]
    fn interactive_prompt_expires_to_ready_after_two_minutes() {
        // `dev-reviewer`-style lockup: operator dismissed the dialog
        // out-of-band, screen went stable, hash-dedup meant `detect()`
        // never re-fired. With no re-detect and only
        // Thinking/ToolUse in the expiry list, the state was stuck
        // indefinitely. Ticking past INTERACTIVE_EXPIRY now drops to
        // Ready on its own.
        let mut t = tracker_at(&Backend::Codex, AgentState::InteractivePrompt, 119);
        t.tick();
        assert_eq!(
            t.get_state(),
            AgentState::InteractivePrompt,
            "must still be latched before expiry"
        );

        let mut t = tracker_at(&Backend::Codex, AgentState::InteractivePrompt, 121);
        t.tick();
        assert_eq!(
            t.get_state(),
            AgentState::Ready,
            "expected Ready after INTERACTIVE_EXPIRY, still {:?}",
            t.get_state()
        );
    }

    #[test]
    fn permission_prompt_also_expires_to_ready() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::PermissionPrompt, 130);
        t.tick();
        assert_eq!(t.get_state(), AgentState::Ready);
    }

    #[test]
    fn tool_use_still_uses_short_expiry() {
        // Regression guard against accidentally widening the short
        // expiry — Thinking / ToolUse should still drop at 30 s.
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::ToolUse, 31);
        t.tick();
        assert_eq!(t.get_state(), AgentState::Ready);
    }

    // ── P0: Core behavior ───────────────────────────────────────────────

    #[test]
    #[allow(clippy::unwrap_used)]
    fn error_state_instant_transition() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
        // Feed a rate limit pattern — error state should transition instantly
        t.feed("429 rate limit exceeded");
        assert_eq!(t.get_state(), AgentState::RateLimit);
    }

    #[test]
    fn active_to_passive_needs_hold() {
        let backend = Backend::ClaudeCode;

        // Thinking held for only 1s — transition to Idle should NOT happen
        let mut t = tracker_at(&backend, AgentState::Thinking, 1);
        t.transition(AgentState::Idle);
        assert_eq!(t.get_state(), AgentState::Thinking);

        // Thinking held for 3s (>= 2s active hold) — transition to Idle SHOULD happen
        let mut t = tracker_at(&backend, AgentState::Thinking, 3);
        t.transition(AgentState::Idle);
        assert_eq!(t.get_state(), AgentState::Idle);
    }

    #[test]
    fn passive_to_passive_needs_5s() {
        let backend = Backend::ClaudeCode;

        // Idle(3) → Ready(2): lower priority, passive hold = 5s
        // Idle held for 3s — should NOT transition
        let mut t = tracker_at(&backend, AgentState::Idle, 3);
        t.transition(AgentState::Ready);
        assert_eq!(t.get_state(), AgentState::Idle);

        // Idle held for 6s (>= 5s passive hold) — SHOULD transition
        let mut t = tracker_at(&backend, AgentState::Idle, 6);
        t.transition(AgentState::Ready);
        assert_eq!(t.get_state(), AgentState::Ready);
    }

    #[test]
    fn higher_priority_instant() {
        let backend = Backend::ClaudeCode;

        // Idle → Thinking: higher priority, should transition immediately even at 0s
        let mut t = tracker_at(&backend, AgentState::Idle, 0);
        t.transition(AgentState::Thinking);
        assert_eq!(t.get_state(), AgentState::Thinking);
    }

    #[test]
    fn error_recovery_needs_hold() {
        let backend = Backend::ClaudeCode;

        // RateLimit held for 1s — transition to Idle (lower priority) needs hold time
        // RateLimit priority > Idle priority, and RateLimit is active (priority > Idle),
        // so 2s active hold applies
        let mut t = tracker_at(&backend, AgentState::RateLimit, 1);
        t.transition(AgentState::Idle);
        assert_eq!(t.get_state(), AgentState::RateLimit);

        // RateLimit held for 3s (>= 2s) — should transition
        let mut t = tracker_at(&backend, AgentState::RateLimit, 3);
        t.transition(AgentState::Idle);
        assert_eq!(t.get_state(), AgentState::Idle);
    }

    // ── P1: Edge cases ──────────────────────────────────────────────────

    #[test]
    fn dismissed_prompt_clears_on_next_feed() {
        // Screen-based detection: once the prompt text leaves the current
        // screen, the next feed re-evaluates without lag — the replacement
        // for the old state_buf-clearing behavior.
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Starting, 0);
        // Simulate the update-menu screen
        t.feed("Update available! 0.120.0 -> 0.121.0\n1. Update now\n2. Skip");
        // Then the banner re-renders after dismissal
        t.feed("bypass permissions");
        assert_eq!(t.get_state(), AgentState::Ready);
    }

    #[test]
    fn unchanged_screen_does_not_reset_last_output() {
        // Hash dedup: feeding the same screen twice must not bump
        // last_output (used by hang/awaiting-operator predicates).
        let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
        t.feed("hello world");
        let first = t.last_output;
        std::thread::sleep(Duration::from_millis(20));
        t.feed("hello world");
        assert_eq!(
            t.last_output, first,
            "identical screen must not bump last_output"
        );
    }

    #[test]
    fn same_state_no_timer_reset() {
        let backend = Backend::ClaudeCode;
        let mut t = tracker_at(&backend, AgentState::Thinking, 10);
        let since_before = t.since;
        // Re-transition to same state — should be no-op
        t.transition(AgentState::Thinking);
        assert_eq!(t.since, since_before);
    }

    // ── Phase 1c: latched-state fallback ────────────────────────────────

    #[test]
    fn feed_fallback_expires_thinking_after_threshold() {
        // Thinking held past LATCHED_STATE_EXPIRY (30s) with no pattern
        // matching on the current screen must drop to Ready so a vanished
        // spinner cannot latch the tracker.
        let mut t = tracker_at(&Backend::Gemini, AgentState::Thinking, 31);
        // Fresh screen content that matches no pattern for gemini.
        t.feed("some unrelated output that matches nothing");
        assert_eq!(t.get_state(), AgentState::Ready);
    }

    #[test]
    fn feed_fallback_does_not_expire_before_threshold() {
        // Under the threshold Thinking must stay — legitimate thinking can
        // run for tens of seconds with a quiet but still-active spinner.
        let mut t = tracker_at(&Backend::Gemini, AgentState::Thinking, 10);
        t.feed("some unrelated output that matches nothing");
        assert_eq!(t.get_state(), AgentState::Thinking);
    }

    #[test]
    fn feed_fallback_expires_tooluse_after_threshold() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::ToolUse, 35);
        t.feed("no tool banner, no ready footer visible here");
        assert_eq!(t.get_state(), AgentState::Ready);
    }

    #[test]
    fn feed_fallback_does_not_expire_fresh_permission_prompt() {
        // Before INTERACTIVE_EXPIRY (120 s) a PermissionPrompt must stay
        // latched — an operator answering a dialog within a reasonable
        // window expects the banner to still be marked as active.
        // Post-INTERACTIVE_EXPIRY expiry is covered by
        // `permission_prompt_also_expires_to_ready`.
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::PermissionPrompt, 60);
        t.feed("nothing matches here");
        assert_eq!(t.get_state(), AgentState::PermissionPrompt);
    }

    // ── tick() regression pins (t-20260423040613) ───────────────────────

    #[test]
    fn tick_expires_stale_tool_use_without_feed() {
        // ToolUse held > 30s with no PTY output. tick() must expire it
        // to Ready without requiring feed() (which needs new screen text).
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::ToolUse, 35);
        t.tick();
        assert_eq!(t.get_state(), AgentState::Ready);
    }

    #[test]
    fn tick_does_not_expire_fresh_tool_use() {
        // ToolUse held < 30s should NOT be expired by tick().
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::ToolUse, 10);
        t.tick();
        assert_eq!(t.get_state(), AgentState::ToolUse);
    }

    #[test]
    fn tick_does_not_expire_fresh_permission_prompt() {
        // Within INTERACTIVE_EXPIRY the prompt stays latched so operators
        // have a real reaction window. Post-expiry recovery is covered
        // by `permission_prompt_also_expires_to_ready`.
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::PermissionPrompt, 60);
        t.tick();
        assert_eq!(t.get_state(), AgentState::PermissionPrompt);
    }

    #[test]
    fn tick_called_twice_still_expires() {
        // Verify tick() works on repeated calls (no hash-based early return).
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::ToolUse, 35);
        t.tick();
        assert_eq!(t.get_state(), AgentState::Ready);
        // Second tick on Ready is a no-op (Ready is not expiring).
        t.tick();
        assert_eq!(t.get_state(), AgentState::Ready);
    }

    #[test]
    fn feed_fallback_does_not_expire_fresh_interactive_prompt() {
        // Same contract as PermissionPrompt — within INTERACTIVE_EXPIRY
        // the prompt stays latched; after the threshold it recovers
        // (see `interactive_prompt_expires_to_ready_after_two_minutes`).
        let mut t = tracker_at(&Backend::Codex, AgentState::InteractivePrompt, 60);
        t.feed("nothing matches here");
        assert_eq!(t.get_state(), AgentState::InteractivePrompt);
    }

    #[test]
    fn feed_fallback_no_op_when_already_ready() {
        // Ready → Ready must not reset `since` (that would defeat the hold).
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Ready, 60);
        let since_before = t.since;
        t.feed("arbitrary text without any markers");
        assert_eq!(t.get_state(), AgentState::Ready);
        assert_eq!(t.since, since_before);
    }

    // ── Phase 1d: structural startup prompt detection ──────────────────

    #[test]
    fn generic_startup_prompt_matches_yes_no() {
        assert!(is_generic_startup_prompt("Trust this workspace? (y/n)"));
        assert!(is_generic_startup_prompt("Continue? (Y/n)"));
        assert!(is_generic_startup_prompt("Accept? (yes/no)"));
        assert!(is_generic_startup_prompt("Overwrite [y/N]?"));
        assert!(is_generic_startup_prompt("Use default [Y/n]"));
    }

    #[test]
    fn generic_startup_prompt_matches_press_enter() {
        assert!(is_generic_startup_prompt("Press enter to continue"));
        assert!(is_generic_startup_prompt("PRESS ENTER to dismiss"));
        assert!(is_generic_startup_prompt("Press Return when ready"));
        assert!(is_generic_startup_prompt("press any key to exit"));
    }

    #[test]
    fn generic_startup_prompt_rejects_ordinary_prose() {
        // Question marks and colons alone must not trigger — AI model
        // output is full of these.
        assert!(!is_generic_startup_prompt(
            "Should I continue with the refactor?"
        ));
        assert!(!is_generic_startup_prompt("Next steps:"));
        assert!(!is_generic_startup_prompt("Select: option A vs option B"));
        assert!(!is_generic_startup_prompt("Type your message"));
        assert!(!is_generic_startup_prompt(""));
    }

    #[test]
    fn starting_transitions_to_interactive_prompt_on_generic_token() {
        // Starting + pattern-None + generic prompt → InteractivePrompt,
        // no silence waiting required.
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Starting, 0);
        t.feed("Trust this workspace? (y/n)");
        assert_eq!(t.get_state(), AgentState::InteractivePrompt);
    }

    #[test]
    fn non_starting_ignores_generic_prompt_token() {
        // Ready + a model output containing `(y/n)` must not flip state —
        // false positives here were the reason we scope generic detection
        // to Starting.
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Ready, 0);
        t.feed("Here's an example: `git clean -n (y/n)` — the -n flag previews");
        assert_eq!(t.get_state(), AgentState::Ready);
    }

    #[test]
    fn starting_without_generic_prompt_stays_starting() {
        // Starting + no recognized pattern + no generic token → still
        // Starting; silence fallback in supervisor handles this.
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Starting, 0);
        t.feed("loading configuration...");
        assert_eq!(t.get_state(), AgentState::Starting);
    }

    #[test]
    fn feed_fallback_gated_by_hash_dedup() {
        // If the screen hasn't changed, fallback must not fire even if the
        // tracker has been latched forever — the hash dedup short-circuits
        // feed() before detection runs. Feed the same no-match text twice;
        // only the first call can possibly trigger the fallback path.
        let mut t = tracker_at(&Backend::Gemini, AgentState::Thinking, 31);
        t.feed("no marker");
        // Tracker already dropped to Ready on the first feed. Reset to
        // Thinking to exercise the dedup-gate specifically.
        t.current = AgentState::Thinking;
        t.since = Instant::now() - Duration::from_secs(31);
        t.feed("no marker"); // same text → hash dedup → early return
        assert_eq!(t.get_state(), AgentState::Thinking);
    }

    #[test]
    fn starting_hang_120s() {
        let mut h = HealthTracker::new();
        assert!(!h.check_hang(AgentState::Starting, Duration::from_secs(119), 1_000_000, 0));
        assert!(h.check_hang(AgentState::Starting, Duration::from_secs(121), 1_000_000, 0));
    }

    #[test]
    fn idle_never_hangs() {
        let mut h = HealthTracker::new();
        // Even with 10000s of silence, Idle should never be considered hung.
        assert!(!h.check_hang(AgentState::Idle, Duration::from_secs(10_000), 1_000_000, 0));
    }

    #[test]
    fn thinking_hang_600s() {
        let mut h = HealthTracker::new();
        assert!(!h.check_hang(AgentState::Thinking, Duration::from_secs(599), 1_000_000, 0));
        assert!(h.check_hang(AgentState::Thinking, Duration::from_secs(601), 1_000_000, 0));
    }

    // ── P2: Pattern matching ────────────────────────────────────────────

    #[test]
    fn claude_tooluse_spinner_match() {
        let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
        let detected = patterns.detect("⠋Read file.txt");
        assert_eq!(detected, Some(AgentState::ToolUse));
    }

    #[test]
    fn claude_tooluse_record_glyph_match() {
        // Claude 2.1.98 prefixes completed-tool banners with `⏺` (U+23FA,
        // RECORD) — distinct from `●` (U+25CF). Real-PTY recording
        // claude-perm.raw exhibits `⏺ Write(/tmp/...)` after the user
        // denies a write; both glyph and verb must be in the pattern for
        // the state to fire.
        let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
        let detected = patterns.detect("⏺ Write(/tmp/claude-perm-test.txt)");
        assert_eq!(detected, Some(AgentState::ToolUse));
    }

    #[test]
    fn claude_permission_prompt_dialog_match() {
        // Claude 2.1.98 permission dialog — distinctive footer + body.
        // Observed in claude-perm.raw ~byte 9216.
        let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
        assert_eq!(
            patterns.detect("Esc to cancel · Tab to amend"),
            Some(AgentState::PermissionPrompt),
            "dialog footer must fire PermissionPrompt",
        );
        assert_eq!(
            patterns.detect("Do you want to create /tmp/out.txt?"),
            Some(AgentState::PermissionPrompt),
            "dialog question prefix must fire PermissionPrompt",
        );
        assert_eq!(
            patterns.detect("   2. Yes, allow all edits during this session (shift+tab)"),
            Some(AgentState::PermissionPrompt),
            "allow-all-edits option must fire PermissionPrompt",
        );
    }

    #[test]
    fn claude_permission_prompt_legacy_wording_still_matches() {
        // Keep compat with the pre-2.1.98 wording in case earlier
        // CLI builds surface through the same backend.
        let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
        for sample in ["Allow once", "Allow always", "approve"] {
            assert_eq!(
                patterns.detect(sample),
                Some(AgentState::PermissionPrompt),
                "legacy wording {sample:?} must still fire PermissionPrompt",
            );
        }
    }

    #[test]
    fn claude_tooluse_ing_verb_match() {
        // Claude 2.1.98 in-flight banners use present-participle verbs —
        // `⏺ Listing 1 directory…`, `⏺ Reading file`, etc. — rather than
        // the bare tool name on the completion banner. Real-PTY recording
        // claude-tooluse.raw exhibits `⏺ Listing 1 directory…` mid-stream;
        // this synthetic test anchors each -ing verb we added to the
        // alternation.
        let patterns = StatePatterns::for_backend(&Backend::ClaudeCode);
        for sample in [
            "⏺ Listing 1 directory…",
            "⏺ Reading src/main.rs",
            "⏺ Writing /tmp/out.txt",
            "⏺ Searching for TODO",
            "⏺ Editing Cargo.toml",
        ] {
            assert_eq!(
                patterns.detect(sample),
                Some(AgentState::ToolUse),
                "expected ToolUse for {sample:?}"
            );
        }
    }

    #[test]
    fn pattern_does_not_cross_backends() {
        // Claude's "❯" idle pattern should not match on Gemini tracker
        let gemini_patterns = StatePatterns::for_backend(&Backend::Gemini);
        let detected = gemini_patterns.detect("❯");
        assert_ne!(detected, Some(AgentState::Idle));
    }

    #[test]
    fn empty_input_no_change() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Starting, 0);
        t.feed("");
        assert_eq!(t.get_state(), AgentState::Starting);
    }

    #[test]
    fn ready_detection() {
        let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
        t.feed("bypass permissions");
        assert_eq!(t.get_state(), AgentState::Ready);
    }

    #[test]
    fn idle_detection() {
        let mut t = StateTracker::new(Some(&Backend::ClaudeCode));
        // First get to Ready so that Idle (lower prio than Starting) can be tested
        // Starting → Ready (higher prio) is instant
        t.feed("bypass permissions");
        assert_eq!(t.get_state(), AgentState::Ready);
        // Now wait enough time for passive hold (5s) then feed idle pattern
        t.since = Instant::now() - Duration::from_secs(6);
        t.feed("❯");
        assert_eq!(t.get_state(), AgentState::Idle);
    }

    // ── Additional edge cases ───────────────────────────────────────────

    #[test]
    fn context_full_instant_transition() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Thinking, 0);
        t.feed("compacting context");
        assert_eq!(t.get_state(), AgentState::ContextFull);
    }

    #[test]
    fn auth_error_instant_transition() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Idle, 0);
        t.feed("API key invalid");
        assert_eq!(t.get_state(), AgentState::AuthError);
    }

    #[test]
    fn permission_prompt_higher_than_thinking() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Thinking, 0);
        // PermissionPrompt (priority 8) > Thinking (priority 6) — instant
        t.feed("Allow once");
        assert_eq!(t.get_state(), AgentState::PermissionPrompt);
    }

    #[test]
    fn set_restarting_transitions_state() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Thinking, 5);
        t.set_restarting();
        assert_eq!(t.get_state(), AgentState::Restarting);
    }

    #[test]
    fn error_state_is_error() {
        assert!(AgentState::ContextFull.is_error());
        assert!(AgentState::RateLimit.is_error());
        assert!(AgentState::UsageLimit.is_error());
        assert!(AgentState::AuthError.is_error());
        assert!(AgentState::ApiError.is_error());
        assert!(AgentState::Crashed.is_error());
        assert!(AgentState::Restarting.is_error());
        assert!(!AgentState::Thinking.is_error());
        assert!(!AgentState::Idle.is_error());
        assert!(!AgentState::Starting.is_error());
    }

    #[test]
    fn awaiting_operator_not_error() {
        // AwaitingOperator means "needs human keystrokes", not a failure mode.
        assert!(!AgentState::AwaitingOperator.is_error());
        assert!(!AgentState::AwaitingOperator.is_unavailable());
    }

    #[test]
    fn awaiting_operator_priority_between_hang_and_ready() {
        // Hang < AwaitingOperator < Ready so it preempts Starting/Hang in
        // tab-bar highest-priority display but doesn't outrank real activity.
        assert!(AgentState::Hang.priority() < AgentState::AwaitingOperator.priority());
        assert!(AgentState::AwaitingOperator.priority() < AgentState::Ready.priority());
    }

    #[test]
    fn awaiting_operator_display_name() {
        assert_eq!(
            AgentState::AwaitingOperator.display_name(),
            "awaiting_operator"
        );
    }

    #[test]
    fn set_awaiting_operator_from_starting() {
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Starting, 5);
        t.set_awaiting_operator();
        assert_eq!(t.current, AgentState::AwaitingOperator);
    }

    #[test]
    fn set_awaiting_operator_noop_from_non_starting() {
        // Only Starting transitions. All other states (including Ready) are
        // no-ops so late-firing tick-loop detections can't corrupt a healthy
        // mid-task agent. Known interactive prompts from Ready are caught
        // by pattern-based detection, not this time-based fallback.
        for s in [
            AgentState::Ready,
            AgentState::Idle,
            AgentState::Thinking,
            AgentState::ToolUse,
            AgentState::PermissionPrompt,
            AgentState::AwaitingOperator,
            AgentState::Crashed,
        ] {
            let mut t = tracker_at(&Backend::ClaudeCode, s, 10);
            t.set_awaiting_operator();
            assert_eq!(t.current, s, "state {:?} should be unchanged", s);
        }
    }

    #[test]
    fn ready_pattern_lifts_awaiting_operator() {
        // Once operator unblocks the stall and the ready banner fires,
        // transition() takes the usual higher-priority-wins path.
        let mut t = tracker_at(&Backend::ClaudeCode, AgentState::Starting, 5);
        t.set_awaiting_operator();
        assert_eq!(t.current, AgentState::AwaitingOperator);
        t.feed("bypass permissions");
        assert_eq!(t.current, AgentState::Ready);
    }

    // ── Full pipeline: PTY bytes → VTerm → screen → StateTracker ────────
    //
    // Exercises the production path `agent::pty_read_loop` takes: push
    // raw bytes (with ANSI escapes) through the vterm, pull tail_lines of
    // the screen, feed to state. Without these, unit tests can drift from
    // how detection actually behaves once vterm rendering is involved —
    // wrapped lines, cleared screens, scroll-off, etc.

    use crate::vterm::VTerm;

    /// Drive one full PTY cycle: process bytes, snapshot screen, feed state.
    fn drive(vterm: &mut VTerm, state: &mut StateTracker, bytes: &[u8]) {
        vterm.process(bytes);
        let rows = vterm.rows() as usize;
        let screen = vterm.tail_lines(rows);
        state.feed(&screen);
    }

    #[test]
    fn pipeline_claude_ready_via_vterm() {
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
        // Bytes include ANSI colors — vterm must resolve them so screen
        // text is plain and pattern can match.
        drive(
            &mut vt,
            &mut st,
            b"\x1b[1;32mClaude Code\x1b[0m ready (bypass permissions mode)\r\n",
        );
        assert_eq!(st.get_state(), AgentState::Ready);
    }

    #[test]
    fn pipeline_codex_ready_via_vterm() {
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::Codex));
        drive(&mut vt, &mut st, b"\x1b[1mOpenAI Codex\x1b[0m v0.120.0\r\n");
        assert_eq!(st.get_state(), AgentState::Ready);
    }

    #[test]
    fn pipeline_dismiss_drops_stale_pattern() {
        // Regression for the bug this whole refactor exists to fix: an
        // interactive prompt shows up, pattern matches; operator dismisses;
        // screen re-renders without the prompt text; state must re-evaluate
        // and fall back — NOT stay wedged on the stale buffered text.
        //
        // Uses codex-style flow: usage-limit banner (high priority) appears,
        // then a clear-screen + ready banner re-renders.
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::Codex));

        // Step 1: screen shows usage limit
        drive(
            &mut vt,
            &mut st,
            b"You've hit your usage limit. Try again at 10:00 AM.\r\n",
        );
        assert_eq!(st.get_state(), AgentState::UsageLimit);

        // Step 2: clear screen + fresh banner (simulates user re-auth / reset)
        // \x1b[2J clears screen, \x1b[H moves cursor home.
        // Advance `since` so the lower-priority Ready transition clears the
        // active-hold gate (UsageLimit is an error state; leaving requires
        // active hold 2s).
        st.since = std::time::Instant::now() - std::time::Duration::from_secs(3);
        drive(&mut vt, &mut st, b"\x1b[2J\x1b[HOpenAI Codex v0.120.0\r\n");
        assert_eq!(
            st.get_state(),
            AgentState::Ready,
            "after screen clear + ready banner, stale UsageLimit must release"
        );
    }

    #[test]
    fn pipeline_screen_unchanged_preserves_silence_timer() {
        // Cursor-blink-like bytes (show/hide cursor) must not bump
        // last_output since they leave the rendered grid unchanged.
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::Codex));
        drive(&mut vt, &mut st, b"OpenAI Codex v0.120.0\r\n");
        let before = st.last_output;
        std::thread::sleep(Duration::from_millis(20));
        // Cursor hide/show — visible grid unchanged.
        drive(&mut vt, &mut st, b"\x1b[?25l\x1b[?25h");
        assert_eq!(
            st.last_output, before,
            "cursor-visibility toggles must not reset silence timer"
        );
    }

    #[test]
    fn pipeline_usage_limit_instant_from_idle() {
        // Claude flow: agent sitting at idle prompt, then rate-limit burst
        // arrives. Error state must win immediately (no hysteresis).
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
        drive(
            &mut vt,
            &mut st,
            b"bypass permissions\r\n> ready\r\n\xe2\x9d\xaf",
        );
        assert!(matches!(
            st.get_state(),
            AgentState::Ready | AgentState::Idle
        ));
        drive(
            &mut vt,
            &mut st,
            b"\r\n\x1b[31m429 rate limit exceeded\x1b[0m\r\n",
        );
        assert_eq!(st.get_state(), AgentState::RateLimit);
    }

    #[test]
    fn pipeline_codex_update_menu_is_interactive_prompt() {
        // Regression for BUG 2: codex launches with an "Update available!"
        // modal overlaid on the usual ready banner. Without a dedicated
        // pattern, the Ready pattern still matched the banner and the
        // operator saw a "ready" pane that silently ignored input —
        // really it was waiting on the modal.
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::Codex));
        // Banner + modal rendered together on startup.
        drive(
            &mut vt,
            &mut st,
            b"OpenAI Codex v0.120.0\r\nUpdate available! Press enter to continue\r\n",
        );
        assert_eq!(st.get_state(), AgentState::InteractivePrompt);
    }

    #[test]
    fn interactive_prompt_notice_armed_on_entry_and_dedupes() {
        // Fresh tracker (state = Starting) must not claim a pending notice —
        // we only arm after a real transition INTO InteractivePrompt.
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::Codex));
        assert!(!st.take_interactive_prompt_notice());

        // Codex update menu → InteractivePrompt. First take fires, second
        // is debounced so supervisor ticks don't spam Telegram.
        drive(
            &mut vt,
            &mut st,
            b"OpenAI Codex v0.120.0\r\nUpdate available! Press enter to continue\r\n",
        );
        assert_eq!(st.get_state(), AgentState::InteractivePrompt);
        assert!(st.take_interactive_prompt_notice(), "first entry must arm");
        assert!(
            !st.take_interactive_prompt_notice(),
            "subsequent ticks within the same InteractivePrompt must not re-arm"
        );
    }

    #[test]
    fn interactive_prompt_notice_rearms_on_reentry() {
        // Dismiss → Ready → re-enter InteractivePrompt should re-arm the
        // notice so the operator is told again on the second modal.
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::Codex));
        drive(
            &mut vt,
            &mut st,
            b"OpenAI Codex v0.120.0\r\nUpdate available! Press enter to continue\r\n",
        );
        assert_eq!(st.get_state(), AgentState::InteractivePrompt);
        assert!(st.take_interactive_prompt_notice());

        // Simulate passive-hold window so InteractivePrompt can drop back.
        st.since = std::time::Instant::now() - std::time::Duration::from_secs(3);
        drive(&mut vt, &mut st, b"\x1b[2J\x1b[HOpenAI Codex v0.120.0\r\n");
        assert_eq!(st.get_state(), AgentState::Ready);
        assert!(
            !st.take_interactive_prompt_notice(),
            "no notice while Ready"
        );

        // Second modal appears.
        drive(
            &mut vt,
            &mut st,
            b"\x1b[2J\x1b[HOpenAI Codex v0.120.0\r\nUpdate available! Press enter to continue\r\n",
        );
        assert_eq!(st.get_state(), AgentState::InteractivePrompt);
        assert!(
            st.take_interactive_prompt_notice(),
            "re-entry after a leave must re-arm the notice"
        );
    }

    #[test]
    fn pipeline_codex_update_menu_dismiss_returns_to_ready() {
        // After the operator presses Enter, the modal text leaves the
        // screen. Screen re-renders with just the ready banner → detect
        // returns Ready, and after hysteresis the state drops from
        // InteractivePrompt (prio 7) back to Ready (prio 3).
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::Codex));
        drive(
            &mut vt,
            &mut st,
            b"OpenAI Codex v0.120.0\r\nUpdate available! Press enter to continue\r\n",
        );
        assert_eq!(st.get_state(), AgentState::InteractivePrompt);
        // Simulate passive-hold window elapsing so the downgrade can fire.
        st.since = std::time::Instant::now() - std::time::Duration::from_secs(3);
        // Clear screen, banner alone re-renders.
        drive(&mut vt, &mut st, b"\x1b[2J\x1b[HOpenAI Codex v0.120.0\r\n");
        assert_eq!(st.get_state(), AgentState::Ready);
    }

    #[test]
    fn recovery_notice_armed_when_leaving_interactive_prompt() {
        // Fresh tracker has nothing armed.
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::Codex));
        assert!(!st.take_recovery_notice());

        // Enter InteractivePrompt.
        drive(
            &mut vt,
            &mut st,
            b"OpenAI Codex v0.120.0\r\nUpdate available! Press enter to continue\r\n",
        );
        assert_eq!(st.get_state(), AgentState::InteractivePrompt);
        // Still nothing to report — we only arm when we LEAVE the blocked
        // state, not when we enter it.
        assert!(!st.take_recovery_notice());

        // Dismiss → Ready.
        st.since = std::time::Instant::now() - std::time::Duration::from_secs(3);
        drive(&mut vt, &mut st, b"\x1b[2J\x1b[HOpenAI Codex v0.120.0\r\n");
        assert_eq!(st.get_state(), AgentState::Ready);

        // First take fires; subsequent ticks within the same Ready don't
        // re-spam.
        assert!(st.take_recovery_notice(), "recovery must arm on exit");
        assert!(
            !st.take_recovery_notice(),
            "supervisor ticks after the first must not re-arm"
        );
    }

    #[test]
    fn recovery_notice_armed_when_leaving_awaiting_operator() {
        // AwaitingOperator → Ready goes through `transition()` with the
        // forced AwaitingOperator as the previous state, so the symmetric
        // arm path still fires.
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::Codex));
        st.set_awaiting_operator();
        assert_eq!(st.get_state(), AgentState::AwaitingOperator);
        assert!(!st.take_recovery_notice());

        // Fresh Ready banner appears. Ready (prio 3) > AwaitingOperator
        // (prio 2) so the transition is immediate.
        drive(&mut vt, &mut st, b"\x1b[2J\x1b[HOpenAI Codex v0.120.0\r\n");
        assert_eq!(st.get_state(), AgentState::Ready);
        assert!(
            st.take_recovery_notice(),
            "recovery must arm on AwaitingOperator → Ready"
        );
    }

    #[test]
    fn recovery_notice_not_armed_for_unrelated_transitions() {
        // Ready → Thinking → Ready must not arm the recovery notice: the
        // operator never saw a blocked state, so "ready again" is noise.
        let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
        st.current = AgentState::Ready;
        st.since = std::time::Instant::now() - std::time::Duration::from_secs(10);
        st.transition(AgentState::Thinking);
        assert_eq!(st.get_state(), AgentState::Thinking);
        st.since = std::time::Instant::now() - std::time::Duration::from_secs(10);
        st.transition(AgentState::Ready);
        assert_eq!(st.get_state(), AgentState::Ready);
        assert!(!st.take_recovery_notice());
    }

    #[test]
    fn gemini_tooluse_banner_match() {
        // Gemini 0.38.2 renders completed tool calls as `✓ <ToolName>
        // <target>`. Observed in gemini-tooluse.raw ~byte 143360.
        // Byte-level replay cannot surface the transition (Thinking
        // latches at ~16384 before the banner at ~143360, prio 6 >
        // prio 5), but production elapsed time clears the min_hold.
        let patterns = StatePatterns::for_backend(&Backend::Gemini);
        for sample in [
            "   ✓  ReadFile  Cargo.toml",
            "   ✓  WriteFile  /tmp/out.txt",
            "   ✓  Edit  Cargo.toml",
            "   ✓  Shell  ls -la",
            "   ✓  WebFetch  https://example.com",
        ] {
            assert_eq!(
                patterns.detect(sample),
                Some(AgentState::ToolUse),
                "expected ToolUse for {sample:?}"
            );
        }
    }

    #[test]
    fn codex_tooluse_title_line_match() {
        // Codex 0.120.0 emits `• Explored|Edited|Ran` as the title of a
        // tool-output block. Observed in codex-tooluse.raw ~byte 40960.
        let patterns = StatePatterns::for_backend(&Backend::Codex);
        for sample in ["• Explored", "• Edited", "• Ran"] {
            assert_eq!(
                patterns.detect(sample),
                Some(AgentState::ToolUse),
                "expected ToolUse for {sample:?}"
            );
        }
    }

    #[test]
    fn codex_tooluse_continuation_line_match() {
        // Continuation line under the title uses `└` (U+2514) + tool
        // verb — `└ Read README.md`, `└ Write /tmp/x`, etc.
        let patterns = StatePatterns::for_backend(&Backend::Codex);
        for sample in [
            "  └ Read README.md",
            "  └ Write /tmp/out.txt",
            "  └ Edit Cargo.toml",
            "  └ Ran apply_patch",
            "  └ List src/",
        ] {
            assert_eq!(
                patterns.detect(sample),
                Some(AgentState::ToolUse),
                "expected ToolUse for {sample:?}"
            );
        }
    }

    #[test]
    fn codex_tooluse_does_not_false_positive_on_spinner_or_narration() {
        // `• Working (...)` is the spinner; `• I'm reading ...` is
        // assistant narration. Neither should fire ToolUse — pattern
        // must only match the Explored/Edited/Ran titles and the `└`
        // continuation line. (Codex Thinking pattern is `Thinking`
        // which never matches the literal `Working` spinner, so these
        // lines simply fall through to lower-priority matches or None.)
        let patterns = StatePatterns::for_backend(&Backend::Codex);
        assert_ne!(
            patterns.detect("• Working (1s • esc to interrupt)"),
            Some(AgentState::ToolUse),
            "`• Working` spinner must not fire ToolUse"
        );
        assert_ne!(
            patterns.detect("• I'm reading README.md from the repo root"),
            Some(AgentState::ToolUse),
            "narration `• I'm reading ...` must not fire ToolUse"
        );
    }

    #[test]
    fn codex_permission_prompt_dialog_match() {
        // Codex 0.120.0 approval dialog — header, every option, and
        // footer must all fire PermissionPrompt. Observed in
        // codex-perm.raw byte ~68K-90K.
        let patterns = StatePatterns::for_backend(&Backend::Codex);
        for sample in [
            "  Would you like to run the following command?",
            "  1. Yes, proceed (y)",
            "› 3. No, and tell Codex what to do differently (esc)",
            "  Press enter to confirm or esc to cancel",
        ] {
            assert_eq!(
                patterns.detect(sample),
                Some(AgentState::PermissionPrompt),
                "expected PermissionPrompt for {sample:?}"
            );
        }
    }

    #[test]
    fn codex_permission_prompt_legacy_wording_still_matches() {
        // Keep compat with legacy / docs wording in case earlier Codex
        // builds or adjacent tooling surface through the same backend.
        let patterns = StatePatterns::for_backend(&Backend::Codex);
        for sample in ["Request approval", "approve", "deny"] {
            assert_eq!(
                patterns.detect(sample),
                Some(AgentState::PermissionPrompt),
                "legacy wording {sample:?} must still fire PermissionPrompt",
            );
        }
    }

    #[test]
    fn codex_permission_prompt_does_not_false_positive_on_narration() {
        // Narration lines that precede the dialog (`• I'm writing...`,
        // `outside the writable sandbox`, `escalated command`) must not
        // fire PermissionPrompt — they're assistant narration, not the
        // interactive dialog itself. We only assert they don't fire
        // PermissionPrompt (they may legitimately match other states).
        let patterns = StatePatterns::for_backend(&Backend::Codex);
        for sample in [
            "• I'm writing the requested content to /tmp/foo.txt.",
            "  writable sandbox, so I need to run one escalated command",
            "• Running printf 'hello' > /tmp/foo.txt",
        ] {
            assert_ne!(
                patterns.detect(sample),
                Some(AgentState::PermissionPrompt),
                "narration {sample:?} must NOT fire PermissionPrompt",
            );
        }
    }

    #[test]
    fn kiro_tooluse_banner_match() {
        // Kiro 2.0.1 renders tool banners as `● <Verb> <target>` — e.g.
        // `● Read .`, `● Write /tmp/out`. Observed in kiro-tooluse.raw
        // around byte 40960; byte-level replay cannot surface the
        // transition (Thinking fires first at ~25088 and ToolUse prio
        // is lower), but production elapsed time clears the min_hold.
        let patterns = StatePatterns::for_backend(&Backend::KiroCli);
        for sample in [
            "● Read .",
            "● Write /tmp/out.txt",
            "● Edit Cargo.toml",
            "● Bash ls -la",
            "● Grep TODO src/",
        ] {
            assert_eq!(
                patterns.detect(sample),
                Some(AgentState::ToolUse),
                "expected ToolUse for {sample:?}"
            );
        }
    }

    #[test]
    fn kiro_tooluse_legacy_internal_names_still_match() {
        // Preserve backwards-compat: old pattern alternation
        // (execute_bash|fs_read|fs_write) continues to match stack-trace
        // / error-surface output that may leak these internal tool IDs.
        let patterns = StatePatterns::for_backend(&Backend::KiroCli);
        for sample in ["execute_bash", "fs_read", "fs_write"] {
            assert_eq!(
                patterns.detect(sample),
                Some(AgentState::ToolUse),
                "expected ToolUse for {sample:?}"
            );
        }
    }

    #[test]
    fn pipeline_kiro_thinking_via_vterm() {
        // Regression for BUG 3: kiro-cli prints the literal word "Thinking"
        // during generation. The old pattern "Generating" never fired.
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::KiroCli));
        drive(&mut vt, &mut st, b"ask a question or describe a task\r\n");
        assert_eq!(st.get_state(), AgentState::Idle);
        drive(&mut vt, &mut st, b"Thinking...\r\n");
        assert_eq!(st.get_state(), AgentState::Thinking);
    }

    #[test]
    fn pipeline_kiro_slash_menu_does_not_trigger_context_full() {
        // Regression: kiro's slash-command autocomplete renders `/compact`
        // as one entry among many when the user types `/`. The old
        // ContextFull pattern `|/compact` matched that menu text and
        // wrongly reported the agent as ContextFull on every `/` keypress.
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::KiroCli));
        drive(&mut vt, &mut st, b"Trust All Tools active\r\n");
        assert_eq!(st.get_state(), AgentState::Ready);
        // User opens slash menu — `/compact` is listed alongside `/quit`.
        drive(
            &mut vt,
            &mut st,
            b"/quit    Quit the application\r\n/compact Compact context\r\n",
        );
        assert_ne!(
            st.get_state(),
            AgentState::ContextFull,
            "slash-menu listing /compact must not trigger ContextFull"
        );
    }

    #[test]
    fn pipeline_opencode_thinking_via_vterm() {
        // Regression for BUG 4: opencode never prints "Working". While a
        // request is in flight it draws `■⬝⬝⬝⬝⬝⬝⬝  esc interrupt` on its
        // bottom bar; that line disappears once streaming completes.
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::OpenCode));
        drive(
            &mut vt,
            &mut st,
            b"\xe2\x96\xa0\xe2\xac\x9d\xe2\xac\x9d\xe2\xac\x9d\xe2\xac\x9d  esc interrupt   tab agents\r\n",
        );
        assert_eq!(st.get_state(), AgentState::Thinking);
    }

    #[test]
    fn pipeline_gemini_thinking_via_vterm() {
        // Regression for BUG 1: matching on the bare word "Thinking"
        // latched the state permanently because chat history kept the
        // token visible. "esc to cancel" lives only on the active spinner
        // line and is overwritten when streaming completes.
        let mut vt = VTerm::new(120, 24);
        let mut st = StateTracker::new(Some(&Backend::Gemini));
        drive(&mut vt, &mut st, b"Type your message or @path/to/file\r\n");
        assert_eq!(st.get_state(), AgentState::Idle);
        drive(
            &mut vt,
            &mut st,
            b"\xe2\xa0\xa6 Thinking... (esc to cancel, 2s)\r\n",
        );
        assert_eq!(st.get_state(), AgentState::Thinking);
    }

    #[test]
    fn pipeline_gemini_chat_history_does_not_latch_thinking() {
        // Once the spinner line is gone, "Thinking" left behind in chat
        // history (e.g. the model's narrative) must NOT keep detect() in
        // Thinking. After hysteresis, screen should allow transition back
        // to Idle on "Type your message" prompt re-appearing.
        let mut vt = VTerm::new(120, 24);
        let mut st = StateTracker::new(Some(&Backend::Gemini));
        drive(
            &mut vt,
            &mut st,
            b"\xe2\xa0\xa6 Thinking... (esc to cancel, 2s)\r\n",
        );
        assert_eq!(st.get_state(), AgentState::Thinking);
        // Simulate hysteresis window closing — in production this is wall
        // time between reads, here we backdate `since` so the passive-hold
        // gate doesn't block the downgrade.
        st.since = std::time::Instant::now() - std::time::Duration::from_secs(3);
        // Clear screen, redraw with "I was thinking about..." in chat
        // history and a fresh prompt. "Thinking" alone would have latched;
        // now only the spinner line matches — and it's gone.
        drive(
            &mut vt,
            &mut st,
            b"\x1b[2J\x1b[HI was thinking about your question. 2+2 = 4.\r\nType your message or @path/to/file\r\n",
        );
        assert_eq!(
            st.get_state(),
            AgentState::Idle,
            "stale 'Thinking' word in chat history must not latch"
        );
    }

    #[test]
    fn pipeline_opencode_ready_prompt_resolves_to_idle() {
        // OpenCode's input prompt "Ask anything" matches BOTH the Idle
        // pattern (listed first) and the Ready pattern, so first-match
        // returns Idle. This reflects the backend's pattern table as
        // currently configured — noted here so future tweaks don't
        // accidentally flip to Ready without intent. Semantically OK:
        // Idle is "waiting for input at an alive prompt", and that's
        // what the user sees when opencode is ready.
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::OpenCode));
        drive(
            &mut vt,
            &mut st,
            b"Ask anything   \xe2\x8c\x85 tab agents\r\n",
        );
        assert_eq!(st.get_state(), AgentState::Idle);
    }

    #[test]
    fn opencode_tooluse_banner_match() {
        // OpenCode 1.4.0 prefixes tool banners with `✱` (in-flight) or
        // `→` (completed) followed by the capitalized tool name.
        // Observed in tests/fixtures/state-replay/opencode-tooluse.raw
        // at ~30720 and ~61440; byte-level replay cannot surface the
        // transition (Thinking priority > ToolUse and the spinner fires
        // first), but production elapsed time clears the min_hold.
        let patterns = StatePatterns::for_backend(&Backend::OpenCode);
        for sample in [
            "   ✱ Glob \"README.md\" (1 match)",
            "   → Read README.md",
            "   ✱ Write src/lib.rs",
            "   → Edit Cargo.toml",
        ] {
            assert_eq!(
                patterns.detect(sample),
                Some(AgentState::ToolUse),
                "expected ToolUse for {sample:?}"
            );
        }
    }

    // ── Replay harness (empirical A/B test vs pre-Phase-1a) ─────────────
    // Driven by env vars so it works without cargo arg plumbing:
    //   REPLAY_FILE=/tmp/session.raw REPLAY_BACKEND=gemini \
    //     cargo test --release -- --ignored --nocapture replay_session
    #[test]
    #[ignore]
    #[allow(clippy::unwrap_used)]
    fn replay_session() {
        let path = std::env::var("REPLAY_FILE").expect("REPLAY_FILE env var required");
        let backend_name = std::env::var("REPLAY_BACKEND").unwrap_or_else(|_| "gemini".to_string());
        let chunk_size: usize = std::env::var("REPLAY_CHUNK")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(512);
        // REPLAY_DUMP_AT=N[,N...] dumps the visible vterm grid after the
        // chunk that crosses each byte offset. Used to inspect dialog /
        // banner wording mid-stream when a pattern fails to match.
        let mut dump_offsets: Vec<usize> = std::env::var("REPLAY_DUMP_AT")
            .ok()
            .map(|s| s.split(',').filter_map(|x| x.trim().parse().ok()).collect())
            .unwrap_or_default();
        dump_offsets.sort_unstable();
        let mut dump_idx = 0;
        let backend = match backend_name.as_str() {
            "gemini" => Backend::Gemini,
            "codex" => Backend::Codex,
            "claude" | "claude-code" => Backend::ClaudeCode,
            "kiro" | "kiro-cli" => Backend::KiroCli,
            "opencode" => Backend::OpenCode,
            other => panic!("unknown backend: {other}"),
        };

        let bytes = std::fs::read(&path).unwrap();
        let mut vt = VTerm::new(120, 40);
        let mut st = StateTracker::new(Some(&backend));
        let mut transitions: Vec<(usize, AgentState)> = vec![(0, st.current)];

        let mut total = 0usize;
        for chunk in bytes.chunks(chunk_size) {
            total += chunk.len();
            vt.process(chunk);
            let rows = vt.rows() as usize;
            let screen = vt.tail_lines(rows);
            st.feed(&screen);
            let last = transitions.last().map(|x| x.1);
            if last != Some(st.current) {
                transitions.push((total, st.current));
            }
            while dump_idx < dump_offsets.len() && total >= dump_offsets[dump_idx] {
                eprintln!(
                    "--- screen at byte {} (requested {}) ---",
                    total, dump_offsets[dump_idx]
                );
                for (i, line) in screen.lines().enumerate() {
                    let t = line.trim_end();
                    if !t.is_empty() {
                        eprintln!("  {:>2}| {}", i + 1, t);
                    }
                }
                dump_idx += 1;
            }
        }

        eprintln!(
            "[POST-Phase-1a] file={} backend={} bytes={} chunk={}",
            path,
            backend_name,
            bytes.len(),
            chunk_size
        );
        eprintln!("Transitions (byte_offset → state):");
        for (off, s) in &transitions {
            eprintln!("  {:>8} → {:?}", off, s);
        }
        eprintln!("Final state: {:?}", st.current);

        // Probe: what does pattern.detect return on the final screen?
        // This bypasses hysteresis — tells us the "if time had passed, would
        // we transition?" answer, which is what matters in production.
        let patterns = StatePatterns::for_backend(&backend);
        let final_screen = vt.tail_lines(vt.rows() as usize);
        eprintln!(
            "Final detect() on screen: {:?}",
            patterns.detect(&final_screen)
        );

        // Simulated production pacing: backdate `since` by 10s so any
        // pending downward transition can fire, then re-feed.
        st.since = std::time::Instant::now() - std::time::Duration::from_secs(10);
        // Force re-detection by bumping the screen hash (clear it).
        // We can't easily clear the private field here; just feed a tiny
        // visible diff and then the real screen to force two detects.
        vt.process(b"\n");
        st.feed(&vt.tail_lines(vt.rows() as usize));
        st.since = std::time::Instant::now() - std::time::Duration::from_secs(10);
        st.feed(&vt.tail_lines(vt.rows() as usize));
        eprintln!("After simulated +10s pacing: {:?}", st.current);

        eprintln!("--- final tail_lines(40) ---");
        let screen = vt.tail_lines(40);
        for (i, line) in screen.lines().enumerate() {
            let t = line.trim_end();
            if !t.is_empty() {
                eprintln!("  {:>2}| {}", i + 1, t);
            }
        }
    }

    // ── Phase 1e: manifest-driven replay regression ─────────────────────
    //
    // Feeds each recorded PTY session through vterm + StateTracker and
    // asserts the observed transition sequence matches the expected list
    // in MANIFEST.yaml exactly. Catches regressions when backend patterns
    // are changed without re-recording — any deviation from the baseline
    // sequence should be reviewed manually (either a legitimate pattern
    // improvement, or a regression).
    //
    // When a CLI version is updated, re-record the fixture, bump
    // `cli_version` + `recorded_on`, and regenerate the expected
    // transitions from a manual inspection of the ignored replay_session
    // output.

    #[derive(serde::Deserialize)]
    struct ReplayManifest {
        fixtures: Vec<ReplayFixture>,
    }

    #[derive(serde::Deserialize)]
    struct ReplayFixture {
        file: String,
        backend: String,
        cli_version: String,
        #[allow(dead_code)]
        recorded_on: String,
        #[allow(dead_code)]
        scenario: String,
        expected_transitions: Vec<String>,
        expected_final_state: String,
        expected_final_detect: Option<String>,
    }

    fn parse_state(name: &str) -> AgentState {
        match name {
            "starting" => AgentState::Starting,
            "hang" => AgentState::Hang,
            "awaiting_operator" => AgentState::AwaitingOperator,
            "ready" => AgentState::Ready,
            "idle" => AgentState::Idle,
            "tool_use" => AgentState::ToolUse,
            "thinking" => AgentState::Thinking,
            "interactive_prompt" => AgentState::InteractivePrompt,
            "permission" => AgentState::PermissionPrompt,
            "context_full" => AgentState::ContextFull,
            "rate_limit" => AgentState::RateLimit,
            "usage_limit" => AgentState::UsageLimit,
            "auth_error" => AgentState::AuthError,
            "api_error" => AgentState::ApiError,
            "crashed" => AgentState::Crashed,
            "restarting" => AgentState::Restarting,
            other => panic!("unknown state name in manifest: {other}"),
        }
    }

    fn parse_backend(name: &str) -> Backend {
        match name {
            "claude" | "claude-code" => Backend::ClaudeCode,
            "kiro" | "kiro-cli" => Backend::KiroCli,
            "codex" | "codex-cli" => Backend::Codex,
            "opencode" | "opencode-cli" => Backend::OpenCode,
            "gemini" | "gemini-cli" => Backend::Gemini,
            other => panic!("unknown backend name in manifest: {other}"),
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn replay_manifest_regression() {
        let fixtures_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/state-replay");
        let manifest_path = fixtures_dir.join("MANIFEST.yaml");
        let raw = std::fs::read_to_string(&manifest_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", manifest_path.display()));
        let manifest: ReplayManifest =
            serde_yaml::from_str(&raw).unwrap_or_else(|e| panic!("parse MANIFEST.yaml: {e}"));

        assert!(
            !manifest.fixtures.is_empty(),
            "MANIFEST.yaml must list at least one fixture"
        );

        for f in &manifest.fixtures {
            let ctx = format!("{} ({} v{})", f.file, f.backend, f.cli_version);
            let fixture_path = fixtures_dir.join(&f.file);
            let bytes = std::fs::read(&fixture_path)
                .unwrap_or_else(|e| panic!("[{ctx}] read fixture: {e}"));

            let backend = parse_backend(&f.backend);
            let mut vt = VTerm::new(120, 40);
            let mut st = StateTracker::new(Some(&backend));
            let mut observed: Vec<AgentState> = vec![st.current];

            for chunk in bytes.chunks(512) {
                vt.process(chunk);
                let rows = vt.rows() as usize;
                let screen = vt.tail_lines(rows);
                st.feed(&screen);
                if observed.last().copied() != Some(st.current) {
                    observed.push(st.current);
                }
            }

            let expected: Vec<AgentState> = f
                .expected_transitions
                .iter()
                .map(|s| parse_state(s))
                .collect();
            assert_eq!(
                observed, expected,
                "[{ctx}] transition mismatch — pattern change or upstream CLI UI drift? \
                 observed {:?}, expected {:?}",
                observed, expected
            );

            let expected_final = parse_state(&f.expected_final_state);
            assert_eq!(
                st.current, expected_final,
                "[{ctx}] final state mismatch: got {:?}, expected {:?}",
                st.current, expected_final
            );

            let patterns = StatePatterns::for_backend(&backend);
            let final_screen = vt.tail_lines(vt.rows() as usize);
            let detect_result = patterns.detect(&final_screen);
            let expected_detect = f.expected_final_detect.as_deref().map(parse_state);
            assert_eq!(
                detect_result, expected_detect,
                "[{ctx}] final detect() mismatch: got {:?}, expected {:?}",
                detect_result, expected_detect
            );
        }
    }

    // -----------------------------------------------------------------
    // Track 1 PR-2: Heartbeat gate regression pins (design §7)
    // -----------------------------------------------------------------

    #[test]
    fn heartbeat_fresh_overrides_permission_prompt() {
        // Pin 1 — A5 incident scenario: agent has fresh heartbeat, PTY
        // shows permission pattern → must NOT latch PermissionPrompt.
        let mut t = tracker_at(&Backend::KiroCli, AgentState::Thinking, 5);
        t.update_heartbeat(Duration::from_secs(10)); // 10s ago = fresh
        t.feed("Allow this action y/n/t");
        assert_ne!(
            t.get_state(),
            AgentState::PermissionPrompt,
            "fresh heartbeat must suppress PermissionPrompt"
        );
        assert_eq!(t.get_state(), AgentState::Thinking);
    }

    #[test]
    fn stale_heartbeat_allows_permission_prompt() {
        // Pin 2 — stale heartbeat: agent silent for 200s, PTY shows
        // permission pattern → must latch PermissionPrompt normally.
        let mut t = tracker_at(&Backend::KiroCli, AgentState::Thinking, 5);
        t.update_heartbeat(Duration::from_secs(200)); // 200s ago > 120s
        t.feed("Allow this action y/n/t");
        assert_eq!(t.get_state(), AgentState::PermissionPrompt);
    }

    #[test]
    fn no_heartbeat_allows_permission_prompt() {
        // Pin 3 — no heartbeat ever: default behavior preserved.
        let mut t = tracker_at(&Backend::KiroCli, AgentState::Thinking, 5);
        // last_heartbeat is None by default
        t.feed("Allow this action y/n/t");
        assert_eq!(t.get_state(), AgentState::PermissionPrompt);
    }

    #[test]
    fn test_classify_fixtures() {
        use crate::health::BlockedReason;

        let fixtures: &[(&str, &Backend, Option<BlockedReason>)] = &[
            (
                "claude_429.txt",
                &Backend::ClaudeCode,
                Some(BlockedReason::RateLimit {
                    retry_after_secs: None,
                }),
            ),
            (
                "claude_quota.txt",
                &Backend::ClaudeCode,
                Some(BlockedReason::QuotaExceeded),
            ),
            (
                "kiro_throttle.txt",
                &Backend::KiroCli,
                Some(BlockedReason::RateLimit {
                    retry_after_secs: None,
                }),
            ),
            (
                "kiro_quota.txt",
                &Backend::KiroCli,
                Some(BlockedReason::QuotaExceeded),
            ),
            (
                "kiro_false_usage_limit.txt",
                &Backend::KiroCli,
                None, // false positive: agent discussing usage limits, not an error
            ),
            (
                "codex_quota.txt",
                &Backend::Codex,
                Some(BlockedReason::QuotaExceeded),
            ),
            (
                "gemini_resource_exhausted.txt",
                &Backend::Gemini,
                Some(BlockedReason::QuotaExceeded),
            ),
        ];

        let fixture_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/backend_error_fixtures");

        for (file, backend, expected) in fixtures {
            let path = fixture_dir.join(file);
            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("missing fixture {file}: {e}"));
            let result = super::classify_pty_output(backend, &content);
            assert_eq!(
                result, *expected,
                "fixture {file}: expected {expected:?}, got {result:?}"
            );
        }
    }

    // --- Sprint 34 PR-1: thinking pattern fixture tests ---

    #[test]
    fn claude_spinner_verb_triggers_thinking() {
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
        drive(&mut vt, &mut st, b"bypass permissions\r\n");
        assert_eq!(st.get_state(), AgentState::Ready);
        // Claude spinner uses random verbs, not "Thinking"
        drive(&mut vt, &mut st, b"Cogitating\xe2\x80\xa6\r\n");
        assert_eq!(
            st.get_state(),
            AgentState::Thinking,
            "claude spinner verb 'Cogitating' must trigger Thinking"
        );
    }

    #[test]
    fn claude_thought_for_triggers_thinking() {
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::ClaudeCode));
        drive(&mut vt, &mut st, b"bypass permissions\r\n");
        assert_eq!(st.get_state(), AgentState::Ready);
        // Post-thinking summary line
        drive(&mut vt, &mut st, b"thought for 12s\r\n");
        assert_eq!(
            st.get_state(),
            AgentState::Thinking,
            "claude 'thought for Ns' must trigger Thinking"
        );
    }

    #[test]
    fn kiro_working_triggers_thinking() {
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::KiroCli));
        drive(&mut vt, &mut st, b"Trust All Tools active\r\n");
        assert_eq!(st.get_state(), AgentState::Ready);
        // Kiro shows "Kiro is working" during generation
        drive(&mut vt, &mut st, b"Kiro is working\r\n  esc to cancel\r\n");
        assert_eq!(
            st.get_state(),
            AgentState::Thinking,
            "kiro 'Kiro is working' must trigger Thinking"
        );
    }

    #[test]
    fn codex_working_triggers_thinking() {
        let mut vt = VTerm::new(80, 24);
        let mut st = StateTracker::new(Some(&Backend::Codex));
        drive(&mut vt, &mut st, b"OpenAI Codex gpt-4.1 left\r\n");
        assert_eq!(st.get_state(), AgentState::Ready);
        // Codex shows "Working (Ns • esc to interrupt)"
        drive(
            &mut vt,
            &mut st,
            b"\xc2\xb7 Working (3s \xe2\x80\xa2 esc to interrupt)\r\n",
        );
        assert_eq!(
            st.get_state(),
            AgentState::Thinking,
            "codex 'Working' or 'esc to interrupt' must trigger Thinking"
        );
    }

    #[test]
    fn kiro_literal_thinking_in_chat_does_not_trigger() {
        // After pattern change, "Thinking" alone should NOT trigger on kiro
        let patterns = StatePatterns::for_backend(&Backend::KiroCli);
        let detected = patterns.detect("The agent was Thinking about the problem");
        assert_ne!(
            detected,
            Some(AgentState::Thinking),
            "literal 'Thinking' in chat must not trigger Thinking on kiro"
        );
    }
}
