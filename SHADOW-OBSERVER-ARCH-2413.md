# Shadow Observer — Architecture (#2413 Phase B)

Branch `feat/2413-shadow-local-plane-hooks`. Task `t-…24134-3`. **HARD-STOP at reducer + quantification report.** flag-OFF (`AGEND_SHADOW_OBSERVER=1`), zero in-path, DUAL review. Do NOT touch in-path proxy productization (wait for the numbers).

Operator chose **A**: build the reducer + *quantify* "cleaner than raw screen-scrape", THEN decide whether to invest in an in-path proxy. This doc is the design the reducer is built against.

---

## §1 Data flow (zero in-path)

Three Evidence sources fold into one reducer; nothing here sits in the agent's request path.

```
 claude lifecycle hooks ──unix socket──▶ shadow::ingest_frame ──▶ per-agent Evidence buffer
   (real, authority=Hook, confidence=Confirmed)                     (shadow/mod.rs: push/drain/peek)
                                                                            │
 vterm screen ──StateTracker.feed_with_fg──▶ agent_state (AgentState) ─────┤  reducer WRAPS as
   (the baseline to BEAT; src/state/mod.rs)   read via core.state.get_state()│  authority=Screen Evidence
                                                                            │
 lsof probe ──▶ core.api_activity.in_flight / last_active_epoch_ms ─────────┤  reducer WRAPS as
   (Phase-1, out-of-path, 5s; api_activity_probe.rs)                        │  authority=ProcessHeuristic Evidence
                                                                            ▼
                                          ┌─────────────────────────────────────────┐
                                          │  REDUCER (src/daemon/shadow/reducer.rs)   │
                                          │  per-agent, per-tick:                     │
                                          │   1. drain Evidence buffer (hook events)  │
                                          │   2. synth Screen Evidence (agent_state)  │
                                          │   3. synth Liveness Evidence (api/output) │
                                          │   4. fold → AgentRuntime (episode + decay)│
                                          │   5. precedence → state                   │
                                          │   6. liveness backstop reconcile          │
                                          └─────────────────────────────────────────┘
                                                                            ▼
              ObservedStatus { state, confidence, authority, evidence[], since }
                                                                            │
                additive (mirror api_in_flight): hung on AgentCore BESIDE agent_state,
                serialized in list_instances (query.rs) under the SAME core.lock().
                NEVER overwrites agent_state. raw-read deciders untouched.
```

**Why wrap screen as Evidence** (not just read it): the reducer's whole value is *fusing* sources under one precedence+decay model. Screen is the baseline we must measure against, so it enters the same pipeline tagged `authority=Screen` and is *overridden* by fresher `Hook` evidence (and reconciled back when hooks drop). The quantification (§4) compares reducer output vs the raw `agent_state` it wrapped.

**Coexistence with existing planes** (do NOT duplicate):
- `hook_shadow.rs` already maps hook→`AgentState` + freshness + Phase-1 `authoritative_state()` promotion (snapshot-scoped, gated `AGEND_HOOK_STATE_POC=1`). That stays. My Evidence plane is the *richer typed* representation; the reducer is its Phase-B consumer. ObservedStatus is a NEW additive field, not a re-write of `agent_state`.
- `recovery_shadow.rs` = 529-recovery telemetry; unrelated, reuse only its fire-once-latch dedup *pattern*.

---

## §2 ObservedStatus shape

```rust
pub struct ObservedStatus {
    pub state: ObservedState,        // the MVP/refined state (below)
    pub confidence: Confidence,      // reuse evidence::Confidence
    pub authority: Authority,        // reuse evidence::Authority — which source decided
    pub evidence: Vec<EvidenceRef>,  // the (kind, authority, at_ms) that justified it — bounded (last N)
    pub since_ms: u64,               // epoch ms the state was first entered (stable across re-derive)
}
```
`since_ms` is **stable**: only reset when `state` actually changes, so "how long Active/Waiting" is meaningful. `evidence[]` is a bounded explain-trail (last ~4) for debuggability + the quantification diff — not the whole buffer.

Additive surface: `AgentCore.observed_status: Option<ObservedStatus>` (None until first reduce), serialized in `list_instances` as `"observed_status": {...}` beside `"agent_state"`. Read under `core.lock()` atomically with `agent_state` (so a consumer can diff them — that diff IS the quantification).

---

## §3 State model + MVP

**MVP (3 high-reliability states)** — ship these first; refine only when evidence is sufficient, else conservatively fall back UP to the coarser state:

| ObservedState   | Meaning                              | Primary evidence |
|-----------------|--------------------------------------|------------------|
| `WaitingForUser`| blocked awaiting a human decision    | Hook `ApprovalRequired`; Screen `PermissionPrompt`/`InteractivePrompt`/`AwaitingOperator` |
| `Active`        | turn in progress / working           | Hook `TurnStarted`(open episode) / `ToolStarted`(open span); `api_in_flight`; Screen `Thinking`/`ToolUse` |
| `Idle`          | at a ready prompt, nothing running   | Hook `TurnEnded`/`PromptReady`/`SessionExited`; Screen `Idle`; AND no `api_in_flight` |

