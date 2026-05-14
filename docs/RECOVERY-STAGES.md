# Recovery Stages

Source-of-truth for the `#685` Phase 2 staged auto-recovery dispatcher.
This PR (`#685` sub-task 7a) ships **Stage 1 ESC interrupt** with full
infrastructure (state machine, env-var gate, anti-thrash cooldown,
telemetry pattern) reusable by future Stages 2 (auto-restart) and 3
(pause + escalate).

Decision: `d-20260514030404021793-1` (three-party consensus: lead-claude
+ dev-claude + reviewer-opencode).

Sibling chain: sub-task 1 (PR #750) + 2 (#752) + 3 (#763) + 4 (#766) +
5 (#769) + 6 (#770). Stages 2 (7b) and 3 (7c) are follow-up sub-tasks
of `#685` that add their dispatch arms but reuse this module's
infrastructure.

Maintenance: section IDs (`§RS.1`-`§RS.8`) are stable contract anchors
per the M1/M2/M3 discipline established in sub-task 1.

## §RS.1 — Why staged auto-recovery

Before this sub-task: when `check_hang` (sub-task 1) detects `Hung`,
the daemon emits a single `tracing::warn!("hang detected")` and does
nothing. Operators must manually press ESC in the agent's TUI pane to
recover. Issue `#685` Phase 2 mandates staged automation:

- **Stage 1**: daemon writes ESC byte to PTY (simulate operator ESC)
- **Stage 2**: Stage 1 fails to recover → auto-restart agent + telegram
  warn operator
- **Stage 3**: Stage 2 fails N times → pause + telegram escalate +
  flag for manual investigation

Each stage gated behind an env var with operator default `warn-only`.

## §RS.2 — Lifecycle (state machine)

```rust
pub enum RecoveryStageState {
    None,
    Stage1Pending { entered_at: Instant },
    Stage2Eligible,
    Stage2Pending { entered_at: Instant },
    Stage3Eligible,
    Stage3Pending,
}
```

Carried inside `HealthTracker` so the dispatcher reads both
`HealthState` and stage progression under one per-agent lock.

```
                      ┌──────┐
                      │ None │◄────── spontaneous recovery
                      └──┬───┘        (HealthState::Healthy)
                         │
        HealthState::Hung + alive-stuck branch
                         ▼
              ┌─────────────────┐
              │ Stage1Pending   │── Stage 1 timeout / dead-likely / cooldown
              └────────┬────────┘
                       ▼
              ┌─────────────────┐
              │ Stage2Eligible  │── (7b) Stage 2 fires
              └────────┬────────┘
                       ▼
              ┌─────────────────┐
              │ Stage2Pending   │── (7b) timeout
              └────────┬────────┘
                       ▼
              ┌─────────────────┐
              │ Stage3Eligible  │── (7c) Stage 3 fires
              └────────┬────────┘
                       ▼
              ┌─────────────────┐    HealthState transitions to Paused;
              │ Stage3Pending   │    operator unpause action only.
              └─────────────────┘
```

Phase 1 (this PR) implements:
- `None → Stage1Pending` (alive-stuck branch)
- `None → Stage2Eligible` (dead-likely branch OR cooldown skip)
- `Stage1Pending → Stage2Eligible` (Stage 1 timeout expired)
- `* → None` (spontaneous recovery on `Healthy`)

Phase 2 follow-ups (7b/7c) add Stage 2/3 dispatch arms.

## §RS.3 — Tick order & dispatcher placement

The dispatcher runs as the **second** entry in
`src/daemon/mod.rs:537-546` `handlers: Vec<Box<dyn PerTickHandler>>`,
immediately after `HangDetectionHandler`. Sequencing matters:

1. `HangDetectionHandler` runs `check_hang` → possibly transitions
   `core.health.state` to `Hung` (sub-task 1 §Invariants 5b — sole
   mutator).
2. `RecoveryDispatcherHandler` runs immediately after, reading the
   fresh `core.health.state` value. Subsequent ticks while still
   `Hung` use the same read — dispatcher does NOT subscribe to
   `check_hang`'s `bool` return (which only fires on the transition
   edge per sub-task 1 audit).

Location: `src/daemon/per_tick/recovery_dispatcher.rs`. Modular
per-tick handler mirroring the sub-task 5 / #694 BLOCK 1 idiom.

## §RS.4 — Combined-gate three branches

Decision §1.4 Delta 2 — dispatcher inspects raw silence + productive
silence elapsed times directly (NOT via F9 classification flag) so
Stage 1 ships valuable independent of F9 promotion timeline:

| Branch | Condition | Action |
|---|---|---|
| **alive-stuck** | `productive_silence > threshold` && `silence < threshold` | Fire Stage 1 ESC (agent process reading PTY, just not productive). State → `Stage1Pending`. |
| **dead-likely** | `silence > threshold` | Skip Stage 1, ESC won't help a process not reading. State → `Stage2Eligible`. |
| **anomaly** | Neither condition holds | Log warning, leave state unchanged. Agent shouldn't be `Hung`. |

Thresholds match `silence_exceeds_threshold` in `check_hang`:
- `AgentState::Idle`: never (waiting for input)
- `AgentState::Starting`: 120s
- `AgentState::Thinking | ToolUse`: 600s
- Other states: 120s

Productive-silence threshold extracted via `health::productive_silence_exceeds`
helper (decision §1.4 Delta 2 Option a — DRY, single source of truth
shared with future Stages 2/3 and any other dispatcher consumers).

## §RS.5 — Shadow-mode default + env var gate

| Env var | Default | Purpose |
|---|---|---|
| `AGEND_AUTO_RECOVERY_STAGE1` | unset (shadow) | `"1"` activates: dispatcher writes ESC byte to PTY. Unset: same telemetry, no I/O. |
| `AGEND_AUTO_RECOVERY_STAGE1_TIMEOUT_MS` | 10000 | Window between Stage 1 fire and `Stage2Eligible` transition. |
| `AGEND_AUTO_RECOVERY_STAGE1_COOLDOWN_MS` | 60000 | Window during which a re-entry into `Hung` skips Stage 1 (anti-thrash). |

The dispatcher reads env vars **each tick** — operator can flip
`AGEND_AUTO_RECOVERY_STAGE1=1` without restarting the daemon. Important
for the shadow→active promotion workflow.

### Promotion criteria (operator action)

Mirrors F9 sub-task 4 §F9.5 SOP and sub-task 5 corpus-growth-delegate
methodology:

1. Operator runs daemon with `AGEND_AUTO_RECOVERY_STAGE1` unset
   (shadow) for at least 2 weeks across the agent fleet.
2. Operator reviews `recovery_shadow` tracing target output to
   classify each shadow fire:
   - **Would-have-helped**: agent was alive-stuck and subsequent recovery
     within timeout suggests ESC would have unblocked.
   - **Would-have-hurt**: agent was actively producing useful output
     that an ESC would have cancelled.
3. Once operator confidence is high (e.g. ≥95% would-have-helped on
   N ≥ 30 shadow fires), flip `AGEND_AUTO_RECOVERY_STAGE1=1` in
   production env.

Anti-dead-infra clause: if 6 weeks pass without measurement, Stage 1
becomes a candidate for removal. Mirror sub-task 4's "dead shadow infra
is worse than no infra" discipline.

## §RS.6 — Anti-thrash cooldown

Decision §1.4 Refinement B — if agent re-enters `Hung` within
`STAGE1_COOLDOWN_DEFAULT_MS` of a recent Stage 1 fire, dispatcher skips
Stage 1 and transitions directly to `Stage2Eligible`. Prevents
rapid-fire ESC sending that would mask underlying issues like infinite
loops or persistent backend bugs.

`last_stage1_fired_at: Option<Instant>` on `HealthTracker` stamps the
clock. Cleared only on spontaneous recovery (HealthState::Healthy) per
the linear-escalation discipline.

## §RS.7 — `HealthState::Paused` guards

Stage 3 (sub-task 7c) will transition `HealthState::Hung →
HealthState::Paused` when Stage 2 exhausts its retry budget. `Paused`
is an operator-action-required terminal state distinct from `Failed`
(crash counter exhausted):

| State | Trigger | Recovery |
|---|---|---|
| `Failed` | `record_crash` counter ≥ `max_retries` (5 process crashes within window) | Operator action OR `maybe_decay` slowly clears the counter |
| `Paused` | Stage 3 dispatcher | Operator unpause command (separate sub-task) |

Phase 1 implements the guards already (decision §5):

- `check_hang` short-circuits on `Paused` (returns `false` — no
  auto-recovery dispatcher work; operator already alerted via Stage 3
  telegram notify, further warns are noise).
- `maybe_decay` does NOT touch `Paused` (crash decay must not exit
  Paused; only operator unpause can).
- `display_name() -> "paused"` for telegram visibility + JSON API
  consumer (`api/handlers/query.rs`).

The variant itself ships in this PR but is constructed only by
sub-task 7c's Stage 3 dispatcher arm.

## §RS.8 — Cross-references & out-of-scope

### Cross-references

- `docs/HUNG-STATE-TRANSITIONS.md §F39.5` — open questions list now
  references this doc for staged-recovery details.
- `docs/F9-PRODUCTIVE-OUTPUT-GATE.md §F9.5` — recovery dispatcher
  treats all `Hung` sources same; F9 promotion does not require
  separate recovery wiring.
- `docs/F685-FIXTURE-CORPUS.md §F685-CORPUS.6` — recovery dispatcher
  shadow telemetry will inform future corpus growth (operator can
  capture PTY traces around Stage 1 shadow fires for fixture
  collection).
- `src/daemon/per_tick/recovery_dispatcher.rs` — module implementation.
- `src/health.rs::RecoveryStageState` — state machine variants.
- `src/health.rs::HealthState::Paused` — terminal state for Stage 3.
- `src/agent.rs::AgentExitEvent::Stage2Restart` — variant definition
  for sub-task 7b emission.

### Out of scope (this sub-task)

- Stage 2 auto-restart dispatcher arm — sub-task 7b.
- Stage 3 pause + escalate dispatcher arm — sub-task 7c.
- Operator unpause command (CLI or MCP tool) — separate sub-task,
  required before Stage 3 ships in production.
- Per-backend stage timing tuning — needs corpus measurement, follow-up
  similar to sub-task 6's per-backend marker calibration.
- Telegram notify for Stage 1 — decision §6 Refinement A: Stage 1
  silent on success (info-level log only). Stages 2/3 will fire
  telegram via existing `gated_notify` infrastructure.
- F39 mitigation selection / F9 promotion — fixture-corpus-N-gated.
- Multi-stage timeout per-backend overrides beyond uniform defaults +
  env-var overrides.
