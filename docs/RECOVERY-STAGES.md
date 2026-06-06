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
              │ Stage3Eligible  │── (7c) Stage 3 fires:
              └────────┬────────┘    enter_paused(now) writes
                       ▼               HealthState::Paused atomically
              ┌─────────────────┐    Terminal. `Paused` short-circuits
              │ Stage3Pending   │    check_hang + maybe_decay; only an
              │   { entered_at }│    operator unpause (future sub-task)
              └─────────────────┘    leaves this state.
```

Sub-task 7a (Stage 1) implemented:
- `None → Stage1Pending` (alive-stuck branch)
- `None → Stage2Eligible` (dead-likely branch OR cooldown skip)
- `Stage1Pending → Stage2Eligible` (Stage 1 timeout expired)
- `* → None` (spontaneous recovery on `Healthy`)

**Sub-task 7b (Stage 2) implemented** (this PR):
- `None → Stage3Eligible` (cumulative restart cap reached — direct
  escalation, bypass Stages 1/2)
- `Stage2Eligible → Stage2Pending` (Stage 2 fire — emits
  `AgentExitEvent::Stage2Restart` via `try_send`; counter increment
  lives in respawn worker `selective-restore` arm)
- `Stage2Pending → Stage3Eligible` (Stage 2 timeout expired without
  recovery)
- `Stage2Eligible → Stage2Eligible` (channel-full retry — `try_send`
  failed, state stays for next-tick retry without counter increment)

**Sub-task 7c (Stage 3) implemented** (this PR):
- `Stage3Eligible → Stage3Pending { entered_at }` (Stage 3 fire — only
  via `HealthTracker::enter_paused` per §F39.5; atomically writes
  `state = Paused`, `recovery_stage_state = Stage3Pending`,
  `last_stage3_fired_at = Some(now)`)
- `Stage3Pending` is terminal — explicit no-op arm in dispatcher; only
  a future operator-unpause sub-task transitions out.

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

The dispatcher reads the gate env var **each tick** — operator can flip
`AGEND_AUTO_RECOVERY_STAGE1=1` without restarting the daemon. Important
for the shadow→active promotion workflow.

The Stage 1 timeout (10 s, `STAGE1_TIMEOUT_DEFAULT_MS`) and cooldown
(60 s, `STAGE1_COOLDOWN_DEFAULT_MS`) are **fixed consts, not
env-configurable** (#env-cleanup: the
`AGEND_AUTO_RECOVERY_STAGE1_TIMEOUT_MS` / `_COOLDOWN_MS` overrides were
demoted).

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

### Out of scope (sub-task 7a baseline)

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

## §RS.9 — Stage 2 specifics (sub-task 7b)

Sub-task 7b (decision `d-20260514034230950032-2`) implements Stage 2 on
top of the 7a infrastructure. Stage 2 is **controlled auto-restart**:
when an agent fails to recover from Stage 1 ESC (or is dead-likely from
the start), the dispatcher emits an `AgentExitEvent::Stage2Restart`
event to `crash_tx`; the respawn worker's Stage 2 arm in
`src/daemon/mod.rs::handle_stage2_restart` runs `spawn_agent` with
selective field preservation.

### 9.1 Cumulative restart cap

`HealthTracker.recovery_restart_count: u32` mirrors `total_crashes`
discipline. Each successful Stage 2 fire increments the counter (in the
respawn worker, NOT the dispatcher — avoids double-counting if the
channel send succeeds but the spawn fails). Default cap
`STAGE2_MAX_RESTARTS_DEFAULT = 3` (per decision §Q1/Q2 — issue body
"fails N times → Stage 3"). Operator override via env var
`AGEND_AUTO_RECOVERY_STAGE2_MAX_RESTARTS`.

When `recovery_restart_count >= cap`, the dispatcher's Stage 1 entry
arm short-circuits the cycle and escalates **directly** to
`Stage3Eligible` — operator intervention required rather than further
automated thrashing.

### 9.2 Selective field preservation across spawn

Decision §1 critical wrinkle (dev round 1): `spawn_agent` at
`rg "reg.insert" src/agent.rs` creates a **fresh `AgentCore` with
default `HealthTracker`**. Existing Crash path preserves all health
via `saved_health.clone()` at `daemon/mod.rs` Stage 2 needs different
semantics:

| Field | Stage 2 behaviour |
|---|---|
| `state` | Reset to fresh `Healthy` — recovery success seed |
| `recovery_stage_state` | Reset to fresh `None` — linear escalation reset |
| `last_stage1_fired_at` | Reset to fresh `None` (Stage 2 implies Stage 1 either fired or skipped, but next cycle starts clean) |
| `crash_times` | **PRESERVE** — don't lose crash history due to recovery restart |
| `total_crashes` | **PRESERVE** — same reason |
| `last_notification` | **PRESERVE** — notify cooldown discipline |
| `recovery_restart_count` | **PRESERVE + INCREMENT by 1** — counter must survive the restart it drove |
| `last_stage2_fired_at` | Set to `Some(now)` — drives decay clock |

`record_crash` is **NOT** called (Stage 2 ≠ crash). `respawn_ok` is
**NOT** called (state is already fresh `Healthy`).

### 9.3 1-second backoff

Decision §1.4 Delta 2: 1s default backoff before `spawn_agent` runs in
the Stage 2 arm. Defensive padding against tight-loop on transient
spawn errors (filesystem / network / PTY allocation). Crash path uses
exponential 5s+ backoff; Stage 2's controlled action permits shorter
delay. Fixed const `STAGE2_BACKOFF_DEFAULT_MS` (1 s), not env-configurable
(#env-cleanup: the `AGEND_AUTO_RECOVERY_STAGE2_BACKOFF_MS` override was
demoted).

### 9.4 Stage 2 fail criteria (3 modes)

The dispatcher's `Stage2Pending` monitor escalates to `Stage3Eligible`
on any of:

1. **`spawn_agent` returns `Err`** — Stage 2 cannot complete; agent
   removed from registry, dispatcher next-tick sees nothing to do.
   Operator already received telegram pre-emit. Phase 1 limitation:
   manual respawn or future operator-unpause command required.
2. **30s timeout window expired** without recovery (`state != Healthy`
   when `entered_at.elapsed() >= STAGE2_TIMEOUT_DEFAULT_MS`). Fixed const
   (30 s), not env-configurable (#env-cleanup: the
   `AGEND_AUTO_RECOVERY_STAGE2_TIMEOUT_MS` override was demoted).
3. **Agent re-Hungs within Stage 2 window** — `Stage2Pending` and
   state == Hung implies brief Healthy then back to Hung; more
   aggressive escalation. (Phase 1 implementation: timeout check
   covers this; re-Hung is just a specific instance of "still not
   Healthy after timeout".)

### 9.5 Channel-full safety (try_send)

`crash_tx` is `bounded::<>(64)` at `daemon/mod.rs:438`. Under extreme
load (e.g. many agents crashing simultaneously), the send may fail
with `TrySendError::Full`. Dispatcher uses `try_send`:

- **`Ok`**: state transitions to `Stage2Pending`,
  `last_stage2_fired_at` stamped. Counter increment lives on the
  respawn worker side so a successful event delivery without spawn
  completion does not falsely increment.
- **`Err`**: state stays `Stage2Eligible`, counter NOT incremented.
  Next dispatcher tick retries.

This is the **race coverage** mentioned in decision §extras: a crash
arriving on the same channel during Stage 2 spawn does NOT cause
double-counting because the dispatcher's `try_send` operates on a
different (`Stage2Restart`) variant; the crash flows through its own
path independently.

### 9.6 Spawn failure Phase 1 limitation

If `spawn_agent` in `handle_stage2_restart` returns `Err`, the agent is
**removed** from the registry. Dispatcher next-tick won't find it and
the recovery sequence ends. Operator visibility is preserved via the
Stage 2 telegram emitted **pre-emit** (before the spawn attempt).

Full lifecycle (operator-driven re-spawn or unpause) ships in sub-task
7c + a separate operator-unpause command sub-task. Phase 1 acceptable:
spawn-failure is edge case; operator can manually re-spawn via the
existing `start` CLI or MCP `agent spawn` tool.

### 9.7 Telegram notify content

```
[recovery] {agent_name}: Stage 2 auto-restart triggered.
Hung silence: {silent_ms}ms (productive silence: {prod_ms}ms)
Recovery restart count: {count}
Next: monitoring 30s for recovery; Stage 3 (pause + operator action)
on continued failure.
```

Operator-actionable: surfaces what triggered (silence vs productive
silence — distinguishes alive-stuck from dead-likely), current
restart-count progression toward cap, and expected next-step
escalation timeline.

### 9.8 Activation gate (mirrors §RS.5 Stage 1 pattern)

| Env var | Default | Purpose |
|---|---|---|
| `AGEND_AUTO_RECOVERY_STAGE2` | unset (shadow) | `"1"` activates: dispatcher emits `Stage2Restart` event. Unset: same telemetry, no emission. |
| `AGEND_AUTO_RECOVERY_STAGE2_MAX_RESTARTS` | 3 | Cumulative cap → direct `Stage3Eligible` escalation. |

The Stage 2 monitoring window (30 s, `STAGE2_TIMEOUT_DEFAULT_MS`) and
respawn backoff (1 s, `STAGE2_BACKOFF_DEFAULT_MS`) are **fixed consts, not
env-configurable** (#env-cleanup: the
`AGEND_AUTO_RECOVERY_STAGE2_TIMEOUT_MS` / `_BACKOFF_MS` overrides were
demoted).

Same shadow-mode promotion workflow as Stage 1: operator runs in
shadow for ≥2 weeks, classifies would-have-fires via
`recovery_shadow` tracing target, flips to active when confidence is
high. Anti-dead-infra clause: 6 weeks without measurement → Stage 2
removal candidate.

### Out of scope (sub-task 7b)

- Stage 3 dispatcher arm + `HealthState::Paused` activation —
  sub-task 7c.
- Operator unpause command — separate sub-task (required before
  Stage 3 ships in production).
- Per-backend Stage 2 timeout / backoff tuning — needs corpus
  measurement, follow-up.
- Full PTY-backed integration test for the variant-split spawn —
  unit tests cover the state machine + counter discipline; full
  integration deferred unless shadow telemetry surfaces edge cases.

## §RS.10 — Stage 3 specifics (sub-task 7c)

Stage 3 is the terminal stage of the auto-recovery state machine.
After Stage 1 ESC failed and Stage 2 auto-restart was attempted up to
the cumulative cap (`recovery_restart_count >= STAGE2_MAX_RESTARTS`),
the dispatcher escalates the agent to `HealthState::Paused` and
notifies the operator that manual intervention is required.

### 10.1 Stage 3 purpose

Auto-recovery is exhausted; further unattended retries would just
thrash the agent. Stage 3's job is to **stop trying**, lock the
agent's `HealthState` into a non-acting terminal value, and surface
the situation to the operator via an Error-level telegram. The
escalation is **single-shot per Hung cycle** — once `Stage3Pending`,
the dispatcher's `Stage3Pending` arm is an explicit no-op (see
§10.5).

### 10.2 `enter_paused` atomic invariants

`src/health.rs::HealthTracker::enter_paused(&mut self, now: Instant)`
is the **sole writer** of `HealthState::Paused` in the codebase
(§F39.5 invariant — single grep target). The method writes three
invariants under the caller's lock:

1. `state = HealthState::Paused`
2. `recovery_stage_state = RecoveryStageState::Stage3Pending { entered_at: now }`
3. `last_stage3_fired_at = Some(now)`

The `Stage3Pending` variant carries `entered_at` so the dispatcher's
no-op debug log can report Paused-since duration without reaching back
into `HealthTracker`. `last_stage3_fired_at` is reserved for the future
operator-unpause sub-task (UX "Paused since N minutes") and carries
`#[allow(dead_code)]` until that sub-task reads it.

