use super::AgentState;
use crate::backend::Backend;
use regex::Regex;

/// #1587: the ServerRateLimit network-error alternation, deduped from the
/// byte-identical copy that previously lived in ALL 5 backend pattern blocks
/// (claude/kiro/codex/opencode/gemini — same copy-paste-the-bug family as
/// #1639/#1642; one edit now applies uniformly). Sourced from #1136's
/// operator-observed proxy/network faults (`InvalidHTTPResponse`,
/// `ECONNRESET`/`ETIMEDOUT`, `fetch failed`, `connection reset`) plus the
/// realistic Node messages `socket hang up` / `proxy disconnect`.
///
/// #1587 deliberately does NOT blind-tighten these tokens. ServerRateLimit is
/// already `is_high_fp_state`, so the #919 red-SGR anchor + #1518 live-bottom-N
/// gate apply, and #1586 eliminated the retry-storm — the residual is ≤1
/// red-gated spurious `continue`. Tightening risks false-NEGATIVES: the English
/// phrases ARE the Node backends' literal error messages, and the real
/// rendering carries an `Error:`/`FetchError:` label prefix that a line-start
/// anchor would reject (cf. the `detect("Error: ECONNRESET")` test). A proper
/// tighten would need captured per-backend network-error fixtures — a deferred
/// follow-up only if the single FP ever becomes a real problem.
///
/// The bare `network error` token WAS dropped: it was never in #1136's observed
/// list (an unverified guess), is the most prose-FP-prone of the set, and no
/// backend was observed emitting it literally — so dropping it is
/// false-negative-free (the #1136 coverage test never listed it either).
// #8 Phase 2 (KiroCli migration): pub(crate) so the co-located BackendProfile
// references this shared net-error alternation by the SAME const (not a re-typed
// copy), keeping byte-identity with the legacy compile_for arm that also uses it.
pub(crate) const SERVER_RATE_LIMIT_NET_ERRORS: &str = r"ECONNRESET|ETIMEDOUT|InvalidHTTPResponse|fetch failed|connection reset|socket hang up|proxy.*disconnect";

/// Compiled patterns for one backend.
pub struct StatePatterns {
    /// (state, regex) pairs in priority order (highest priority first).
    patterns: Vec<(AgentState, Regex)>,
}

impl StatePatterns {
    /// Pattern sources: [実測] = verified from real capture, [文件] = from docs/source, [推測] = estimated
    /// Tested versions: Claude v2.1.89, Codex v0.118.0, OpenCode v1.4.0, Gemini v0.37.1
    ///
    /// #1125 M3: cached per-backend via `OnceLock` — regex compilation
    /// happens once per variant per process, not on every call.
    #[allow(clippy::unwrap_used)] // patterns are const — compile failure is a code bug
    pub fn for_backend(backend: &Backend) -> &'static Self {
        use std::sync::OnceLock;
        static CLAUDE: OnceLock<StatePatterns> = OnceLock::new();
        static KIRO: OnceLock<StatePatterns> = OnceLock::new();
        static CODEX: OnceLock<StatePatterns> = OnceLock::new();
        static OPENCODE: OnceLock<StatePatterns> = OnceLock::new();
        static GEMINI: OnceLock<StatePatterns> = OnceLock::new();
        static AGY: OnceLock<StatePatterns> = OnceLock::new();
        static EMPTY: OnceLock<StatePatterns> = OnceLock::new();

