[繁體中文](BACKEND-CAPABILITY-MATRIX.zh-TW.md)

# Backend Capability Matrix

Every backend runs through the same PTY-orchestration machinery, but the UI each one presents — and how much of that UI agend-terminal can trust — is not uniform. This document records, per `Backend` enum variant, what state-detection signal it actually uses today, whether it exposes context usage, how injection/submit behaves, whether resume works, whether MCP is wired, and its known fragile points.

**Discipline**: every cell here is either backed by a `path:line` code citation (this repo, `main` branch) or explicitly marked **unverified**. No cell is a guess. Where a claim couldn't be confirmed against the source, it says so — an honest gap is worth more than a plausible-sounding fabrication.

**Revalidated**: `main@1d83b423` (2026-07-16). `Backend::all()` contains six
first-class backends: ClaudeCode, KiroCli, Codex, OpenCode, Agy, and Grok.
Gemini is retired; Shell and `Raw(String)` remain utility variants outside
`Backend::all()`.

## Scope boundary vs. #2413

This matrix documents **current, already-shipped** behavior — what state detection actually consumes in production today, for each backend. It is a snapshot, not a plan.

Two views intentionally coexist: `core.state.current` is always the raw
screen-heuristic baseline, while the default-on Shadow Observer may apply a
high-confidence Hook/Stream correction through `shadow::operated_state` for
`snapshot.json` and selected dispatch deciders. Health/recovery paths keep the
raw view. The signal column below names both when a backend has that second
plane; it does not imply that hooks rewrite `core.state.current`.

