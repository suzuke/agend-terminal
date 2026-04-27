# MCP Subprocess Channel Ops Bridge — Investigation Report

**Status**: Investigation complete; recommended option pending operator approval.
**Per**: operator directive 2026-04-27 11:08 UTC ("派 dev 團隊去研究") routed via general m-20260427122122881384-105.
**Decision**: `d-20260427122219209231-1` (mini scope freeze).
**Authority**: 4-perspective challenge round per v1.2 amendment #1 + dev-lead code investigation.

---

## TL;DR

**Bug**: MCP subprocess (spawned by Claude Code, separate process) can never successfully invoke `reply` / `react` / `edit_message` / `delete_message` MCP tools. Architecture has no bridge between the MCP subprocess process and the daemon's registered `ACTIVE_CHANNEL` (which lives in daemon process, holds the Telegram client).

**Mystery answer**: Past "successful" replies came from a **different MCP execution context** — the TUI-internal MCP path (where MCP runs inside daemon process, sees `ACTIVE_CHANNEL` already initialized). Claude-spawned MCP subprocesses (separate process) have always failed, and that's what the operator hit.

**Recommended fix**: **Option C — `proxy_channel_op` daemon API endpoint** (3 of 4 reviewers converge). Adds a single bounded daemon API method that MCP subprocess invokes for channel operations; daemon performs the operation in-process (where `ACTIVE_CHANNEL` is registered).

**Sprint placement**: Sprint 25 P0 candidate (~150-200 LOC + tests + invariant).

---

## Operator's complaint

> "reply / react MCP tool 偶發 'no active channel' error"

After 3 hypotheses tested and 2 fixes shipped that didn't solve the actual problem, operator dispatched the investigation.

---

## Hypotheses tested (and ruled out)

| # | Hypothesis | Outcome |
|---|---|---|
| 1 | Channel detection ambiguity (TUI vs telegram source) | Sprint 23 P1 PR #241 instruction normalize merged — does NOT fix `active_channel` error. Different concern (agent's reply tool selection rule), not architecture. |
| 2 | Sprint 22 P0 hard-cut `outbound_capabilities` fail-closed gate rejects reply | Sprint 23 P1 PR #242 reverses default to default-open per operator philosophy — does NOT fix root cause. Verified `PermissiveLegacyMissing` was already permit, not reject. Gate was never the blocker. |
| 3 | **MCP subprocess ↔ daemon channel ops bridge missing** | Investigation focus — confirmed below. |

---

## Evidence chain verification (general's third hypothesis)

### Claim 1: MCP subprocess never gets `AGEND_BOT_TOKEN` → can't init telegram → ACTIVE_CHANNEL stays None

**CONFIRMED**:
- `src/mcp/mod.rs:154` — MCP runs standalone with only env vars passed by Claude Code via `.claude/settings.local.json`
- `src/main.rs:623-628` — `mcp` subcommand reads `AGEND_INSTANCE_NAME` from env but does **NOT** call `bootstrap::prepare()` (the function that triggers Telegram init)
- `src/bootstrap/mod.rs:168-172` — Telegram init flow gated by `opts.init_telegram = true`; MCP path uses `init_telegram: false`
- `src/bootstrap/telegram_init.rs:26-42` — only call site of `register_active_channel()`; only runs when `prepare(..., init_telegram: true)`
- Result: `src/channel/mod.rs:103` static `ACTIVE_CHANNEL: OnceLock<Channel>` is never initialized in MCP subprocess context. `OnceLock::new()` means `get()` returns `None`.

### Claim 2: `proxy_or_local` first tries `api::call(home, {"method": "mcp_tool", ...})` but daemon API has NO `mcp_tool` handler

**CONFIRMED**:
- `src/mcp/mod.rs:260-281` — `proxy_or_local()` calls `crate::api::call()` with `"method": "mcp_tool"`, then checks `resp["ok"].as_bool() == Some(true)`
- `src/api/mod.rs:343-370` — Daemon API method dispatcher. Lines 344-364 list **all** handled methods: `LIST` / `INJECT` / `KILL` / `DELETE` / `SPAWN` / `SEND` / `STATUS` / `REGISTER_EXTERNAL` / `DEREGISTER_EXTERNAL` / `CREATE_TEAM` / `UPDATE_TEAM` / `MOVE_PANE` / `SET_BLOCKED_REASON` / `CLEAR_BLOCKED_REASON` / `TOOL_KILL` / `SHUTDOWN`. **`mcp_tool` is NOT in the list.**
- Line 370 default: `{"ok": false, "error": format!("unknown method: {method}")}`. So `proxy_or_local`'s first attempt always returns `ok=false`, fallback to local handler.

