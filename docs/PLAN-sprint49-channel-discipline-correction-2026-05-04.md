# Sprint 49 PLAN — Channel Discipline Correction

**Date**: 2026-05-04
**Author**: lead
**Status**: PLAN (awaiting §8 GO + scope ruling)
**Source-of-truth**: `origin/main` HEAD `834f30d` (Sprint 48 PR 4 just merged)
**Synthesis inputs**:
- dev STRUCTURAL — m-20260504113841219730-5
- reviewer PRIOR-ART — m-20260504113717979051-4
- reviewer COST-BENEFIT — m-20260504113936047827-7
- lead MINIMAL-DELTA — this document §5

---

## §0 Context

Fleet-wide recurring problem: agent receives input via Telegram → emits direct text to its TUI pane (without using `reply` MCP tool) → operator on Telegram never sees the response. Channel discipline slip occurs daily; protocol rules + memory accumulation are treating symptoms not root cause.

Operator dispatch m-20260504113415876345-0 set Sprint 49 = daemon-level enforcement: detect channel mismatch + reroute response + inject correction prompt to agent.

## §1 Goal

Daemon-side enforcement of channel discipline: when an agent receives input on channel X but emits its response on channel Y, daemon catches the mismatch and:
1. **Re-routes** the response to the correct channel (operator sees reply)
2. **Injects correction** to the agent (next time use the right tool)
3. **Cooldowns** to prevent context bloat from correction-loop

**Non-goals**:
- Strict-block mode (deferred — risk too high for day 1)
- Confidence-score PTY pattern gate (deferred — over-engineering for current fleet scale per reviewer COST-BENEFIT m-7 §4)
- Agent-initiated broadcast / non-reply traffic (out-of-scope per reviewer §8)

## §2 Verified state (origin/main 834f30d)

Existing infrastructure relevant to design:
- `src/inbox.rs` — inbox dispatch + `save_metadata` for `last_message_id`
- `src/state.rs` — agent state machine with Thinking / ToolUse / Idle / Ready transitions (existing per-backend regex patterns sufficient per dev STRUCTURAL m-5 §2)
- `src/heartbeat_pair.rs` — per-instance heartbeat metadata
- `src/daemon/supervisor.rs::tick()` — periodic per-instance check loop
- `src/mcp/handlers/channel.rs::handle_reply()` — MCP `reply` tool callsite
- `src/channel/{telegram,...}` — channel adapters with `send_from_agent` API
- `src/agent.rs` — agent registry + Pane.last_input_at field

## §3 Design

### §3.1 State model (per-instance)

```rust
// Added to metadata/{instance}.json (or heartbeat_pair in-memory)
last_input_channel: Option<String>,     // "telegram" | "tui" | "agent_peer"
last_input_channel_at: Option<i64>,     // ms timestamp
last_reply_tool_at_ms: Option<i64>,     // last MCP reply call
reply_tool_called_since_input: bool,    // reset on new inbox dequeue
channel_discipline_reroute_count: u32,  // P2 cooldown counter
channel_discipline_window_start_ms: i64,// P2 10min window anchor
channel_discipline_silent_until_ms: i64,// P2 30min silent backoff
```

### §3.2 Hook sites (per dev STRUCTURAL m-5 §1)

| Site | LOC | Risk | Phase |
|------|-----|------|-------|
| `inbound.rs:283` write `last_input_channel` on dequeue | 15 | Low | P1 |
| `state.rs:transition()` arm `response_complete_pending` on Thinking/ToolUse → Idle/Ready | 25 | Med | P1 |
| `mcp/handlers/channel.rs::handle_reply()` write `last_reply_tool_at_ms` + flag | 10 | Low | P1 |
| `daemon/supervisor.rs::tick()` mismatch detection + reroute path | 60 | High | P1 |
| Cooldown logic + escalation | 15 | Low | P2 |
| Active-channel sticky 60s + 3s race guard | 10 | Low | P1 |
| Reroute failure retry queue | 20 | Med | P2 |
| Feature flag (`AGEND_CHANNEL_DISCIPLINE` env + fleet.yaml) | 10 | Low | P1 |

### §3.3 Detection logic (P1 supervisor tick)

