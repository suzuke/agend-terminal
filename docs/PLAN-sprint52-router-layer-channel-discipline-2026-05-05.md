# Sprint 52 PLAN — Router-Layer Channel Discipline (#426 redesign)

**Date**: 2026-05-05
**Author**: lead
**Status**: PLAN (awaiting §8 GO + scope ruling)
**Source-of-truth**: `origin/main` HEAD `895e341` (Hotfix #430 just merged)
**Synthesis inputs**:
- dev STRUCTURAL — m-20260505003548002175-40
- reviewer PRIOR-ART — m-20260505003455789492-39
- reviewer COST-BENEFIT — m-20260505003700687500-42
- lead MINIMAL-DELTA — this document §5

**Source issue**: GitHub issue [#426](https://github.com/suzuke/agend-terminal/issues/426)
**Supersedes**: Sprint 49 (PR #424 reverted via PR #425, deadlock + design issues)

---

## §0 Context

Sprint 49 attempted prompt-injection nudge approach: detect mismatch on supervisor tick, inject correction prompt to agent. Reverted due to:
- Supervisor self-IPC deadlock (supervisor calls daemon API which acquires same lock)
- 10s tick latency unacceptable for UX
- PTY content extraction accuracy bar fundamentally hard

Sprint 52 = router-layer fan-out (architectural redesign per operator m-35 + issue #426):
1. **Input attribution**: tag next response stream with `reply_to: X` channel
2. **Output mirror**: daemon observes PTY → if `reply_to` set, auto-mirror outbound text
3. **Reset on TUI input**: TUI keyboard clears `reply_to`
4. **Dedup**: agent uses `reply` tool → mirror skip that turn

Operator m-35 explicit: "PTY content extraction accuracy 跟 Sprint 49 本質一樣". Sprint 52 solves architectural issues but not semantic 100% accuracy. Confidence score deferred to Sprint 53.

## §1 Goal

Daemon-side router layer that:
1. Mirrors PTY output to inbound channel when agent fails to use `reply` tool
2. Avoids supervisor self-IPC deadlock (Sprint 49 root cause)
3. Sub-2s mirror latency (vs Sprint 49 10s tick)
4. Backend-agnostic (works for claude / kiro-cli / codex / gemini / opencode)
5. Verifiable via stress test + lock-ordering assertion

**Non-goals**:
- Confidence-score PTY accuracy gate (deferred Sprint 53 per reviewer COST-BENEFIT m-42 §1)
- vte full normalization (deferred Sprint 53, lightweight ANSI strip in P1 sufficient)
- 100% semantic accuracy (architectural fix only)

## §2 Verified state (origin/main 895e341)

Existing infrastructure:
- `src/agent.rs::AgentCore.subscribers` — PTY broadcast mechanism (used by TUI subscribers)
- `src/agent.rs::pty_read_loop` L700-740 — already broadcasts raw bytes
- `src/heartbeat_pair.rs` — per-instance lock-protected state
- `src/state.rs::StatePatterns::for_backend` — existing per-backend Ready/Idle transitions
- `src/layout/pane.rs::write_to_agent` L104 — single TUI keyboard write site
- `src/inbox.rs::notify_agent` — inbox→PTY inject path
- `src/mcp/handlers/channel.rs::handle_reply` — MCP reply tool callsite
- `src/mcp/handlers/instance.rs::handle_inject` — MCP inject tool callsite
- `docs/DAEMON-LOCK-ORDERING.md` — lock hierarchy doc

## §3 Design

### §3.1 InputSource enum (per reviewer COST-BENEFIT m-42 §5)

```rust
pub enum InputSource {
    TuiKey,       // local keyboard via pane.rs::write_to_agent
    MCPTool,      // programmatic MCP inject
    PeerMsg,      // agent_peer inbox delivery
    Telegram,     // telegram inbox delivery
}
```

Stamp every PTY write with source. Used by `reply_to` tracker to distinguish.

### §3.2 reply_to + dedup state (HeartbeatPair)

```rust
struct RouterState {
    reply_to_channel: Option<String>,        // "telegram" | "agent_peer" | None
    reply_to_input_id: Option<u64>,          // monotonic seq, set on inbox dequeue
    reply_to_set_at_ms: i64,                 // for fallback timeout
    last_mirror_event_id: Option<u64>,       // dedup primary key
    mirror_dispatched_for_turn: bool,        // dedup turn-window flag (1s)
    mirror_skip_until_next_turn: bool,       // set when handle_reply succeeds
}
```

**Lifecycle**:
- Set: `inbox::dequeue` → assign monotonic `input_id`, set `reply_to_channel` + `reply_to_set_at_ms`
- Clear: `pane.rs::write_to_agent` (TUI keyboard) OR Ready/Idle state transition (per-turn lifecycle, operator Q1)
- Reset on next dequeue: `mirror_dispatched_for_turn = false`, `mirror_skip = false`, fresh `input_id`

### §3.3 PTY observer architecture (per reviewer COST-BENEFIT m-42 §3 + dev §1)

```
agent.rs::pty_read_loop
    │
    ├──► existing TUI subscribers (unchanged)
    │
    └──► NEW router subscriber (sidecar, per-agent)
              │
              ▼
         router thread (in src/daemon/router.rs)
              │
              ├──► accumulate bytes between input_id N and Ready/Idle transition
              ├──► lightweight ANSI strip (P1, vte deferred to Sprint 53)
              ├──► state edge OR silence-timeout fallback (3s) → end-of-turn
              ├──► dedup check (event_id + 1s window)
              └──► channel::send_from_agent (NO daemon API self-call)
```

**Lock ordering** (per `docs/DAEMON-LOCK-ORDERING.md`):
- L1 `registry` → L2 `agent_core` → L3 `heartbeat_pair`
- Router thread reads from subscriber channel (no lock), writes to L3 `heartbeat_pair` only
- **Never acquires L1 or L2** → no deadlock cycle with supervisor (which holds L1/L2)

### §3.4 End-of-turn detection (per reviewer COST-BENEFIT m-42 §2)

3-tier degradation:
1. **Primary**: existing `AgentState::Ready` / `AgentState::Idle` state transition (state edge)
2. **Fallback**: silence timeout 3s (no PTY output for 3s after last input)
3. **Deferred Sprint 53**: confidence score (per backend regex + heuristics)

P1 ships #1 + #2. #3 deferred per operator m-35 + reviewer COST-BENEFIT.

### §3.5 Dedup mechanism (per reviewer COST-BENEFIT m-42 §4)

**Event-id-first** (primary, not boolean flag):
- Each `reply_to` set assigns monotonic `input_id`
- Mirror dispatch tagged with `(input_id, mirror_event_id)`
- HeartbeatPair tracks `last_mirror_event_id` per agent
- Dedup: skip mirror if same `mirror_event_id` already dispatched

**1s window fallback** (secondary):
- After mirror dispatched, set `mirror_dispatched_for_turn = true`
- Cleared on Ready/Idle transition (next turn)
- Catches race where agent emits within 1s but turn boundary not yet hit

**handle_reply skip** (tertiary):
- `mcp/handlers/channel.rs::handle_reply` success → `mirror_skip_until_next_turn = true`
- Router observer respects flag → skip current turn mirror

### §3.6 Mirror text extraction

```
Accumulator buffer (per agent):
- Reset on inbox dequeue (input_id N)
- Append bytes from PTY observer subscriber
- Skip bytes received while state == Thinking or ToolUse
- On Ready/Idle transition: extract text via VTerm tail_lines + ANSI strip
- Length check (>0) + dedup check + mirror_skip check
- Dispatch via channel::send_from_agent
```

ANSI strip in P1: simple regex-based strip (4-5 LOC). vte parser deferred Sprint 53.

### §3.7 Multi-channel race (per reviewer m-42 §5)

Last-write-wins via natural HeartbeatPair lock + `InputSource` stamp for diagnostics.

When agent_peer + telegram both set `reply_to` within ms:
- Last `inbox::dequeue` overwrites
- `InputSource` stamp surfaces "telegram superseded peer" in trace logs
- Acceptable per operator open Q4 (PTY order = ground truth)

## §4 Phase split (per reviewer COST-BENEFIT m-42 §7)

### PR-A — Observer infra + reply_to wiring (Tier-2 dual)

**Scope** ~80 LOC:
- `src/heartbeat_pair.rs` add RouterState fields (~25 LOC)
- `src/agent.rs` add router subscriber registration (~10 LOC)
- `src/daemon/router.rs` (new) skeleton thread + subscriber consumer (~30 LOC)
- `src/inbox.rs::dequeue` set reply_to + input_id (~10 LOC)
- `src/layout/pane.rs::write_to_agent` clear reply_to on TUI keyboard (~5 LOC)
- `src/mcp/handlers/channel.rs::handle_reply` set mirror_skip flag (~5 LOC)

**Tests**:
- `reply_to_set_on_inbox_dequeue` (round-trip serde)
- `reply_to_cleared_on_tui_input`
- `reply_to_cleared_on_ready_transition`
- `mirror_skip_set_on_reply_tool_call`
- `lock_ordering_assertion` (no router→registry calls)

**Tier**: Tier-2 dual review (codex PRIMARY + lead cross-vantage). Touches state + lifecycle.

**Done definition**: Pilot wiring observable in trace logs. No mirror dispatch yet (PR-B).

### PR-B — Mirror + dedup + fallback (Tier-2 dual)

**Scope** ~100 LOC:
- `src/daemon/router.rs` mirror dispatch logic (~50 LOC)
- ANSI strip helper (~10 LOC, regex-based)
- Accumulator buffer + state-skip filter (~20 LOC)
- Event-id dedup + 1s window fallback (~10 LOC)
- Silence timeout fallback (~10 LOC)

**Tests**:
- `mirror_fires_on_telegram_input_direct_text_response`
- `mirror_skipped_when_reply_tool_used`
- `mirror_dedup_blocks_double_emit`
- `silence_timeout_fallback_triggers_when_state_stuck`
- `mirror_filtered_thinking_state_bytes`
- `mirror_with_anti_ansi_text` (extraction correctness)

**Stress tests** (per reviewer COST-BENEFIT m-42 §6):
- Flood test: 50+ synthetic PTY events / sec for 10min
- Queue overflow: bounded subscriber channel saturation
- Lock contention: 50 agents concurrent dequeue + mirror
- Restart recovery: ephemeral state cleared correctly
- Property-based state machine test: random event sequence convergence
- 1-2h soak: drift counter on mirror dedup correctness

**Tier**: Tier-2 dual + stress test gate. **Mandatory stress green before merge**.

### Deferred (Sprint 53+)

- Confidence score PTY accuracy gate
- vte full ANSI parser
- 24h soak test
- Per-channel mirror policy customization

## §5 MINIMAL-DELTA verification (lead vantage)

**Smallest viable** = ship PR-A only. Rejected — observer wiring without mirror dispatch = useless infra.

**Rejected as too small**: skip silence-timeout fallback. Risk too high — Ready/Idle detection has known false-negative on some backends.

**Rejected as too large**: include confidence score. Per operator m-35 + reviewer m-42 §1 — Sprint 52 is architectural fix; semantic accuracy = Sprint 53 separate scope.

**Lock ordering verified**: dev m-40 §8 design respects `DAEMON-LOCK-ORDERING.md`. Stress test gate enforces empirically.

**Sprint 49 deadlock root**: supervisor calls `api::call(...)` which loops back to daemon. Sprint 52 router thread NEVER calls daemon API — uses `channel::send_from_agent` directly (same path as `handle_reply` MCP).

## §6 Backward compat

- Existing TUI subscribers unchanged
- Existing `reply` tool path unchanged (still primary documented path)
- `reply_to` ephemeral (cleared on daemon restart) — no migration
- Feature flag `AGEND_ROUTER_LAYER=1` env var (default off until pilot stable)
- Per-instance opt-in via `fleet.yaml` `router_layer: true` field (lead+dev pilot first)

## §7 Risks

**HIGH**:
- Mirror false-positive sends thinking text to operator. **Mitigation**: state-skip filter + silence timeout fallback + dedup. Not perfect — semantic accuracy = Sprint 53.
- Lock ordering violation under specific concurrency. **Mitigation**: stress test mandatory gate + lock-order assertion logging.
- Sprint 49-class deadlock returns. **Mitigation**: separate router thread, no daemon API self-call, lock-ordering test.

**MED**:
- ANSI strip regex incomplete (some sequences leak). **Mitigation**: P1 acceptable, vte upgrade Sprint 53.
- Subscriber channel saturation under heavy PTY traffic. **Mitigation**: bounded channel + drop+metrics policy.

**LOW**:
- Multi-channel race produces wrong reply_to. **Mitigation**: PTY order = ground truth, naturally serialized.
- Trace log volume from event-id stamping. **Mitigation**: log level config.

## §8 §13 candidate questions for operator

1. **Phase split**: 2-PR (PR-A observer + PR-B mirror) per reviewer COST-BENEFIT vs single PR ~170-200 LOC?
2. **Stress test soak**: 1-2h for Sprint 52 + 24h deferred Sprint 53 — agree, or different threshold?
3. **Closure metrics**: p95 mirror latency <2s + false-positive mirror <0.5% + dedup drift <0.1% + 4-category stress green — accept, or adjust thresholds?
4. **Silence timeout fallback value**: 3s — accept, or longer/shorter?
5. **Feature flag rollout**: `AGEND_ROUTER_LAYER=1` env + per-instance opt-in (lead+dev pilot first), default off — agree?
6. **Confidence score Sprint 53 timing**: defer indefinitely, or Sprint 53 follow-up after Sprint 52 observation window?
7. **vte full normalization Sprint 53**: defer indefinitely, or Sprint 53 if ANSI strip regex shows gaps?
8. **Stress test agent count**: 50 synthetic agents — sufficient, or higher?
9. **Tier classification**: Both PR-A + PR-B Tier-2 dual — confirm?
10. **Sprint 49 lessons applied verification**: lead pre-IMPL invariant tests (lock-ordering, no-self-IPC) before dispatch — operator agrees?

## §9 Estimates

- PR-A IMPL: ~3-4h code + 2-3 review cycles ~6-9h elapsed
- PR-B IMPL: ~5-7h code + stress test setup ~2-3h + 2-3 review cycles ~10-15h elapsed
- Pilot observation: 2-4 days on lead + dev
- Total Sprint 52: ~3-5 wall-clock days (including pilot)

**Sprint 49 lesson "不急 ship"**: lead pre-IMPL invariant test plan (lock-ordering assertion, no-self-IPC verification) before dev IMPL dispatch.

## §10 Reuse from prior synthesis

- Sprint 49 PRIOR-ART m-4 §1-3 (Slack/Discord/Matrix routing) — confirms router-layer mirror approach
- Sprint 49 STRUCTURAL m-5 §2 (existing Ready/Idle patterns) — reused as primary end-of-turn signal
- `feedback_kind_task_self_route_workaround.md` — Sprint 46 ID routing now ships, no name-collision routing concern
- Sprint 47 P1 timeout + concurrency — refactor PRs benefit
- Sprint 49 deadlock root cause analysis — explicit avoidance design

## §11 Sprint 53+ deferred items

- Confidence-score PTY accuracy gate
- vte full ANSI normalization parser
- 24h soak test extension
- Per-channel mirror policy customization
- Cross-instance routing (current is per-instance)
- Strict-block mode (replace mirror with explicit reject)

---

**End of PLAN — awaiting operator §13 answers + §8 GO**