DI-friendly signature parallels `maybe_decay_at(now)`: production
passes `Instant::now()`; tests pass a deterministic base for
cross-platform-safe arithmetic (PR #775 v2 lesson — `Instant::add`
saturates on all platforms; `Instant::now() - Duration` can underflow
on low-uptime Windows CI VMs).

### 10.3 `NotifySeverity::Error` + telegram format

The `NotifySeverity` enum has three levels: `Info`, `Warn`, `Error`.
Stage 2 uses `Warn`; crash notifications use `Error`. Stage 3 = "auto-
recovery exhausted, operator must act", so its severity must be ≥ the
crash level → `Error`. `silent=false` so the operator's channel
surfaces it alongside crash notifications.

Telegram body (built by `format_stage3_body(name, count)` so unit
tests pin the operator-facing wording):

```
[recovery ESCALATION] {name}: PAUSED — manual intervention required.
  Stage 2 auto-restart fired {count} time(s), all exhausted.
  Final state: Paused (no further auto-recovery).
  Action: investigate root cause + manual unpause (CLI command pending sub-task).
```

Telegram fires in **both** shadow and active modes so operators see
the escalation pattern before flipping the gate. Pre-emitted before
the state write so a crash between telegram and `enter_paused` still
surfaces the decision.

### 10.4 `recovery_restart_count` NOT reset on `enter_paused`

