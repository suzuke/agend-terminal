# Dispatch Idle Tracking — Task Response Timeout Monitoring

## Usage Scenarios

> **Target audience:** Agent infrastructure — agents use this via MCP tools; operators typically don't interact directly.

**Dropped task detection.** A lead agent dispatches a task to a dev agent with `expect_reply_within_secs=600`. The dev agent crashes before processing the message. After 10 minutes of silence, the daemon sends a `dispatch_idle_threshold_exceeded` notification to the lead, alerting it that the task may have been missed. The lead can then re-dispatch to another agent or investigate.

**Active work acknowledgment.** A dev agent receives a complex task and starts working on it. Partway through, it sends a `kind=update` message to report progress. The daemon sees this activity and resets the idle timer, preventing a false alarm while the dev is clearly engaged.

**Fixup team nudge.** In the fixup team, after the L1 notification goes to the lead, the L2 layer sends an additional `dispatch_idle_nudge` directly to the target dev agent. This two-pronged approach ensures both the dispatcher and the recipient are aware of the stalled task.

## Design Rationale

In multi-agent collaboration, an orchestrator (typically the lead) dispatches
tasks to dev or reviewer agents. If the recipient does not respond — perhaps
the agent is stuck, crashed, or occupied with other work — the orchestrator has
no way of knowing the task was dropped.

Dispatch Idle Tracking solves this. It starts a timer when a task is dispatched.
If no report (`kind=report`) is received within the configured time window, the
daemon automatically notifies the dispatcher: "the task you sent 10 minutes ago
has not received a response."

This is a two-layer (L1 + L2) system:

- **L1 (cross-team safe)**: notifies the dispatcher only; contains no team names or team logic.
- **L2 (team-specific)**: an additional nudge for the fixup team, notifying the target agent.

---

## Usage

### Enabling Tracking

When dispatching a task via the `send` tool, include the
`expect_reply_within_secs` parameter:

```json
{
  "tool": "send",
  "instance": "dev",
  "message": "Please implement the fix for #123",
  "request_kind": "task",
  "task_id": "t-20260525-1",
  "expect_reply_within_secs": 600
}
```

This creates a dispatch idle sidecar. After 600 seconds (10 minutes), the
daemon checks whether a matching `kind=report` (by `correlation_id`) has been
received.

### Fixup Team Defaults

For the fixup team, `kind=task` and `kind=query` dispatches automatically
inherit a 10-minute idle tracking window. No manual `expect_reply_within_secs`
is needed. Other teams must specify it explicitly.

### Parameter Reference

| Parameter | Type | Description |
|-----------|------|-------------|
| `expect_reply_within_secs` | int | Expected reply time in seconds; exceeding triggers an alert |

---

## How It Works

### Sidecar Mechanism

Each dispatch with `expect_reply_within_secs` creates a JSON sidecar file
under `$AGEND_HOME/dispatch-pending/`.

Sidecar contents:

```json
{
  "dispatch_id": "disp-1716616000000-1",
  "dispatcher": "fixup-lead",
  "target": "fixup-dev",
  "correlation_id": "t-20260525-1",
  "threshold_secs": 600,
  "created_at": "2026-05-25T05:00:00Z",
  "status": "pending",
  "nudge_sent_at": null
}
```

The `dispatch_id` is generated from a microsecond timestamp plus a
process-local atomic counter, ensuring uniqueness.

### L1: Timeout Detection and Notification

The daemon scans `dispatch-pending/` every 60 seconds:

1. Read all `pending` sidecars.
2. Calculate `elapsed = now - created_at`.
3. If `elapsed > threshold_secs`:
   - Send a `dispatch_idle_threshold_exceeded` notification to the **dispatcher**.
   - Update sidecar status to `exceeded`.
   - Log to the event log.

The notification includes the dispatch ID, target agent, elapsed time, and
correlation_id.

### L2: Fixup Nudge

L2 is a team-specific supplement to L1, active only for the fixup team.

After a sidecar transitions to `exceeded`, L2 sends an additional
`dispatch_idle_nudge` notification to the **target agent** (the recipient),
reminding it of the pending task.

