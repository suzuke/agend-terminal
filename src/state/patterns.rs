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
// copy). (#1580: the legacy compile_for arm that also used it is now gone.)
//
// #1523 #1470-1: the alternation is wrapped in a scoped case-insensitive group
// `(?i:…)` so backend-rendered casing variants of the SAME literal phrase match
// — e.g. `Connection reset by peer` (capital C, the real Node/libc wording) hit
// the lowercased `connection reset` token, which the previous case-SENSITIVE
// compile silently missed. The loosening is FP-safe: ServerRateLimit is a
// `is_high_fp_state` so a const match still has to clear `in_error_line_excluding_input`
// (the token must sit on a real error-shaped line, not a prose mention) and the
// #1518 live-bottom-N position gate — case-folding only normalises the SAME
// token's casing, it does not widen what counts as the token. The `(?i:…)` group
// is scoped (not a leading `(?i)` flag) so it stays self-contained if the const
// is ever embedded in a larger pattern.
pub(crate) const SERVER_RATE_LIMIT_NET_ERRORS: &str = r"(?i:ECONNRESET|ETIMEDOUT|InvalidHTTPResponse|fetch failed|connection reset|socket hang up|proxy.*disconnect)";

// #1757's `is_net_error_match` red-anchor EXEMPTION was removed by
// t-coloranchor-remove-ratelimit: ServerRateLimit now uses the general content
// anchor (`in_error_line_excluding_input`) instead of the red anchor, so the net-error special
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
/// Does `text` contain a recognised error-line indicator (`…Error:`, `429`,
/// `resource_exhausted`, JSON error shapes, …)? Pure regex check over the WHOLE
/// `text` — the caller is responsible for scoping `text` (a single line in
/// [`in_error_line_excluding_input`]; a tight proximity window in the SRL hard-wrap fallback,
/// where a fully-flattened tail has no `\n` to line-scope on).
pub(crate) fn line_has_error_indicator(text: &str) -> bool {
    use std::sync::OnceLock;
    static ERROR_LINE: OnceLock<Regex> = OnceLock::new();
    let re = ERROR_LINE.get_or_init(|| {
        Regex::new(
            r#"(?i)(\b\w*error\s*[:?]|\b\w*exception\s*[:?]|\b429\b|resource_exhausted|too many requests|got status\s*:|"(type|code|status)"\s*:\s*"\w*error\w*"|"reason"\s*:\s*"[^"]*exceeded")"#,
        )
        .expect("error-line regex")
    });
    re.is_match(text)
}

/// #1947: true when `line` is the backend's INPUT line or an echoed/submitted
/// user-message line — trimmed, it starts with one of the backend's prompt
/// markers (claude `❯`, codex `›`, kiro `>`). An error pattern matched on such
/// a line is operator-typed / quoted text, not CLI error output (real error
/// blocks render bare at line start across the fixture corpus).
pub(crate) fn is_input_line(line: &str, input_markers: &[&str]) -> bool {
    let trimmed = line.trim_start();
    input_markers.iter().any(|m| trimmed.starts_with(m))
}

/// #1947: `in_error_line` with input-line exclusion — at least one occurrence
/// of `matched` must sit on an error-indicator line that is NOT an input /
/// user-message line. With empty `input_markers` this is exactly the original
/// content anchor (backends without a stable prompt prefix — opencode / agy —
/// keep the pre-#1947 behavior, honestly uncovered).
pub(crate) fn in_error_line_excluding_input(
    screen_text: &str,
    matched: &str,
    input_markers: &[&str],
) -> bool {
    if matched.is_empty() {
        return false;
    }
    let mut search = 0;
    while let Some(rel) = screen_text[search..].find(matched) {
        let pos = search + rel;
        let line_start = screen_text[..pos].rfind('\n').map_or(0, |i| i + 1);
        let line_end = screen_text[pos..]
            .find('\n')
            .map_or(screen_text.len(), |i| pos + i);
        let line = &screen_text[line_start..line_end];
        if line_has_error_indicator(line) && !is_input_line(line, input_markers) {
            return true;
        }
        search = pos + matched.len();
    }
    false
}

