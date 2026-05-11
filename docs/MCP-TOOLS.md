# AgEnD MCP Tools Reference (29 tools)

## Action-based Tools

### `task`
Manage task board. Actions: create, list, claim, done, update.
- **action**: create / list / claim / done / update
- title, description, id, assignee, priority, status, branch, depends_on, filter_status, filter_assignee, result, due_at, duration

### `decision`
Manage decisions. Actions: post, list, update.
- **action**: post / list / update
- title, content, id, tags, scope, supersedes, archive, include_archived, ttl_days

### `team`
Manage teams. Actions: create, delete, list, update.
- **action**: create / delete / list / update
- name, members, orchestrator, description, source_repo, add, remove

### `schedule`
Manage schedules. Actions: create, list, update, delete.
- **action**: create / list / update / delete
- id, label, target, message, cron, run_at, timezone, enabled

### `deployment`
Manage deployments. Actions: deploy, teardown, list.
- **action**: deploy / teardown / list
- name, template, branch, directory

### `ci`
Manage CI watching. Actions: watch, unwatch, status.
- **action**: watch / unwatch / status
- repo, branch, interval_secs

### `repo`
Manage repo worktrees. Actions: checkout, release.
- **action**: checkout / release
- source, branch, path

### `health`
Manage health state. Actions: report, clear.
- **action**: report / clear
- reason (rate_limit / quota_exceeded / awaiting_operator), retry_after_secs, instance, note

## Communication

### `send`
Send a message to another instance or broadcast to multiple. Unified replacement for send_to_instance/delegate_task/report_result/request_information/broadcast.
- **message**: text content
- target_instance, targets, team, tags (routing)
- request_kind: query / task / report / update
- task_id (required for kind=task), success_criteria, branch, working_directory
- correlation_id, parent_id, thread_id
- force, force_reason, second_reviewer, second_reviewer_reason
- reviewed_head, artifacts

### `inbox`
Check pending messages, look up by ID, or fetch thread messages.
- message_id, thread_id, instance

### `reply`
Reply to the user via the active channel (NOT for inter-agent use).
- **text**: reply content
- default_action, timeout_secs

### `download_attachment`
Download a file attachment (telegram multimedia). Returns local path.
- **file_id**: attachment file ID

## Instance Lifecycle

### `create_instance`
Create agent instance(s). Supports homogeneous teams (count + backend) and heterogeneous teams (backends list).
- **name**: instance or team base name
- backend, model, args, branch, working_directory, task
- team, count, backends, layout, target_pane

### `delete_instance`
Stop and remove an instance.
- **name**: instance to delete

### `start_instance`
Start a stopped instance.
- **name**: instance to start

### `replace_instance`
Replace an instance with a fresh one.
- **name**: instance to replace
- reason

### `list_instances`
List all active agent instances. No parameters.

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
- **target**: instance name
- reason

### `move_pane`
Move an instance's pane into a different tab in the TUI.
- **agent**: instance to move
- **target_tab**: destination tab name
- split_dir (horizontal / vertical)

### `pane_snapshot`
Read visible text from a target instance's PTY scrollback (ANSI stripped).
- **target**: instance name
- lines (default 100, max 10000)

## Worktree & Binding

### `bind_self`
Bind the calling agent to a fresh worktree on the named branch. Rejects main/master (E4.5) and cross-agent conflicts.
- **branch**: branch to bind
- source_repo, repo (deprecated), rebase_mode

### `release_worktree`
Release the daemon-managed worktree and clear binding. Only removes worktrees with `.agend-managed` marker.
- **agent**: agent to release
- dry_run

### `force_release_worktree`
Force-release a stale daemon-managed worktree directory. Emergency recovery tool.
- **agent**: agent name
- **branch**: branch name

### `binding_state`
Report structured daemon-side bind state for an agent. Non-destructive introspection.
- **agent**: agent to inspect

### `gc_dry_run`
List Phase 4 GC candidates without deleting. Non-destructive.
- format (human / json)

## Daemon Operations

### `task_sweep_config`
Configure GitHub-PR auto-close sweep daemon.
- repo, dry_run, pause
