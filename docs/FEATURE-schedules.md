# Schedules & Deployments — Timed Jobs and Batch Deployment

Schedules let you define cron jobs or one-shot jobs. Deployments let you stamp out a multi-agent team in one step. Both remove repetitive operator work.

## Usage Scenarios

> **Target audience:** Both operators and agents.

An operator wants a daily 9:00 AM standup reminder to go out automatically to the team. A cron-based schedule handles that without requiring anyone to remember the trigger time by hand.

An agent or operator wants to queue a cleanup action after a PR is merged, such as waiting 30 minutes before running a follow-up task. A one-shot schedule is a better fit because the action should happen once, not repeatedly.

For repeatable team setups, a deployment can create the whole arrangement at once while schedules handle the later reminders and follow-up jobs. The two features complement each other rather than overlapping.

## Design Goals

- **Schedules**: send a morning standup reminder at 9:00 every day, or check CI every hour, without relying on somebody to remember.
- **Deployments**: deploy an entire team in one command, including worktree creation, fleet.yaml updates, and team registration.

---

## Schedules

### Quick Start

```json
// Send a daily standup reminder at 9:00 AM
{
  "action": "create",
  "cron": "0 9 * * *",
  "message": "Good morning! Please report yesterday's progress and today's plan.",
  "target": "lead"
}

// Run once in 30 minutes
{
  "action": "create",
  "run_at": "2026-05-25T10:00:00",
  "message": "Reminder: the PR review deadline is here.",
  "target": "reviewer"
}
```

### `create` — create a schedule

| Parameter | Type | Required | Description |
|---|---|---:|---|
| `cron` | string | one of two | Cron expression for recurring jobs |
| `run_at` | string | one of two | One-shot time (RFC 3339 or local time) |
| `message` | string | yes | Message to send when the job fires |
| `target` | string | no | Target agent, defaults to the creator |
| `label` | string | no | Human-readable label |

`cron` and `run_at` are mutually exclusive. One of them must be set.

### `list` — list schedules

```json
{"action": "list"}
```

Returns all schedules, including execution history. The history is capped at the most recent 50 runs.

### `update` — edit a schedule

| Parameter | Type | Description |
|---|---|---|
| `id` | string | Schedule ID, required |
| `cron` | string | New cron expression |
| `run_at` | string | Switch to a one-shot schedule |
| `message` | string | New message body |
| `target` | string | New target agent |
| `label` | string | New label |
| `enabled` | bool | Enable or disable the schedule |

You can switch between recurring and one-shot schedules.

### `delete` — delete a schedule

```json
{"action": "delete", "id": "s-20260525..."}
```

### Cron Format

Both 5-field and 6-field cron formats are supported:

```text
# 5-field: minute hour day-of-month month day-of-week
0 9 * * *

# 6-field: second minute hour day-of-month month day-of-week
0 0 9 * * *
```

Day-of-week follows Quartz style, where `1=Sun` and `7=Sat`.

### Common Schedule Targets

Typical targets include:

- `lead`
- `reviewer`
- `dev`
- `general`

If `target` is omitted, the schedule usually defaults to the creator or the current agent context.

### Operational Notes

- Schedules persist on disk.
- The scheduler evaluates them as part of daemon activity.
- Execution history is useful for debugging when a job fires unexpectedly or not at all.

---

## Deployments

Deployments are for creating a whole team configuration from a template.

### What a deployment does

A deployment can create or update a multi-agent setup, including:

- worktrees
- fleet.yaml entries
- team metadata
- deployment-scoped coordination state

### Typical usage

```json
{
  "action": "deploy",
  "name": "docs-batch",
  "template": "docs-team",
  "branch": "docs/1195-bilingual-batch-c"
}
```

### `deploy`

| Parameter | Type | Required | Description |
|---|---|---:|---|
| `name` | string | yes | Deployment name |
| `template` | string | yes | Template name from `fleet.yaml` |
| `branch` | string | no | Git branch for worktrees |
| `directory` | string | no | Override working directory |

### `teardown`

Deletes the deployment scaffolding. Use this when a batch is finished and the temporary team no longer needs to exist.

### `list`

Lists existing deployments so you can see what has already been stamped out.

---

## When to Use Schedules vs Deployments

Use **Schedules** when you need a message or action to happen later.

Use **Deployments** when you need to create a reusable team setup now.

A good rule of thumb:

- if the question is "when should this happen?" use schedules
- if the question is "what should exist right now?" use deployments

---

## Storage Model

Schedules and deployments both persist state under the AgEnD home directory. That means they survive daemon restarts and are visible to the operator from the filesystem if needed.

The important implication is that these features are not ephemeral memory:

- a scheduled reminder still exists after restart
- a deployment entry can be queried later
- the daemon reconstructs state from disk on startup

---

## Failure Modes

### Invalid cron

If the cron expression does not parse, creation fails immediately. Fix the expression before retrying.

### Missing target

If a schedule should fire to a specific agent but the name is wrong, the message will route nowhere useful. Always validate the target name against the fleet.

### Deployment template mismatch

If the deployment template does not match the fleet structure, the resulting setup may be incomplete or partially populated. Treat the template as the source of truth for that deployment shape.

---

## Common Patterns

### Daily standup reminder

Use a recurring schedule with a target like `lead` or `general`.

### Time-boxed review nudge

Use a one-shot schedule when a single deadline matters more than a recurring cadence.

### Team bootstrap

Use a deployment when you need to provision a repeatable multi-agent arrangement for a sprint or batch.

---

## Source Pointers

- `src/schedule.rs`: schedule storage and dispatch
- `src/deployment.rs`: deployment orchestration
- `src/main.rs`: CLI subcommand routing
- `src/mcp/handlers/schedule.rs`: MCP surface
- `src/mcp/handlers/deployment.rs`: deployment surface

---

## Practical Advice

1. Prefer one-shot schedules for deadline-driven work.
2. Add labels that will make sense in logs weeks later.
3. Use deployments for repeatable fleet setups, not ad hoc reminders.
4. Keep cron expressions simple unless you have a strong reason to complicate them.
