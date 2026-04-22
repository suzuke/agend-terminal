# FOLLOWUP: PermissionPrompt pattern coverage gaps (Stage 2b finding)

> **Status: SHIPPED** — implementation landed on `main` (commits `de2b197 ⇢ 4aa1dbf`, rebased to `5c95982 ⇢ 24ff0e0`). Doc retained for historical/provenance.

**Filed**: 2026-04-20
**Status**: Closed 2026-04-20 — claude + codex PermissionPrompt patterns fixed on `main` (commits `de2b197 ⇢ 4aa1dbf`, rebased to `5c95982 ⇢ 24ff0e0`)
**Origin**: `docs/PLAN-state-replay-fixture-expansion.md` Stage 2b recording run
**Sibling**: `docs/FOLLOWUP-tooluse-pattern-gaps.md` (same root cause, different state)

## What happened

Stage 2b recorded two real-PTY permission-denial sessions (claude-code
and codex) per the plan. Each recording was verified to have triggered
a real permission dialog: claude's final screen shows `User rejected
write to ../../tmp/claude-perm-test.txt`; codex's shows `✗ You canceled
the request to run printf 'hello' > /Users/suzuke/codex-perm-test.txt`.

**Neither recording triggered `AgentState::PermissionPrompt`** under
the current `StatePatterns`. Observed transitions only cover `Starting`,
`Idle`, and (for codex) `InteractivePrompt`.

## Root cause per backend

Source: `src/state.rs` `StatePatterns::for_backend`.

| Backend | Current pattern | Why it missed |
|--------|-----------------|---------------|
| claude-code | `Allow once\|Allow always\|approve` | 2.1.98 dialog wording differs (likely ink-rendered `Yes, proceed` / `No, keep asking` style, or modal overlay outside the vterm grid) |
| codex | `Request approval\|approve\|deny` | 0.120.0 escalation wording is `escalated command` / `outside the writable sandbox` / `You canceled the request` — none of the three pattern alternatives appear |

### Friction for reproducing the codex dialog

Codex 0.120.0 defaults to `sandbox = "workspace-write"`, which silently
auto-approves writes inside the workspace **and `/tmp/`**. To force a
permission dialog the recorded prompt had to target `$HOME/` — writing
to `/tmp/` (the obvious test path from PLAN Stage 2b) wouldn't prompt.
Any future re-recording of codex permission scenarios must target a
path outside `workspace-write`'s allowlist or pass `--sandbox read-only`.

## What Stage 2b fixtures provide anyway

- They lock in the current transition behavior for real permission
  flows. A future pattern fix that actually fires PermissionPrompt will
  shift these sequences and be flagged by `replay_manifest_regression`
  for deliberate review.
- `claude-perm.raw` additionally exposes a Stage 2a ToolUse gap:
  `⏺ Write(/tmp/...)` is visible but the ClaudeCode ToolUse pattern's
  character class uses `●` (U+25CF), not the actual `⏺` (U+23FA) glyph.
  Fixing that one byte in the pattern is the single highest-value win
  across both FOLLOWUP docs.

## Suggested next steps

1. Record intermediate frames (smaller `REPLAY_CHUNK`, or temporarily
   dump mid-stream screens) to read the exact dialog wording used by
   each CLI version.
2. Update `StatePatterns::for_backend` entries. Candidate claude
   addition: something that matches the ink select-prompt's rendered
   row ("Do you want to..." header or the numbered-option format).
   Candidate codex addition: match `escalated|outside the .*sandbox|
   canceled the request`.
3. Fix the `●`/`⏺` byte in the claude ToolUse pattern while in there.
4. Re-run `replay_manifest_regression` — fixture transitions that now
   include `permission_prompt` should be reviewed and their
   `expected_transitions` updated as a deliberate change.

## Out of scope

- Stage 2c ContextFull recording (PLAN explicitly defers).
- Changing replay harness hysteresis pacing.