        let lock = match backend {
            Backend::ClaudeCode => &CLAUDE,
            Backend::KiroCli => &KIRO,
            Backend::Codex => &CODEX,
            Backend::OpenCode => &OPENCODE,
            Backend::Gemini => &GEMINI,
            Backend::Agy => &AGY,
            Backend::Shell | Backend::Raw(_) => &EMPTY,
        };
        // #8 Phase 2 step-0: route migrated backends (profile() == Some) through
        // their co-located BackendProfile; un-migrated backends fall back to the
        // legacy `compile_for` match. `from_raw_patterns` compiles via the SAME
        // `Regex::new` pipeline as `compile_for`, and
        // `profile_patterns_byte_identical_to_legacy` proves the pattern sources
        // are identical — so this fork is byte-identical detection, not a change.
        // The per-backend `OnceLock` still owns the compile-once cache (the fork
        // lives INSIDE `get_or_init`, so each variant compiles exactly once).
        lock.get_or_init(|| match crate::backend_profile::profile(backend) {
            Some(p) => Self::from_raw_patterns(&p.patterns),
            None => Self::compile_for(backend),
        })
    }

    // #8 Phase 2: `pub(crate)` so the parity tests can reference the TRUE legacy
    // compilation directly — after the step-0 reroute, `for_backend` sources
    // migrated backends from the profile, so it can no longer stand in for
    // "legacy" in a parity assertion (that would be circular).
    #[allow(clippy::unwrap_used)]
    pub(crate) fn compile_for(backend: &Backend) -> Self {
        let patterns = match backend {
            // Gemini CLI v0.37.1
            Backend::Gemini => vec![
                // [docs] OAuth errors from API
                (
                    AgentState::AuthError,
                    r"OAuth not authenticated|OAuth expired|UNAUTHENTICATED|check API key",
                ),
                // #848 PR-B narrowed bare `\b429\b` to specific
                // gemini-cli verbatim wordings (issues #10722/#8437/
                // #22545/#2305/#1502). #854 follow-up further drops
                // the residual bare `rate limit exceeded` alternation
                // — it was added as defensive insurance against a
                // "CLI casual wording" case that turned out to have
                // no empirical anchor in any of the cited issues, and
                // it false-positive-matched any prose / commit /
                // discussion containing the 3-word phrase (same class
                // as the pre-#848 bare `\b429\b` surface).
                //
                // All four remaining alternations are anchored on a
                // `429` numeric or on the distinctive camelCase gRPC
                // field `rateLimitExceeded`, so a real Gemini
                // RateLimit display still matches via 4× coverage:
                //
                // - `429 RESOURCE_EXHAUSTED` — numeric + gRPC enum
                // - `rateLimitExceeded` — single distinctive token
                // - `got status: 429` — verb-anchored numeric
                // - `429 Too Many Requests` — HTTP status-text numeric
                //
                // Negative regression fixture
                // `gemini-rate-limit-prose-discussion.raw` exercises
                // the false-positive class this drop closes; the
                // existing positive fixture
                // `gemini-rate-limit-typical.raw` exercises detection
                // survival on the canonical wording (still matches
                // 4× over after the drop). New positive fixture
                // `gemini-rate-limit-canonical-429.raw` exercises
                // detection on a minimal 429-anchored display that
                // does NOT include the dropped bare phrase, locking
                // in the post-#854 contract.
                (
                    AgentState::RateLimit,
                    r"429 RESOURCE_EXHAUSTED|rateLimitExceeded|got status: 429|429 Too Many Requests",
                ),
                // #1136: network errors — transient, auto-retry safe.
                (AgentState::ServerRateLimit, SERVER_RATE_LIMIT_NET_ERRORS),
                // [docs] Usage limit messages
                // #1125 M4: added `RESOURCE_EXHAUSTED` — the gRPC status
                // code for quota exhaustion. Positioned AFTER RateLimit so
                // the more specific `429 RESOURCE_EXHAUSTED` (transient)
                // matches first; bare `RESOURCE_EXHAUSTED` (permanent
                // quota) falls through to this pattern. Pre-#1125 this
                // was only in classify_pty_output's divergent regex set.
                (
                    AgentState::UsageLimit,
                    r"Usage limit reached|Access resets at|RESOURCE_EXHAUSTED",
                ),
                // [docs] Token/quota limit
                (AgentState::ContextFull, r"quota.*exceeded|token.*limit"),
                // #1559: gemini permission dialog chrome (operator capture
                // gemini-perm.raw v0.44.1, 2026-06-01). The prior [docs]
                // alternation `Allow once|Allow for this session|suggest changes`
                // matched the real dialog but its bare `suggest changes` is
                // ordinary code-review language (GitHub "suggest changes", any
                // review/self-analysis pane) → high content-FP. The live frame
                // is a boxed select: header `Allow execution of [<tool>]?` with
                // numbered options `1. Allow once` / `2. Allow for this session` /
                // `3. No, suggest changes (esc)`. Anchor the self-identifying
                // boxed header `Allow execution of` (prose never asks "Allow
                // execution of [X]?"); DROP the bare option words. Pair with
                // `gate_on_heartbeat` + the #1552 live-bottom-N escalation gate.
                (AgentState::PermissionPrompt, r"Allow execution of"),
                // Phase A Piece-1: git conflict output (backend-independent).
                (
                    AgentState::GitConflict,
                    r"Automatic merge failed; fix conflicts|CONFLICT \(content\)|Resolve all conflicts manually|Failed to merge submodule|Failed to merge in",
                ),
                // #1005 Phase A1 (CRITICAL): the gemini ToolUse
                // alternation was REMOVED. `✓` (U+2713 CHECK MARK) is
                // gemini's COMPLETION marker — every prior match
                // (`✓ ReadFile Cargo.toml`, etc.) was a historical
                // completion record, never an active execution. The
                // legacy `tool.*call|MCP.*tool` substrings (intended for
                // end-of-session "Tool Calls: 1 ✓ 1" summary + MCP
                // surfaces) ALSO matched post-completion — and were
                // broad enough to false-positive on prose / docs / commit
                // messages.
                //
                // Gemini's in-flight indicator is the `⠦ Thinking…
                // (esc to cancel, Ns)` spinner which fires Thinking
                // (line below). No replacement ToolUse pattern exists
                // in gemini's current rendering vocabulary.
                //
                // FOLLOW-UP RISK (dev-2 cross-audit flag): the existing
                // gemini-tooluse.raw fixture does NOT exercise an
                // in-flight tool-call shape distinct from Thinking. If
                // gemini 0.38.2+ adds a dedicated in-flight banner we
                // can detect, capture a new fixture + re-introduce a
                // narrow ToolUse pattern. Until then, gemini tool calls
                // surface as Thinking — operationally equivalent for
                // upstream consumers (active vs idle is the load-bearing
                // distinction).
                // [measured] Gemini's spinner line ("⠦ Thinking... (esc to
                // cancel, Ns)") only renders while a request is in flight and
                // is overwritten in place when streaming completes. Matching
                // the bare word "Thinking" previously latched the state and
                // never released — chat history kept the token visible on
                // screen and detect() kept returning Thinking forever.
                // F39 risk: prior bare `r"Thinking"` already narrowed to
                // `esc to cancel` (history note above). Same scrollback
                // stickiness surface as Kiro pattern; same Scenario A/B/C
                // taxonomy applies. Further narrow (e.g. require leading
                // Braille spinner) is a speculative quick-win for a separate
                // follow-up PR, not committed here.
                // See docs/HUNG-STATE-TRANSITIONS.md §F39.1
                (AgentState::Thinking, r"\(esc to cancel,"),
                // [measured] Input prompt text + YOLO mode (both = idle, ready
                // for input; the Ready/Idle split was collapsed into Idle).
                (AgentState::Idle, r"Type your message|YOLO"),
            ],
            _ => vec![],
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
    ///
    /// #919/#1450: production calls now go through `detect_with_match`
    /// (which returns the matched substring so the VTerm cell-color anchor
    /// can locate every on-screen occurrence of the phrase and check its
    /// rendered color); this thin wrapper is retained for tests + future
    /// callers that only need the state.
    #[allow(dead_code)] // Test seam + backward-compat; prod uses detect_with_match.
    pub fn detect(&self, text: &str) -> Option<AgentState> {
        self.detect_with_match(text).map(|(s, _)| s)
    }

    /// #8 Phase 2: compile a raw `(state, regex)` list exactly as `compile_for`
    /// does (same `Regex::new` pipeline). Step-0 makes this a PROD path —
    /// `for_backend` uses it to compile a migrated backend's `BackendProfile`
    /// patterns; the parity harness uses it too. (Un-gated from `#[cfg(test)]`
    /// when prod started consuming it.)
    #[allow(clippy::unwrap_used)]
    pub(crate) fn from_raw_patterns(raw: &[(AgentState, &'static str)]) -> Self {
        let compiled = raw
            .iter()
            .map(|(state, pat)| {
                let re = Regex::new(pat)
                    .unwrap_or_else(|e| panic!("BUG: invalid state regex {pat:?}: {e}"));
                (*state, re)
            })
            .collect();
        Self { patterns: compiled }
    }

    // #8 delete-legacy: `pattern_sources` removed — it was the parity harness's
    // legacy-source extractor; the four byte-identical tests that used it are
    // gone now that legacy is deleted (the profile is the source of truth).

    /// #919/#1450: detect + return the matched substring so callers can
    /// locate the phrase's rendered grid cells and check their foreground
    /// color (#1450 replaced the raw-byte SGR ring with VTerm cell color).
    /// Returns `Some((state, matched_text))` on first hit, `None` if no
    /// pattern matches. Matched text is the regex `Match::as_str()` slice
    /// (smallest match).
    pub fn detect_with_match<'a>(&self, text: &'a str) -> Option<(AgentState, &'a str)> {
        // Patterns are already in priority order (highest first)
        for (state, re) in &self.patterns {
            if let Some(m) = re.find(text) {
                return Some((*state, m.as_str()));
            }
        }
        None
    }
}

/// Classify PTY output into a [`BlockedReason`] for the given backend.
///
/// Returns `None` when the output does not match any known error pattern.
///
/// #1125 M4: delegates to [`StatePatterns::for_backend`] as the single
/// source of truth for per-backend regex patterns. The pre-#1125 version
/// maintained a separate set of `LazyLock` regexes that diverged from
/// `for_backend`'s canonical patterns (e.g. pre-#848 broad `rate.?limit`
/// survived here after `for_backend` narrowed it). Now both callers
/// — `StateTracker::feed()` and `classify_pty_output()` — run against
/// the same compiled patterns.
///
/// Stacking dep: production caller wired in S2-T4 (daemon watchdog).
pub fn classify_pty_output(
    backend: &crate::backend::Backend,
    output: &str,
) -> Option<crate::health::BlockedReason> {
    use crate::health::BlockedReason;

    let patterns = StatePatterns::for_backend(backend);
    let (state, _) = patterns.detect_with_match(output)?;
    match state {
        AgentState::UsageLimit => Some(BlockedReason::QuotaExceeded),
        AgentState::RateLimit | AgentState::ServerRateLimit => Some(BlockedReason::RateLimit {
            retry_after_secs: None,
        }),
        _ => None,
    }
}

/// Cheap structural test for a generic startup-time interactive prompt.
///
/// Only called while the agent is still in `Starting` state, so false
/// positives during Thinking/Idle (where model output might legitimately
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
pub(super) fn is_generic_startup_prompt(text: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        // Case-insensitive so `(Y/n)` etc. hit the same token set.
        Regex::new(r"(?i)\(y/n\)|\(yes/no\)|\[y/n\]|press\s+(enter|return|any\s+key)")
            .expect("generic startup prompt regex compiles")
    });
    re.is_match(text)
}
