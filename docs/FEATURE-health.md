[繁體中文](FEATURE-health.zh-TW.md)

# Health & Monitoring — Agent Health State and Auto-Recovery

## Usage Scenarios

> **Target audience:** Daemon infrastructure — fully automated, operators observe via TUI.

**Automatic crash recovery.** A dev agent's process crashes due to an unexpected API error. The daemon detects the exit, records it in the crash sliding window, waits for an exponential backoff delay (starting at 5 seconds), and restarts the agent automatically. If the agent stabilizes, it resumes work as if nothing happened. If it crashes repeatedly (5+ times), the daemon stops restarting and notifies the operator.

**Hang detection and recovery.** An agent receives a task but produces no PTY output for over 600 seconds while in `Thinking` state. The daemon classifies it as `Hung` and initiates the three-stage recovery ladder: first sending an ESC key to interrupt, then restarting the process if ESC fails, and finally pausing the agent and alerting the operator if restarts are also ineffective.

**Operator health dashboard.** The operator checks the TUI and sees that one agent is in `Unstable` state (3 crashes in 10 minutes) while another shows `IdleLong` (no pending work, just waiting). The structured health state lets the operator quickly distinguish between agents that need attention and those that are simply idle.

## Design Rationale

AI coding agents encounter various anomalies: process crashes, API rate limits,
response timeouts (hangs), and error loops. If every incident required manual
operator intervention, a multi-agent system could not run unattended for
extended periods.

Health & Monitoring is a core daemon subsystem that continuously monitors each
agent's health, automatically executes recovery actions (restart, send ESC key,
notify operator), and provides structured status reports for operators and other
agents.

---

## Two-Layer State Model

AgEnD separates agent state into two independent layers:

### AgentState — Instant Detection

AgentState is parsed in real time from PTY output. Every line of output can
change the AgentState.

| State | Description |
|-------|-------------|
| `Starting` | Process just launched, not yet ready |
| `Ready` | Ready, awaiting input |
| `Idle` | Idle, no pending work |
| `Thinking` | Generating intermediate output |
| `ToolUse` | Executing a tool call |
| `RateLimit` | Hit API rate limit |
| `InteractivePrompt` | Displaying an interactive prompt (e.g., permission confirmation) |
| `PermissionPrompt` | Awaiting permission confirmation |
| `AwaitingOperator` | Waiting for operator action |
| `Hang` | Suspected hang |
| `Crashed` | Process has exited |

### HealthState — Cumulative Lifecycle

HealthState is derived from multiple accumulated events. It reflects the
agent's overall health trend and does not change from a single line of output.

| State | Description | Triggers Auto-Recovery |
|-------|-------------|----------------------|
| `Healthy` | Running normally | No |
| `Recovering` | Restarting after a crash | No |
| `Unstable` | 3+ crashes within 10 minutes | No |
| `Failed` | Exceeded max retries (5); auto-restart stopped | No (terminal) |
| `Hung` | Has pending input but timed out without responding | Yes |
| `IdleLong` | No activity for a long time, but no pending input (not anomalous) | No |
| `ErrorLoop` | 3+ same-state errors within 10 minutes | No |
| `Paused` | All 3 auto-recovery stages failed; awaiting manual intervention | No (terminal) |

---

## Crash Handling

### Auto-Restart

When an agent process crashes, the daemon restarts it automatically:

1. **Record the crash**: add the crash timestamp to a sliding window (10 minutes).
2. **Calculate backoff delay**: exponential backoff starting at 5 seconds, doubling each time, capped at 5 minutes.
3. **Decide whether to restart**:
   - Total crashes < 5: restart.
   - Total crashes >= 5: enter `Failed` state, stop restarting.
4. **Notification logic**:
   - 1st crash: silent restart.
   - 2nd crash onward: send notification (subject to 5-minute cooldown).

### State Transitions

```
Healthy → (1 crash) → Recovering → (respawn OK) → Healthy
Healthy → (3 crashes in 10min) → Unstable
Any state → (5+ crashes) → Failed (terminal; requires operator intervention or decay)
```

### Crash Decay

If an agent runs stably for 30 minutes (no new crashes), the crash count
decays automatically:

- `total_crashes` decreases by 1 every 30 minutes.
- `Failed` → `Recovering` (when count drops below 5).
- `Recovering` → `Healthy` (when count drops below 3).
- `Unstable` → `Healthy` (when count drops below 3).

---

## Hang Detection

### Detection Logic

The daemon checks each agent's silence duration every tick. When the threshold
is exceeded, classification begins:

| AgentState | Silence Threshold |
|------------|-------------------|
| `Idle` | Never treated as hang |
| `Starting` | 120 seconds |
| `Thinking` / `ToolUse` | 600 seconds |
| Other | 120 seconds |

### Hung vs. IdleLong

After exceeding the silence threshold, the daemon further classifies the state:

