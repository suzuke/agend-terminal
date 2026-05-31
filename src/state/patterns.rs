use super::AgentState;
use crate::backend::Backend;
use regex::Regex;

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
        lock.get_or_init(|| Self::compile_for(backend))
    }

    #[allow(clippy::unwrap_used)]
    fn compile_for(backend: &Backend) -> Self {
        let patterns = match backend {
            // Claude Code v2.1.89
            Backend::ClaudeCode => vec![
                // [docs] Claude Code SDK error handling
                (
                    AgentState::AuthError,
                    r"API key|authentication failed|unauthorized|API Error: 40[13]\b",
                ),
                // [docs] SDK retry logic for 429/overloaded
                // Sprint 31+ #4: word-boundary `429` to avoid false-positive
                // on substrings like "build #4290" / "request id: 4291...".
                // Server-side throttle (distinct from user usage limit) — auto-retry safe.
                // Issue #668: extend to cover generic 5xx server faults
                // ("API Error: 500/502/503/504") and the explicit
                // "server-side issue ... temporary" phrasing seen on
                // newer Claude Code SDK releases. All four are the same
                // class of transient upstream fault as the existing
                // "Server is temporarily limiting requests" message, so
                // they share the SERVER_RATE_LIMIT_MAX_RETRIES auto-retry
                // budget (cap is per-agent, not per-pattern — see
                // daemon::supervisor::process_server_rate_limit_retries).
                // `\b` after the 3-digit code rejects false positives
                // like "API Error: 5000123" (timestamp / request id).
                // #848: extend ServerRateLimit with Anthropic-docs verbatim
                // `error.type` field values (`overloaded_error`, `api_error`,
                // `timeout_error`) plus the canonical 529-overload CLI wording
                // (`API Error: Repeated 529 Overloaded errors`). All four are
                // the transient-upstream class the existing ServerRateLimit
                // pattern already covers; the new alternations close gaps
                // observed when the casual `overloaded` substring (which
                // pre-#848 lived in the RateLimit alternation as a broad
                // English-word match) is replaced by specific error phrases.
                (
                    AgentState::ServerRateLimit,
                    r"Server is temporarily limiting requests|temporarily limiting.*not your usage|API Error: 5\d{2}\b|server-side issue.*temporary|API Error: Repeated 529 Overloaded|overloaded_error|api_error|timeout_error|ECONNRESET|ETIMEDOUT|InvalidHTTPResponse|fetch failed|connection reset|socket hang up|network error|proxy.*disconnect",
                ),
                // #848: narrow Claude RateLimit to specific error phrases.
                // The pre-#848 pattern `r"overloaded|rate.?limit|\b429\b"`
                // matched the bare substring `rate_limit` / `rate-limit` /
                // `rate limit` / `overloaded` anywhere in PTY scrollback,
                // producing false-positive RateLimit classifications on
                // discussion prose (recursive dogfood — fleet agents
                // debugging the bug TRIGGERED the bug, see #841/#846 RCAs).
                // New pattern keys on canonical Anthropic phrasing only:
                // - `API Error: Request rejected (429) · this may be a
                //   temporary capacity issue` — verbatim Claude Code CLI
                //   wording for project-level 429 rejection.
                // - `rate_limit_error` — Anthropic API `error.type` field
                //   for HTTP 429 responses.
                // - `hit a rate limit` — observed Claude Code CLI casual
                //   wording from various SDK versions.
                // The legacy `\b429\b` bare-token alternation is intentionally
                // dropped — when a real 429 surfaces, the wrapping wording
                // is `Request rejected (429)` which still matches the first
                // alternation. False-positives from build numbers / request
                // ids stay protected (they never carry the wrapping wording).
                (
                    AgentState::RateLimit,
                    r"API Error: Request rejected \(429\)|rate_limit_error|hit a rate limit",
                ),
                // #848: Claude UsageLimit pattern (NEW — pre-#848 Claude
                // had no UsageLimit pattern at all, so subscription quota
                // errors fell through to whatever broader pattern matched).
                // Anthropic docs distinguish subscription quota (session /
                // weekly / Opus / credit balance — permanent until reset)
                // from rate limit (transient throttle). The four phrases
                // below are the verbatim Claude Code CLI subscription-quota
                // wordings. A "continue" recovery nudge (#841) cannot
                // resolve any of these — adding the explicit pattern
                // routes them to the permanent-error band so the nudge
                // gate excludes them via `AgentState::is_transient_error()`.
                (
                    AgentState::UsageLimit,
                    r"You've hit your session limit|You've hit your weekly limit|You've hit your Opus limit|Credit balance is too low|credit_balance_too_low",
                ),
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
                // Phase A Piece-1: git rebase/merge/cherry-pick conflict
                // output is identical regardless of which CLI invoked git,
                // so the same regex installs in every backend's pattern
                // list. The 5 alternations cover the canonical conflict
                // markers `git` emits to stdout/stderr.
                (
                    AgentState::GitConflict,
                    r"Automatic merge failed; fix conflicts|CONFLICT \(content\)|Resolve all conflicts manually|Failed to merge submodule|Failed to merge in",
                ),
                // #1005 Phase A1: ToolUse = active tool execution, NOT
                // historical completion record. Pre-fix regex matched
                // BOTH the braille spinner (`⠋ Listing`) AND the
                // completion-banner glyphs (`✓ Bash`, `● Read`, `⏺ Write`).
                // Latter caused priority oscillation against Idle/Ready:
                // `✓ Bash` stays in scrollback indefinitely → detect()
                // returns ToolUse → `LATCHED_STATE_EXPIRY` force-expire →
                // `since=now` reset → next feed re-detects → oscillation.
                // See issue #1005 + docs/HUNG-STATE-TRANSITIONS.md §F39.3.
                //
                // Fix splits the pattern into TWO alternations gated on
                // glyph-source:
                //   (a) braille spinner ⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ + any tool verb (bare
                //       or -ing). The braille glyph is an animation tick —
                //       only renders while the spinner is ACTIVE. Once the
                //       tool completes, the spinner stops and braille is
                //       cleared; no scrollback false-positive surface.
                //   (b) completion glyphs ✓●⏺ + ONLY -ing verbs
                //       (Listing|Reading|Writing|Searching|Editing). The
                //       glyph itself can persist in scrollback, but the
                //       -ing verb is the in-flight progress shape;
                //       completion banners use bare verbs (`✓ Bash`,
                //       `● Read`, `⏺ Write`) which (a) excludes by glyph
                //       class and (b) excludes by verb shape.
                //
                // Real claude 2.1.98 renders `⠋ Listing 1 directory…`
                // mid-flight; the bare `⏺ Read` form appears only AFTER
                // tool completion.
                (
                    AgentState::ToolUse,
                    r"(?m)^(?:[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]\s+(?:Read|Bash|Edit|Write|Grep|Glob|Listing|Reading|Writing|Searching|Editing)|[✓●⏺]\s+(?:Listing|Reading|Writing|Searching|Editing))\b",
                ),
                // #1541: verb-AGNOSTIC structural anchor for the Claude thinking
                // spinner. The old fix enumerated verbs (Cogitating, Bloviating,
                // …) but Claude's spinner rolls a *random* gerund every run
                // (Undulating, Julienning, Whisking, …). Any unlisted verb went
                // undetected → the snapshot recorded `idle` for a busy agent →
                // the dispatch-idle watchdog mis-fired. (claude-thinking.raw,
                // verb `Undulating`, is exactly that miss: it used to replay
                // `[starting, idle]` with no thinking at all.)
                //
                // ORDER: placed AFTER the ToolUse arm on purpose. Claude keeps
                // the sparkle spinner animating WHILE a tool runs, so a tool
                // frame (`⏺ Listing 1 directory…`) and a spinner frame
                // (`✳ Burrowing…`) coexist on screen. detect_with_match is
                // first-match-wins, so ToolUse must precede Thinking to stay
                // detectable — this keeps #1541 orthogonal to #1005 (a spinner
                // that is NOT a tool frame still falls through to Thinking).
                //
                // [measured] the spinner renders `<glyph> <Verb>… (<elapsed> · …)`
                // where:
                //   - <glyph> rolls through the SPARKLE family (✻ ✢ ✶ ✳ ✽) and
                //     also `*` / `·` — it animates, so do NOT hard-enumerate it.
                //     (NB: this is sparkle, NOT the braille `⠋⠙⠹` spinner — that
                //     is ToolUse, the arm directly above.)
                //   - <Verb>… ends in U+2026 (`…`), never the ASCII `...`.
                //   - the tail is usually ` (16m · thinking)` / ` (21m · ↑ N
                //     tokens)` (minutes OR seconds), occasionally `(running stop
                //     hook)`, occasionally absent.
                //
                // Anchor = U+2026 AND (leading sparkle/`*` glyph OR a structural
                // `(elapsed|running` tail). That double requirement is the prose
                // false-positive guard the verb list used to provide:
                //   - `Let me think…`         → U+2026 but no glyph/tail   → no
                //   - `Thinking...(7s)`       → ASCII `...`, not U+2026    → no
                //   - `Churned for 7m39s`     → past-tense completion, no `…` → no
                // `thought for Ns` stays a separate alternation (post-thinking
                // summary line has no spinner glyph). Backend-scoped to Claude —
                // codex/kiro/gemini keep their own arms (cross-backend negative
                // test in src/state/tests.rs).
                (
                    AgentState::Thinking,
                    r"(?i)[✻✢✶✳✽*·]\s*\w+\x{2026}|\w+\x{2026}\s*\((?:\d+[smh]|running )|thought for [0-9]+s",
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
                // #848 PR-B: extend AWS quota errors with the casual
                // `you have reached the limit` wording observed in Kiro #5876.
                // Additive — existing `ServiceQuotaExceeded` /
                // `InsufficientModelCapacity` alternations preserved.
                (
                    AgentState::UsageLimit,
                    r"ServiceQuotaExceeded|InsufficientModelCapacity|you have reached the limit",
                ),
                // [docs] HTTP 429 handling
                // Sprint 31+ #4: word-boundary `429` per Claude pattern.
                // #848 PR-B: add AWS Bedrock `ThrottlingException` and
                // generic `Rate exceeded` wording. Additive — Kiro's
                // pre-#848 pattern was already specific (no broad-substring
                // false-positive class to fix), so this commit only widens
                // coverage to more error wordings without reintroducing
                // the pre-#848 broadness seen on Claude/Codex/OpenCode.
                (
                    AgentState::RateLimit,
                    r"Too Many Requests|ThrottlingError|ThrottlingException|Rate exceeded|\b429\b",
                ),
                // #1136: network errors — transient, auto-retry safe.
                (
                    AgentState::ServerRateLimit,
                    r"ECONNRESET|ETIMEDOUT|InvalidHTTPResponse|fetch failed|connection reset|socket hang up|network error|proxy.*disconnect",
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
                // Phase A Piece-1: git conflict output (backend-independent).
                (
                    AgentState::GitConflict,
                    r"Automatic merge failed; fix conflicts|CONFLICT \(content\)|Resolve all conflicts manually|Failed to merge submodule|Failed to merge in",
                ),
                // #1005 Phase A1: drop the `● <Verb>` alternation — `●`
                // (U+25CF BLACK CIRCLE) is the COMPLETION banner per
                // dev-2 fixture inspection of kiro-tooluse.raw; matching
                // it caused the same priority oscillation class as the
                // claude bug (#1005). Kiro's in-flight indicator is the
                // `Kiro is working` spinner which fires Thinking (line
                // below) — no replacement ToolUse pattern needed for the
                // banner shape.
                //
                // Legacy internal tool-name alternation
                // (`execute_bash|fs_read|fs_write`) preserved — those
                // strings surface in stack traces and error output, not
                // in completion banners.
                (AgentState::ToolUse, r"execute_bash|fs_read|fs_write"),
                // [measured] Kiro shows "Kiro is working" + "esc to cancel"
                // during generation. Earlier "Thinking" pattern no longer
                // matches current kiro-cli versions.
                // F39 risk: pattern text persisting in scrollback can re-detect
                // when screen scrolls. Mitigated by transition() same-state
                // early-return + feed() hash-dedup for Scenarios A/B; Scenario C
                // (priority oscillation under conflicting patterns) is the
                // unaddressed bug surface.
                // See docs/HUNG-STATE-TRANSITIONS.md §F39.1
                (AgentState::Thinking, r"Kiro is working|esc to cancel"),
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
                // #848: narrow Codex RateLimit to specific OpenAI SDK error
                // wordings. The pre-#848 pattern matched the bare `rate_limit`
                // substring anywhere — same false-positive class as Claude
                // (recursive dogfood on agents discussing the issue itself).
                // New pattern uses OpenAI SDK conventional phrases:
                // - `rate_limit_exceeded` — OpenAI API error code field
                // - `RateLimitError` — OpenAI Python SDK exception class
                // - `hit your rate limit` — Codex CLI casual wording
                // Sprint 31+ #4's `\b429\b` boundary was protecting against
                // benign build numbers / request ids; the new pattern drops
                // the bare `429` token entirely (false-positives no longer
                // possible since the new alternations all carry distinctive
                // wrapping context).
                (
                    AgentState::RateLimit,
                    r"rate_limit_exceeded|RateLimitError|hit your rate limit",
                ),
                // #1136: network errors — transient, auto-retry safe.
                (
                    AgentState::ServerRateLimit,
                    r"ECONNRESET|ETIMEDOUT|InvalidHTTPResponse|fetch failed|connection reset|socket hang up|network error|proxy.*disconnect",
                ),
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
                // Phase A Piece-1: git conflict output (backend-independent).
                (
                    AgentState::GitConflict,
                    r"Automatic merge failed; fix conflicts|CONFLICT \(content\)|Resolve all conflicts manually|Failed to merge submodule|Failed to merge in",
                ),
                // #1005 Phase A1: dropped the `└ <Verb>` continuation
                // and `• <PastTense>` title-line alternations — both are
                // COMPLETION render forms (the past-tense verbs
                // `Explored|Edited|Ran` are unambiguously historical;
                // the `└` continuation lines render under the title
                // AFTER the tool finishes). Matching them caused the
                // same priority oscillation class as the claude bug
                // (#1005). Codex's in-flight indicator is the
                // `• Working (...)` spinner which fires Thinking — no
                // replacement ToolUse pattern needed.
                //
                // #1005 Phase A1 RC1 (reviewer #1009 verdict): the
                // legacy `r"apply_patch"` alternation was also REMOVED.
                // Pre-RC1 it was preserved as "error / stack-trace
                // surface" — but unanchored substring matched
                // `└ Ran apply_patch` / `• Ran apply_patch` in
                // completion-banner context, re-triggering the same
                // priority oscillation class the rest of this audit
                // closed. No fixture-backed active-only `apply_patch`
                // shape exists distinct from the completion banner;
                // until one surfaces, codex has no ToolUse pattern.
                // Errors / stack-traces should classify under a
                // dedicated state if needed (out of scope for #1005).
                // [measured] Codex shows "◦ Working (Ns • esc to interrupt)"
                // during generation. Both "Working" and "esc to interrupt"
                // are stable anchors.
                (AgentState::Thinking, r"Working|esc to interrupt"),
                // [measured] Prompt symbol + model info in status
                (AgentState::Idle, r"›"),
                // [measured] Version + model display
                (AgentState::Ready, r"OpenAI Codex|gpt-.*left"),
            ],
            // OpenCode v1.4.0
            Backend::OpenCode => vec![
                // #848 PR-B: narrow OpenCode RateLimit to specific
                // sst→anomalyco fork CLI wordings. The pre-#848 pattern
                // `r"rate.?limit|\b429\b"` matched bare `rate_limit` /
                // `rate-limit` / `rate limit` substrings anywhere — same
                // false-positive class as Claude/Codex pre-PR-A. New
                // pattern keys on:
                // - `API rate limited (429)` — observed in sst#8203 / #3525
                // - `Rate limited. Quick retry` — observed in sst#9091
                // - `API rate limit exceeded` — observed in sst#1491
                // The bare `\b429\b` token is dropped; real 429s carry
                // the wrapping `API rate limited (429)` wording which
                // still matches the first alternation.
                (
                    AgentState::RateLimit,
                    r"API rate limited \(429\)|Rate limited\. Quick retry|API rate limit exceeded",
                ),
                // #1136: network errors — transient, auto-retry safe.
                (
                    AgentState::ServerRateLimit,
                    r"ECONNRESET|ETIMEDOUT|InvalidHTTPResponse|fetch failed|connection reset|socket hang up|network error|proxy.*disconnect",
                ),
                // #848 PR-B: NEW OpenCode UsageLimit pattern (pre-#848
                // OpenCode had no UsageLimit pattern at all, so
                // subscription-quota strings fell through to whatever
                // broader pattern matched — frequently RateLimit via
                // the `rate.?limit` substring).
                (AgentState::UsageLimit, r"Quota Limit Exceeded"),
                // [measured] Provider-side validation errors (e.g. MiniMax
                // M2.5 rejecting eager_input_streaming in tool spec).
                (
                    AgentState::ApiError,
                    r"Error from provider:|request validation errors",
                ),
                // [docs] Context overflow
                (AgentState::ContextFull, r"ContextOverflow"),
                // [docs] Permission UI
                (
                    AgentState::PermissionPrompt,
                    r"Permission required|Allow once|Allow always",
                ),
                // Phase A Piece-1: git conflict output (backend-independent).
                (
                    AgentState::GitConflict,
                    r"Automatic merge failed; fix conflicts|CONFLICT \(content\)|Resolve all conflicts manually|Failed to merge submodule|Failed to merge in",
                ),
                // OpenCode 1.4.0 tool-banner markers:
                // - `✱` (U+2731 HEAVY ASTERISK, in-flight bare verb) —
                //   e.g. `✱ Glob "README.md" (1 match)`. The `→` (U+2192,
                //   COMPLETED) form was DROPPED by #1005 Phase A1 (#1009)
                //   to close the priority-oscillation class (`→ Read
                //   README.md` lingered in scrollback, kept re-firing
                //   ToolUse on every screen change, blocked
                //   `LATCHED_STATE_EXPIRY`).
                // - `~` (TILDE, in-flight `-ing` verb form) — e.g.
                //   `~ Reading file...`. Captured at byte ~30720 of
                //   tests/fixtures/state-replay/opencode-tooluse.raw.
                //   #1005 Phase A2 companion (this PR, dev-2): adds the
                //   `~ -ing` alternation branch after fixture inspection
                //   during the A1 cross-audit surfaced the false-negative
                //   — pre-A2 `[✱→]\s+(Read|...)` (and post-A1 just `✱`)
                //   missed sessions that sustained `~ Reading…` without
                //   firing `✱`, so they never entered ToolUse.
                //
                // Priority above the Thinking pattern so active tool use
                // outranks the generic spinner.
                (
                    AgentState::ToolUse,
                    r"✱\s+(Read|Write|Edit|Glob|Grep|Bash|List|Task)\b|~\s+(Reading|Writing|Editing|Searching|Listing|Globbing|Grepping)\b",
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
                (
                    AgentState::ServerRateLimit,
                    r"ECONNRESET|ETIMEDOUT|InvalidHTTPResponse|fetch failed|connection reset|socket hang up|network error|proxy.*disconnect",
                ),
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
                // [docs] Permission select options
                (
                    AgentState::PermissionPrompt,
                    r"Allow once|Allow for this session|suggest changes",
                ),
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
                // [measured] Input prompt text
                (AgentState::Idle, r"Type your message"),
                // [measured] Full ready prompt + YOLO mode
                (AgentState::Ready, r"Type your message|YOLO"),
            ],
            // #987: agy shares gemini-cli's agent engine + TUI structure;
            // start with the same pattern set. Calibrate against
            // `tests/fixtures/state-replay/agy-thinking.raw` in follow-up
            // PR if AGY's TUI introduces divergent rate-limit / idle
            // strings.
            Backend::Agy => vec![(AgentState::Ready, r"Antigravity CLI|Type your message")],
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
