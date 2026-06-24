//! #8: per-backend detection data, co-located.
//!
//! The team voted (3-1, ORIGINAL/per-variant-structs unanimously rejected) for
//! the `BackendProfile` data-bundle: co-locate the per-backend detection data
//! that previously lived scattered across FIVE `match backend` sites in three
//! files — `state::patterns::{for_backend, compile_for}` (patterns), `behavioral::
//! {config_for, config_for_productivity}` (behavioral + productivity), and
//! `state::StateTracker::new` (initial_state). One backend's full detection
//! profile is gathered into ONE `*_profile()` block here; a single trivial
//! dispatch `match` ([`profile`]) replaces the five.
//!
//! Production routes EVERY backend through these profiles: `for_backend` /
//! `config_for[_productivity]` / `StateTracker::new` all source from
//! [`profile`], which is TOTAL (`&'static BackendProfile`, no `Option`). The
//! data-bundle migration train (step-0 #1683 reroute → OpenCode #1686 → Codex
//! #1687 → ClaudeCode #1689 → delete-legacy) was each proven byte-identical to
//! legacy at its own merge; #1580 then retired the last legacy backend
//! (`Gemini`), deleting the legacy detection spine (`compile_for`,
//! `config_for_legacy`, `config_for_productivity_legacy`, `legacy_initial_state`,
//! the `_ =>` catch-alls) and the runtime drift-guards — the exhaustive `match`
//! in [`profile`] now statically guarantees every backend has a profile.
//! Detection correctness is pinned by the fixture-replay suite + per-backend
//! state tests.

use crate::backend::Backend;
use crate::behavioral::{BehavioralConfig, MarkerCacheId, ProductivityConfig};
use crate::state::AgentState;
use std::sync::OnceLock;

/// Whether a backend can passively report context-window usage — surfaced as the
/// LIST `context_provider` telemetry field (#2439). DERIVED at runtime from the
/// presence of a statusline `context_pattern` (see
/// [`crate::state::StateTracker::context_provider`]); it is NOT a separately stored
/// per-backend field, so it can never drift from the pattern table it summarizes.
/// Unlike `context_source`/`context_pct` (absent when there is no fresh reading),
/// this is ALWAYS reported: it tells a consumer whether the backend CAN report
/// context at all, distinct from "has a reading right now".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextProvider {
    /// The backend renders a status/footer line we can read a context% from (it
    /// declares a `context_pattern`): Claude, Kiro.
    StatusLine,
    /// The backend exposes no trustworthy passive context signal: Codex, OpenCode,
    /// Agy, and the shell/raw fallbacks. Its `context_pct` is honestly absent
    /// rather than a guess.
    Unavailable,
}

impl ContextProvider {
    /// Stable LIST/telemetry string. CONTRACT: external dashboards key off these
    /// values — do not rename without coordinating consumers.
    pub fn source_name(self) -> &'static str {
        match self {
            ContextProvider::StatusLine => "statusline",
            ContextProvider::Unavailable => "unavailable",
        }
    }
}

/// All per-backend detection data for one backend, co-located.
///
/// #8 Phase 2 step-0: now consumed by PROD — `StatePatterns::for_backend`,
/// `behavioral::config_for[_productivity]`, and `StateTracker::new` route
/// every backend through [`profile`] (#1580: the legacy path is gone).
pub struct BackendProfile {
    /// Raw `(state, regex)` detection patterns in priority order — compiled by
    /// the state machine via `StatePatterns::from_raw_patterns`.
    pub patterns: Vec<(AgentState, &'static str)>,
    pub behavioral: BehavioralConfig,
    pub productivity: ProductivityConfig,
    pub initial_state: AgentState,
    /// Context% telemetry: regex whose capture group 1 extracts the backend's
    /// self-reported context usage percent from the BOTTOM status rows of the
    /// rendered screen (`None` = backend displays no usable percent). Scanned
    /// only over the bottom status rows — NOT the error tail window — because
    /// agents routinely DISCUSS context% in conversation text (prose-FP).
    pub context_pattern: Option<&'static str>,
    /// #1947: prompt markers identifying the backend's INPUT line (and echoed /
    /// submitted user-message lines). An error pattern matched on a line whose
    /// trimmed start is one of these is operator-typed / quoted text, not CLI
    /// error output — the content anchor excludes it. Empty = no stable prompt
    /// prefix (opencode / agy): input-line exclusion honestly unavailable.
    pub input_line_markers: &'static [&'static str],
}

