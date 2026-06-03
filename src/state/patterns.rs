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
        static AGY: OnceLock<StatePatterns> = OnceLock::new();
        static EMPTY: OnceLock<StatePatterns> = OnceLock::new();

        let lock = match backend {
            Backend::ClaudeCode => &CLAUDE,
            Backend::KiroCli => &KIRO,
            Backend::Codex => &CODEX,
            Backend::OpenCode => &OPENCODE,
            Backend::Agy => &AGY,
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
pub(super) fn is_generic_startup_prompt(text: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        // Case-insensitive so `(Y/n)` etc. hit the same token set.
        Regex::new(r"(?i)\(y/n\)|\(yes/no\)|\[y/n\]|press\s+(enter|return|any\s+key)")
            .expect("generic startup prompt regex compiles")
    });
    re.is_match(text)
}