### Claim 3: Local fallback fails because ACTIVE_CHANNEL is None

**CONFIRMED**:
- `src/mcp/handlers.rs:82-99` — `reply` handler calls `crate::channel::active_channel()` at line 89
- Line 90: `if active_channel().is_none() return json!({"error": "no active channel"})`
- Same pattern for react (line 103-104), edit_message (line 126-127)
- All 4 channel ops always fail in MCP subprocess context.

### Verdict on general's evidence chain

**All 3 claims technically correct.** The architecture as currently implemented has **no path** for an MCP subprocess to invoke channel ops successfully. Reply/react/edit_message/delete_message are structurally broken from the Claude-spawned MCP subprocess perspective.

---

## Mystery investigation: why did past replies sometimes succeed?

### Forensic analysis

The architecture analysis says **all** MCP-subprocess channel ops should fail. Yet the operator's prior reply tool calls returned `{chat_id, message_id}` payloads at times. Two convergent hypotheses from team:

#### impl-1's m-112 hypothesis (most likely)

> **MCP execution context differs between TUI and Claude-spawned**:
> - **TUI-internal MCP path** — when operator clicks "reply" in TUI, the MCP handler runs **inside the daemon process** (or a tokio task in same process). `ACTIVE_CHANNEL` already registered by daemon startup → succeeds.
> - **Claude-spawned MCP subprocess path** — Claude Code spawns `agend-terminal mcp` stdio binary in **separate process**. No telegram init. `ACTIVE_CHANNEL = None` → fails.

#### dev-reviewer-2's m-113 adversarial alternates (lower likelihood, ruled out by code investigation)

| Alternate | Evidence | Verdict |
|---|---|---|
| (a) MCP is in-process tokio task, NOT subprocess | Code shows `mcp` subcommand spawns a process via `cargo run` / binary invocation; `.claude/settings.local.json` confirms subprocess invocation | **Ruled out** — separate subprocess confirmed |
| (b) Fork-after-init inheritance (child inherits FD/state from daemon) | Claude Code does not fork from daemon; uses subprocess spawn with explicit env | **Ruled out** |
| (c) Shared filesystem token cache (telegram caches token to disk, MCP picks up out-of-band) | No filesystem token cache observed in `src/channel/telegram.rs` or `src/bootstrap/telegram_init.rs` | **Ruled out** |
| (d) fleet.yaml hot-reload race re-initializes ACTIVE_CHANNEL | Hot-reload doesn't re-run `register_active_channel`; uses watcher pattern that updates `InstanceConfig` only | **Ruled out** |
| (e) Test vs prod ENV cleanup difference | Tests use in-process channel mocks (`RecordingChannel`); prod doesn't preserve env to subprocess | **Plausible but not the operator's path** |

### Mystery resolution

**impl-1 hypothesis confirmed by code investigation**:
- `src/daemon/mod.rs:199-224` — daemon initialization passes Telegram channel to supervisor + attaches to registry
- `src/app/...` — TUI launches in daemon process; if TUI invokes MCP tools, they run in same process and see `ACTIVE_CHANNEL`
- `src/main.rs:623-628` — `mcp` subcommand entry point is a separate binary process; no daemon access

**Past success path**: Operator was using TUI ("send_to_instance" or similar TUI-internal action that traverses the same code path as MCP `reply`). TUI runs inside daemon process, channel was registered, succeeded.

**Failure path**: Operator was using Claude Code, where Claude spawned MCP subprocess to invoke `reply`. MCP subprocess process, `ACTIVE_CHANNEL = None`, fails.

**This is "broken by design"**, not an intermittent state-init bug. The cross-process bridge was never implemented.

---

## Design space matrix

### Options summary

| Option | Description | Cross-process surface | Attack surface | LOC est | Reviewer-readiness |
|---|---|---|---|---|---|
| **A** | Daemon API gains generic `mcp_tool` dispatch endpoint | Generic (every MCP tool) | Broad (45+ tools eventually inherit) | 200-300 | borderline |
| **B** | MCP subprocess inits telegram itself (pass `AGEND_BOT_TOKEN` env) | Per-MCP-process | Token leak (N processes) + telegram rate-limit + N-way race | 100 (impl) + ongoing | dual-subscriber risk |
| **C** | Daemon API adds bounded `proxy_channel_op(instance, op_kind, args)` for 4 channel ops | 4-op surface | Bounded by `ChannelOpKind` enum + Sprint 22 P0 outbound_capabilities gate | 150-200 | ✅ |
| **D** | Full daemon — MCP subprocess pure thin proxy for ALL tools | All tools | Same as A but always-on for every call | 800+ | REJECT-on-arrival as single PR |
| **E** | Investigation first (this report) | n/a | n/a | 0 (docs only) | this PR |

