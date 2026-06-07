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

// #1757's `is_net_error_match` red-anchor EXEMPTION was removed by
// t-coloranchor-remove-ratelimit: ServerRateLimit now uses the general content
// anchor (`in_error_line`) instead of the red anchor, so the net-error special
// case (net faults render grey → exempt from red) is subsumed — a grey net-error
// on an error-line-shaped line latches via content, and a bare prose mention is
// suppressed by content + #1518 position. The shared const above stays: it is
// still the ServerRateLimit DETECTION pattern (backend_profile + compile_for).

/// Does some on-screen occurrence of `matched` sit in a real backend ERROR LINE
/// (vs a bare prose/source mention of the same token)?
///
/// Generalized from the #1768 net-error gate (was `net_error_in_error_line`) so
/// the content-shape signal can serve EVERY HIGH_FP state, not just net errors.
/// The #1757 net-error red-anchor exemption was too broad (fired for the token
/// ANYWHERE in the tail), so an agent merely *discussing* a net error mis-latched
/// `ServerRateLimit` → retry storm (codex's #1769 review). Gate on the token's
/// LINE looking like a real error line.
///
/// Error-line signatures (corpus-derived, `t-step1-detection-corpus`):
/// - labeled error — `\w*error\s*[:?]`: `Error:` / `API Error:` / `FetchError:` /
///   `api_error:` / `invalid_request_error:`. The `\w*` (not `\b`) fixes the
///   corpus bug where the `_error` underscore killed the old `\berror`
///   word-boundary, so `ModelUnsupported`'s `invalid_request_error:` never
///   qualified;
/// - labeled exception — `\w*exception\s*[:?]`: `ThrottlingException:` /
///   `ThrottlingError:` (AWS/Bedrock-style names kiro renders). The
///   `t-coloranchor-corpus-gate` rg-on-fixture check found kiro RateLimit
///   (`ThrottlingException: Rate exceeded …`) was the one color-gated content
///   false-negative — `Exception` has no `error` substring and the line carries
///   no `429`/JSON, so it failed every alternation above. The structured
///   `…Exception:` label is the same strong error-line signal as `…error:`; the
///   bare phrase `Rate exceeded` is deliberately NOT added — it is prose-ambiguous
///   (the #848/#854 class) and the real kiro error always carries the label;
/// - bare HTTP-status (gemini RateLimit lines with no `Error:` label):
///   `\b429\b` / `RESOURCE_EXHAUSTED` / `Too Many Requests` / `got status:`;
/// - structured-JSON error fields: `"type"|"code"|"status": "…error…"` /
///   `"reason": "…exceeded"`.
///
/// A bare prose/source mention (the const name `…NET_ERRORS:`, "the ECONNRESET
/// error happened" with no colon, "see issue 4290") does NOT qualify → stays
/// red-anchored. Checks every occurrence so a real error line alongside a prose
/// mention still qualifies. ⚠ This is the CONTENT-shape signal ONLY — it does NOT
/// change the red→content anchor decision (a follow-up pending the operator's
/// call on keeping/dropping the color signal); today it still gates only the
/// #1757 net-error exemption at its single call site.
pub(crate) fn in_error_line(screen_text: &str, matched: &str) -> bool {
    if matched.is_empty() {
        return false;
    }
    use std::sync::OnceLock;
    static ERROR_LINE: OnceLock<Regex> = OnceLock::new();
    let re = ERROR_LINE.get_or_init(|| {
        Regex::new(
            r#"(?i)(\b\w*error\s*[:?]|\b\w*exception\s*[:?]|\b429\b|resource_exhausted|too many requests|got status\s*:|"(type|code|status)"\s*:\s*"\w*error\w*"|"reason"\s*:\s*"[^"]*exceeded")"#,
        )
        .expect("error-line regex")
    });
    let mut search = 0;
    while let Some(rel) = screen_text[search..].find(matched) {
        let pos = search + rel;
        let line_start = screen_text[..pos].rfind('\n').map_or(0, |i| i + 1);
        let line_end = screen_text[pos..]
            .find('\n')
            .map_or(screen_text.len(), |i| pos + i);
        if re.is_match(&screen_text[line_start..line_end]) {
            return true;
        }
        search = pos + matched.len();
    }
    false
}

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

    /// #1768: if a working-state marker (`Thinking`/`ToolUse`) is rendered BELOW
    /// (more recently than) `error_match` in `text`, return that working state.
    ///
    /// A HIGH_FP error (e.g. `ServerRateLimit`) wins the priority race in
    /// `detect_with_match` even when it has scrolled up and the agent has RESUMED
    /// WORK below it — `ServerRateLimit` > `Thinking` by pattern order. If the
    /// agent's most-recent on-screen activity (the lowest Thinking/ToolUse marker)
    /// sits below the error, the agent recovered, so the caller lands on that
    /// working state instead of re-latching the stale error (which would keep
    /// re-injecting `continue` into a working agent — the #1768 retry storm). This
    /// extends the #1518 absolute-tail-position gate to "relative to the working
    /// marker" and never touches `clears_server_rate_limit_retry` (#1713). A
    /// genuinely-stuck error has NO in-flight working marker below it → `None`.
    ///
    /// Returns the winning `(state, marker_text)`. The matched marker substring is
    /// surfaced for the `#1809-srl-swallow-probe` instrumentation (formerly the
    /// Agy/Kiro-scoped `#1808-flaw2-probe`) so a caller can log *which* on-screen
    /// marker overrode a ServerRateLimit (to check empirically whether a static
    /// bottom-bar chrome — e.g. a bare `esc to cancel` — is masking a stuck
    /// throttle). The state-selection logic is unchanged (same
    /// filter + lowest-marker-below-error pick), so detection behavior is identical.
    pub(crate) fn working_state_below<'a>(
        &self,
        text: &'a str,
        error_match: &str,
    ) -> Option<(AgentState, &'a str)> {
        let err_pos = text.rfind(error_match)?;
        self.patterns
            .iter()
            .filter(|(s, _)| matches!(s, AgentState::Thinking | AgentState::ToolUse))
            .filter_map(|(s, re)| {
                re.find_iter(text)
                    .last()
                    .map(|m| (m.start(), *s, m.as_str()))
            })
            .filter(|(pos, _, _)| *pos > err_pos)
            .max_by_key(|(pos, _, _)| *pos)
            .map(|(_, s, marker)| (s, marker))
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