```
On supervisor tick per instance:
1. If !channel_discipline_enabled → skip (feature flag off)
2. If !response_complete_pending → skip (still working)
3. If reply_tool_called_since_input → clear flag, skip (well-behaved)
4. If !last_input_channel || last_input_channel == "tui" → skip (no inbound provenance per reviewer §8)
5. Capture vterm tail since last_input_at as response_text
6. If response_text contains "ERROR" / "FAILED" / "REJECTED" keywords → escalate priority (per reviewer §5)
7. Reroute via ch.send_from_agent(name, Reply { text: response_text })
8. Inject correction prompt to agent: "Direct text emit detected — please use reply tool for Telegram responses"
9. Increment channel_discipline_reroute_count (P2 only acts on this)
10. Clear response_complete_pending
```

### §3.4 Active-channel selection (edge case #7)

```
fn active_channel(instance) -> &str:
  let tg_ts = metadata.last_input_channel_at;  // telegram inbound
  let tui_ts = pane.last_input_at;             // TUI direct input
  if (tg_ts - tui_ts).abs() < 3_000ms:         // race guard
    return last sticky channel (60s window)
  if tg_ts > tui_ts:
    return "telegram"
  else:
    return "tui"
```

### §3.5 Multi-target emit handling (edge case #3, reviewer §5)

- First MCP `reply` tool call after input → primary response
- Subsequent direct text → supplementary (no reroute)
- **EXCEPTION**: direct text containing `ERROR` / `FAILED` / `REJECTED` keywords → escalate reroute even after reply (high-signal info would be missed)

### §3.6 P2 — Cooldown + retry (per reviewer §2)

```
Cooldown logic:
- N=2 reroute events within 10min window → escalate (inject prompt + notify operator)
- After 2 escalations → silent reroute-only for 30min + jittered re-arm (avoid retry storm)
- Window reset on successful reply tool call

Retry queue (reroute failure):
- ch.send_from_agent fails → write to inbox/{instance}.jsonl with delivery_mode: "reroute_pending"
- Next tick retries
- After 3 failures → drop + log
```

## §4 Phase split

### Phase 1 — Core detection + reroute + feature flag (Tier-2 dual)

**Scope** ~120 LOC across 6 files:
- `src/inbound.rs` last_input_channel tracking (~15)
- `src/state.rs` response_complete_pending flag (~25)
- `src/mcp/handlers/channel.rs` reply tool timestamp (~10)
- `src/daemon/supervisor.rs` mismatch detection + reroute (~60)
- `src/heartbeat_pair.rs` new metadata fields (~10)
- Active-channel sticky helper (~10)
- ERROR/FAILED/REJECTED keyword guard (in supervisor reroute path)
- Feature flag (env + fleet.yaml field) (~10)

**Tier**: Tier-2 dual review — codex PRIMARY + lead cross-vantage. High blast radius on message routing.

**Tests**:
- last_input_channel persists across daemon restart
- response_complete_pending arms on Thinking → Idle (debounced)
- reroute fires when mismatch detected
- reply tool flag prevents reroute (well-behaved case)
- TUI input → no reroute (no inbound provenance)
- Active-channel sticky preserves recent within 3s race
- ERROR keyword escalation triggers reroute
- Feature flag off → no detection at all

**Done definition**: Pilot enabled on lead + dev instances. Observe 24-72h reroute count + zero false-positives on agent-initiated traffic.

### Phase 2 — Cooldown + retry queue (Tier-1 single)

**Scope** ~45 LOC:
- Cooldown state machine (N=2/10min/30min silent/jitter) (~15)
- Retry queue for failed reroute (~20)
- Operator notification on cooldown escalation (~10)

**Tier**: Tier-1 single — codex review only. Lower blast radius.

**Dependency**: requires P1 merged + 24-72h observation.

**Done definition**: Reroute storm prevention verified. Operator-visible notification fires only on N+1 escalation.

### Phase 3 — DEFERRED to Sprint 50+ (per reviewer §4)

- Confidence-score PTY pattern gate (over-engineering for current scale)
- Strict-block mode (risk-bounded, only after long observation)
- Per-conversation routing state (rather than per-instance) — if multi-conversation traffic emerges

## §5 MINIMAL-DELTA verification (lead vantage)

**Smallest viable enforcement**: feature flag + state.rs flag + supervisor reroute = ~80 LOC. Could ship today.

