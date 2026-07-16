[繁體中文](MCP-TOOLS.zh-TW.md)

# AgEnD MCP Tools Reference (32 tools)

The daemon registry and live `tools/list` schema are authoritative. Role filtering can expose a subset of these 32 registered tools to an instance.

## Action-based Tools

### `task`

Manage task boards. Actions: `create`, `list`, `get`, `claim`, `done`, `update`, `sweep`, `health`, `activity`, `metadata_set`, `metadata_get`, `ack_plan`.

- Core fields include `id`/`task_id`, `title`, `description`, `assignee`, `priority`, `status`, `branch`, `depends_on`, `result`, `due_at`, `project`, and `scope`.
- `list` returns actionable tasks by default; use `include_history:true` to include done/cancelled tasks and filters such as `filter_status` or `filter_assignee` to narrow it.
- `list` is terse by default. Use `verbose:true` for full text or `fields:"minimal"` for only the compact identity/status projection.
- `get` returns one full record by `id` or `task_id`.
- Metadata and plan-ack actions operate on the durable task record; use the live schema for their required keys.

### `decision`

Manage durable decisions and operator questions. Actions: `post`, `list`, `update`, `answer`.

- Decision fields: `id`, `title`, `content`, `tags`, `scope`, `supersedes`, `archive`, `include_archived`, `ttl_days`.
- Questions use `needs_answer`, `options`, `allow_free_text`, `timeout_secs`, and `timeout_default`; `answer` records the selected/free-text response.

### `team`

Manage teams. Actions: `create`, `delete`, `list`, `update`.

- Fields: `name`, `members`, `orchestrator`, `description`, `repository_path`, `project_id`, `accept_from`, `add`, `remove`.
- `project_id` overrides project-board derivation; `accept_from` is the cross-team sender allowlist.

### `schedule`

Manage timed delivery. Actions: `create`, `list`, `update`, `delete`.

- Fields: `id`, `label`, `instance`, `message`, `cron`, `run_at`, `timezone`, `enabled`.
- `list` returns the newest three history entries and `runs_total` by default; use `full_history:true` for all retained entries, up to 50.
- `fire_strategy` is `always` or `until_success`; the latter requires `linked_task_id`.

### `deployment`

Manage batch deployments. Actions: `deploy`, `teardown`, `list`.

- Fields: `name`, `template`, `branch`, `directory`.

### `ci`

Manage CI watches. Actions: `watch`, `unwatch`, `status`.

- Fields: `repository`, `branch`, `interval_secs`, `next_after_ci`, `review_class`, `ci_provider`, `ci_provider_url`, `task_id`, `head_sha`.
- Use `repository` (GitHub `owner/repo`), not `repo`. `watch` may derive it from the caller's binding; `unwatch` requires it explicitly.
- Generic `main`/`master` watches are rejected. A protected-ref exact-head watch requires a full 40/64-hex `head_sha`, `task_id`, explicit `next_after_ci`, GitHub, and an authorized orchestrator/operator caller.

### `repo`

Manage repository worktrees, branch cleanup, and PR merge. Actions: `checkout`, `release`, `cleanup_init_commits`, `cleanup_merged_branches`, `merge`.

- Common fields include `repository_path`, `repository`, `branch`, `path`, `instance`, `bind`, `task_id`, `expected_head`, and `checkout_purpose`.
- `checkout bind:true` provisions and binds; `bind:false` creates an inspection worktree.
- `checkout_purpose:"disposable_review"` creates typed review provenance. It requires `bind:true`, a non-empty `task_id`, a full `expected_head`, and a branch proven new locally and on `origin`.
- `cleanup_merged_branches` is dry-run by default and requires `confirm_ids` plus `audit_reason` when applying.
- `merge` uses `pr`; `force:true` requires `force_reason` and is audited.

### `health`

Manage blocked health state. Actions: `report`, `clear`.

- `report` uses the caller identity and accepts `reason` (`rate_limit`, `quota_exceeded`, or `awaiting_operator`), optional `retry_after_secs`, and `note`.
- `clear` requires target `instance`; an optional `reason` limits which blocked reason is cleared.

## Communication

### `send`

Send to one instance or broadcast. This is the unified inter-agent messaging tool.

- Required: `message`. Route with one of `instance`, `instances`, `team`, or `tags`.
- `request_kind`: `query`, `task`, `report`, or `update`; typed reports should set `report_purpose`.
- Task fields include `task_id`, `success_criteria`, `context`, `branch`, `bind`, `worktree_binding_required`, `eta_minutes`, `reporting_cadence`, `expect_reply_within_secs`, and `next_after_ci`.
- Broadcast task dispatches require an existing `task_id`. The current single-target compatibility path can auto-create when it is omitted, but explicit `task action=create` plus `task_id` is the stable contract.
- Thread/correlation fields: `correlation_id`, `parent_id`, `thread_id`.
- Busy/review fields include `force`, `force_reason`, `second_reviewer`, `second_reviewer_reason`, `review_class`, plan-ack fields, typed review-assignment fields, `reviewed_head`, and `artifacts`.
- Report controls include `terminal`, `ack_inbox`, and `triaged`; fire-and-forget tasks can use `no_report_expected`.

### `inbox`

Drain or manage the calling instance's durable inbox.

- No arguments drains unread messages and marks them `delivering`; it does not yet mark them processed.
- `message_id` describes one message; `thread_id` fetches a thread. Optional `instance` scopes authorized lookups.
- `action:"ack"` confirms one delivering `message_id`, or the whole in-flight batch when the ID is omitted.
- `action:"clear"` compact-clears non-obligations while keeping unanswered queries/tasks unread and reporting them in `requires_response`.
- `action:"discharge"` requires `message_id` and non-empty `reason`; it closes a channel-reply obligation without answering and notifies the operator.
- Re-draining implicitly acknowledges the previous delivery batch; an unconfirmed batch can be reclaimed for redelivery after about ten minutes.

