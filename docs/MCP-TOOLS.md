# MCP Tools Reference

Every agent spawned by `agend-terminal` gets an MCP stdio server wired in automatically (via `mcp_config.rs` writing each backend's config file). The server exposes 36 tools grouped into 10 categories. All tool definitions live in `src/mcp/tools.rs`; the canonical JSON schemas come from there.

> Legend: **bold** = required parameter. Types match JSON Schema (`string`, `number`, `integer`, `boolean`, `array<string>`).

## Categories

| # | Category | Count | Tools |
|---|----------|-------|-------|
| 1 | User comms (Telegram) | 4 | `reply`, `react`, `edit_message`, `download_attachment` |
| 2 | Agent comms | 6 | `send_to_instance`, `delegate_task`, `report_result`, `request_information`, `broadcast`, `inbox` |
| 3 | Instance lifecycle | 8 | `list_instances`, `create_instance`, `delete_instance`, `start_instance`, `describe_instance`, `replace_instance`, `set_display_name`, `set_description` |
| 4 | Decisions | 3 | `post_decision`, `list_decisions`, `update_decision` |
| 5 | Task board | 1 | `task` (actions: `create` / `list` / `claim` / `done` / `update`) |
| 6 | Teams | 3 | `list_teams`, `update_team`, `delete_team` (teams are *created* via `create_instance` with `team` + `count`) |
| 7 | Schedules | 4 | `create_schedule`, `list_schedules`, `update_schedule`, `delete_schedule` |
| 8 | Deployments | 3 | `deploy_template`, `teardown_deployment`, `list_deployments` |
| 9 | CI watchers | 2 | `watch_ci`, `unwatch_ci` |
| 10 | Repo sharing | 2 | `checkout_repo`, `release_repo` |

---

## 1. User Comms (Telegram)

Used when the agent's owning instance has a Telegram topic bound.

### `reply`
Reply to the user via Telegram.
- **`text`** (string)

### `react`
React to the last user message with an emoji.
- **`emoji`** (string)

### `edit_message`
Edit a previously sent message.
- **`message_id`** (string), **`text`** (string)

### `download_attachment`
Download a file attachment. Returns the local path.
- **`file_id`** (string)

---

## 2. Agent Comms

### `send_to_instance`
Send a message to another instance.
- **`instance_name`** (string), **`message`** (string)
- `request_kind` (string enum: `query` | `task` | `report` | `update`)
- `requires_reply` (boolean), `task_summary` (string), `correlation_id` (string)
- `working_directory` (string), `branch` (string)

### `delegate_task`
Delegate work to another instance and expect a result report back.
- **`target_instance`** (string), **`task`** (string)
- `success_criteria` (string), `context` (string)

### `report_result`
Report results back to the instance that delegated to you.
- **`target_instance`** (string), **`summary`** (string)
- `correlation_id` (string), `artifacts` (string)

### `request_information`
Ask another instance a question, expect a reply.
- **`target_instance`** (string), **`question`** (string), `context` (string)

### `broadcast`
Send a message to multiple instances. Resolution priority: `team` > `targets` > `tags` > all.
- **`message`** (string)
- `targets` (array<string>), `team` (string), `tags` (array<string>)
- `request_kind` (string enum: `query` | `task` | `update`), `requires_reply` (boolean), `task_summary` (string)

### `inbox`
Check pending messages addressed to this instance. No parameters.

---

## 3. Instance Lifecycle

### `list_instances`
List all active agent instances. No parameters.

### `create_instance`
Create one or more agent instances.
- **`name`** (string) — instance name for single spawn; becomes *base name* and is ignored when `team` is set.
- `backend` (string) — `claude`, `gemini`, `kiro-cli`, `codex`, `opencode`.
- `args` (string), `model` (string), `working_directory` (string)
- `branch` (string) — if set, a git worktree is created.
- `task` (string) — initial task injected after spawn.
- `layout` (string enum: `tab` | `split-right` | `split-below`) — TUI placement. Relative to `target_pane` if set, otherwise relative to the caller's focused pane.
- `target_pane` (string) — name of an existing instance. With `layout=split-right` or `split-below`, the new pane is attached next to that instance's pane in whichever tab currently hosts it. Precedence: `target_pane` → caller's tab → new tab (silent fallback when the target isn't displayed).
- `team` (string) + one of:
  - `count` (integer) — homogeneous team: spawn `<team>-1`..`<team>-N` all on `backend`.
  - `backends` (array<string>) — heterogeneous team: member *i* uses `backends[i]` (e.g. `backends: ["codex", "kiro-cli", "gemini"]`). Length dictates member count; `count` is ignored when `backends` is set.
- `command` (string) — **deprecated**, use `backend`.

### `delete_instance`
Stop and remove an instance. Cleans working dir, metadata, session entry, and Telegram topic.
- **`name`** (string)

### `start_instance`
Start a stopped instance.
- **`name`** (string)

### `describe_instance`
Detailed info about an instance (state, backend, working dir, health, etc.).
- **`name`** (string)

### `replace_instance`
Replace an instance with a fresh one (fresh args, no resume).
- **`name`** (string), `reason` (string)

### `set_display_name`
Set your own display name.
- **`name`** (string)

### `set_description`
Set a description for this instance.
- **`description`** (string)

---

## 4. Decisions

### `post_decision`
Record a decision. `scope: fleet` = visible to all instances; `scope: project` = same working directory only.
- **`title`** (string), **`content`** (string)
- `scope` (string enum: `project` | `fleet`), `tags` (array<string>)
- `ttl_days` (number), `supersedes` (string)

### `list_decisions`
List active decisions.
- `include_archived` (boolean), `tags` (array<string>)

### `update_decision`
Update or archive an existing decision.
- **`id`** (string)
- `content` (string), `tags` (array<string>), `ttl_days` (number), `archive` (boolean)

---

## 5. Task Board

### `task`
Single tool covering the full board lifecycle.
- **`action`** (string enum: `create` | `list` | `claim` | `done` | `update`)
- `create` / `update`: `title`, `description`, `priority` (`low` | `normal` | `high` | `urgent`), `assignee`, `depends_on` (array<string>)
- `claim` / `done` / `update`: `id`, `status` (`open` | `claimed` | `done` | `blocked` | `cancelled`), `result`
- `list`: `filter_assignee`, `filter_status`

---

## 6. Teams

Teams are created via `create_instance` with `team` + `count`. Maintenance tools below.

### `list_teams`
No parameters.

### `update_team`
Add or remove members. When running inside the TUI, added members migrate into the team tab (created on demand) and removed members are dropped from the team tab — panes in other tabs are left untouched.
- **`name`** (string), `add` (array<string>), `remove` (array<string>)

### `delete_team`
- **`name`** (string)

---

## 7. Schedules

Schedules inject messages into a target instance either recurringly (cron)
or at a single future instant (one-shot). One-shots auto-disable after
firing or being detected as missed (daemon down through the instant).

Each row carries a `trigger` object:
- Cron: `{"kind": "cron", "expr": "0 9 * * *"}`
- One-shot: `{"kind": "once", "at": "2026-04-21T15:30:00+08:00"}`

Timezone detection is cross-platform (Linux, macOS, Windows) via the
`iana-time-zone` crate; supply an explicit `timezone` to override.

### `create_schedule`
- **`message`** (string)
- Exactly one of:
  - **`cron`** (string) — 5- or 6-field cron expression.
  - **`run_at`** (string) — ISO 8601 one-shot instant. Either with offset
    (`2026-04-21T15:30:00+08:00`) or naive local (`2026-04-21T15:30:00`)
    combined with `timezone`. Must resolve to the future.
- `target` (string), `label` (string), `timezone` (string, IANA name).

### `list_schedules`
- `target` (string) — optional filter.

### `update_schedule`
- **`id`** (string)
- Any of: `message`, `target`, `label`, `timezone`, `enabled` (boolean).
- Trigger change: supply **either** `cron` **or** `run_at`
  (mutually exclusive). Supplying either replaces the trigger kind.

### `delete_schedule`
- **`id`** (string)

---

## 8. Deployments

Templates from `fleet.yaml` spun up as a named deployment.

### `deploy_template`
- **`template`** (string), **`directory`** (string)
- `name` (string, defaults to template name), `branch` (string — each instance gets its own worktree)

### `teardown_deployment`
- **`name`** (string)

### `list_deployments`
No parameters.

---

## 9. CI Watchers

Poll GitHub Actions; on failure, the log is auto-injected into this agent.

### `watch_ci`
- **`repo`** (string, `owner/repo`)
- `branch` (string, default `main`), `interval_secs` (number, default 60)

### `unwatch_ci`
- **`repo`** (string)

---

## 10. Repo Sharing

### `checkout_repo`
Mount another repo as a read-only worktree in your working directory.
- **`source`** (string), `branch` (string)

### `release_repo`
- **`path`** (string)

---

## How the Tools Are Wired

1. `agend-terminal` daemon exposes a UDS API (`~/.agend/run/{PID}/api.sock`).
2. For each backend, `src/mcp_config.rs` generates the backend's MCP config file pointing at `agend-terminal mcp` (stdio transport).
3. `src/mcp/mod.rs` spawns a stdio server per agent, `src/mcp/tools.rs` serves schemas, `src/mcp/handlers.rs` proxies each call to the daemon API.
4. All tool calls flow: **agent → MCP stdio server → daemon UDS API → `ops.rs` → registry / inbox / fleet**.

## Schemas in Code

The authoritative source is `src/mcp/tools.rs`. If this doc drifts, regenerate by reading that file; the unit test `tool_count_at_least_35` guards against accidental removals.
