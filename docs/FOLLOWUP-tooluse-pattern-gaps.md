# FOLLOWUP: ToolUse pattern coverage gaps (Stage 2a finding)

> **Status: SHIPPED** — implementation landed on `main` (commits `f65d295 ⇢ 24ff0e0`, 2026-04-20). Doc retained for historical/provenance.

**Filed**: 2026-04-20
**Status**: Closed 2026-04-20 — all 5 backends' ToolUse patterns fixed on `main` (commits `f65d295 ⇢ 24ff0e0`)
**Origin**: `docs/PLAN-state-replay-fixture-expansion.md` Stage 2a recording run

## What happened

Stage 2a recorded five real-PTY tool-call sessions (one per backend) per
the plan. Each recording was verified to have executed a tool: claude
printed a `⏺` directory banner, codex returned a README summary, gemini
reported `Tool Calls: 1 ✓` in its session summary, kiro printed a
directory listing, opencode completed a README review session.

**None of the five recordings triggered `AgentState::ToolUse`** under
the current `StatePatterns`. Observed transition sequences only cover
`Starting`, `Idle`, `Thinking`, and (for codex) `InteractivePrompt`.

## Root cause per backend

Source: `src/state.rs` `StatePatterns::for_backend`.

| Backend | Current pattern | Why it missed |
|--------|-----------------|---------------|
| claude-code | `[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏✓●].*(Read\|Bash\|Edit\|Write\|Grep\|Glob)` | 2.1.98 banner glyph is `⏺` (matches char class) but the line contains the tool's *result* (file names), not a literal tool name word |
| codex | `apply_patch` | Only matches the patch tool. Read/list/search tools never match |
| gemini | `tool.*call\|MCP.*tool` | Only matches the end-of-session summary line `Tool Calls:`; in-flight gemini 0.37.1 banners use a different format |
| kiro-cli | `execute_bash\|fs_read\|fs_write` | Kiro 2.0.1 renders tool calls under a banner that does not print these tool-name tokens |
| opencode | *(no ToolUse pattern)* | OpenCode entry in the pattern table has no ToolUse row |

## What Stage 2a fixtures provide anyway

Although they don't cover ToolUse, the fixtures are valid regression
anchors:

- They lock in the *current* observed transition sequence for real tool
  sessions. If a future pattern edit causes ToolUse (or any other
  state) to fire on one of these recordings, `replay_manifest_regression`
  will flag the divergence for manual review.
- They document the CLI behavior at each tracked version — recording
  each backend's banner format at `recorded_on`, so pattern work can
  reference the raw frames rather than re-recording.

## Suggested next steps (separate work item)

1. Inspect each recording's intermediate frames to identify the actual
   tool banner format per backend / version. Example for claude:
   `REPLAY_FILE=tests/fixtures/state-replay/claude-tooluse.raw
   REPLAY_BACKEND=claude-code REPLAY_CHUNK=128
   target/debug/deps/agend_terminal-* replay_session --ignored --nocapture`
   then read the `--- final tail_lines(40) ---` section (or temporarily
   dump mid-stream screens).
2. Update `StatePatterns::for_backend` entries for each backend.
3. Re-run `replay_manifest_regression` — any fixture whose transitions
   now include `tool_use` is expected to need its
   `expected_transitions` updated. Treat each one as a deliberate,
   reviewed change.
4. Add per-backend synthetic unit tests alongside the pattern edits
   (see existing tests around `AgentState::ToolUse` in `src/state.rs`
   as templates).

## Out of scope for this follow-up

- Adding PermissionPrompt / ContextFull fixtures (Stage 2b / 2c in the
  parent plan).
- Changing the replay harness's hysteresis pacing.