[#2413](https://github.com/suzuke/agend-terminal/issues/2413) ("Out-of-path API-activity probe to fix false-idle blind spot in pattern-based agent_state") is the **improvement roadmap**: an ongoing empirical effort to measure whether structured signals beyond raw PTY-pattern matching can close the false-idle blind spot backend-by-backend. Where this matrix says "PTY heuristic," #2413 may be quantifying whether that backend can be upgraded. This document records only shipped behavior; historical measurement reports remain available through [the immutable history snapshot](README.md#historical-records).

## Signal-authority ladder (for reference)

`src/daemon/shadow/evidence.rs` ranks signal authority, highest first: **Hook** (`Confirmed` confidence) → **Stream** (session-log tail, e.g. Kiro's jsonl) → **Screen** (PTY pattern match) → **ProcessHeuristic** → **Inferred**. The per-backend sections distinguish the raw baseline from any higher-authority observed correction actually wired today.

## Overview

| Backend | Agent-state signal | Context usage | Submit / inject | Resume | MCP | Fragile-points summary |
|---|---|---|---|---|---|---|
| **ClaudeCode** | Screen baseline + Hook/`Confirmed` observed correction; shares the top observer rung with Agy | StatusLine (fleet's custom format only) | Bulk, `submit_key="\r"` | `--continue`, gated by on-disk session check | Yes — explicit `--mcp-config` flag | Best-covered backend; still version-pinned to a specific Claude Code release |
| **Codex** | Pure PTY/Screen heuristic — no hook | Unavailable | Typed/paced (`typed_inject=true`, #1670) | Hardcoded `resume --last` in spawn args, not via the generic `ResumeMode` abstraction | Yes — per-project `.codex/config.toml` | Most PTY-dependent backend; root fragility is #1670's ratatui input widget |
| **KiroCli** | Screen baseline + Stream-authority session-tail correction when the observer gate has high confidence | StatusLine | Bulk, `submit_key="\r"`, fixed 50ms pre-submit sleep | `--resume` | Yes — own auto-discovery (`.kiro/settings/mcp.json`) | Only backend needing `redraw_after_resize`; several input-line false-latch guards |
| **OpenCode** | Pure PTY/Screen heuristic — no hook | Unavailable (footer shows a token/cost string that isn't parsed) | Typed/paced (`typed_inject=true`) | `--continue`; carries an **open** dummy-session-id incident | Yes — `opencode.json` `"mcp"` key | Open: dummy-session wedge (rare "process never exits" variant evades detection) |
| **Agy** | Screen baseline + Hook correction for busy/idle; finer states remain screen heuristic | Unavailable | Typed/paced, ~2ms/byte | `--continue` — code comment says "operator-verified," no automated behavior test found | Yes — standard `mcpServers` schema at `.agents/mcp_config.json` | Gemini's successor; hooks were dead once and had to be re-fixed |
| **Grok** | PTY/Screen heuristic; no lifecycle hook | Unavailable | Typed/paced | `--continue`, gated by a cwd-scoped on-disk session probe | Yes — project `.grok/config.toml` | Newest first-class backend; active/idle patterns recalibrated against Grok 0.2.93 after #2707 false-idle |
| **Shell** / **Raw(String)** | None — no detection patterns at all; `agent_state` is hardcoded to `Idle` on spawn | Not applicable | Bulk; text still gets written + submitted (not a true no-op), just no backend-specific customization | Not supported — `args_for()` returns empty, so Resume and Fresh spawn identically | None — MCP config is skipped entirely for this backend | Utility tier; no incidents found in CHANGELOG (**unverified** whether that means "never broke" or "never tracked") |

---

## Harness and model-provider overrides

Backend detection has two separate axes:

1. the **CLI harness**, which owns the PTY/tool loop and is detected from
   `PATH`; and
2. the **model provider**, which supplies the hosted or local token endpoint
   configured through that harness.

A provider is available only when a compatible harness, provider configuration,
and usable credential are all present. Installer artifacts are hints, not proof
of availability.

| Provider | Harness | `base_url` | `env_key` | `wire_api` | Probe |
|---|---|---|---|---|---|
| Fugu / Sakana | `codex` | `https://api.sakana.ai/v1` | `SAKANA_API_KEY` | `responses` | `/models` |

Fugu uses an isolated `CODEX_HOME` (`~/.agend-fugu-codex`) and a per-instance
`env.CODEX_HOME` in `fleet.yaml`, so provisioning never mutates the operator's
global `~/.codex`. Endpoint probes are optional, cached, and fail-open: a probe
failure is reported as `unknown`, not as provider absence, and startup never
depends on a live network call.

`kiro-cli` (AWS signed-auth shape) and `agy` (Google service-account/OAuth
shape) are intentionally fixed-provider backends rather than bearer
`base_url` overrides.

Inspect the resolved boundary with:

```sh
agend-terminal doctor providers
agend-terminal doctor providers --format json
agend-terminal doctor providers --probe
```

## ClaudeCode

**Agent-state signal**: the raw state remains the PTY/Screen heuristic. Claude
lifecycle hooks additionally produce `authority=Hook`, `confidence=Confirmed`
evidence — the highest observer rung (`src/daemon/shadow/evidence.rs`). A fresh,
high-confidence disagreement may correct the operated snapshot/dispatch view;
it never rewrites `core.state.current`. `has_state_hooks()` names Claude and Agy.
Unverified: whether the initial ready-gate (`ready_pattern: "bypass permissions|❯"`) ever consults hooks, or is pure screen-pattern like every other backend's ready-gate — no hook-based ready path was found, but this is absence-of-evidence, not a confirmed negative.

**Context usage**: `ContextProvider::StatusLine` via `CLAUDE_CONTEXT_PATTERN` (`src/backend_profile.rs:39-46,86-92`) — but the regex only matches the fleet's custom statusline format (`Ctx Used: N%`). A vanilla Claude Code install renders an inverted "remaining %" string this pattern deliberately does not match — an unimplemented gap, not a bug.

**Submit / inject**: Bulk inject, `submit_key: "\r"`, `typed_inject: false` (inherits `DEFAULTS`, `src/backend.rs:382-399`; pinned by the preset-default test) — unlike Codex/OpenCode/Agy/Grok, which use paced typed inject. No pre-send confirm-first gate exists; instead there's a post-hoc, hook-history-gated delivery-verification watchdog (`src/daemon/inject_delivery.rs`, 30s window). It can arm for Claude or Agy once that instance has emitted hook-shadow evidence; heuristic-only backends never arm.

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

**Agent-state signal**: PTY/Screen heuristic remains the raw baseline
(`kirocli_profile()`). A separate `~/.kiro/sessions/cli/<uuid>.jsonl` read-only
tail emits `Authority::Stream` evidence (`src/daemon/shadow/kiro.rs`). When the
shared observer gate has a fresh high-confidence correction, that Stream state
may drive the operated snapshot/dispatch view; it never rewrites the raw state.
Kiro has no lifecycle hook. Caveat: the jsonl flushes at tool-round/turn-end,
not prompt-submit, so a pure-thinking turn with no tool call is not caught
mid-turn by this plane.

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

**Agent-state signal**: Hybrid. The raw state remains PTY/Screen heuristic.
Lifecycle hooks (`has_state_hooks() == true`) emit busy/idle evidence through
`PreInvocation` / `Stop`; a fresh high-confidence result may correct the
operated snapshot/dispatch view. They provide no tool-call granularity
(task-board finding `t-...93090-0`), so finer states such as UsageLimit,
RateLimit, ApiError, GitConflict, and PermissionPrompt remain screen-derived.

**Context usage**: Unavailable — `context_pattern: None` (`src/backend_profile.rs:209`), explicitly grouped with Codex/OpenCode as having "no trustworthy passive context signal" (`src/backend_profile.rs:43-45`).

**Submit / inject**: `typed_inject: true`, `inject_prefix: "\r"`, default `submit_key: "\r"` (`src/backend.rs:608-609`); paced ~2ms/byte chunked writes — the shared rationale doc at `src/backend.rs:519` names Agy explicitly alongside Codex. No confirm-first/readback mechanism found.

**Resume**: `ResumeMode::ContinueInCwd { flag: "--continue" }` (`src/backend.rs:613`) — the code comment says "operator-verified in issue body," but no automated CLI-behavior test was found, only a unit-test pin of the args themselves.

**MCP**: Yes — standard `{command, args, env}` `mcpServers` schema at `<workdir>/.agents/mcp_config.json` (`src/mcp_config.rs::configure_agy`, `mcp_server_entry`); `fleet_mcp_supported: true` in the Agy preset and its regression test. A **stale field doc comment** near `BackendPreset::fleet_mcp_supported` still describes Agy as unsupported; the live preset, writer, and test directly contradict it. That source-comment cleanup is independent of this matrix.

**Known fragile points**: #987 (Agy added), #995 (workspace-trust dismiss + a dead `.antigravitycli/` MCP write path), #1547 (fixed the real MCP path to `.agents/`), #1580 (Gemini retired, Agy is its successor), #1523 / #2413 Phase D (hooks were dead at one point and had to be re-fixed — see the scope-boundary note above), #2236 (quota-wall pattern ordering), #2409 (a transient high-traffic `ApiError` pattern), #2524 P1b-r1/r2 (a missing `GitConflict` pattern; the `RateLimit` pattern is flagged low-confidence/synthetic — not yet verified against real Agy output).

---

## Grok

**Agent-state signal**: PTY/Screen heuristic with no lifecycle hook.
`has_state_hooks()` excludes Grok (`src/backend.rs:60-77`). Its profile keeps a
deliberately small ordered set: project-trust `PermissionPrompt`, busy
`Active` (`[stop]` / `Ctrl+c:cancel`), then completion/prompt `Idle`
(`src/backend_profile.rs:129-181`).

**Context usage**: Unavailable — `context_pattern: None`
(`src/backend_profile.rs:182`); no passive token-percentage parser is shipped.

**Submit / inject**: `typed_inject: true`, default `submit_key: "\r"`; the
full-screen TUI requires paced injection (`src/backend.rs:658-667`). The
first-use project trust modal is dismissed with one Enter, and the stable empty
prompt marker is `❯`.

**Resume**: `ResumeMode::ContinueInCwd { flag: "--continue" }`, but unlike an
optimistic resume it is gated by `grok_session::has_resumable`: the encoded cwd
directory must contain a session subdirectory (`src/backend.rs:668-678,978+`).

**MCP**: Yes. `configure_grok` writes project-local `.grok/config.toml` using
Grok's native `[mcp_servers.agend-terminal]` schema and never writes the user's
global `~/.grok` config (`src/mcp_config.rs:906-1025`).

**Known fragile points**: the original one-shot calibration treated permanent
footer chrome as busy/idle evidence and produced systemic false-idle. #2707
recalibrated the profile against a live Grok 0.2.93 soak; finer rate-limit/auth
states still have no reliable screen signature and remain deliberately
unclassified. Grok currently reuses the generic F9 productivity markers, so
backend-specific marker coverage is **unverified**.

---

## Shell and Raw(String)

**Agent-state signal**: None. No detection patterns exist at all — `agent_state` is hardcoded to `Idle` at spawn, skipping the `Starting → Idle` handshake every other backend goes through (`src/backend_profile.rs:506-520` `empty_profile()`; `src/state/mod.rs:1002-1013`; `src/state/patterns.rs:204`).

**Context usage**: Not applicable — `context_pattern: None`, `ContextProvider::Unavailable` (`src/backend_profile.rs:516,44-46`, test `:541-542`).

**Submit / inject**: All fields fall through to `DEFAULTS` (`submit_key: "\r"`, `inject_prefix: ""`, `typed_inject: false`; `src/backend.rs:382-399,648-656`). Important nuance: inject is **not actually a no-op** — text is genuinely written to the PTY and submitted (`src/agent/mod.rs:2776-2818`). What's a no-op is the *preset customization* (no dismiss pattern, no special prefix, no readback confirm).

**Resume**: Not supported — `resume_mode: ResumeMode::NotSupported` (`src/backend.rs:389`), and `args_for()` returns an empty `Vec` for it (`:275-277`) — so a Resume spawn and a Fresh spawn produce byte-identical (empty) args, effectively a no-op distinction.

**MCP**: None — `mcp_config::configure()` returns immediately for `Backend::Shell | Backend::Raw(_) | None` without calling any `configure_*` function (`src/mcp_config.rs:817-830`); `fleet_mcp_supported: false` (`src/backend.rs:648-654`).

**Known fragile points**: None found in `CHANGELOG.md` or tracked incidents. **Unverified** whether that means Shell genuinely never broke (plausible — it has the least backend-specific code to break) or simply was never worth tracking. One structural note, not a fragility: `Backend::from_command` (`src/backend.rs:663-687`) in practice never returns `Some(Shell)` or `Some(Raw(_))` — it only recognizes known preset binary names, so these two `match` arms are a defensive exhaustive-match; the real runtime path for Shell/Raw is the `None` branch.

**Raw(String)**: Shares the exact same preset arm and `empty_profile()` as Shell (`src/backend.rs:648-656`) — identical behavior. The only difference is `command_string()` (`src/backend.rs:228-229`), which uses the stored path string instead of `$SHELL`.
