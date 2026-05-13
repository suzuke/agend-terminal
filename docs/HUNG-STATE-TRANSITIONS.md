# Hung-State Transition Audit

Source-of-truth (primary) for `HealthState::Hung` and `HealthState::IdleLong`
transition semantics in `src/health.rs`. Companion to inline structured
comments at each mutation site and the `check_hang` function-level rustdoc.

Issue: [#685](https://github.com/suzuke/agend-terminal/issues/685) Phase 1
deliverable #1. Decision: `d-20260513154400110972-2`. Scope is strict — see
`§Scope` below.

Maintenance: section IDs (`§Entry.E1`, `§Exit.X1`, etc.) are **contract**
anchors — renaming any heading is a PR-scope-break that must propagate to
inline comments + decision references. Cross-references in this doc to
source use `rg <pattern>` grep hints rather than file-line refs, so refactor
that re-flows lines does not invalidate this doc; line refs in the prose
below are illustrative-only and reflect HEAD `2f24376`.

## Lifecycle overview

```
                      ┌──────────────┐
                      │   Healthy    │◄────────────────┐
                      └──────┬───────┘                 │
                             │ record_crash            │ §Exit.X1
                             ▼                         │ silence drops
                      ┌──────────────┐                 │
                      │  Recovering  │                 │
                      └──────┬───────┘                 │
                             │ recent ≥ 3              │
                             ▼                         │
                      ┌──────────────┐                 │
                      │   Unstable   │                 │
                      └──────┬───────┘                 │
                             │ total_crashes ≥ max     │
                             ▼                         │
                      ┌──────────────┐                 │
                      │    Failed    │                 │
                      └──────────────┘                 │
                                                       │
   ┌───────────────────────────────────────────────────┴──────────────┐
   │                                                                  │
   │  check_hang mutator monopoly (§Invariants 5b)                    │
   │                                                                  │
   │  silence > threshold ──┬── input pending past hb ──► §Entry.E1   │
   │                        │                                          │
   │                        ├── heartbeat fresh ──────► §Entry.E2     │
   │                        │                                          │
   │                        └── neither ──────────────► §IdleLong.E1  │
   │                                                                  │
   │  state ∈ {Hung, IdleLong}, silence drops below ─► §Exit.X1       │
   │                                                                  │
   └──────────────────────────────────────────────────────────────────┘

   ErrorLoop (separate state) — see §Open questions
```

## Scope

In-scope state mutations (audited below):

- `HealthState::Hung` Entry (E1, E2) and Exit (X1)
- `HealthState::IdleLong` Entry (E1) and Exit (X1, shared predicate with Hung)

Out-of-scope (explicit):

- `HealthState::Healthy / Recovering / Unstable / Failed / ErrorLoop` transitions —
  not driven by `check_hang` (see §Invariants 5b). Audited elsewhere.
- `AgentState` (in `src/state.rs`) — F39 evidence lives there but is referenced
  only via the §F39 cross-reference table below; not mutated by this scope.

## Invariants

These hold at HEAD `2f24376` and are forward-locked by decision
`d-20260513154400110972-2`:

- **5a (exhaustive entries)** — `HealthState::Hung` has exactly **two**
  entry sites; `rg "self\.state = HealthState::Hung" src/health.rs` returns
  exactly two matches (`§Entry.E1` and `§Entry.E2`, both inside
  `check_hang`). No third entry path exists.

- **5b (mutator monopoly)** — Every read/write of `HealthState::Hung` lives
  inside `check_hang`. `maybe_decay` (`rg "fn maybe_decay" src/health.rs`)
  mutates only `Failed → Recovering` and `Unstable → Healthy`; F10 verified.
  Implication: a reader auditing Hung semantics needs to read exactly one
  function.

- **5c (wire-compatible external surface)** — External consumers of the
  Hung state are the bool returned by `check_hang` (driven by
  `rg "check_hang" src/daemon/mod.rs`, sole consumer is a `tracing::warn!`)
  and the `display_name()` string serialized by `rg "health_state" src/api/handlers/query.rs`
  and `rg "health_state" src/mcp/handlers/instance.rs`. **No external code
  pattern-matches on the `HealthState::Hung` variant.** Implication:
  follow-up sub-tasks (F9 / F10 / F39) can change Hung internal semantics
  wire-compatibly as long as the `check_hang -> bool` and
  `display_name()` contracts hold.

- **5d (negative invariant — `maybe_decay` does not touch Hung)** — F10
  audit confirmed: `maybe_decay` reads `last_crash.elapsed()`, not
  `last_output.elapsed()`. Its state mutations are scoped to
  `Failed → Recovering` and `Unstable → Healthy`. **It will never exit
  Hung.** A Hung agent stays Hung until `check_hang` itself observes
  silence dropping below threshold (`§Exit.X1`). This negative invariant
  is duplicated in the `check_hang` function-level rustdoc augmentation
  for proximity to the audience that cares.

## Entry transitions

### §Entry.E1 — input pending past heartbeat

- **Find in source**: `rg "Hung Entry \(E1\)" src/health.rs`
- **PRE**:
  - `self.current_reason` is `None` or not in
    `{RateLimit, QuotaExceeded, AwaitingOperator}` (race mutex not held)
  - `silence_exceeds_threshold` is `true` (threshold varies by
    `AgentState`: 120s default; 600s for `Thinking | ToolUse`; never for
    `Idle`; 120s for `Starting`)
  - `input_pending_past_response` is `true`:
    `last_input_at_ms > last_heartbeat_at_ms + INPUT_RESPONSE_GRACE_MS`
    (grace = 5_000 ms)
  - `self.state != HealthState::Hung` (first detection latches the state
    flip; subsequent ticks short-circuit)
- **POST**:
  - `self.state = HealthState::Hung`
  - `check_hang` returns `true` (only on the first detection — caller
    escalates)
  - `tracing::warn!` with structured fields
    `last_input_at_ms / last_heartbeat_at_ms / input_response_delta_ms / silent_secs / agent_state`
- **FP vector** — Operator typed input that incremented `last_input_at_ms`
  but the agent is genuinely producing keystrokes that drain through MCP
  without flushing visible PTY output. Bounded by heartbeat semantics:
  any MCP tool call refreshes `last_heartbeat_at_ms` and pulls the
  delta back below the 5s grace.
- **FN vector** — F9 grey failure: an agent producing 1-byte output
  (spinner / log line / partial token) resets the upstream silence
  timer in `StateTracker`, so `silent` never crosses the threshold even
  if no useful work is happening. Productive-output detection is the
  F9 sub-task; this audit only records the gap.

### §Entry.E2 — heartbeat fresh but PTY silent (F1 cross-check)

- **Find in source**: `rg "Hung Entry \(E2\)" src/health.rs`
- **PRE**:
  - `self.current_reason` race mutex same as §Entry.E1
  - `silence_exceeds_threshold` is `true` (same thresholds as §Entry.E1)
  - `input_pending_past_response` is `false` (no input pending; §Entry.E1 did not fire)
  - `heartbeat_fresh` is `true`: `last_heartbeat_at_ms > 0` AND
    `heartbeat_age_ms < silent.as_millis()` — i.e. the agent has called
    MCP tools recently (refreshing heartbeat) while producing no PTY
    output
  - `self.state != HealthState::Hung`
- **POST**:
  - `self.state = HealthState::Hung`
  - `check_hang` returns `true`
  - `tracing::warn!` with structured fields
    `last_heartbeat_at_ms / heartbeat_age_ms / silent_ms / agent_state`
- **FP vector** — F39: stale `AgentState::Thinking` pattern in vterm
  scrollback (the regex match is against rendered screen text and can
  latch on text that scrolled off-screen). Bounded by
  `LATCHED_STATE_EXPIRY` (30s) in `src/state.rs` but not perfectly. See
  §F39 cross-reference.
- **FN vector** — F9 same as §Entry.E1; sub-threshold output keeps
  `silent` below trigger.

## Exit transitions

### §Exit.X1 — silence drops below threshold (recovery)

- **Find in source**: `rg "Hung Exit \(X1\)" src/health.rs`
- **PRE**:
  - `self.state in {HealthState::Hung, HealthState::IdleLong}` (shared
    predicate; one mutation site serves both states)
  - `!silence_exceeds_threshold` (any output, including a single byte,
    drops `silent` below the per-`AgentState` threshold)
- **POST**:
  - `self.state = HealthState::Healthy`
  - `check_hang` returns `false`
- **FP vector — F10 tangential concern** — There is no productive-work
  evidence requirement. **A single byte of PTY output flips Hung to
  Healthy**, even if it is a TTY spinner tick rather than progress. F10
  sub-task is a doc-only confirmation; F9 sub-task is the productive-
  output gate that would tighten this exit predicate.
- **FN vector** — None directly; this is the recovery path. Indirect:
  if §Exit.X1 fires spuriously (F10), the operator may dismiss a
  genuinely stuck agent on the basis of a stale "Healthy" classification.

## IdleLong transitions

`IdleLong` exists to distinguish "agent silent because no one is asking
it anything" from "agent silent because it stopped responding to input"
(Hung). The 04:00 UTC false-alarm pattern motivated the split.

### §IdleLong.Entry.E1 — silent past threshold, no input pending

- **Find in source**: `rg "IdleLong Entry \(E1\)" src/health.rs`
- **PRE**:
  - `self.current_reason` race mutex same as §Entry.E1
  - `silence_exceeds_threshold` is `true`
  - `input_pending_past_response` is `false` (no input pending past heartbeat)
  - `heartbeat_fresh` is `false` (heartbeat older than silent duration)
  - `self.state != HealthState::IdleLong`
- **POST**:
  - `self.state = HealthState::IdleLong`
  - `check_hang` returns `false` (escalation consumers act only on `Hung`
    per the rustdoc contract at `rg "Returns .true. ONLY when transitioning" src/health.rs`)
  - `tracing::debug!` (not `warn!` — non-escalation)
- **FP vector** — Genuinely idle agent waiting for the next operator
  prompt; classification is correct.
- **FN vector** — F9: same shape as §Entry.E1 / §Entry.E2.

### §IdleLong.Exit.X1 — shared with §Exit.X1

- **Find in source**: same `rg "Hung Exit \(X1\)" src/health.rs` (the
  `matches!(state, Hung | IdleLong)` predicate is one mutation site)
- **PRE**: same as §Exit.X1, but the `state` precondition is
  `HealthState::IdleLong`
- **POST**: same as §Exit.X1 (`state = HealthState::Healthy`,
  `check_hang` returns `false`)
- **FP / FN**: same as §Exit.X1

## F39 cross-reference

F39 evidence in `src/state.rs` shapes the false-positive vector of
§Entry.E2 (via `AgentState::Thinking` pattern stickiness). The
following references exist at HEAD `2f24376` and are surfaced here
rather than mirrored as inline comments in `src/state.rs` (per Trap-A
B-route in decision `d-20260513154400110972-2`):

| Reference | Find in source | Relevance |
|---|---|---|
| `LATCHED_STATE_EXPIRY` constant | `rg "LATCHED_STATE_EXPIRY" src/state.rs` | Bounds how long a stale Thinking pattern can suppress an agent flipping to other states. |
| Kiro Thinking pattern | `rg "Kiro is working" src/state.rs` | Regex anchor for kiro-cli's Thinking detection; can match stale scrollback. |
| Gemini Thinking pattern | `rg "esc to cancel" src/state.rs` | Regex anchor for gemini-cli's Thinking detection; same caveat. |

F39 sub-task (separate PR) will audit which patterns are stickier than
they should be and propose mitigations. This audit only records the
cross-reference.

## F9 / F10 follow-up scope cross-reference

| Finding | Affected transitions | Sub-task scope |
|---|---|---|
| F9 (productive-output gate) | §Entry.E1 FN, §Entry.E2 FN, §IdleLong.Entry.E1 FN, §Exit.X1 FP | New "productive output" signal (PR push, MCP tool success, structured log markers); changes silence-timer reset semantics in `StateTracker` and/or `check_hang` predicates. Separate PR. |
| F10 (doc-only confirmation) | §Exit.X1 FP | Confirm that `maybe_decay` truly does not affect Hung (this audit's §Invariants 5b/5d is the evidence) and that §Exit.X1 is the sole recovery path. Doc-only sub-task. |

## Open questions (for Phase 2 / future sub-tasks)

- **ErrorLoop entry without exit** — `rg "HealthState::ErrorLoop" src/health.rs`
  returns one entry site (in `record_error`) but no observed
  `HealthState::ErrorLoop → Healthy` exit transition. Separate audit
  warranted; out of scope for Hung audit.
- **Fixture corpus design** — Phase 1 deliverable #5 (replay captured
  stuck-thinking incidents from #659 and others) is a separate
  sub-task. Acceptance criterion: FP < 1% / FN < 10% per the issue.
- **Backend-specific tuning hooks** — Phase 1 deliverable #4
  (kiro/gemini may need different thresholds than claude); separate
  sub-task.
- **Stage-1 / Stage-2 / Stage-3 recovery design** — Phase 2 of #685,
  gated behind feature flags and operator default of "warn-only" per
  the issue.

## Consumer audit

Per §Invariants 5c, the entire programmatic surface for Hung detection
is:

- **`check_hang -> bool`** — sole consumer at `rg "check_hang" src/daemon/mod.rs`,
  a `tracing::warn!("hang detected")` with no automated recovery
  action. (#685's headline finding; Phase 2 builds recovery here.)
- **`health.state.display_name()` string** — serialized into JSON via
  `rg "health_state" src/api/handlers/query.rs` and consumed as opaque
  string in `rg "health_state" src/mcp/handlers/instance.rs`. No
  pattern match on the enum variant.

`grep -r "HealthState::Hung" src/ --include="*.rs"` outside `src/health.rs`
and test code returns zero hits at HEAD `2f24376`. Phase 2 recovery work
will add new consumers; this section is forward-locked as the
"pre-recovery" baseline.