/// ClaudeCode context% — matches the fleet statusline's used-form as rendered
/// live (`Model: Fable 5 | Ctx Used: 61.0% | ⎇ branch | (+0,-0)`); the value
/// can be fractional. TODO(left-form): a default Claude Code install without a
/// statusline only shows `Context left until auto-compact: N%` near the
/// threshold — REMAINING semantics, needs inversion; deliberately not matched
/// in v1 (this fleet always runs the custom statusline).
pub const CLAUDE_CONTEXT_PATTERN: &str = r"(?i)\bctx\s+used:\s*(\d+(?:\.\d+)?)\s*%";

/// KiroCli context% — matches the footer as rendered live
/// (`Kiro · auto · ◔ 10%`). The ◔ glyph is kiro's context gauge and never
/// appears in prose.
pub const KIRO_CONTEXT_PATTERN: &str = r"◔\s*(\d+(?:\.\d+)?)\s*%";

/// The single dispatch: `Backend → &'static BackendProfile`, lazy-cached
/// (compile-once, mirroring `StatePatterns::for_backend`'s `OnceLock`). This
/// `match` is the ONE unavoidable flat-enum lookup; the per-backend DATA lives
/// in the builders below, one block each.
///
/// #1580: now total (no `Option`). Gemini was the last legacy backend; with it
/// retired EVERY variant has a profile, so the type encodes "every backend has a
/// profile" and the legacy detection spine is gone. The exhaustive `match`
/// replaces the old drift-guard runtime invariant.
pub fn profile(backend: &Backend) -> &'static BackendProfile {
    static AGY: OnceLock<BackendProfile> = OnceLock::new();
    static KIRO: OnceLock<BackendProfile> = OnceLock::new();
    static OPENCODE: OnceLock<BackendProfile> = OnceLock::new();
    static CODEX: OnceLock<BackendProfile> = OnceLock::new();
    static CLAUDE: OnceLock<BackendProfile> = OnceLock::new();
    // Shell + Raw share one profile — every legacy source treats `Shell | Raw(_)`
    // identically (empty patterns, default behavioral, generic productivity, Idle).
    static EMPTY: OnceLock<BackendProfile> = OnceLock::new();
    match backend {
        Backend::Agy => AGY.get_or_init(agy_profile),
        Backend::KiroCli => KIRO.get_or_init(kirocli_profile),
        Backend::OpenCode => OPENCODE.get_or_init(opencode_profile),
        Backend::Codex => CODEX.get_or_init(codex_profile),
        Backend::ClaudeCode => CLAUDE.get_or_init(claudecode_profile),
        Backend::Shell | Backend::Raw(_) => EMPTY.get_or_init(empty_profile),
    }
}