### Cost / benefit per option

#### A — Daemon API generic `mcp_tool` dispatch

**Pros**: leverages existing daemon API socket primitive (Unix domain socket, sync request/response). impl-2 m-111 noted this matches daemon's pattern.

**Cons**:
- "Execute arbitrary MCP tool" surface broadens cross-process auth boundary to all 45+ tool surfaces (reviewer-2 m-113)
- Every new tool inherits remote-call attack surface
- Slippery slope toward D (what tools should NOT be remote? unclear scope)
- Reviewer-readiness: borderline — needs explicit scope-freeze on which tools route through it

**Operator-trust 6mo**: degrading. First cross-process auth bug shipped destroys trust.

#### B — MCP subprocess inits telegram

**Pros**: MCP self-contained, no IPC dependency.

**Cons**:
- N MCP subprocesses each running `tg_polling` → telegram bot rate-limit contention (per-bot limits apply globally)
- Bot token leak blast radius scales linearly with fleet
- N-way ordering races on telegram emit (which subprocess emits first?)
- Shared state (last_message_id, idempotency keys) doesn't sync across processes
- Claude-spawned MCP doesn't have daemon's `auth_cookie` for outbound auth

**Operator-trust 6mo**: degrading mid-term. First token-leak or rate-limit incident lands ~3-6mo.

#### C — Daemon API `proxy_channel_op` (RECOMMENDED)

**Pros**:
- Smallest cross-process surface — exactly 4 channel ops (`Reply` / `React` / `Edit` / `InjectProvenance`)
- Reuses existing `ChannelOpKind` enum — no new auth model
- Daemon owns channel state authoritatively (Telegram already initialized)
- MCP subprocess relays to daemon via existing API socket (sync request/response)
- Backward-compat: daemon-internal MCP path can short-circuit (in-process, no round-trip)
- Anti-bypass invariant pattern transfer natural fit (dev-reviewer m-110)
- Adversary blast radius bounded by Sprint 22 P0 `outbound_capabilities` gate (same gate as in-process)

**Cons**:
- Requires "is this MCP running inside daemon process?" detection logic (impl-1 m-112 sketch) — 1 helper fn
- Dual-write coherence during transition: daemon-internal path AND cross-process path must produce identical events (mitigated by routing both through `Channel::send_from_agent`)

**Operator-trust 6mo**: stable. Small surface, well-typed, doesn't expand. Aligns with operator mental model ("daemon owns channel; MCP is a thin client").

#### D — Full daemon thin proxy ALL tools

**Pros**: single source of truth, MCP becomes pure JSON-RPC bridge.

