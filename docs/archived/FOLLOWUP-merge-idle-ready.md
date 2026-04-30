# Follow-up: Merge `AgentState::Idle` into `AgentState::Ready`

**Filed**: 2026-04-20 (deferred from Phase 1c design review)
**Status**: Open — not scheduled

## Background

During Phase 1c (`feat/phase-1c-feed-fallback`) we revisited whether the
`Idle` / `Ready` split carries real semantic weight. Findings:

- **Detection signals overlap.** For every backend, the `Idle` pattern is
  either a subset of the `Ready` pattern or trivially co-present:
  - OpenCode: Ready `Ask anything|tab agents` vs. Idle `Ask anything`
  - Gemini: Ready `Type your message|YOLO` vs. Idle `Type your message`
  - ClaudeCode: Ready `bypass permissions` (footer) vs. Idle `❯` (prompt)
  - Kiro: Ready `Trust All Tools active|/quit to exit` vs. Idle `›`
- **Behavioral parity.** Both are passive (5 s hysteresis hold), both are
  exempt from `check_hang` / `check_awaiting_operator`, neither is an
  error state.
- **Only real difference** is render color: `Ready → Green`,
  `Idle → DarkGray`. Operators have no behavioral distinction to make.

## Proposal

Remove `AgentState::Idle`. Everywhere the tracker would transition to
`Idle` today it should transition to `Ready` instead.

## Scope of change

- `src/state.rs`: drop variant from enum, `priority()`, `display_name()`,
  `for_backend()` pattern tables; delete Idle-specific tests; audit the
  `priority() <= AgentState::Idle.priority()` expression in `transition()`
  (replace with `priority() <= AgentState::Ready.priority()` or
  equivalent).
- `src/render.rs`: drop the `Idle` match arm (auto-fallback to `Ready`
  color via exhaustive check).
- `src/health.rs`: remove `AgentState::Idle` from the
  awaiting-operator-exempt list in `test_awaiting_operator_non_starting_exempt`.
- `src/snapshot.rs`: test fixtures currently use the string `"idle"`; pick
  `"ready"` or `"busy"` consistently.
- Any external consumer (telegram status line, docs) referencing the
  string `"idle"`.

## Not in scope

- Renaming `Ready`. The word covers both conditions adequately.
- Changing hysteresis semantics.

## Why deferred

Phase 1c adds a silence-based fallback (`detect()` returning `None` on a
changed screen downgrades `Thinking`/`ToolUse` back to `Ready` after
30 s). That is orthogonal to the Idle/Ready cleanup; bundling would mix
a safety fix with an enum refactor that touches snapshot serialization
and a dozen test sites.

Do this after Phase 1c lands, as its own worktree
(`refactor/merge-idle-into-ready`).