/// Agy — moved VERBATIM from the four legacy sites (patterns.rs:693,
/// behavioral.rs:98 + :459, state/mod.rs:619). The harness proves byte-identity.
fn agy_profile() -> BackendProfile {
    BackendProfile {
        patterns: vec![
            // #2236: agy's individual-quota wall. FIRST so it outranks the Idle
            // markers below (a quota-reached pane still renders "Type your
            // message" / "? for shortcuts" chrome → first-match must pick
            // UsageLimit, not Idle). Feeds #2233's quota-wedge escalate-once-latch
            // so a quota-stuck agy stops spamming the stuck-watchdog every 30min
            // (r5 motivating case: "Individual quota reached … Resets in 146h",
            // wedged 6 days). Phrases are agy UI chrome (not generic) → no FP on
            // an agent merely mentioning a quota in its output.
            (
                AgentState::UsageLimit,
                r"Individual quota reached|Contact your administrator to enable overages",
            ),
            // #2409: Gemini transient "high traffic" server error (agy/Antigravity).
            // ApiError (not ServerRateLimit): SRL is `is_high_fp_state` and needs a
            // content/red anchor (an error-line indicator like `Error:` / `429`),
            // which this banner lacks → SRL would false-negative. ApiError has no
            // anchor/position gate → a pattern match transitions immediately and the
            // #1697 quick-nudge injects `continue` once per episode — right for a
            // transient, retryable error.
            //
            // Match ONLY the specific "high traffic" phrase, NOT the generic
            // "try again in a minute" tail: ApiError is un-gated (and agy has no
            // `input_line_markers`), so a broad phrase would FP on an agent's own
            // prose (e.g. "I'll try again in a minute"). The real banner ("...are
            // experiencing high traffic right now, please try again in a minute.")
            // contains the specific phrase, so this matches it with zero detection
            // loss. (PR #2410 review.)
            (
                AgentState::ApiError,
                r"servers are experiencing high traffic",
            ),
            (
                AgentState::PermissionPrompt,
                r"Requesting permission for:|Do you trust the contents of this project|tab Amend · e edit command",
            ),
            (AgentState::Active, r"●\s+[A-Z][a-zA-Z]+\("),
            (AgentState::Active, r"esc to cancel"),
            (AgentState::Idle, r"\? for shortcuts"),
            (AgentState::Idle, r"Antigravity CLI|Type your message"),
        ],
        behavioral: BehavioralConfig {
            silence_thinking_ms: 3000,
            silence_idle_ms: 8000,
        },
        productivity: ProductivityConfig {
            markers: crate::behavioral::AGY_PRODUCTIVE_MARKERS,
            use_heartbeat: true,
            heartbeat_fresh_window_ms: 10_000,
            cache_id: Some(MarkerCacheId::Agy),
        },
        context_pattern: None,
        input_line_markers: &[],
        initial_state: AgentState::Starting,
    }
}

/// KiroCli — moved VERBATIM from the four legacy sites (patterns.rs:270,
/// behavioral.rs config_for + config_for_productivity, state/mod.rs initial).
/// The ServerRateLimit entry references the SAME shared const the legacy arm
/// does (`SERVER_RATE_LIMIT_NET_ERRORS`), so it stays byte-identical. The
/// harness proves it.
fn kirocli_profile() -> BackendProfile {
    BackendProfile {
        patterns: vec![
            (
                AgentState::AuthError,
                r"Not authenticated|AccessDenied|denied access",
            ),
            (
                AgentState::UsageLimit,
                r"ServiceQuotaExceeded|InsufficientModelCapacity|you have reached the limit",
            ),
            (
                AgentState::RateLimit,
                r"Too Many Requests|ThrottlingError|ThrottlingException|Rate exceeded|\b429\b",
            ),
            (
                AgentState::ServerRateLimit,
                crate::state::patterns::SERVER_RATE_LIMIT_NET_ERRORS,
            ),
            (
                AgentState::ContextFull,
                r"context window overflow|compacting context",
            ),
            (
                AgentState::PermissionPrompt,
                r"requires approval|ESC to close \| Tab to edit",
            ),
            (
                AgentState::GitConflict,
                r"Automatic merge failed; fix conflicts|CONFLICT \(content\)|Resolve all conflicts manually|Failed to merge submodule|Failed to merge in",
            ),
            (AgentState::Active, r"execute_bash|fs_read|fs_write"),
            (AgentState::Active, r"Kiro is working|esc to cancel"),
            (
                AgentState::Idle,
                r"\d+%\s*$|ask a question or describe a task",
            ),
            (AgentState::Idle, r"Trust All Tools active|/quit to exit"),
        ],
        behavioral: BehavioralConfig {
            silence_thinking_ms: 2500,
            silence_idle_ms: 7000,
        },
        productivity: ProductivityConfig {
            markers: crate::behavioral::KIRO_PRODUCTIVE_MARKERS,
            use_heartbeat: true,
            heartbeat_fresh_window_ms: 10_000,
            cache_id: Some(MarkerCacheId::Kiro),
        },
        context_pattern: Some(KIRO_CONTEXT_PATTERN),
        input_line_markers: &[">"],
        initial_state: AgentState::Starting,
    }
}

