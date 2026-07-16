[繁體中文](FEATURE-schedules.zh-TW.md)

# Schedules & Deployments — Timed Jobs and Batch Deployment

Schedules let you define cron jobs or one-shot jobs. Deployments let you stamp out a multi-agent team in one step. Both remove repetitive operator work.

## Usage Scenarios

> **Target audience:** Both operators and agents.

An operator wants a daily 9:00 AM standup reminder to go out automatically to the team. A cron-based schedule handles that without requiring anyone to remember the trigger time by hand.

An agent or operator wants to queue a cleanup action after a PR is merged, such as waiting 30 minutes before running a follow-up task. A one-shot schedule is a better fit because the action should happen once, not repeatedly.

For repeatable team setups, a deployment can create the whole arrangement at once while schedules handle the later reminders and follow-up jobs. The two features complement each other rather than overlapping.

## Design Goals

- **Schedules**: send a morning standup reminder to the team at 9:00 every day, or check CI status every hour, without relying on the operator to remember.
- **Deployments**: deploy an entire team (lead + dev + reviewer) in one command, including worktree creation, fleet.yaml updates, and team registration.

---

## Schedules — Timed Jobs

### Quick Start

```json
// Send a daily standup reminder at 9:00 AM
{
  "action": "create",
  "cron": "0 9 * * *",
  "message": "Good morning! Please report yesterday's progress and today's plan.",
  "instance": "lead"
}

// Run once in 30 minutes
{
  "action": "create",
  "run_at": "2026-05-25T10:00:00",
  "message": "Reminder: the PR review deadline is here.",
  "instance": "reviewer"
}
```

### Operations

#### create — create a schedule

| Parameter | Type | Required | Description |
|------|------|------|------|
| `cron` | string | one of two | Cron expression (recurring jobs) |
| `run_at` | string | one of two | One-shot time (RFC 3339 or local time) |
| `message` | string | yes | Message to send when the job fires |
| `instance` | string | no | Target agent (defaults to the creator) |
| `label` | string | no | Human-readable label |
| `timezone` | string | no | IANA timezone; detected at creation when omitted |
| `fire_strategy` | string | no | `always` (default) or `until_success` |
| `linked_task_id` | string | conditional | Existing task ID required by `until_success` |

`cron` and `run_at` are mutually exclusive. Exactly one of them must be set.

#### list — list schedules

```json
{"action": "list"}
```

Returns all schedules, including `next_scheduled_fire_at`, `runs_total`, and the newest three history entries by default. Pass `full_history: true` for the full retained history (up to 50 entries), or `instance` to filter by delivery target.

#### update — edit a schedule

| Parameter | Type | Description |
|------|------|------|
| `id` | string | Schedule ID (required) |
| `cron` | string | New cron expression |
| `run_at` | string | Switch to a one-shot schedule |
| `message` | string | New message body |
| `instance` | string | New target agent |
| `label` | string | New label |
| `enabled` | bool | Enable / disable |
| `timezone` | string | New IANA timezone |
| `fire_strategy` | string | `always` or `until_success` |
| `linked_task_id` | string | Task linked to `until_success` |

You can switch between recurring and one-shot schedules.

#### Fire strategy

`fire_strategy: "always"` fires on every cron match. With `"until_success"`, the linked task must exist; after that task is done, the schedule skips the remaining matches for the same calendar day in the schedule's timezone and becomes eligible again the next day. If the linked task later disappears, the schedule is disabled and records `target_task_missing`.

#### delete — delete a schedule

```json
{"action": "delete", "id": "s-20260525..."}
```

### Cron Format

Both the standard 5-field and 6-field cron formats are supported:

```
# 5-field (the system auto-fills seconds as 0)
minute hour day month day-of-week
0 9 * * *           → daily at 09:00
30 14 * * 2-6       → Mon–Fri at 14:30
0 */2 * * *         → every 2 hours

# 6-field (second minute hour day month day-of-week)
30 0 9 * * *        → daily at 09:00:30
```

Day-of-week follows the Quartz convention: 1=Sun, 2=Mon, ..., 7=Sat.

### Timezone Handling

Each schedule records the timezone in effect at creation time (IANA format), and the cron expression is evaluated in that timezone.

Detection order:
1. The `TZ` environment variable
2. The system timezone (macOS: CoreFoundation, Linux: `/etc/localtime`)
3. Fall back to `UTC`

The timezone is locked at creation time and does not change if the system timezone changes.

### Trigger Mechanism

The daemon's main loop runs a tick every 10 seconds:

1. Load all enabled schedules
2. Compute the check interval: `(last check time, now]`
3. Decide for each schedule whether it should fire
4. On firing, deliver the message to the target agent

Interval tracking prevents duplicate firing when the daemon restarts.

### Message Delivery

Delivery uses a different method depending on the target agent's state:

| State | Delivery Method | Recorded Status |
|------|----------|----------|
| Online | Injected directly into PTY stdin | `ok` |
| Offline | Written to the inbox | `ok_inbox` |
| Missed (daemon was not running at the time) | Not delivered | `missed` |

### One-Shot Schedules

A one-shot schedule (`run_at`) auto-disables after it fires and will not fire again.

If the daemon was not running at the scheduled time:
- Within 24 hours: the daemon replays it on startup
- More than 24 hours: it is marked `stale_dropped` and disabled; the stale message is not replayed

### Execution History

Each schedule stores at most 50 runs. The `list` response trims this to the newest three by default and reports `runs_total`; set `full_history: true` to return all retained entries:

