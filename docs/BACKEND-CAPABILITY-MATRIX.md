[繁體中文](BACKEND-CAPABILITY-MATRIX.zh-TW.md)

# Backend Capability Matrix

Every backend runs through the same PTY-orchestration machinery, but the UI each one presents — and how much of that UI agend-terminal can trust — is not uniform. This document records, per `Backend` enum variant, what state-detection signal it actually uses today, whether it exposes context usage, how injection/submit behaves, whether resume works, whether MCP is wired, and its known fragile points.

**Discipline**: every cell here is either backed by a `path:line` code citation (this repo, `main` branch) or explicitly marked **unverified**. No cell is a guess. Where a claim couldn't be confirmed against the source, it says so — an honest gap is worth more than a plausible-sounding fabrication.

## Scope boundary vs. #2413

This matrix documents **current, already-shipped** behavior — what state detection actually consumes in production today, for each backend. It is a snapshot, not a plan.

[#2413](https://github.com/suzuke/agend-terminal/issues/2413) ("Out-of-path API-activity probe to fix false-idle blind spot in pattern-based agent_state") is the **improvement roadmap**: an ongoing empirical effort (the Shadow Observer, see `docs/SHADOW-OBSERVER-QUANT-AGY-2413.md` and its claude/codex siblings under `docs/archived/`) to measure whether additional structured signals — beyond raw PTY-pattern matching — can close the false-idle blind spot backend-by-backend. Where this matrix says "PTY heuristic," #2413's work may already be actively quantifying whether that can be upgraded for that backend. This document doesn't speak to that roadmap or its findings — only to what's true today. Read #2413's own docs for where that effort currently stands.

## Signal-authority ladder (for reference)

`src/daemon/shadow/evidence.rs:65-90` ranks signal authority, highest first: **Hook** (`Confirmed` confidence) → **Stream** (session-log tail, e.g. Kiro's jsonl) → **Screen** (PTY pattern match) → **ProcessHeuristic** → **Inferred**. The per-backend sections below say which rung each backend actually sits on for `agent_state` today — not which rung it could theoretically reach.

## Overview

| Backend | Agent-state signal | Context usage | Submit / inject | Resume | MCP | Fragile-points summary |
|---|---|---|---|---|---|---|
| **ClaudeCode** | Hook, `Confirmed` authority — the only backend at the top of the ladder | StatusLine (fleet's custom format only) | Bulk, `submit_key="\r"` | `--continue`, gated by on-disk session check | Yes — explicit `--mcp-config` flag | Best-covered backend; still version-pinned to a specific Claude Code release |
| **Codex** | Pure PTY/Screen heuristic — no hook | Unavailable | Typed/paced (`typed_inject=true`, #1670) | Hardcoded `resume --last` in spawn args, not via the generic `ResumeMode` abstraction | Yes — per-project `.codex/config.toml` | Most PTY-dependent backend; root fragility is #1670's ratatui input widget |
| **KiroCli** | PTY/Screen heuristic drives `agent_state`; a separate jsonl tail exists but is Stream-authority only and never promotes | StatusLine | Bulk, `submit_key="\r"`, fixed 50ms pre-submit sleep | `--resume` | Yes — own auto-discovery (`.kiro/settings/mcp.json`) | Only backend needing `redraw_after_resize`; several input-line false-latch guards |
| **OpenCode** | Pure PTY/Screen heuristic — no hook | Unavailable (footer shows a token/cost string that isn't parsed) | Typed/paced (`typed_inject=true`) | `--continue`; carries an **open** dummy-session-id incident | Yes — `opencode.json` `"mcp"` key | Open: dummy-session wedge (rare "process never exits" variant evades detection) |
| **Agy** | Hybrid — real hooks exist but only for busy/idle transitions; every finer state (rate-limit, API error, git conflict, permission prompt, ...) is PTY/Screen heuristic | Unavailable | Typed/paced, ~2ms/byte | `--continue` — code comment says "operator-verified," no automated behavior test found | Yes — standard `mcpServers` schema at `.agents/mcp_config.json` | Newest first-class backend (#987, Gemini's successor); hooks were dead once and had to be re-fixed |
| **Shell** / **Raw(String)** | None — no detection patterns at all; `agent_state` is hardcoded to `Idle` on spawn | Not applicable | Bulk; text still gets written + submitted (not a true no-op), just no backend-specific customization | Not supported — `args_for()` returns empty, so Resume and Fresh spawn identically | None — MCP config is skipped entirely for this backend | Utility tier; no incidents found in CHANGELOG (**unverified** whether that means "never broke" or "never tracked") |

---

## ClaudeCode

**Agent-state signal**: Hook-based, `authority=Hook`, `confidence=Confirmed` — the highest rung on the ladder (`src/daemon/shadow/evidence.rs:65-90,92-109`). `has_state_hooks()` names only `ClaudeCode` and `Agy` (`src/backend.rs:55-76`).
Unverified: whether the initial ready-gate (`ready_pattern: "bypass permissions|❯"`) ever consults hooks, or is pure screen-pattern like every other backend's ready-gate — no hook-based ready path was found, but this is absence-of-evidence, not a confirmed negative.

**Context usage**: `ContextProvider::StatusLine` via `CLAUDE_CONTEXT_PATTERN` (`src/backend_profile.rs:39-46,86-92`) — but the regex only matches the fleet's custom statusline format (`Ctx Used: N%`). A vanilla Claude Code install renders an inverted "remaining %" string this pattern deliberately does not match — an unimplemented gap, not a bug.

**Submit / inject**: Bulk inject, `submit_key: "\r"`, `typed_inject: false` (inherits `DEFAULTS`, `src/backend.rs:382-399`; pinned by test array at `:1474-1486`) — unlike Codex/OpenCode/Agy, which all use paced typed inject. No pre-send confirm-first gate exists; instead there's a post-hoc, hook-gated delivery-verification watchdog (`src/daemon/inject_delivery.rs:1-20`, 30s window) — which in practice only fires for backends with hooks, i.e. effectively Claude-only.

**Resume**: `ResumeMode::ContinueInCwd { flag: "--continue" }` (`src/backend.rs:405`), gated by on-disk session detection reading `~/.claude/projects/<encoded-cwd>/*.jsonl` (`src/backend.rs:878+`) — auto-downgrades to Fresh when no resumable session exists.

**MCP**: Yes, `fleet_mcp_supported: true`, via an explicit `--mcp-config <workdir>/mcp-config.json` CLI flag injected at spawn (`src/backend.rs:784-799`), written by `configure_claude` (`src/mcp_config.rs:177-201`) — project-local file discovery, not passive `.mcp.json` auto-discovery; global `~/.claude` is never touched.

**Known fragile points**: #468 (anchored dismiss regex against false-positive auto-dismiss from operator text), #996 Phase 1 (trust-dialog keystroke changed from up+up+Enter to bare `\r` after a 37× message-duplication-loop incident), #1001 (related earlier fix), #1944/#1947/#1948 (input-box empty-marker `❯`, verified against real captures — explicitly does not generalize to Codex), #2044 (the inject-delivery watchdog, built after a `/model` picker silently swallowed a dispatch). Pattern set last calibrated against Claude Code `2.1.89` (`src/backend.rs:868`) — a version-drift risk shared by every PTY-pattern-dependent backend.

---

## Codex

**Agent-state signal**: Pure PTY/Screen heuristic — no lifecycle hook. `has_state_hooks()` (`src/backend.rs:74-76`) does not list Codex.

**Context usage**: Unavailable — `context_pattern: None` (`src/backend_profile.rs:365-415`, `codex_profile()`).

**Submit / inject**: `submit_key` defaults to `"\r"`; `typed_inject: true` — paced, per-byte writes, because Codex's ratatui-style input widget cannot reliably accept a bulk write (#1670, full rationale at `src/backend.rs:491-565`, esp. `508-522`; pinning test `codex_uses_paced_inject_and_wake_pointer_is_not_a_system_header_1670` at `:1934-1963`). No confirm-first/readback mechanism beyond the pacing itself was found — **unverified**.

**Resume**: `resume_mode` defaults to `NotSupported` at the `ResumeMode` level, but Codex's actual resume is hardcoded directly into `args`/`fresh_args` as `resume --last` — bypassing the generic `ResumeMode` abstraction every other resumable backend uses. Worth flagging to future maintainers as an inconsistency, not just a quirk.

**MCP**: Yes, `fleet_mcp_supported: true` (default), via a per-project `.codex/config.toml` (not the global `~/.codex/config.toml`) written by `configure_codex_with_home` (`src/mcp_config.rs:677-772`).

**Known fragile points**: #1670 (the paced-inject root cause), #1944/#1948 family (ghost placeholder — an empty input box misread as a real prompt marker), an unnumbered finding at `src/state/mod.rs:2196-2202` (the ready screen can falsely latch `Active` and then stop re-detecting). A user-recalled "Codex PTY injection failure" incident is **unverified** — no ticket matching that exact description was found; the closest candidate is `CHANGELOG.md:237` (#603/PR #629, stdin-only delivery), but that mechanism has no trace left in current `src/` and appears superseded by #1670.

---

## KiroCli

**Agent-state signal**: PTY/Screen heuristic drives the real `agent_state` today (`src/backend_profile.rs:220-273`, `kirocli_profile()`). A separate `~/.kiro/sessions/cli/<uuid>.jsonl` read-only tail exists (`src/daemon/shadow/kiro.rs:1-23`, wired live in production per `src/app/mod.rs:2774-2797`), but the whole Shadow Observer system is explicitly "additive only... never drives `agent_state`" (`src/daemon/shadow/mod.rs:15-16`) — it sits at `Authority::Stream`, below `Authority::Hook`. Kiro has no lifecycle hook at all. Caveat: the jsonl only flushes at tool-round/turn-end (not prompt-submit), so a pure-thinking turn with no tool calls isn't caught mid-turn even by the Stream-authority shadow.

**Context usage**: StatusLine, available — `KIRO_CONTEXT_PATTERN = r"◔\s*(\d+(?:\.\d+)?)\s*%"` (`src/backend_profile.rs:97`). The doc comment at `:38-46` explicitly names Claude and Kiro as the only two `StatusLine` providers.

**Submit / inject**: Bulk, `submit_key: "\r"`, `typed_inject: false` (pinned array, `src/backend.rs:1472-1474`). A fixed 50ms sleep precedes submit (`src/agent/mod.rs:2813-2818`); the `readback_confirm_typed` mechanism (#1912, `:2824-2831`) is gated to `typed_inject` backends only, so Kiro never uses it.

**Resume**: Yes — `ResumeMode::ContinueInCwd { flag: "--resume" }` (`src/backend.rs:457`).

**MCP**: Yes, own auto-discovery — Kiro reads `.kiro/settings/mcp.json` (no explicit CLI flag like Claude's), written by `configure_kiro()` via a wrapper script "because Kiro ignores env block" (`src/mcp_config.rs:321-367`).

**Known fragile points**: #7 (no repaint on `SIGWINCH` — the only backend needing `redraw_after_resize`), #996 Phase 2a (trust-modal defaults to the destructive option; Down+Enter required, verified against fixture byte analysis), #468 (startup-hang dismiss regex), #1005 (completion-banner false-positive guard), #1947 (`>` input-line quote false-latch guard), #1948 (no-prompt-marker placeholder heuristic), #2413 (jsonl observer spike — the Shadow Observer work referenced in the scope-boundary note above). Unverified: whether the #2413 jsonl Stream plane also carries token/context-usage evidence for Kiro specifically — only turn/tool-lifecycle evidence mappings were found.

---

## OpenCode

**Agent-state signal**: Pure PTY/Screen heuristic — no structured signal. `has_state_hooks()` excludes OpenCode (`src/backend.rs:74-76`); everything runs through regex-pattern matching (`src/backend_profile.rs:282-355`).

**Context usage**: Unavailable by explicit code declaration, not merely "not implemented yet." `context_pattern: None` (`src/backend_profile.rs:352`); `src/state/mod.rs:1221-1235` groups Codex/OpenCode/Agy together as having "no trustworthy passive context signal" — the rendered footer does show a token/cost string, but it is not parsed into `context_pct`.

**Submit / inject**: `submit_key` defaults to `"\r"` (not overridden), `inject_prefix: "\r"`, `typed_inject: true` — paced per-byte writes, same rationale documented for Codex (#1670). No confirm-first/readback mechanism found beyond the pacing itself — **unverified**.

**Resume**: `ResumeMode::ContinueInCwd { flag: "--continue" }` (`src/backend.rs:571`). Two distinct, separately-tracked incidents:
- **#2020 (fixed)**: a resumed pane shows no "Ask anything" placeholder, only bare `┃` statusline lines — this used to be misread as `AwaitingOperator`. Fixed by adding a low-priority `ctrl+p commands` Idle pattern, regression-pinned (`src/backend_profile.rs:330-340,564-598`).
- **Dummy-session wedge (open, needs-repro)**: documented in `docs/KNOWN_ISSUES.md:24-46` — an upstream OpenCode bug where `--continue` can send a placeholder session id. The common case is mitigated (#1519/#1526, fresh-session fallback), but a rarer "process never exits" variant evades all three detection layers. This is the task-board incident referenced by the dispatch note: `t-20260702144219394508-56872-6` (`docs/KNOWN_ISSUES.md:41`), tracked structurally under #2549.

**MCP**: Yes — project-local `opencode.json` `"mcp"` key discovery, `fleet_mcp_supported: true` (pinned test at `src/backend.rs:1432`), implemented by `configure_opencode` (`src/mcp_config.rs:599-638`), plus an auto-approve permission block so OpenCode's own "Permission required" prompt doesn't block MCP tool calls.

**Known fragile points**: #2020 (closed — resumed-pane false-idle) and the dummy-session wedge above (open, `t-20260702144219394508-56872-6`, #2549). These are two separate bugs that likely get conflated in casual conversation — one is fixed, the other isn't.

---

## Agy

**Agent-state signal**: Hybrid. Real lifecycle hooks exist (`has_state_hooks()` returns true, `src/backend.rs:74-76`) but fire only for busy/idle transitions (`PreInvocation`→`UserPromptSubmit`/`Stop`, `src/mcp_config.rs:470`) — empirically confirmed to carry no tool-call-granularity signal (task-board finding `t-...93090-0`). Every finer state (`Active`, `UsageLimit`, `RateLimit`, `ApiError`, `GitConflict`, `PermissionPrompt`) is pure PTY/Screen heuristic — 8 ordered regexes (`src/backend_profile.rs:129-213`).

**Context usage**: Unavailable — `context_pattern: None` (`src/backend_profile.rs:209`), explicitly grouped with Codex/OpenCode as having "no trustworthy passive context signal" (`src/backend_profile.rs:43-45`).

**Submit / inject**: `typed_inject: true`, `inject_prefix: "\r"`, default `submit_key: "\r"` (`src/backend.rs:608-609`); paced ~2ms/byte chunked writes — the shared rationale doc at `src/backend.rs:519` names Agy explicitly alongside Codex. No confirm-first/readback mechanism found.

**Resume**: `ResumeMode::ContinueInCwd { flag: "--continue" }` (`src/backend.rs:613`) — the code comment says "operator-verified in issue body," but no automated CLI-behavior test was found, only a unit-test pin of the args themselves.

**MCP**: Yes — standard `{command, args, env}` `mcpServers` schema at `<workdir>/.agents/mcp_config.json` (`src/mcp_config.rs:391-436`, `mcp_server_entry` at `:86-99`); `fleet_mcp_supported: true` (`src/backend.rs:641`, test `:1436`). A **stale doc comment** was found at `src/backend.rs:1416-1421` claiming Agy's MCP support is `false`/unsupported — this is outdated and directly contradicted by the actual assertion two lines later. Worth a follow-up doc-comment cleanup, independent of this matrix.

**Known fragile points**: #987 (Agy added), #995 (workspace-trust dismiss + a dead `.antigravitycli/` MCP write path), #1547 (fixed the real MCP path to `.agents/`), #1580 (Gemini retired, Agy is its successor), #1523 / #2413 Phase D (hooks were dead at one point and had to be re-fixed — see the scope-boundary note above), #2236 (quota-wall pattern ordering), #2409 (a transient high-traffic `ApiError` pattern), #2524 P1b-r1/r2 (a missing `GitConflict` pattern; the `RateLimit` pattern is flagged low-confidence/synthetic — not yet verified against real Agy output).

---

## Shell and Raw(String)

**Agent-state signal**: None. No detection patterns exist at all — `agent_state` is hardcoded to `Idle` at spawn, skipping the `Starting → Idle` handshake every other backend goes through (`src/backend_profile.rs:506-520` `empty_profile()`; `src/state/mod.rs:1002-1013`; `src/state/patterns.rs:204`).

**Context usage**: Not applicable — `context_pattern: None`, `ContextProvider::Unavailable` (`src/backend_profile.rs:516,44-46`, test `:541-542`).

**Submit / inject**: All fields fall through to `DEFAULTS` (`submit_key: "\r"`, `inject_prefix: ""`, `typed_inject: false`; `src/backend.rs:382-399,648-656`). Important nuance: inject is **not actually a no-op** — text is genuinely written to the PTY and submitted (`src/agent/mod.rs:2776-2818`). What's a no-op is the *preset customization* (no dismiss pattern, no special prefix, no readback confirm).

**Resume**: Not supported — `resume_mode: ResumeMode::NotSupported` (`src/backend.rs:389`), and `args_for()` returns an empty `Vec` for it (`:275-277`) — so a Resume spawn and a Fresh spawn produce byte-identical (empty) args, effectively a no-op distinction.

**MCP**: None — `mcp_config::configure()` returns immediately for `Backend::Shell | Backend::Raw(_) | None` without calling any `configure_*` function (`src/mcp_config.rs:817-830`); `fleet_mcp_supported: false` (`src/backend.rs:648-654`).

**Known fragile points**: None found in `CHANGELOG.md` or tracked incidents. **Unverified** whether that means Shell genuinely never broke (plausible — it has the least backend-specific code to break) or simply was never worth tracking. One structural note, not a fragility: `Backend::from_command` (`src/backend.rs:663-687`) in practice never returns `Some(Shell)` or `Some(Raw(_))` — it only recognizes known preset binary names, so these two `match` arms are a defensive exhaustive-match; the real runtime path for Shell/Raw is the `None` branch.

**Raw(String)**: Shares the exact same preset arm and `empty_profile()` as Shell (`src/backend.rs:648-656`) — identical behavior. The only difference is `command_string()` (`src/backend.rs:228-229`), which uses the stored path string instead of `$SHELL`.