/// OpenCode (#8 Phase 2 step-1) — moved VERBATIM from the legacy sites
/// (patterns.rs `compile_for` Backend::OpenCode arm, behavioral.rs
/// `config_for_legacy` + `config_for_productivity_legacy`, managed→Starting
/// initial state). The ServerRateLimit entry references the SAME shared
/// `SERVER_RATE_LIMIT_NET_ERRORS` const and the markers reference the SAME
/// `OPENCODE_PRODUCTIVE_MARKERS` const the legacy arms use, so they stay
/// byte-identical. The harness proves it.
fn opencode_profile() -> BackendProfile {
    BackendProfile {
        patterns: vec![
            (
                AgentState::RateLimit,
                r"API rate limited \(429\)|Rate limited\. Quick retry|API rate limit exceeded",
            ),
            (
                AgentState::ServerRateLimit,
                crate::state::patterns::SERVER_RATE_LIMIT_NET_ERRORS,
            ),
            (
                AgentState::UsageLimit,
                r"Quota Limit Exceeded|monthly usage limit reached",
            ),
            (
                AgentState::ApiError,
                r"Error from provider:|request validation errors",
            ),
            (AgentState::ContextFull, r"ContextOverflow"),
            (
                AgentState::PermissionPrompt,
                r"Permission required|Allow once\s+Allow always\s+Reject",
            ),
            (
                AgentState::GitConflict,
                r"Automatic merge failed; fix conflicts|CONFLICT \(content\)|Resolve all conflicts manually|Failed to merge submodule|Failed to merge in",
            ),
            (
                AgentState::Active,
                r"✱\s+(Read|Write|Edit|Glob|Grep|Bash|List|Task)\b|~\s+(Reading|Writing|Editing|Searching|Listing|Globbing|Grepping)\b",
            ),
            (AgentState::Active, r"esc interrupt"),
            (
                AgentState::PermissionPrompt,
                r"Update Available|Skip\s+Confirm",
            ),
            (AgentState::Idle, r"Ask anything"),
            (AgentState::Idle, r"Ask anything|tab agents"),
            // #2020: a RESPAWNED opencode pane resuming a session renders NO
            // "Ask anything" placeholder (the input box is bare `┃` lines) —
            // the only stable idle chrome is the bottom statusline hint. Listed
            // LAST so every working/error pattern above wins first-match (the
            // statusline persists during Thinking/ToolUse/PermissionPrompt —
            // `esc interrupt` / tool markers / perm chrome match first; grid
            // validation = the opencode replay fixtures, #1559 discipline).
            // Without this, a restarted idle opencode agent never leaves
            // Starting and the startup-stall fallback forces a false
            // AwaitingOperator (live: fixup-reviewer-3, 3× on 2026-06-11).
            (AgentState::Idle, r"ctrl\+p commands"),
        ],
        behavioral: BehavioralConfig {
            silence_thinking_ms: 3000,
            silence_idle_ms: 8000,
        },
        productivity: ProductivityConfig {
            markers: crate::behavioral::OPENCODE_PRODUCTIVE_MARKERS,
            use_heartbeat: true,
            heartbeat_fresh_window_ms: 10_000,
            cache_id: Some(MarkerCacheId::OpenCode),
        },
        context_pattern: None,
        input_line_markers: &[],
        initial_state: AgentState::Starting,
    }
}