/// #1955: does ANY on-screen occurrence of `matched` sit on a line that is
/// NOT an input / user-message line? The UsageLimit anchor: the real banner
/// carries no error indicator (`⎿  You've hit your weekly limit · resets
/// 4am`), so the error-line content anchor would false-negative it —
/// input-line exclusion alone is the right-sized gate (fail-toward-detection:
/// anything not typed/quoted at the prompt still latches; prose mentions in
/// agent output are bounded by the position gate + working-marker override +
/// the #1955 release anchor).
pub(crate) fn any_match_off_input_lines(
    screen_text: &str,
    matched: &str,
    input_markers: &[&str],
) -> bool {
    if matched.is_empty() {
        return false;
    }
    let mut search = 0;
    while let Some(rel) = screen_text[search..].find(matched) {
        let pos = search + rel;
        let line_start = screen_text[..pos].rfind('\n').map_or(0, |i| i + 1);
        let line_end = screen_text[pos..]
            .find('\n')
            .map_or(screen_text.len(), |i| pos + i);
        if !is_input_line(&screen_text[line_start..line_end], input_markers) {
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
        static AGY: OnceLock<StatePatterns> = OnceLock::new();
        static GROK: OnceLock<StatePatterns> = OnceLock::new();
        static EMPTY: OnceLock<StatePatterns> = OnceLock::new();

        let lock = match backend {
            Backend::ClaudeCode => &CLAUDE,
            Backend::KiroCli => &KIRO,
            Backend::Codex => &CODEX,
            Backend::OpenCode => &OPENCODE,
            Backend::Agy => &AGY,
            Backend::GrokCli => &GROK,
            Backend::Shell | Backend::Raw(_) => &EMPTY,
        };
        // #1580: every backend now routes through its co-located BackendProfile —
        // `profile()` is total, the legacy `compile_for` fork is gone. The
        // per-backend `OnceLock` still owns the compile-once cache (the
        // `from_raw_patterns` call lives INSIDE `get_or_init`, so each variant
        // compiles exactly once).
        lock.get_or_init(|| {
            Self::from_raw_patterns(&crate::backend_profile::profile(backend).patterns)
        })
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

    /// #8 Phase 2: compile a raw `(state, regex)` list via the standard
    /// `Regex::new` pipeline. Step-0 makes this a PROD path —
    /// `for_backend` uses it to compile a backend's `BackendProfile`
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
///
/// #2044 (model-picker gap, deliberately NOT patched here): an OPERATOR-opened
/// runtime dialog like claude's `/model` picker (an arrow-key selection menu)
/// matches NEITHER this token set NOR the PermissionPrompt chrome, so it isn't
/// classified — and that's the right call for two reasons. (1) Adding a pattern
/// without a grid-validated capture of the real picker is the #1559 hazard (a
/// "remembered" string that's absent on the rendered grid); we have no such
/// capture. (2) The actual harm (#2044) was an injected dispatch SWALLOWED by
/// the open dialog — and the fix for that is the dialog-shape-AGNOSTIC
/// inject-delivery watchdog (`daemon::inject_delivery`): it detects a swallowed
/// inject via the absent `UserPromptSubmit` hook regardless of WHICH dialog
/// ate it, covering the model-picker and every future operator dialog at once.
/// Per-dialog pattern-chasing is strictly weaker. If a recognized picker state
/// is ever wanted in the badge, capture the real grid first (replay fixture).
pub(super) fn is_generic_startup_prompt(text: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        // Case-insensitive so `(Y/n)` etc. hit the same token set.
        Regex::new(r"(?i)\(y/n\)|\(yes/no\)|\[y/n\]|press\s+(enter|return|any\s+key)")
            .expect("generic startup prompt regex compiles")
    });
    re.is_match(text)
}
