# Sprint 49 PLAN — Channel Discipline Correction (NARROW)

**Date**: 2026-05-04
**Author**: lead
**Status**: PLAN (awaiting §8 GO + scope ruling)
**Source-of-truth**: `origin/main` HEAD `834f30d`
**Synthesis inputs (preserved)**:
- dev STRUCTURAL — m-20260504113841219730-5
- reviewer PRIOR-ART — m-20260504113717979051-4
- reviewer COST-BENEFIT — m-20260504113936047827-7
- lead MINIMAL-DELTA — this document §5

**Re-scope note (2026-05-04 ~11:50 UTC, operator m-15)**: Original PLAN proposed reroute + inject + sticky + keyword escalation + feature flag + P1+P2 split (~165 LOC across 2 PRs). Operator narrowed to **inject-only nudge** (no PTY text extraction, no reroute, no sticky, no keyword, no feature flag, single PR). Reasoning: daemon only nudges; agent re-emits via proper tool; avoids PTY parsing accuracy bar entirely.

---

## §0 Context

Fleet-wide recurring problem: agent receives input via Telegram → emits direct text to its TUI pane (without using `reply` MCP tool) → operator on Telegram never sees the response. Channel discipline slip occurs daily.

**Narrow design**: daemon detects mismatch (last input was Telegram + agent's response went to direct text instead of `reply` tool) → injects a system message to the agent: "You responded to the wrong channel. Please use the `reply` tool to send your response." Agent re-emits via the right tool. Operator/peer receive naturally.

## §1 Goal

Daemon-side enforcement via **inject-only nudge**:
1. Track `last_input_channel` per instance
2. On agent emit completion (Idle→Ready transition), check if response went via `reply` tool when input was inbound (Telegram or peer)
3. If mismatch → inject correction prompt to agent's pane
4. Cooldown to prevent inject-loop

**Non-goals (cut from original PLAN)**:
- ❌ Reroute / PTY text extraction (no daemon-side parsing)
- ❌ Per-backend regex / confidence-score gate
- ❌ Active-channel sticky 60s race guard
- ❌ ERROR/FAILED keyword escalation
- ❌ Feature flag (direct enable for fleet)
- ❌ P1+P2 split (single PR)

## §2 Verified state (origin/main 834f30d)

Existing infrastructure:
- `src/inbox.rs` — inbox dispatch with `save_metadata` for `last_message_id`
- `src/state.rs` — agent state machine; Idle→Ready transition exists (no new regex needed)
- `src/heartbeat_pair.rs` — per-instance heartbeat metadata
- `src/daemon/supervisor.rs::tick()` — periodic per-instance check
- `src/mcp/handlers/channel.rs::handle_reply()` — MCP reply tool callsite

## §3 Design

### §3.1 State (per-instance metadata)

```rust
last_input_channel: Option<String>,        // "telegram" | "tui" | "agent_peer"
last_input_at_ms: Option<i64>,             // for cooldown/window checks
reply_tool_called_since_input: bool,       // reset on new inbox dequeue
last_inject_at_ms: Option<i64>,            // cooldown anchor
inject_count_since_input: u32,             // cooldown counter (per input cycle)
```

### §3.2 Hook sites + LOC

| Site | LOC | Risk | Notes |
|------|-----|------|-------|
| `inbox.rs` write `last_input_channel` on dequeue | 10 | Low | Telegram/peer paths only; TUI direct skip |
| `state.rs::transition()` arm `response_complete_pending` on Idle→Ready | 15 | Low | Use existing transition, add 1 flag |
| `mcp/handlers/channel.rs::handle_reply()` write `reply_tool_called_since_input = true` | 5 | Low | Existing handler, 1 line write |
| `daemon/supervisor.rs::tick()` mismatch detect + inject + cooldown | 20 | Med | Core logic |

**Total ~50 LOC** across 4 files. Single Tier-2 PR.

### §3.3 Detection logic (supervisor tick)

```
On supervisor tick per instance:
1. If !response_complete_pending → skip
2. If reply_tool_called_since_input → clear flag, skip (well-behaved)
3. If last_input_channel == "tui" || None → skip (no inbound provenance)
4. If inject_count_since_input >= cooldown_N → skip (already nudged enough)
5. If now_ms - last_inject_at_ms < cooldown_window_ms → skip (too recent)
6. Inject correction prompt to agent's pane via existing pane.send_text()
7. Increment inject_count_since_input
8. Update last_inject_at_ms
9. Clear response_complete_pending (next response cycle re-arms)
```

### §3.4 Inject message wording (default; §13 question)

```
[CHANNEL DISCIPLINE] You received input from {channel} but your last response went to direct text. Please re-send using the `reply` MCP tool so the operator/peer sees it.
```

(Wording adjustable per operator §13.2.)

### §3.5 Cooldown

- Default `N=2` injects per input cycle
- Counter resets on next inbox dequeue (new input cycle)
- Cooldown window between injects: 30s minimum
- After N hits: silent (no further injects until next input cycle)

## §4 Phasing (single PR)

**Single PR — Tier-2 dual review** (codex PRIMARY + lead cross-vantage). High blast radius on supervisor tick + state transitions.

**Scope** ~50 LOC across 4 files (above).

**Tests**:
- `last_input_channel` persists across daemon restart (round-trip serde test)
- Agent receives Telegram + replies via `reply` tool → no inject (well-behaved)
- Agent receives Telegram + emits direct text → inject fires once
- After inject, agent uses `reply` tool → flag clears, no re-inject on next response
- TUI direct input → no inject (skip rule #3)
- Cooldown: 3 consecutive direct-text emits → 2 injects + 1 silent

**Done definition**: Pilot on lead + dev pair-test (lead → dev via Telegram-bound channel). Verify:
- 0 false-positives on TUI direct input
- 1 inject per direct-text emit (within cooldown)
- Cooldown caps at N=2 per input cycle

## §5 MINIMAL-DELTA verification (lead vantage)

**Smallest viable enforcement**: skip cooldown entirely (~40 LOC). Rejected — unbounded inject loop on stuck agent context bloats fast.

**Smaller alternative considered + rejected**: skip the `reply_tool_called_since_input` flag (just check direct text always). Rejected — well-behaved agents that already use `reply` tool would still get false-positive nudges.

**Larger alternative considered + rejected**: original PLAN (reroute + PTY extract + sticky + keyword + flag + 2 PRs). Rejected per operator m-15 — complexity exceeds 80/20 floor.

**Hook minimum**: 4 sites (inbox dequeue + state transition + reply handler + supervisor tick). All exist; this PR adds metadata field reads/writes + 1 inject path. No new modules.

## §6 Backward compat

- Direct enable for fleet (no feature flag per operator §scope cuts)
- Day 1: all instances check channel discipline once shipped
- Risk acceptable per operator m-15: inject-only is safe (no text extraction, no reroute)
- Existing agent code path unchanged — only adds nudge on miss

## §7 Risks

**MED**:
- False-positive inject when agent receives Telegram input but the response is genuinely meant for pane (e.g., agent says "thinking..." in pane while preparing tool call). **Mitigation**: only inject on `Idle→Ready` transition (response complete), not during Thinking/ToolUse. Cooldown N=2 caps loop.
- Agent doesn't honor inject prompt (keeps emitting direct text). **Mitigation**: cooldown stops after N=2; operator notification (§13.4) surfaces persistent miss.

**LOW**:
- `last_input_channel` race: input arrives during emit. **Mitigation**: input cycle tracked per inbox dequeue + reply tool flag; new input resets flag.
- `inject_count` not persisted across daemon restart. **Mitigation**: acceptable — restart resets cooldown is benign behavior.

## §8 §13 candidate questions for operator

1. **Cooldown N**: default `N=2` per input cycle — accept, or different (1 / 3 / configurable)?
2. **Inject message wording**: default `[CHANNEL DISCIPLINE] You received input from {channel} but your last response went to direct text. Please re-send using the `reply` MCP tool so the operator/peer sees it.` — accept or revise (specifically: tool name, channel name interpolation, brevity)?
3. **Detection hook**: use existing `state.rs::transition()` Idle→Ready edge (lead recommend, no new code) vs new dedicated hook?
4. **First-inject operator notification**: when daemon first nudges an instance (per input cycle), should daemon send a `notify_telegram` to operator so operator knows enforcement fired? Default = no (silent first inject), but adjustable.

## §9 Estimates

- IMPL: ~3-4h code + 1-2 review cycles ~5-7h elapsed total
- Pilot observation: ~1 day on lead + dev pair
- Total Sprint 49: ~1-2 wall-clock days

## §10 Reuse from prior synthesis

- dev STRUCTURAL m-5 §1 hook sites: 4 of 4 still apply (with smaller LOC)
- dev STRUCTURAL m-5 §2 PTY analysis: only the "Idle→Ready already detects response complete" finding applies; per-backend regex unused per re-scope
- reviewer PRIOR-ART m-4 §3 IRC + §6 cooldown idioms: confirms inject + cooldown is canonical
- reviewer COST-BENEFIT m-7 §2 cooldown N=2: applied as default

## §11 Sprint 50+ deferred items

- Reroute (PTY text extraction) — original Phase 1 deliverable, deferred
- Active-channel sticky 60s race guard — deferred until day-1 race observed
- ERROR/FAILED keyword escalation — deferred
- Feature flag + per-instance opt-in — deferred (direct enable instead)
- Confidence-score PTY pattern gate — deferred (Sprint 50 candidate per reviewer §4)
- Strict-block mode — deferred (risk-bounded, only after long observation)

---

**End of PLAN — awaiting operator §13 answers + §8 GO**