/// Codex (#8 Phase 2 step-2) — moved VERBATIM from the legacy sites (patterns.rs
/// `compile_for` Backend::Codex arm, behavioral.rs `config_for_legacy` +
/// `config_for_productivity_legacy`, managed→Starting). ServerRateLimit
/// references the SAME shared `SERVER_RATE_LIMIT_NET_ERRORS` const and the
/// markers the SAME `CODEX_PRODUCTIVE_MARKERS` const the legacy arms use → byte-
/// identical. Includes the #1634 ModelUnsupported pattern (HIGH_FP, #919 red-
/// anchor gated downstream — unchanged by this move). The harness proves it.
fn codex_profile() -> BackendProfile {
    BackendProfile {
        patterns: vec![
            // auth_error FP fix: drop the generic `api.?key` token (matched any
            // "api key" prose) — keep the env-var anchor + the real OpenAI
            // auth-error forms. AuthError is also red-anchored downstream.
            (
                AgentState::AuthError,
                r"OPENAI_API_KEY|Incorrect API key|invalid_api_key",
            ),
            (AgentState::UsageLimit, r"hit your usage limit|try again at"),
            (
                AgentState::RateLimit,
                r"rate_limit_exceeded|RateLimitError|hit your rate limit",
            ),
            (
                AgentState::ServerRateLimit,
                crate::state::patterns::SERVER_RATE_LIMIT_NET_ERRORS,
            ),
            (AgentState::ContextFull, r"ContextOverflow"),
            (
                AgentState::ModelUnsupported,
                r"invalid_request_error|model is not supported|Model metadata for .*? not found",
            ),
            (
                AgentState::PermissionPrompt,
                r"Would you like to run the following command\?|Press enter to confirm or esc to cancel|No, and tell Codex what to do differently",
            ),
            (
                AgentState::GitConflict,
                r"Automatic merge failed; fix conflicts|CONFLICT \(content\)|Resolve all conflicts manually|Failed to merge submodule|Failed to merge in",
            ),
            (AgentState::Active, r"Working|esc to interrupt"),
            (AgentState::Idle, r"›"),
            (AgentState::Idle, r"OpenAI Codex|gpt-.*left"),
        ],
        behavioral: BehavioralConfig {
            silence_thinking_ms: 3000,
            silence_idle_ms: 8000,
        },
        productivity: ProductivityConfig {
            markers: crate::behavioral::CODEX_PRODUCTIVE_MARKERS,
            use_heartbeat: true,
            heartbeat_fresh_window_ms: 10_000,
            cache_id: Some(MarkerCacheId::Codex),
        },
        context_pattern: None,
        input_line_markers: &["›"],
        initial_state: AgentState::Starting,
    }
}

