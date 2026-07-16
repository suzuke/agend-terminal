[繁體中文](MCP-TOOLS.zh-TW.md)

# AgEnD MCP Tools Reference (32 tools)

## Action-based Tools

### `task`
Manage task board. Actions: create, list, get, claim, done, update.
- **action**: create / list / get / claim / done / update
- title, description, id, assignee, priority, status, branch, depends_on, filter_status, filter_assignee, result, due_at, duration, fields
- `list` is **terse by default** (#2475): `description` / `result` are length-capped (~200 chars). Pass `verbose: true` for full text; response carries `terse: true` when capping fired.
- `list` accepts `fields: "minimal"` (#2475) to project rows down to id/title/status/assignee/priority; response carries `fields: "minimal"|"full"`.
- `get` (#2475) returns ONE task's FULL record by `id` (alias `task_id`) — the companion to the terse `list` when you need one task's full text.

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

### `restart_instance`
Kill and restart an instance. Default mode `resume` preserves conversation state; `fresh` starts clean.
- **instance**: instance to restart
- mode (resume / fresh), reason, force
- `fresh` refuses by default if the bound worktree has uncommitted changes (#2476); commit/push or leave a task-board handoff first, or pass `force: true`.

### `set_model`
#2744: persist an instance's model intent to fleet.yaml. Exactly ONE of `model`/`tier`; setting one atomically clears the other (last-write-wins intent). Takes effect on the next respawn unless `restart: true`.
- **instance**: fleet instance whose entry to update
- model (concrete id/alias for the DECLARED backend), tier (symbolic `model_tiers` key), restart (default false)
- Shell/Raw/custom backends have no declared model capability → hard error; a pre-existing model flag in the entry's `args` is a hard conflict (no automatic argv rewriting; move payload after `--` or clean the args). A restart failure after a durable persist reports `persisted:true, restart_ok:false` — the intent still applies on the next respawn.

### `bind_topic`
#991: retrofit a Telegram topic for an instance spawned with `topic_binding=deferred` (or `auto` that ended up without one, e.g. spawned during the ~6s post-boot channel-init window).
- **instance**: instance to bind a topic for
- channel (defaults to `telegram` — the only channel currently supported)
- Idempotent: an instance that already has a topic returns `already_bound: true`, no-op.
- Refuses `skip`-mode instances (`code: not_eligible`) — `skip`'s promise is "no topic, ever"; change `topic_binding_mode` first if you want one.

### `list_instances`
List all active agent instances. Pass optional `instance` for detailed info on a single instance.
- **compact by default** (#2475): each row drops the noisy `observed_status.evidence` trail. Pass `verbose: true` (or `include_evidence: true`) to include it.
- **operator_mode** (#2548): the response also carries a top-level `operator_mode: {mode, delegate_to, delegate_scope}` field — the retired `mode` tool's read side folded in here, so agents can observe operator availability alongside fleet state. Setting the mode stays CLI-only (`agend-terminal mode <active|away|sleep>`).
- **topic_binding_mode** (#991): each row carries `topic_binding_mode` when the instance was spawned with `topic_binding: skip`/`deferred` — omitted for `auto` (the default), so an operator can grep the fleet for intentionally topic-less agents.

### `set_metadata`
Set per-instance display metadata. #2547: merged from the former standalone `set_display_name` / `set_description` tools.
- **action**: display_name / description
- action=display_name: **name** — new display name
- action=description: **description** — instance description

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

### `instance`
#2550: folded **read-only** alias for the per-name instance tools. Read-only only — the standalone `list_instances` / `pane_snapshot` tools remain available unchanged, and structural lifecycle (create/delete/start/restart/move_pane) stays on its own tools.
- **action**: list / pane_snapshot
- action=list ≡ `list_instances` (optional `instance` for one instance's detail; `verbose` / `include_evidence` for the full evidence trail)
- action=pane_snapshot ≡ `pane_snapshot` (`instance` required; `lines`, `to_file`, `head`)

## Worktree & Binding

### `bind_self`
Bind the calling agent to a fresh worktree on the named branch. Rejects main/master (E4.5) and cross-agent conflicts.
- **branch**: branch to bind
- repository_path, repository (deprecated), rebase_mode

### `release_worktree`
Release the daemon-managed worktree and clear binding. Only removes worktrees with `.agend-managed` marker. #2548: `force:true` absorbs the former standalone `force_release_worktree` tool — cleans a stale worktree directory directly (no marker check, requires `branch`), for emergency recovery when a directory survives after its binding is already gone.
- **instance**: instance to release
- dry_run, force, branch (required when force:true), repository_path

### `binding_state`
Report structured daemon-side bind state for an agent. Non-destructive introspection.
- **instance**: instance to inspect

### `revoke_review_assignment`
Revoke a specific reviewer assignment by exact `assignment_id`. Authorization: team orchestrator or operator. Idempotent — repeated calls with a stale/missing assignment_id return success. After successful revoke, merge readiness is recomputed.
- **assignment_id**: UUID of the assignment to revoke (exact CAS identity)

### `usage_limit_takeover`
Architecture-14 item 5 Slice 2A operator-only PREPARE seam. Validates the persisted `CandidateReady` episode and writes one durable `Prepared` journal; it does not execute takeover or mutate the source binding/task/process.
- **instance**: source instance whose persisted usage-limit episode is being prepared
- **episode_id**: exact persisted episode id; the candidate is derived from `CandidateReady` and cannot be supplied by the caller

## Daemon Operations

### `config`
Runtime-mutable daemon configuration. Actions: get, list. #2548: the set action moved to the `agend-terminal admin config-set` CLI (zero MCP calls in 20 days). (Available keys are derived from the daemon's runtime config and listed in the live tool description.)
- **action**: get / list
- key (required for get)

### `restart_daemon`

Request graceful daemon restart. Daemon exits with code 42; wrapper script restarts it. Idempotent.

**Note**: All agent PTY sessions will be interrupted. Persistent state (tasks, bindings, ci_watch) survives; in-flight inbox messages may be lost.

**Parameters**: None.