### `reply`

Reply to the user/operator through an external channel; do not use it for inter-agent messages.

- Required: `message`.
- `message_id` routes by the original inbox message's channel and settles that row after a successful send.
- Optional `task_id` and `correlation_id` preserve reply-to correlation.
- Pair `default_action` with `timeout_secs` to record a timed default decision.

### `download_attachment`

Download a Telegram multimedia attachment and return its local path.

- Required: `file_id`.

## Instance Lifecycle

### `create_instance`

Create one instance or a homogeneous/heterogeneous team.

- Fields include `name`, `backend`, `model`, `model_tier`, `args`, `working_directory`, `branch`, `task`, `role`, `env`, `topic_binding`, `team`, `count`, `backends`, `layout`, and `target_pane`.

### `delete_instance`

Stop and remove an instance.

- Required: `instance`. A creator-path delete of an instance with in-flight work additionally requires `force:true` and non-empty `force_reason`; the override is audited.

### `start_instance`

Start a stopped instance.

- Required: `instance`.

### `restart_instance`

Restart an instance.

- Required: `instance`; optional `mode` (`resume` or `fresh`), `reason`, and `force`.
- `resume` is the default and preserves backend conversation state.
- `fresh` starts clean and refuses a dirty bound worktree unless `force:true` is explicitly supplied.

### `set_model`

Persist exactly one model intent (`model` or `tier`) for an instance; setting one clears the other. `restart:true` applies it immediately, otherwise it takes effect on the next respawn.

- Required: `instance` and exactly one of `model`/`tier`.

### `bind_topic`

Create a deferred/eligible Telegram topic binding.

- Required: `instance`; optional `channel` currently defaults to `telegram`.
- Already-bound instances are an idempotent no-op; `skip` mode is not eligible.

### `list_instances`

List active instances, or pass `instance` for detail. Output is compact by default; `verbose:true` or `include_evidence:true` includes observed-status evidence. The response also exposes operator mode.

### `set_metadata`

Set display metadata for the caller. Actions: `display_name`, `description`.

- `display_name` uses `name`; `description` uses `description`.

### `set_waiting_on`

Declare the caller's current wait condition; send an empty `condition` to clear it.

### `interrupt`

Send ESC to a target PTY.

- Required: `instance`; optional `reason` and `snapshot`. Set `snapshot:true` to return a post-ESC diagnostic snapshot.

### `move_pane`

Move an instance pane to a TUI tab.

- Required: `instance`, `target_tab`; optional `split_dir` (`horizontal` or `vertical`).

### `pane_snapshot`

Read ANSI-stripped PTY scrollback.

- Required: `instance`; optional `lines`, `head`, and `to_file`.
- `to_file:true` stores the full capture under `$AGEND_HOME/captures/` and returns a compact response.

### `instance`

Read-only folded alias. Actions: `list`, `pane_snapshot`; semantics match the standalone tools above.

## Worktree & Binding

### `bind_self`

Recover or rebind the calling instance to a branch worktree. Prefer `repo action=checkout bind:true` for fresh work.

- Required: `branch`; optional `repository_path`, legacy mutually exclusive `repository`, `rebase_mode`, and `task_id`.
- Rejects protected branches and cross-agent lease conflicts. It does not silently create a CI continuation.

### `release_worktree`

Guardedly release the exact daemon-managed worktree and binding. The normal path preserves WIP and checks a fresh binding fingerprint; it is idempotent after success.

- Required: `instance`; optional `dry_run` and `force`.
- `force:true` additionally requires `branch`; `repository_path` is an optional cleanup hint. Markerless, opaque, ambiguous, or mismatched state is preserved.

### `binding_state`

Non-destructively report binding content, worktree/marker state, signature diagnostics, CI subscriptions, in-flight guard, and branch holders.

- Required: `instance`.

### `revoke_review_assignment`

Revoke a reviewer assignment by exact CAS identity. Authorized for the owning team orchestrator or operator; repeated revoke is idempotent.

- Required: `assignment_id`.

### `usage_limit_takeover`

Operator-only PREPARE step for a persisted usage-limit takeover episode. It writes the durable prepared journal but does not execute the takeover.

- Required: source `instance` and exact `episode_id`.

## Daemon Operations

### `config`

Read runtime configuration. Actions: `get`, `list`; MCP mutation is not supported.

- `get` requires `key`.
- Current keys: `dev_idle_threshold_secs`, `fleet_idle_threshold_secs`, `fleet_idle_ack_ttl_secs`, `hang_auto_recovery_enabled`, `usage_limit_propagation_enabled`, `idle_watchdog_enabled`, `show_pane_state`, `copy_on_select`, `dim_unfocused_panes`, `observed_badge`, `context_alert_pct`, `context_handoff_pct`, `context_handoff_escalate_pct`.
- Change a value with `agend-terminal admin config-set <KEY> <VALUE>`.

### `restart_daemon`

Request a graceful daemon restart. Parameters: none.

- Default standalone mode self-respawns a successor, waits for its health gate, then exits normally; no external supervisor is required.
- With `AGEND_RESTART_HANDOFF=0`, legacy mode exits with code 42 and requires an installed service supervisor or wrapper; it returns failure if none is detected.
- In Unix `agend-terminal app` mode, restart preflights and re-execs in place with the same PID. A successful preparation response is followed by the connection dropping during re-exec.
- Windows app mode remains fail-closed; quit and relaunch instead.
- A shared gate permits at most one restart in flight; a concurrent request is retryable.