Deduplication: each sidecar triggers at most one nudge (recorded in
`nudge_sent_at`).

L2 isolation guarantees:
- L1 code contains no team name strings (enforced by the `no_team_name_strings_in_l1` CI test).
- L2 is loaded as an independent module (`fixup_nudge.rs`) and only activates when the dispatcher belongs to the fixup team.

### Tracking Dismissal

When the target agent sends a `kind=report` with a matching `correlation_id`,
the daemon automatically deletes the corresponding sidecar. Matching is by
`correlation_id` (not by sender or target).

This supports multi-dispatch scenarios: the same orchestrator can dispatch to
multiple agents simultaneously, with each sidecar tracked independently.

### Sidecar Refresh

While a sidecar is `pending` and has not timed out, if the target agent sends a
non-report message (e.g., `kind=update`), the daemon resets the sidecar's
`created_at` timestamp (restarting the timer), avoiding false alarms when the
agent is clearly active.

---

## Expiry Cleanup

### Three Types of Expired Sidecars

The daemon automatically cleans up expired sidecars during scanning:

| Type | Description |
|------|-------------|
| Placeholder correlation_id | `correlation_id` is a placeholder like `t-pending` (a dispatch hygiene issue on the lead's side) |
| Deleted target | The target instance has been removed from the fleet |
| Completed task | The `correlation_id` maps to a task board entry in done/cancelled status |

Fail-open semantics: if reading fleet.yaml or the task board fails during
cleanup, the sidecar is treated as active (not cleaned up), preserving existing
behavior.

---

## Visibility Query

Agents can query dispatch idle state related to themselves:

```json
{
  "tool": "dispatch_idle",
  "action": "list"
}
```

The response is split into two perspectives:

- **as_dispatcher**: all pending/exceeded sidecars dispatched by you.
- **as_target**: all sidecars targeting you.

Expired, cleaned-up, or report-dismissed sidecars do not appear.

---

## Configuration

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `AGEND_DISPATCH_IDLE_THRESHOLD_SECS` | 600 | Default timeout for the fixup team |

### Behavioral Details

- Scan frequency: every 60 seconds (synced with the daemon tick cycle).
- Each sidecar triggers at most one L1 exceeded notification and one L2 nudge.
- L1 notification target: dispatcher.
- L2 nudge target: target (recipient).
- Tracking dismissal uses `correlation_id`, not sender or target identity.

### Concurrency (#1340)

`mark_resolved` (called from the MCP report handler) and `scan_and_emit`
(called from the daemon tick) use flock serialization on the sidecar file
to prevent lost-update races. Without locking, a concurrent `mark_resolved`
could overwrite a sidecar that `scan_and_emit` was in the middle of
processing, causing a missed or duplicate notification.

---

## Typical Flow

```
1. Lead dispatches task to dev (expect_reply_within_secs=600)
   → daemon creates sidecar (status=pending)

2. Dev replies with report within 600 seconds (correlation_id matches)
   → daemon deletes sidecar ✓

---- or ----

2. Dev does not respond within 600 seconds
   → daemon scan: elapsed > threshold
   → L1: send exceeded notification to lead
   → sidecar status → exceeded

3. L2 scan (fixup team)
   → send nudge notification to dev
   → nudge_sent_at recorded

4. Dev receives nudge, sends report
   → daemon deletes sidecar ✓
```

---

## FAQ

### Q: Can non-fixup teams use this?

Yes. L1 is cross-team safe — any agent can use `expect_reply_within_secs`.
Only L2's nudge functionality is currently limited to the fixup team.

### Q: Will an agent be nudged if it's working but hasn't finished?

If the agent has sent `kind=update` or other non-report messages, the sidecar
timer resets. Only complete communication silence triggers the timeout.

### Q: Will sidecars accumulate on disk?

No. Three cleanup mechanisms:
1. Target sends a report → immediate deletion.
2. Expiry cleanup (placeholder / deleted target / closed task) → removed during scan.
3. Daemon startup sweep.