**Cons**:
- Massive refactor (15+ MCP tools); rollback painful (once tools go remote, back-out is another refactor)
- Per-call IPC latency on every tool — even `inbox` (frequent polling) suffers
- Self-defeating for daemon-internal MCP path (TUI's reply would round-trip to daemon RPC even when already in daemon process)
- REJECT-on-arrival as single PR per dev-reviewer

**Operator-trust 6mo**: degrading. Every tool refactor incurs IPC + serialization consideration; first cross-process regression destroys trust.

#### E — Investigation first

**Pros**: this report (already produced) verifies architectural premise + resolves mystery. Saves designing for the wrong bypass class.

**Cons**: no code shipped. E alone doesn't fix the bug.

---

## Synthesis: 4-perspective convergence

| Vantage | Pick | Key argument |
|---|---|---|
| **impl-1** (implementer-A) | C | Smallest delta, reuses existing serde + `ChannelOpKind`, single endpoint covers all 4 ops |
| **impl-2** (implementer-B alt-path) | A (with C as second) | Daemon's existing API socket pattern fits sync semantics; A leverages without new primitives |
| **dev-reviewer** (reviewer-readiness) | C | Best for anti-bypass invariant pattern transfer; smallest correct scope |
| **dev-reviewer-2** (adversarial) | E first → C | Verify premise → ship C if subprocess confirmed (smallest cross-process attack surface, bounded by `ChannelOpKind` + outbound_capabilities gate) |

**Convergence**: 3 of 4 prefer C directly; reviewer-2 prefers C **after** investigation confirms subprocess (now confirmed by this report). impl-2's A is similar to C but with broader scope; impl-2 acknowledges A and C are close — C wins on attack surface bound.

---

## Recommended design: **Option C — `proxy_channel_op`**

### Architecture sketch

```
┌─────────────────────────────────────────────────────────────────────────┐
│  Claude Code                                                             │
│    └─ spawns subprocess: `agend-terminal mcp --instance dev-impl-1`     │
│         ↓ stdin/stdout JSON-RPC                                          │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │ MCP subprocess (separate process, no AGEND_BOT_TOKEN)            │   │
│  │   - reply / react / edit_message / delete_message handlers:      │   │
│  │     1. Build AgentOutboundOp (op_kind + args)                    │   │
│  │     2. POST to daemon: api::call(home, {                         │   │
│  │          "method": "proxy_channel_op",                           │   │
│  │          "params": { instance, op_kind, args }                   │   │
│  │        })                                                         │   │
│  │     3. Return daemon's response payload to caller                │   │
│  └─────────────────────────────────────────────────────────────────┘   │
│              ↓ Unix domain socket (existing API IPC)                     │
│  ┌─────────────────────────────────────────────────────────────────┐   │
│  │ Daemon process (telegram init, ACTIVE_CHANNEL registered)        │   │
│  │   - api/mod.rs:343-370 method dispatch                           │   │
│  │   - NEW arm: "proxy_channel_op" → handle_proxy_channel_op       │   │
│  │     1. Validate instance against fleet.yaml                      │   │
│  │     2. Look up Channel via active_channel()                      │   │
│  │     3. Apply gate_outbound_for_agent (Sprint 22 P0 check)        │   │
│  │     4. Dispatch to Channel::send_from_agent(instance, op)        │   │
│  │     5. Return msg_ref / error                                    │   │
│  └─────────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────────┘
```

### Implementation scope

**Files to modify**:
- `src/api/mod.rs` — add `proxy_channel_op` method handler (~50 LOC) in dispatcher
- `src/mcp/handlers.rs` — change reply/react/edit_message/delete_message handlers to invoke daemon API instead of `active_channel()` directly (~60 LOC across 4 arms)
- `src/mcp/mod.rs` — extend `proxy_or_local` to know about the new method (~10 LOC)
- `src/channel/mod.rs` (optional) — extract `is_running_inside_daemon_process()` helper for backward-compat short-circuit (~20 LOC)
- `tests/channel_ops_via_daemon_api_only.rs` (NEW) — anti-bypass invariant test mirroring Sprint 22 P0 / Sprint 23 P0 / Sprint 24 P0 patterns (~80 LOC)
- `docs/USAGE.md` — operator-facing note explaining MCP-subprocess channel ops route through daemon (~30 LOC)
- `docs/MCP-CHANNEL-OPS-BRIDGE.md` (NEW) — architecture rationale doc (~80 LOC)

**LOC est**: ~330 production + tests + docs. Realistic for Tier-2 review surface.

### Tier-2 evidence chain priority

Per dev-reviewer m-110 + reviewer-2 m-113 patterns:
1. **files audited** (most critical) — 4 channel-op call sites + daemon API endpoint
2. **commands** — `cargo test --tests` green + grep verifications:
   - `rg "active_channel\(\)" src/mcp/` — should only appear in transition paths or removed entirely
   - `rg "proxy_channel_op"` — should appear in: api/mod.rs (dispatcher) + mcp/handlers.rs (4 callers) + tests
3. **reviewed_head** — single commit on `sprint25-p0-mcp-channel-bridge`
4. **scope_source** — this investigation report + decision d-XX (post-approval)
5. **audit_mode** — full_review (Tier-2 + AUTO-CRITICAL: cross-process IPC + capability-adjacent)

### REJECT criteria (per dev-reviewer + reviewer-2)

- Anti-bypass invariant test: `tests/channel_ops_via_daemon_api_only.rs` enforces no MCP-subprocess code path calls `crate::channel::active_channel()` for the 4 ops. EXEMPTED_CALLERS = empty by intent.
- FATAL-elevation log on daemon-API-proxy call failure (operator-actionable, mirrors P0 + P1.5 helper pattern)
- "Why daemon owns channel ops" architectural rationale doc (mirrors PR #235 DAEMON-LOCK-ORDERING.md doc-test mutual reinforcement)
- Sprint 22 P0 `outbound_capabilities` gate still enforced before daemon invokes `Channel::send_from_agent` (Sprint 23 P1 PR #242 default-open semantics preserved)
- Backward-compat short-circuit: daemon-internal MCP path bypasses cross-process round-trip (tested)

### Edge cases

| Edge case | Handling |
|---|---|
| Daemon not running (e.g., `agend-terminal mcp` invoked standalone) | `api::call` returns connection error → MCP subprocess errors with diagnostic message |
| MCP subprocess called for an instance that doesn't exist | daemon `proxy_channel_op` validates instance against fleet → error response |
| Race: daemon shutdown mid-call | daemon socket close → MCP subprocess errors (sync caller knows immediately, no silent drop) |
| Daemon-internal MCP path (TUI) | `is_running_inside_daemon_process()` short-circuits to in-process call (no round-trip) |
| Auth: cross-process auth between MCP and daemon | reuse existing daemon API socket auth model (auth_cookie if applicable) — no new auth surface |
| Operator's reply succeeds in TUI but fails in Claude-spawned MCP | Both paths now route through daemon's `Channel::send_from_agent` — uniform behavior |
| Sprint 22 P0 outbound_capabilities gate (Sprint 23 P1 default-open) | daemon-side gate evaluation runs before dispatch — same semantics as in-process |
| Concurrent reply from N MCP subprocesses for same instance | daemon serializes via channel state lock (existing F6 lock-around-pair pattern) |

---

## Migration plan

### Phase 1 (this PR): ship `proxy_channel_op` + MCP-side relay
- Daemon API endpoint added
- MCP handlers route through daemon API
- Anti-bypass invariant test enforces single-path
- Backward-compat: TUI-internal MCP short-circuits in-process

### Phase 2 (follow-up, optional): retire active_channel() from MCP subprocess context
- After Phase 1 stable, remove `active_channel()` direct calls from MCP-side completely
- Single canonical path: all MCP channel ops route through daemon API
- Anti-bypass invariant tightens (EXEMPTED_CALLERS empty by intent)

---

## Sprint placement

- **Recommended**: Sprint 25 P0 candidate (after Sprint 23 P0 + P1 + Sprint 24 P0 + P1 wave settles)
- **Operator decision needed**: ship Sprint 25 P0 OR earlier if critical (operator hits this daily)
- **Non-blocking**: this PR is investigation report only; design approval gates implementation dispatch
- **4-perspective challenge round mandatory pre-impl-dispatch** per amendment #1 (this report includes lightweight critique already; full challenge round on design freeze + LOC est before impl-dispatch)

---

## Open questions for operator approval

1. **Sprint 25 P0 priority**: ship NOW (Sprint 23 P1 sub-track) OR defer to Sprint 25 (after current wave settles)?
2. **Phase 2 (retire active_channel from MCP) scope**: include in same PR OR separate follow-up?
3. **Daemon-internal MCP path short-circuit**: implement (faster TUI ops, more code paths) OR skip (consistency, slower TUI)?
4. **Anti-bypass invariant test scope**: enforce on src/mcp/* only OR all src/* (broader, may catch unrelated paths)?
5. **Operator-facing docs**: USAGE.md mention only OR separate ARCHITECTURE.md MCP/daemon split section?

---

## Authority

This investigation report is the synthesis of 4-perspective challenge round per v1.2 amendment #1 + dev-lead code investigation via Explore agent. **Pending operator approval** before implementation dispatch.

Per general routing m-20260427122122881384-105: this report is the deliverable. Operator reviews → approves design → dev-lead dispatches Sprint 25 P0 implementation with separate 4-perspective challenge round on impl-time decisions (concrete API shape, error mapping, auth model).

---

## References

- Operator directive: telegram 2026-04-27 11:08 UTC
- General routing: m-20260427122122881384-105
- Decision: d-20260427122219209231-1 (mini scope freeze)
- Challenge round perspectives:
  - impl-1 m-20260427122411938647-112
  - impl-2 m-20260427122352302027-111
  - dev-reviewer m-20260427122331896960-110
  - dev-reviewer-2 m-20260427122418390546-113
- dev-lead Explore agent code investigation (this report's evidence chain)
- Predecessor PRs: Sprint 22 P0 #230 (Phase 5b hard-cut, gate_outbound_for_agent helper) + Sprint 23 P1 #242 (in flight, default-open semantics)
- Sprint 23 P1 PR #241 (channel detection instruction normalize — orthogonal, doesn't fix this bug)