The counter is **preserved** across Stage 3 entry. Rationale: Paused
means "automated retry is exhausted; root cause must be addressed".
If a future operator-unpause sub-task brings the agent back to
`Healthy` and the agent Hungs again without the root cause being
fixed, the dispatcher's cap check immediately escalates to
`Stage3Eligible` again rather than burning further auto-restart
budget. The operator semantics is: pause is sticky; counter reset is
the unpause sub-task's design space.

### 10.5 `Stage3Pending` idempotent no-op

The dispatcher's `Stage3Pending` arm is an explicit no-op:

```rust
RecoveryStageState::Stage3Pending { entered_at } => {
    tracing::debug!(
        target: "recovery_shadow",
        agent = %name,
        paused_for_ms = entered_at.elapsed().as_millis() as u64,
        "stage3_pending: awaiting operator unpause"
    );
}
```

No state mutation, no telegram re-fire, no timeout escalation. Double
protection against any subtle re-entry path comes from the top-level
`HealthState::Paused` early-`continue` in `run()` (§RS.7 7a guard).
Together: dispatcher cannot escalate out of Paused, cannot re-fire
Stage 3 telegrams on subsequent ticks, and `maybe_decay_at` honours
the Paused short-circuit so the counter the operator sees stays
faithful to the moment Paused entered.

