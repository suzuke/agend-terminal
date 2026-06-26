[繁體中文](MCP-TOOLS.zh-TW.md)

# AgEnD MCP Tools Reference (37 tools)

## Action-based Tools

### `task`
Manage task board. Actions: create, list, claim, done, update.
- **action**: create / list / claim / done / update
- title, description, id, assignee, priority, status, branch, depends_on, filter_status, filter_assignee, result, due_at, duration
- `list` is **terse by default** (#2475): `description` / `result` are length-capped (~200 chars). Pass `verbose: true` for full text; response carries `terse: true` when capping fired.

### `decision`
Manage decisions. Actions: post, list, update.
- **action**: post / list / update
- title, content, id, tags, scope, supersedes, archive, include_archived, ttl_days

### `team`
Manage teams. Actions: create, delete, list, update.
- **action**: create / delete / list / update
- name, members, orchestrator, description, repository_path, add, remove

### `schedule`
Manage schedules. Actions: create, list, update, delete.
- **action**: create / list / update / delete
- id, label, instance, message, cron, run_at, timezone, enabled

### `deployment`
Manage deployments. Actions: deploy, teardown, list.
- **action**: deploy / teardown / list
- name, template, branch, directory

### `ci`
Manage CI watching. Actions: watch, unwatch, status.
- **action**: watch / unwatch / status
- repository, branch, interval_secs

### `repo`
Manage repo worktrees. Actions: checkout, release.
- **action**: checkout / release
- repository_path, branch, path

### `health`
Manage health state. Actions: report, clear.
- **action**: report / clear
- reason (rate_limit / quota_exceeded / awaiting_operator), retry_after_secs, instance, note

## Communication

### `send`
Send a message to another instance or broadcast to multiple. Unified replacement for send_to_instance/delegate_task/report_result/request_information/broadcast.
- **message**: text content
- instance, instances, team, tags (routing)
- request_kind: query / task / report / update
- task_id (required for kind=task), success_criteria, branch, working_directory
- context, requires_reply, task_summary, correlation_id, parent_id, thread_id
- force, force_reason, second_reviewer, second_reviewer_reason
- reviewed_head, artifacts

### `inbox`
Check pending messages, look up by ID, or fetch thread messages.
- message_id, thread_id, instance

### `reply`
Reply to the user via the active channel (NOT for inter-agent use).
- **message**: reply content
- default_action, timeout_secs

### `download_attachment`
Download a file attachment (telegram multimedia). Returns local path.
- **file_id**: attachment file ID

## Instance Lifecycle

### `create_instance`
Create agent instance(s). Supports homogeneous teams (count + backend) and heterogeneous teams (backends list).
- **name**: instance or team base name
- backend, model, model_tier, args, branch, working_directory, task
- team, count, backends, layout, target_pane

### `delete_instance`
Stop and remove an instance.
- **instance**: instance to delete

### `start_instance`
Start a stopped instance.
- **instance**: instance to start

### `replace_instance`
Replace an instance with a fresh one.
- **instance**: instance to replace
- reason

### `restart_instance`
Kill and restart an instance. Default mode `resume` preserves conversation state; `fresh` starts clean (like `replace_instance`).
- **instance**: instance to restart
- mode (resume / fresh), reason, force
- `fresh` refuses by default if the bound worktree has uncommitted changes (#2476); commit/push or leave a task-board handoff first, or pass `force: true`.

### `list_instances`
List all active agent instances. Pass optional `instance` for detailed info on a single instance.
- **compact by default** (#2475): each row drops the noisy `observed_status.evidence` trail. Pass `verbose: true` (or `include_evidence: true`) to include it.

### `set_display_name`
Set your display name.
- **name**: new display name

### `set_description`
Set a description for this instance.
- **description**: instance description

### `set_waiting_on`
Declare what this instance is currently waiting for. Empty string to clear.
- **condition**: what you're waiting for

### `interrupt`
Send ESC to target agent's PTY to interrupt current LLM turn.
- **instance**: instance name
- reason

### `move_pane`
Move an instance's pane into a different tab in the TUI.
- **instance**: instance to move
- **target_tab**: destination tab name
- split_dir (horizontal / vertical)

### `pane_snapshot`
Read visible text from a target instance's PTY scrollback (ANSI stripped).
- **instance**: instance name
- lines (default 100, max 10000)
- `to_file: true` (#2478) writes the full snapshot under `$AGEND_HOME/captures/` and returns only a compact summary + path, keeping diagnostic dumps out of context.

### `tui_screenshot`
Capture the current TUI state as an SVG image. Only works in TUI mode (not daemon-only).
- **Parameters**: None.

## Worktree & Binding

### `bind_self`
Bind the calling agent to a fresh worktree on the named branch. Rejects main/master (E4.5) and cross-agent conflicts.
- **branch**: branch to bind
- repository_path, repository (deprecated), rebase_mode

### `release_worktree`
Release the daemon-managed worktree and clear binding. Only removes worktrees with `.agend-managed` marker.
- **instance**: instance to release
- dry_run

### `force_release_worktree`
Force-release a stale daemon-managed worktree directory. Emergency recovery tool.
- **instance**: instance name
- **branch**: branch name

### `binding_state`
Report structured daemon-side bind state for an agent. Non-destructive introspection.
- **instance**: instance to inspect

### `gc_dry_run`
List Phase 4 GC candidates without deleting. Non-destructive.
- format (human / json)

## Daemon Operations

### `task_sweep_config`
Configure GitHub-PR auto-close sweep daemon.
- repository, dry_run, pause

### `ephemeral`
Manage short-lived cross-backend ephemeral workers outside managed bookkeeping (no roster/binding/worktree). Actions: spawn, list, reap.
- **action**: spawn / list / reap
- backend, workflow_id, parent, ttl_secs, token_budget, prompt, model, worker_id, all_stale

### `watchdog`
Fleet idle watchdog control. Actions: snooze, resume, status, ack. `ack` suppresses fleet alerts until post-ack agent activity is detected, then auto-clears.
- **action**: snooze / resume / status / ack
- duration (e.g. `2h`, `30m`; clamped to max 4h)

### `config`
Runtime-mutable daemon configuration. Actions: get, set, list. (Available keys are derived from the daemon's runtime config and listed in the live tool description.)
- **action**: get / set / list
- key, value

### `tokens`
On-demand token usage + estimated USD cost from Claude Code + Codex session transcripts. Cost is an estimate; OpenCode/Kiro/Gemini are not yet covered.
- **action**: summary / by_instance
- group_by (instance / task), since (`24h` / `7d` / `90m` / `all`), instance

### `mode`
Read the operator availability/authority mode (read-only for agents). Setting the mode is operator-only via the `agend-terminal mode <active|away|sleep>` CLI.
- **action**: get

### `restart_daemon`

Request graceful daemon restart. Daemon exits with code 42; wrapper script restarts it. Idempotent.

**Note**: All agent PTY sessions will be interrupted. Persistent state (tasks, bindings, ci_watch) survives; in-flight inbox messages may be lost.

**Parameters**: None.