```json
{
  "run_history": [
    {"triggered_at": "2026-05-25T09:00:00Z", "status": "ok"},
    {"triggered_at": "2026-05-24T09:00:00Z", "status": "ok_inbox"},
    {"triggered_at": "2026-05-23T09:00:00Z", "status": "missed"}
  ]
}
```

### Storage

- Location: `$AGEND_HOME/schedules.json`
- Format: versioned JSON (v1 → v2 auto-upgrade)
- Locking: flock + atomic write (temp → fsync → rename)

---

## Deployments — Batch Deployment

### Quick Start

```json
// Deploy a three-person team
{
  "action": "deploy",
  "template": "fixup-team",
  "directory": "/tmp/fixup-workspace",
  "branch": "main"
}
```

### Deployment Templates

Define deployment templates in `fleet.yaml`:

```yaml
templates:
  fixup-team:
    orchestrator: lead
    instances:
      lead:
        backend: claude
        role: "團隊 orchestrator，負責任務分派和審查結果彙整"
      dev:
        backend: claude
        role: "實作者，負責寫程式碼和修 bug"
      reviewer:
        backend: claude
        role: "審查者，負責 code review"
```

### Operations

#### deploy — deploy

| Parameter | Type | Required | Description |
|------|------|------|------|
| `template` | string | yes | Template name (defined in `fleet.yaml`) |
| `directory` | string | yes | Parent path for the working directories |
| `name` | string | no | Deployment name (defaults to the template name) |
| `branch` | string | no | Git branch (worktree created automatically) |

The deployment flow has four phases:

1. **Validation and Worktree**: validate the template, create a `<directory>/<name>-<suffix>` subdirectory for each agent. If `branch` is specified, use `git worktree add`
2. **Fleet.yaml Write**: write all instance definitions into `fleet.yaml`
3. **Agent Startup**: spawn each agent one by one
4. **Team Creation**: if it is a multi-agent template, create a team automatically and assign the orchestrator

#### teardown — tear down

```json
{
  "action": "teardown",
  "name": "fixup-team"
}
```

The teardown flow:
1. Delete all agent instances
2. Clean up the filesystem (delete the working directories)
3. Remove the instance definitions from `fleet.yaml`
4. Delete the team (if any)
5. Remove the entry from the deployment records

If the parent directory is empty after teardown, it is cleaned up as well.

#### list — list deployments

```json
{"action": "list"}
```

Returns all deployment records, including the instance list and creation time.

### Orphan Deployment Cleanup

On startup the daemon automatically checks for orphan deployments — cases where an instance in the deployment records no longer exists in `fleet.yaml`. Orphan deployments have their associated team and filesystem cleaned up automatically.

### Storage

- Location: `$AGEND_HOME/deployments.json`
- Format: versioned JSON
- Locking: flock + atomic write

---

## Common Patterns

### Daily Standup Reminder

```json
{
  "action": "create",
  "cron": "0 9 * * 2-6",
  "message": "早安！請回報：1) 昨天完成了什麼 2) 今天計畫做什麼 3) 有沒有阻塞",
  "instance": "lead",
  "label": "daily-standup"
}
```

### Periodic PR Status Check

```json
{
  "action": "create",
  "cron": "0 */3 * * *",
  "message": "請檢查所有 open PR 的 CI 狀態，回報任何失敗的 check。",
  "instance": "reviewer",
  "label": "pr-health-check"
}
```

### Delayed Reminder

```json
{
  "action": "create",
  "run_at": "2026-05-25T15:00:00",
  "message": "提醒：今天 3 點有 release cut，確認所有 PR 已合併",
  "instance": "lead"
}
```

### One-Command Team Deployment

```json
{
  "action": "deploy",
  "template": "fixup-team",
  "directory": "/tmp/sprint-59",
  "branch": "main",
  "name": "sprint-59"
}
```

After deployment completes, the three agents each work in the `/tmp/sprint-59/sprint-59-lead`, `/tmp/sprint-59/sprint-59-dev`, and `/tmp/sprint-59/sprint-59-reviewer` directories, the team is created, and lead is the orchestrator.

### Teardown After Work Is Done

```json
{
  "action": "teardown",
  "name": "sprint-59"
}
```

One command cleans up all agents, the team, the working directories, and the fleet.yaml records.

---

## When to Use Schedules vs Deployments

Use **Schedules** when you need a message or action to happen later.

Use **Deployments** when you need to create a reusable team setup now.

A good rule of thumb:

- if the question is "when should this happen?" use schedules
- if the question is "what should exist right now?" use deployments

---

## Failure Modes

### Invalid cron

If the cron expression does not parse, creation fails immediately. Fix the expression before retrying.

### Missing instance

If a schedule should fire to a specific agent but `instance` is wrong, the schedule cannot deliver usefully. Validate the instance name against the fleet. Deleting a target instance disables its schedules and records an orphaned history entry rather than deleting them.

### Deployment template mismatch

If the deployment template does not match the fleet structure, the resulting setup may be incomplete or partially populated. Treat the template as the source of truth for that deployment shape.

---

## Source Pointers

- `src/schedules.rs`: schedule storage and validation
- `src/deployments.rs`: deployment orchestration
- `src/main.rs`: CLI subcommand routing
- `src/mcp/handlers/schedule.rs`: MCP surface
- `src/daemon/cron_tick.rs`: schedule evaluation and delivery

---

## Practical Advice

1. Prefer one-shot schedules for deadline-driven work.
2. Add labels that will make sense in logs weeks later.
3. Use deployments for repeatable fleet setups, not ad hoc reminders.
4. Keep cron expressions simple unless you have a strong reason to complicate them.