### 10.6 Promotion criteria (`AGEND_AUTO_RECOVERY_STAGE3=1`)

Hybrid template (round 2 convergence):

1. Operator runs daemon with `AGEND_AUTO_RECOVERY_STAGE3` unset
   (shadow) for ≥2 weeks across the agent fleet.
2. Operator reviews `recovery_shadow` tracing target output, focusing
   on **trigger rate per week** rather than FP-per-trigger. FP
   semantics are undefined for a terminal stage — Stage 3 only fires
   after Stage 2 retries are demonstrably exhausted, so the risk of
   inappropriate action is structurally near-zero. The observation
   target is "how often is the fleet hitting auto-recovery exhaustion
   at all?".
3. Once trigger-rate baseline is stable and operator is confident
   that paused agents are genuinely stuck (rather than e.g. mis-tuned
   thresholds), flip `AGEND_AUTO_RECOVERY_STAGE3=1`.

Anti-dead-infra clause carries over from Stage 1 / Stage 2: 6 weeks
without measurement → Stage 3 promotion infrastructure becomes a
removal candidate.

### 10.7 Paused 解除 limitations (Phase 1)

Once `enter_paused` writes, the agent stays in `Paused` until one of
the following:

- **Operator manual restart** via the existing CLI agent-restart
  surface — fully resets the agent (fresh `HealthTracker`), which
  also resets `recovery_restart_count`. This is the Phase 1 operator
  workflow.
- **Future operator-unpause sub-task** will provide a dedicated
  `unpause` CLI / MCP command that transitions `Paused → Healthy`
  without a full restart. Its scope includes the design space of
  whether to reset `recovery_restart_count` on unpause; 7c does
  **not** make that decision in advance.

No automatic exit from `Paused` exists. `maybe_decay_at` honours the
short-circuit at line 1 of its body (7a guard) — the counter the
operator sees is faithful to the Paused-entry moment.

### Out of scope (sub-task 7c)

- Operator unpause command (separate sub-task)
- Per-backend Stage 3 customization (Phase 3)
- Multiple-Pause aggregation (single Paused tracking only)
- `recovery_restart_count` reset on unpause (defer to unpause sub-task)
- `last_stage3_fired_at` consumer code (reserved for unpause sub-task;
  `#[allow(dead_code)]` carries the field through 7c)
- Full PTY-backed integration test for `enter_paused` via a registered
  agent — unit tests pin the atomic invariants + idempotency at the
  `HealthTracker` boundary; integration deferred unless shadow
  telemetry surfaces edge cases.