/// ClaudeCode (#8 Phase 2 step-3) — moved VERBATIM from the legacy sites
/// (patterns.rs `compile_for` Backend::ClaudeCode arm, behavioral.rs
/// `config_for_legacy` + `config_for_productivity_legacy`, managed→Starting).
/// The two ServerRateLimit entries (one literal alternation + the shared
/// `SERVER_RATE_LIMIT_NET_ERRORS` const) and the unicode spinner/glyph patterns
/// (`⠋…`, `✓●⏺`, sparkle `✻✢✶✳✽`, `❯`, `\x{2026}`) are copied byte-for-byte;
/// markers reference the SAME `CLAUDE_PRODUCTIVE_MARKERS` const. Does NOT include
/// any model-unsupported pattern (#1646 is blocked-on-fixture, not in legacy).
/// The harness proves byte-identity.
fn claudecode_profile() -> BackendProfile {
    BackendProfile {
        patterns: vec![
            (
                // auth_error FP fix (operator-reported): the bare `API key` and
                // `unauthorized` tokens are generic English that any pane
                // reviewing/writing authz code (#2369) shows, so they misflagged
                // healthy agents. Anchor on the REAL Claude/Anthropic
                // auth-failure banners instead (+ the AuthError red-anchor below).
                AgentState::AuthError,
                r"Invalid API key|invalid x-api-key|authentication_error|authentication failed|OAuth token has expired|Please run /login|API Error: 40[13]\b",
            ),
            (
                AgentState::ServerRateLimit,
                r"Server is temporarily limiting requests|temporarily limiting.*not your usage|API Error: 5\d{2}\b|server-side issue.*temporary|API Error: Repeated 529 Overloaded|overloaded_error|api_error|timeout_error",
            ),
            (
                AgentState::ServerRateLimit,
                crate::state::patterns::SERVER_RATE_LIMIT_NET_ERRORS,
            ),
            (
                AgentState::RateLimit,
                r"API Error: Request rejected \(429\)|rate_limit_error|hit a rate limit",
            ),
            (
                AgentState::UsageLimit,
                r"You've hit your session limit|You've hit your weekly limit|You've hit your Opus limit|Credit balance is too low|credit_balance_too_low",
            ),
            (
                // #2090 P2: the broad `context.*(full|limit)` arm is replaced by a
                // char-BOUNDED same-context arm `context.{0,16}(full|limit)`. The
                // unbounded `.*` was the dominant hard-wrap-shadow false-positive
                // source: flattening the bottom-N rows joins them into one line, so
                // `.*` spanned the whole conversation blob and matched any "context"
                // + any later "full"/"limit" (agents discussing context/limits while
                // actively working — 1162 shadow records, ~97% FP). The {0,16} bound
                // keeps real same-context wording ("context window full",
                // "context window is full", "context limit") while killing the
                // cross-row/cross-sentence amplification. The concrete
                // `compacting context` arm is kept verbatim (zero FP, real signal).
                AgentState::ContextFull,
                r"compacting context|context.{0,16}(full|limit)",
            ),
            (
                AgentState::PermissionPrompt,
                r"Esc to cancel · Tab to amend|allow all edits during this session|Enter to confirm · Esc to cancel",
            ),
            (
                AgentState::GitConflict,
                r"Automatic merge failed; fix conflicts|CONFLICT \(content\)|Resolve all conflicts manually|Failed to merge submodule|Failed to merge in",
            ),
            (
                AgentState::Active,
                r"(?m)^(?:[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]\s+(?:Read|Bash|Edit|Write|Grep|Glob|Listing|Reading|Writing|Searching|Editing)|[✓●⏺]\s+(?:Listing|Reading|Writing|Searching|Editing))\b",
            ),
            (
                AgentState::Active,
                r"(?i)[✻✢✶✳✽*·]\s*\w+\x{2026}|\w+\x{2026}\s*\((?:\d+[smh]|running )|thought for [0-9]+s",
            ),
            (AgentState::Idle, r"❯"),
            (AgentState::Idle, r"bypass permissions"),
        ],
        behavioral: BehavioralConfig {
            silence_thinking_ms: 2000,
            silence_idle_ms: 6000,
        },
        productivity: ProductivityConfig {
            markers: crate::behavioral::CLAUDE_PRODUCTIVE_MARKERS,
            use_heartbeat: true,
            heartbeat_fresh_window_ms: 10_000,
            cache_id: Some(MarkerCacheId::Claude),
        },
        context_pattern: Some(CLAUDE_CONTEXT_PATTERN),
        input_line_markers: &["❯", ">"],
        initial_state: AgentState::Starting,
    }
}