**Hung (truly stuck)**: has pending input but the agent is not responding.
- Condition: `last_input_at_ms > last_heartbeat_at_ms + 5s`
- Meaning: the operator sent input, but the agent has not made any MCP calls (heartbeat) for over 5 seconds.
- Triggers the auto-recovery ladder.

**Hung (F1 cross-check)**: heartbeat is fresh but PTY has no output.
- Condition: heartbeat was recently updated, but PTY has been silent past the threshold.
- Meaning: the agent is making MCP tool calls (refreshing heartbeat) but producing no PTY output.
- May indicate the agent is stuck in a tight MCP loop.

**IdleLong (normal idle)**: no pending input; the agent is simply waiting for the next task.
- Does not trigger any recovery action.

### 5-Second Grace Window

There is a 5-second grace window between input arrival and heartbeat refresh to
avoid false Hung classification during MCP roundtrip completion.

---

## Auto-Recovery Ladder

When an agent is classified as `Hung`, the daemon initiates a three-stage
recovery:

### Stage 1: ESC Key

- Sends an ESC key to the agent's PTY (interrupts the current operation).
- Waits 10 seconds for recovery.
- Cooldown: 60 seconds (prevents rapid ESC spam).

### Stage 2: Auto-Restart

- If Stage 1 fails, restarts the agent process.
- Waits 30 seconds for recovery.
- Maximum 3 restarts (cumulative across Hung cycles).
- Backoff delay: 1 second.

### Stage 3: Pause

- If all 3 Stage 2 restarts fail:
  - Sets HealthState to `Paused` (terminal state).
  - Notifies the operator that manual intervention is needed.
  - `check_hang` short-circuits (returns false) for this agent.
  - Crash decay does not affect the `Paused` state.

The entire ladder can be toggled via the `hang_auto_recovery_enabled` runtime
config gate.

---

## Watchdog — PTY Output Classification

The daemon's watchdog scans each agent's latest PTY output every tick, matching
against known anomaly patterns:

| Detected Pattern | BlockedReason Set |
|-----------------|-------------------|
| "rate limit" / "Too Many Requests" | `RateLimit` |
| "quota exceeded" | `QuotaExceeded` |
| "awaiting operator" / interactive prompt | `AwaitingOperator` |
| "permission" prompt | `PermissionPrompt` |

After setting a BlockedReason:
- `RateLimit` / `QuotaExceeded` / `AwaitingOperator`: suppress hang detection (avoid false alarms).
- `PermissionPrompt`: does **not** suppress hang detection.

### Dry-Run Mode

Set `AGEND_WATCHDOG_DRY_RUN=1` to enable dry-run mode: the watchdog logs
detection results to the event log but does not modify health state. Useful for
testing pattern matching accuracy before going live.

---

## Idle Watchdog — Inactivity Detection

An independent watchdog that tracks agent and fleet-wide activity.

### Two Observation Angles

**Dev perspective**: a single agent idle for over 60 minutes.
- Notifies the lead: "dev has been idle for 60 minutes."

**Fleet perspective**: all agents idle for over 30 minutes.
- Notifies general: "the entire fleet has been idle for 30 minutes."
- Suppressed when the task board is empty and no dispatches are pending.

### Activity Tracking

Each agent's activity is recorded in an `$AGEND_HOME/agent-activity/<agent>.json`
sidecar. The timestamp updates automatically each time the agent sends a
message via the `send` tool.

### Snooze and Ack

| Operation | Description |
|-----------|-------------|
| Snooze | Pause fleet idle alerts until a specified time |
| Ack | Acknowledge the alert; no repeat until new activity occurs |
| Resume | Clear snooze/ack, restore normal detection |

---

## MCP Tools

### health report

Report the current blocked reason to the daemon:

```json
{
  "tool": "health",
  "action": "report",
  "reason": "rate_limit",
  "retry_after_secs": 60
}
```

Agents can proactively report issues so the watchdog knows not to trigger hang
detection.

### health clear_blocked_reason

Clear a previously set blocked reason, restoring normal hang detection:

```json
{
  "tool": "health",
  "action": "clear_blocked_reason"
}
```

---

## FAQ

### Q: Agent is classified as Hung but is actually working normally?

Possible causes:
1. PTY output is being consumed by the backend's TUI framework (not reaching the daemon's vterm).
2. The agent is executing a long-running tool call (exceeding 600 seconds).

Solutions:
- Enable `AGEND_PRODUCTIVE_GATE=1` for the F9 productive-output gate.
- Use `health action=report` to proactively report the agent's state.

### Q: Can Failed state recover automatically?

Yes, but it takes time. The crash count decays by 1 every 30 minutes. When
`total_crashes` drops below 5, the state transitions to Recovering; below 3, to
Healthy. However, while in Failed state the process is stopped — the operator
must manually restart it or wait for Stage 2 auto-recovery to trigger.

### Q: How to clear Paused state?

Currently, Paused is a terminal state requiring manual operator intervention.
A future release will add an operator unpause command.

### Q: How to check an agent's current health state?

```bash
agend-terminal list --detailed
```

Or query the agent registry via MCP tools.