**Refined Active sub-states** (only when evidence is unambiguous; otherwise stay `Active`):
- `ToolUse` — `ToolStarted` open (no matching `ToolEnded`).
- `Responding` — assistant output streaming (Stream plane — DEFERRED this phase; weakly inferable from `api_in_flight` + fresh output, default-fold into `Active`).
- `Thinking` — **derived**, not string-matched: episode open ∧ no open tool span ∧ not WaitingForUser ∧ no recent output. (matches hook_shadow's `UserPromptSubmit→Thinking` intuition but as a *derivation*.)

**Error/limit states** (carry through from Screen authority this phase; Hook plane is blind to them, API plane DEFERRED):
- `RateLimited` — Screen `RateLimit`/`ServerRateLimit`/`UsageLimit`.

**Precedence** (highest wins when multiple fire): `RateLimited > WaitingForUser > ToolUse > Responding > Thinking > Idle`. (`Active` is the coarse fold of ToolUse/Responding/Thinking for MVP.)

---

## §4 Decay / reconcile / liveness backstop  ← hardest, most-tested

The episode model + the phantom-stuck failure it must survive.

**Episode tracking (AgentRuntime, per agent):**
- `TurnStarted` opens an *episode* (`episode_open=true`, `episode_since`). `TurnEnded`/`SessionExited`/`PromptReady` closes it.
- `ToolStarted{name}` opens a *tool span*. `ToolEnded` closes it.
- `ApprovalRequired` sets `waiting=true`; cleared by the next `ToolEnded`/`TurnEnded`/`PromptReady`/`TurnStarted`.

**The failure to defeat:** a closing hook (`Stop`/`PostToolUse`) is DROPPED → episode/span never closes → reducer reports `Active`/`ToolUse` forever though the agent is idle. (Hook delivery is best-effort: async, 10s timeout, socket can drop.)

**Two decay mechanisms:**
1. **TTL decay** — each Evidence carries `ttl_ms`; hook evidence ships `ttl_ms=0` (no local opinion), so the reducer assigns a *default budget* per kind (e.g. an open tool span older than `TOOL_SPAN_MAX` with no closing event is suspect). Time alone never flips state — it only marks an open span *stale* and eligible for reconcile.
2. **Liveness backstop reconcile** (the real fix) — an open episode/span is force-closed → `Idle` (authority downgraded to `Inferred`/`Screen`, confidence `Probable`) when liveness CONTRADICTS it, ALL of:
   - Screen `agent_state == Idle` (prompt-ready) sustained > `RECONCILE_GRACE`, **and**
   - `api_activity.in_flight == false` (no live LLM socket), **and**
   - `last_productive_output` silence > `RECONCILE_SILENCE`.
   → "hook said ToolUse, but screen is back at the prompt + no LLM socket + quiet → the Stop hook was dropped; decay to Idle."

**Symmetric guard (false-active from stale screen):** if Screen says `Idle` but a FRESH hook episode is open AND `api_in_flight==true` → keep `Active` (this is the **mid-API false-idle** the reducer must beat — screen looks idle mid-request, hook+socket prove otherwise).

**Freshness fallback:** no hook Evidence within `HOOK_FRESHNESS` (≈600s, match hook_shadow) AND no live signal → fall back to Screen authority (confidence `Weak`).

**Tests (decay/reconcile is the must-test surface):**
- `dropped_stop_reconciles_to_idle` — open episode + screen Idle + !in_flight + silence → Idle.
- `dropped_posttool_reconciles_tool_span` — open tool span, same contradiction → span closed.
- `mid_api_false_idle_stays_active` — screen Idle + fresh episode + in_flight → Active (beats screen).
- `approval_then_proceed_clears_waiting` — ApprovalRequired then ToolEnded → not stuck WaitingForUser.
- `precedence_orders` — RateLimited > WaitingForUser > ToolUse > … table-driven.
- `ttl_open_span_without_liveness_does_not_flip` — time alone (no contradicting liveness) does NOT flip (avoid over-eager decay).
- `since_ms_stable_across_redrive` — re-reducing identical evidence keeps `since_ms`.

---

## §5 Quantification (operator's core deliverable)

Isolated real claude (mirror the live-verify recipe — **SHORT AGEND_HOME, fleet.yaml, NOT --agents, do NOT touch the live fleet**; see [[reference_shadow_local_plane_live_verify_recipe]]). Drive real turns that screen-scrape gets WRONG, and count reducer corrections vs raw `agent_state`:
1. **mid-API false-idle** — during a long tool/think, screen momentarily renders idle/prompt-ready; raw `agent_state=Idle` while hook episode open + `api_in_flight=true`. Reducer should say `Active`. Count: false-idles caught.
2. **waiting-vs-idle** — at a permission prompt, raw screen may read `Idle`/ambiguous; hook `ApprovalRequired` → reducer `WaitingForUser`. Count: approvals correctly distinguished from idle.
3. (control) steady idle / steady tool-use — reducer must NOT regress these (no new false-actives).

Report: a table of `raw agent_state` vs `ObservedStatus.state` per sampled tick across the scripted turns, with the correction counts (`+N false-idle caught`, `+M approval-vs-idle split`, `0 regressions`). This is the number that justifies (or not) the in-path proxy phase.

---

## §6 Build order

1. `reducer.rs` core: `AgentRuntime` + `reduce(evidence: &[Evidence], screen: AgentState, live: LivenessSnapshot) -> ObservedStatus`. PURE fn over inputs (no globals) → trivially testable; the per-tick driver calls it.
2. Decay/reconcile + the §4 test suite (RED first on the reconcile cases).
3. Additive surface: `AgentCore.observed_status` + per-tick reduce call + `query.rs` serialization (mirror `api_in_flight`).
4. Screen→Evidence + liveness→Evidence wrappers.
5. Quantification harness + report (§5).
6. fmt + clippy + `AGEND_GIT_BYPASS=1` nextest; DUAL review.

Invariants: flag-OFF default (reduce only runs / surfaces under `AGEND_SHADOW_OBSERVER=1`); reducer reads ONLY hook-buffer + screen + lsof (zero in-path); `agent_state` never mutated; spawn-site rationale on any thread.

---

## §7 Implementation map (file:line — so a fresh restart skips re-exploring)

- **Screen baseline**: `src/state/mod.rs` — `enum AgentState` @ L24-83 (Idle/ToolUse/Thinking/PermissionPrompt/InteractivePrompt/AwaitingOperator/RateLimit/ServerRateLimit/UsageLimit/…). Derive `StateTracker::feed_with_fg()` @ L1549; read `core.state.get_state()`; `last_output` @ L237, `last_productive_output` @ L251, `productive_silence()`; `SERVER_RATE_LIMIT_RECOVERY_SILENCE=45s` @ L461.
- **hook_shadow.rs** (coexist, DON'T duplicate): `derive_state()` @ L277-299, `resolved_state_for()` @ L129 (`HookResolution::Fresh(600s)/Stale/Unknown`, ToolUse event-pair-closed @ L149), `authoritative_state()` @ L226 (Phase-1 promotion, gated `AGEND_HOOK_STATE_POC=1`, snapshot-scoped), `HookShadow` @ L31. The reducer does NOT route through `authoritative_state()`.
- **Additive surface to MIRROR**: `ApiActivity{in_flight,last_active_epoch_ms}` on AgentCore (`src/agent/mod.rs` field `api_activity`); serialized **`src/api/handlers/query.rs:30,49-67`** under `core.lock()` atomically with agent_state → `"api_in_flight"`/`"last_api_activity_at"`. Probe `src/api_activity_probe.rs:76` (`probe_once()` @ L113).
- **Liveness signals**: `core.api_activity.in_flight`; `core.state.last_productive_output` / `last_output` / `productive_silence()`; `child.lock().process_id()` (`src/instance_monitor.rs:132`); heartbeat (`src/daemon/per_tick/hang_detection.rs:73-80`).
- **Consumed (my Phase-A modules)**: `shadow::{drain,peek}(agent)->Vec<Evidence>`; `Evidence{kind,authority,confidence,at_ms,ttl_ms}`; `EvidenceKind`=TurnStarted/Responding/TurnEnded{stop_reason}/ToolStarted{name}/ToolEnded/ApprovalRequired/RateLimited{retry_at_ms}/TokenUsage/PromptReady/SessionExited; `Authority`=Hook/Stream/Transcript/Screen/ProcessHeuristic/Inferred; `Confidence`=Confirmed/Strong/Probable/Weak.
- **Per-tick driver** under the flag: see `src/daemon/per_tick/`; snapshot buffer+screen+api_activity under one `core.lock()`, write `core.observed_status`.
- **Quantification harness**: reuse the live-verify recipe — SHORT `AGEND_HOME` (`/tmp/svN`, SUN_LEN), `fleet.yaml` (NOT `--agents`, which has no working_dir → `claude --continue` crash), non-bypass claude via temp-removing `--dangerously-skip-permissions` from `src/backend.rs:408` (revert `git restore src/backend.rs`; agend-git denies `git checkout <file>`). macOS has no `timeout`. Read `<home>/daemon.<date>.log` for `#shadow-observer`.
- **Preflight**: `AGEND_GIT_BYPASS=1` + `PATH=/usr/bin` nextest; fmt --check; clippy -D warnings (tray feature); new real-git test files join nextest git-subprocess group (#821); file-size invariant.