/// Shell / Raw — moved VERBATIM (empty patterns, default behavioral, generic
/// productivity, Idle initial state).
fn empty_profile() -> BackendProfile {
    BackendProfile {
        patterns: vec![],
        behavioral: BehavioralConfig::default(),
        productivity: ProductivityConfig {
            markers: crate::behavioral::GENERIC_PRODUCTIVE_MARKERS,
            use_heartbeat: false,
            heartbeat_fresh_window_ms: 0,
            cache_id: Some(MarkerCacheId::Generic),
        },
        context_pattern: None,
        input_line_markers: &[],
        initial_state: AgentState::Idle,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod context_pattern_tests {
    use super::*;

    /// Pin which backends advertise a context% pattern: Claude + Kiro display
    /// a usable percent (live-verified 2026-06-10); the rest are honestly
    /// None (codex would require editing the user's global ~/.codex/config.toml
    /// to display one; opencode/agy show none).
    #[test]
    fn context_pattern_presence_per_backend() {
        // #1580: `profile()` is now TOTAL (returns `&BackendProfile`, not
        // `Option`) since Gemini — the last legacy backend — was retired.
        let has = |b: &Backend| profile(b).context_pattern.is_some();
        assert!(has(&Backend::ClaudeCode));
        assert!(has(&Backend::KiroCli));
        assert!(!has(&Backend::Codex));
        assert!(!has(&Backend::OpenCode));
        assert!(!has(&Backend::Agy));
        assert!(!has(&Backend::Shell));
        assert!(!has(&Backend::Raw("x".into())));
    }

    /// Both patterns compile and extract the percent (capture group 1) from
    /// the live-captured renders they were written against.
    #[test]
    fn context_patterns_capture_live_renders() {
        let claude = regex::Regex::new(CLAUDE_CONTEXT_PATTERN).unwrap();
        let caps = claude
            .captures("  Model: Fable 5 | Ctx Used: 61.0% | ⎇ fix/879 | (+0,-0)")
            .expect("claude live render matches");
        assert_eq!(&caps[1], "61.0");

        let kiro = regex::Regex::new(KIRO_CONTEXT_PATTERN).unwrap();
        let caps = kiro
            .captures("Kiro · auto · ◔ 10%        ~/.agend-terminal/workspace/kiro")
            .expect("kiro live render matches");
        assert_eq!(&caps[1], "10");
    }
}

#[cfg(test)]
mod opencode_resumed_idle_2020 {
    use super::*;

    /// #2020 live shape 1 (fixup-reviewer-3, 3× on 2026-06-11): the tail of a
    /// RESPAWNED opencode pane resuming a session — captured verbatim from the
    /// stuck agent's live pane. No "Ask anything" placeholder anywhere; the
    /// statusline hint is the only idle chrome. Must detect Idle, or the
    /// agent never leaves Starting and the stall fallback fires.
    const RESUMED_IDLE_TAIL: &str = "  ┃\n  ┃\n  ┃  Build · DeepSeek V4 Pro OpenCode Go\n\n          270.3K (27%) · $2.23  ctrl+p commands";

    #[test]
    fn resumed_idle_pane_detects_idle() {
        let patterns = crate::state::StatePatterns::for_backend(&Backend::OpenCode);
        assert_eq!(
            patterns.detect(RESUMED_IDLE_TAIL),
            Some(AgentState::Idle),
            "resumed-session idle pane (no 'Ask anything') must read Idle"
        );
    }

    /// Order pin: the statusline hint persists during WORK — a working pane
    /// (with `esc interrupt`) must still detect Thinking, because the
    /// working patterns precede the statusline Idle pattern (first match
    /// wins). The opencode replay fixtures re-verify this on full captures.
    #[test]
    fn working_pane_with_statusline_still_detects_thinking() {
        let working = format!("  working...  esc interrupt\n{RESUMED_IDLE_TAIL}");
        let patterns = crate::state::StatePatterns::for_backend(&Backend::OpenCode);
        assert_eq!(
            patterns.detect(&working),
            Some(AgentState::Active),
            "statusline must not outrank the working marker"
        );
    }
}

#[cfg(test)]
mod opencode_monthly_usage_limit_2026_06 {
    use super::*;

    /// Operator-captured 2026-06-16 from mlx-pair (OpenCode 1.15.13 / DeepSeek
    /// V4 Pro): a real monthly usage-limit wall the prior pattern
    /// (`Quota Limit Exceeded`) did not match, so the agent read as Idle and the
    /// #2233 quota-wedge escalate-once-latch never fired. Verbatim shape (the
    /// `monthly usage limit reached` phrase is the stable anchor; the reset
    /// countdown and truncation chrome vary).
    const MONTHLY_USAGE_LIMIT_PANE: &str = "\u{2b1d}monthly usage limit reached. It will reset in 11 days 20 hours. To continue usin... (click to ex\n retrying in ~1 week attempt #1";

    #[test]
    fn monthly_usage_limit_detects_usage_limit() {
        let patterns = crate::state::StatePatterns::for_backend(&Backend::OpenCode);
        assert_eq!(
            patterns.detect(MONTHLY_USAGE_LIMIT_PANE),
            Some(AgentState::UsageLimit),
            "operator-captured OpenCode 'monthly usage limit reached' must read UsageLimit (feeds the #2233 quota-wedge latch)"
        );
    }

    /// FP guard (#2236 lesson): the new alternation must not over-match ordinary
    /// output. A normal opencode idle pane (no quota wall) must still read Idle.
    #[test]
    fn normal_idle_not_misread_as_usage_limit() {
        let idle = "  \u{2503}\n  \u{2503}  Build \u{b7} DeepSeek V4 Pro OpenCode Go\n\n          270.3K (27%) \u{b7} $2.23  ctrl+p commands";
        let patterns = crate::state::StatePatterns::for_backend(&Backend::OpenCode);
        assert_eq!(
            patterns.detect(idle),
            Some(AgentState::Idle),
            "#2236: ordinary opencode idle pane must NOT be misread as UsageLimit"
        );
    }
}

#[cfg(test)]
mod agy_quota_2236 {
    use super::*;

    /// #2236: agy's individual-quota wall must classify as `UsageLimit` so it
    /// feeds #2233's quota-wedge escalate-once-latch (otherwise the stuck-
    /// watchdog re-pings every 30min — r5 wedged 6 days). Verbatim shape from the
    /// r5 live snapshot, WITH the Idle chrome ("Type your message") that co-
    /// renders on the quota pane — proving UsageLimit outranks Idle (first-match
    /// priority), which is why the pattern is ordered first in `agy_profile`.
    const AGY_QUOTA_PANE: &str = "  \u{26a0} Individual quota reached. Contact your administrator to enable overages. Resets in 146h\n\n  Type your message\n  ? for shortcuts";

    #[test]
    fn agy_quota_reached_detects_usage_limit() {
        let patterns = crate::state::StatePatterns::for_backend(&Backend::Agy);
        assert_eq!(
            patterns.detect(AGY_QUOTA_PANE),
            Some(AgentState::UsageLimit),
            "#2236: agy quota-reached pane must read UsageLimit (not Idle), feeding the #2233 latch"
        );
    }

    /// FP guard: a normal agy idle pane (no quota chrome) must still read Idle —
    /// the new UsageLimit pattern must not over-match ordinary output.
    #[test]
    fn agy_normal_idle_not_misread_as_usage_limit() {
        let idle = "  Antigravity CLI\n  Type your message\n  ? for shortcuts";
        let patterns = crate::state::StatePatterns::for_backend(&Backend::Agy);
        assert_eq!(
            patterns.detect(idle),
            Some(AgentState::Idle),
            "#2236: ordinary agy idle pane must NOT be misread as UsageLimit"
        );
    }
}

#[cfg(test)]
mod agy_apierror_2409 {
    use super::*;

    /// #2409: Gemini's transient "high traffic" banner must classify as `ApiError`
    /// so `process_error_recovery` fires the #1697 quick-nudge (`continue`) instead
    /// of leaving the agy agent wedged. Verbatim shape from the issue, WITH the agy
    /// Idle chrome ("Type your message" / "? for shortcuts") that co-renders — so
    /// this also proves ApiError outranks Idle (first-match priority; the pattern is
    /// ordered before Idle in `agy_profile`).
    const AGY_HIGH_TRAFFIC_PANE: &str = "  Our servers are experiencing high traffic right now, please try again in a minute.\n\n  Type your message\n  ? for shortcuts";

    #[test]
    fn agy_high_traffic_detects_api_error() {
        let patterns = crate::state::StatePatterns::for_backend(&Backend::Agy);
        assert_eq!(
            patterns.detect(AGY_HIGH_TRAFFIC_PANE),
            Some(AgentState::ApiError),
            "#2409: agy 'high traffic' banner must read ApiError (not Idle), feeding the #1697 quick-nudge"
        );
    }

    /// FP guard (PR #2410 review): the pattern matches ONLY the specific "high
    /// traffic" phrase, NOT the generic "try again in a minute" tail. ApiError is
    /// un-gated (not `is_high_fp_state`, no anchor/position gate, agy has no
    /// `input_line_markers`), so an agent's OWN prose mentioning "try again in a
    /// minute" must NOT trip a false ApiError + spurious `continue` nudge. This
    /// test is RED against the original broad `...|try again in a minute` alternation.
    const AGY_RETRY_PROSE_PANE: &str = "  I hit a transient upstream error, so I'll try again in a minute once it clears.\n\n  Type your message\n  ? for shortcuts";

    #[test]
    fn agy_retry_prose_not_misread_as_api_error() {
        let patterns = crate::state::StatePatterns::for_backend(&Backend::Agy);
        assert_eq!(
            patterns.detect(AGY_RETRY_PROSE_PANE),
            Some(AgentState::Idle),
            "#2409/PR2410: an agent's own 'try again in a minute' prose must NOT be misread as ApiError"
        );
    }
}