**Rejected as too small**: skipping the active-channel sticky (~10 LOC) creates false-positive reroutes when operator switches between Telegram + TUI. The 3s race + 60s sticky is necessary correctness.

**Rejected as too large**: PRIOR-ART m-4 §7(E) confidence-score gate + control-mode block boundaries — ~50+ extra LOC. Not necessary at fleet scale per reviewer COST-BENEFIT m-7 §4.

**Smaller alternative considered + rejected**: rely entirely on `reply_tool_called_since_input` boolean (no PTY detection at all). Rejected because some agent backends emit final response without any tool call — no flag would ever flip and reroute never fires.

**Larger alternative considered + rejected**: bundle P1 + P2 into single PR. Rejected per reviewer COST-BENEFIT m-7 §3 — P1 first, observe, P2 second to bound rollback radius.

## §6 Backward compat

- Feature flag off by default → zero behavior change for existing fleet
- Per-instance opt-in via `fleet.yaml` `channel_discipline: true` field
- Day 1 rollout: enable on `lead` + `dev` only (highest signal sources)
- Week 2 rollout: fleet-wide after P1 stable
- Existing agent code path unchanged when flag off

## §7 Risks

**HIGH**:
- False-positive reroute when operator narrates intent in pane via TUI then expects agent to reply on TUI but agent caught mid-response → daemon reroutes to Telegram (last_input_channel still telegram). **Mitigation**: 3s race + 60s sticky on active-channel selection.
- Inject loop: agent receives correction prompt + treats as task → emits another response → caught again → loops. **Mitigation**: P2 cooldown N=2 / silent reroute after.

**MED**:
- vterm tail capture window: where to begin reading from? `last_input_at` is one anchor but agent may have responded BEFORE input was logged. **Mitigation**: capture from `last_input_at` minus small margin (300ms); test against false-positive sequence.
- Telegram down → reroute fails → retry queue grows. **Mitigation**: P2 retry queue with 3-fail drop.

**LOW**:
- ERROR keyword false-positive (legitimate error message in narration). **Mitigation**: keyword-based escalation only changes priority, doesn't change reroute behavior.

## §8 §13 candidate questions for operator

1. **Cooldown N**: N=2 (reviewer COST-BENEFIT) vs N=3 (dev STRUCTURAL) — pick?
2. **Day 1 rollout phase**: skip phase1 (reroute-only) and go directly to phase2 (reroute+inject) per reviewer COST-BENEFIT m-7 §1+§7? Or observe 1-2d in reroute-only first?
3. **ERROR/FAILED/REJECTED keyword escalation rule**: include in P1 (~5 LOC) or defer?
4. **Active-channel sticky window**: 60s (per lead MINIMAL-DELTA) vs longer/shorter?
5. **Feature flag default**: off (lead recommend) vs on for lead+dev only?
6. **P1+P2 same Sprint vs split**: reviewer COST-BENEFIT m-7 §9 推 same Sprint sequential. Confirm or split?
7. **Operator notification on cooldown escalation (P2)**: telegram message? Inbox entry? Daemon log only?
8. **Closure metric**: reviewer COST-BENEFIT m-7 §10 recommends "misroute-visible incidents=0 for 7 days continuous + reroute events/day reduction ≥80%". Agree or different threshold?
9. **PR ordering**: P1 → observe 24-72h → P2 — confirm sequential, no parallel?
10. **Sprint 50+ deferred items**: confirm confidence-score gate + strict-block + per-conversation routing all permanently deferred (not just Sprint 50)?

## §9 Estimates

- P1 IMPL: ~3-5h code + 2-3 review cycles ~6-9h elapsed total
- P1 observation: 24-72h pilot on lead + dev
- P2 IMPL: ~1.5-2h code + 1 review cycle ~3-4h elapsed
- Total Sprint 49: 2-4 wall-clock days (most time in observation window)

## §10 Reuse from prior synthesis

- Sprint 47 P1 timeout/concurrency hardening — refactor PRs benefit + supervisor tick already has known cadence
- Sprint 48 channel/telegram/* refactor — reroute path lands on cleanly split adapter modules
- Existing state.rs Idle/Ready transitions = response-complete signal (no new PTY regex per dev STRUCTURAL m-5 §2)

---

**End of PLAN — awaiting operator §13 answers + §8 GO**
