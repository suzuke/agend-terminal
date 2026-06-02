//! MCP tool definitions — JSON schemas for all exposed tools.

use serde_json::{json, Value};

pub fn tool_definitions() -> Value {
    let tools: Vec<Value> = crate::mcp::registry::all()
        .iter()
        .map(|entry| (entry.definition)())
        .collect();
    json!({"tools": tools})
}

pub(crate) fn def_reply() -> Value {
    json!({"name": "reply", "description": "Reply to the user via the active channel. Requires daemon API. Sprint 59 Wave 1 PR-4 ((B) decision default with timeout): when both `default_action` and `timeout_secs` are set, the daemon records a pending operator decision sidecar and auto-fires the default after the timeout window. Subsequent reply calls without `default_action` resolve the pending decision (operator override / explicit answer arrived).",
        "inputSchema": {"type": "object", "properties": {
            "message": {"type": "string", "description": "The reply text to send to the user."},
            "default_action": {"type": "string", "description": "Action to auto-execute on timeout when the operator doesn't reply within `timeout_secs`. e.g. 'proceed-with-lean' / 'abort'. Pair with `timeout_secs` (Sprint 59 Wave 1 PR-4)."},
            "timeout_secs": {"type": "integer", "description": "Seconds to wait for an operator response before firing `default_action`. Required when `default_action` is set; ignored otherwise (Sprint 59 Wave 1 PR-4)."}
        }, "required": ["message"]}})
}

pub(crate) fn def_download_attachment() -> Value {
    json!({"name": "download_attachment", "description": "Download a file attachment (telegram multimedia: images, audio, documents). Returns local path. Requires daemon API.",
        "inputSchema": {"type": "object", "properties": {"file_id": {"type": "string"}}, "required": ["file_id"]}})
}

pub(crate) fn def_send() -> Value {
    json!({"name": "send", "description": "Send a message to another instance or broadcast to multiple. Replaces send_to_instance/delegate_task/report_result/request_information/broadcast. Sprint 58 Wave 4 PR-1: kind=task dispatches MUST include task_id (call task action=create first to obtain a 't-...' id).",
        "inputSchema": {"type": "object", "properties": {
            "instance": {"type": "string", "description": "Name of the existing instance to send to (single recipient)"},
            "instances": {"type": "array", "items": {"type": "string"}, "description": "Names of existing instances to broadcast to (broadcast mode)"},
            "team": {"type": "string", "description": "Team name (broadcast to team)"},
            "tags": {"type": "array", "items": {"type": "string"}, "description": "Tags filter (broadcast mode)"},
            "message": {"type": "string", "description": "Message text (or 'task' for delegate, 'summary' for report, 'question' for query)"},
            "request_kind": {"type": "string", "enum": ["query", "task", "report", "update"], "description": "Message kind (determines behavior). NOTE: kind=task requires task_id (Sprint 58 Wave 4 PR-1 anti-stall contract)."},
            "requires_reply": {"type": "boolean"},
            "task_summary": {"type": "string"},
            "success_criteria": {"type": "string", "description": "For task delegation"},
            "context": {"type": "string"},
            "task_id": {"type": "string", "description": "Task board ID for correlation. REQUIRED when request_kind=task — caller must obtain via `task action=create` and reference the resulting `t-...` id, closing the Wave 3 PR-1 dispatch protocol gap."},
            "correlation_id": {"type": "string"},
            "parent_id": {"type": "string"},
            "thread_id": {"type": "string"},
            "force": {"type": "boolean", "description": "Override busy gate (requires force_reason)"},
            "force_reason": {"type": "string"},
            "second_reviewer": {"type": "boolean", "description": "Signal dual review (§3.5)"},
            "second_reviewer_reason": {"type": "string"},
            "reviewed_head": {"type": "string", "description": "Git HEAD SHA at time of review"},
            "artifacts": {"type": "string"},
            "branch": {"type": "string"},
            "working_directory": {"type": "string"},
            "sequencing": {"type": "string", "enum": ["parallel", "sequential", "sequential-merge-only"], "description": "Task execution order constraint"},
            "eta_minutes": {"type": "integer", "description": "Expected completion time in minutes"},
            "reporting_cadence": {"type": "string", "enum": ["per-pr", "wave-end", "both"], "description": "When implementer should report back"},
            "worktree_binding_required": {"type": "boolean", "description": "Whether target must bind to a worktree before starting"},
            "expect_reply_within_secs": {"type": "integer", "description": "Opt-in dispatch-idle watchdog (PR1). When set on a kind=task/query send, the daemon records a pending-dispatch sidecar and fires `dispatch_idle_threshold_exceeded` to the dispatcher's inbox if the matching kind=report (correlation_id-keyed) hasn't arrived within this many seconds. Default unset = no tracking (cross-team-safe). Fixup-team dispatches inherit a 10-min default automatically; other teams must opt in explicitly."},
            "next_after_ci": {"type": "string", "description": "#931 Fix 2 (H5a): when dispatching kind=task with a `branch`, set this to the agent that should receive `[ci-ready-for-action]` after CI passes on that branch. The daemon's auto-armed ci-watch carries the chain target so the handoff fires without a manual follow-up `ci action=watch next_after_ci=…`. Example: lead dispatches dev with `next_after_ci=reviewer` — reviewer is auto-notified when dev's PR goes green."},
            "terminal": {"type": "boolean", "description": "Set true on kind=report to signal task completion. When correlation_id matches a task and reporter is the assignee, the task is auto-closed. Default false — progress reports and review verdicts do not trigger auto-close."}
        }, "required": ["message"]}})
}

pub(crate) fn def_inbox() -> Value {
    json!({"name": "inbox", "description": "Check pending messages, OR look up a single message by ID, OR fetch a thread's messages. No params = drain pending. With message_id = describe message status (read/unread/expired/notfound). With thread_id = fetch all messages in thread ordered by timestamp.",
    "inputSchema": {"type": "object", "properties": {
        "message_id": {"type": "string", "description": "Look up message status by ID"},
        "thread_id": {"type": "string", "description": "Fetch all messages in a thread"},
        "instance": {"type": "string", "description": "For message_id: target instance (defaults to caller). For thread_id: filter to a specific instance's inbox (optional, scans all if omitted)"}
    }}})
}

pub(crate) fn def_list_instances() -> Value {
    json!({"name": "list_instances", "description": "List all active agent instances. Pass optional `instance` for detailed info on a single instance.",
        "inputSchema": {"type": "object", "properties": {"instance": {"type": "string", "description": "Optional: name of an existing instance for detailed info"}}}})
}

pub(crate) fn def_create_instance() -> Value {
    json!({"name": "create_instance", "description": "Create agent instance(s). Team modes: (a) homogeneous — count:3, backend:\"claude\", team:\"dev\" → dev-1..dev-3 all claude; (b) heterogeneous — backends:[\"codex\",\"kiro-cli\",\"gemini\"], team:\"mixed\" → mixed-1=codex, mixed-2=kiro-cli, mixed-3=gemini, all grouped in one tab.",
    "inputSchema": {"type": "object", "properties": {
        "name": {"type": "string", "description": "Instance name (single instance) or base name (ignored when team is set — team name is used as prefix)"},
        "backend": {"type": "string", "description": "Backend CLI name: claude, gemini, kiro-cli, codex, opencode"},
        "args": {"type": "string", "description": "Extra CLI arguments"},
        "model": {"type": "string", "description": "Model override (e.g. --model flag)"},
        "working_directory": {"type": "string"},
        "branch": {"type": "string", "description": "Git branch — creates worktree if specified"},
        "task": {"type": "string", "description": "Initial task to inject after spawn"},
        "layout": {"type": "string", "enum": ["tab", "split-right", "split-below"], "description": "TUI layout: tab (default), split-right, or split-below. Places the new pane relative to `target_pane` if given, otherwise relative to the caller."},
        "target_pane": {"type": "string", "description": "Name of an existing instance. When set with layout=split-right/split-below, the new pane is attached next to that instance's pane (wherever it currently lives), instead of the caller's focused pane. Falls back to caller, then new tab, if the target isn't currently displayed."},
        "count": {"type": "integer", "description": "Number of instances to spawn (requires team; ignored when `backends` is set)"},
        "team": {"type": "string", "description": "Team name — members become <team>-1, <team>-2, ... grouped in one tab"},
        "backends": {"type": "array", "items": {"type": "string"}, "description": "Per-member backend list for a mixed-backend team (requires team). Length dictates member count."},
        "command": {"type": "string", "description": "Deprecated: use 'backend' instead"},
        "role": {"type": "string", "description": "Agent role label (e.g. `reviewer`) recorded on the spawned instance's fleet.yaml entry."},
        "env": {"type": "object", "description": "#900: environment variables (object of string→string) injected into the spawned backend process and persisted to the fleet.yaml entry so replace/restart flows re-apply them."},
        "topic_binding": {"type": "string", "description": "Telegram topic binding for the spawned agent, forwarded to the spawn RPC."}
    }}})
}

pub(crate) fn def_delete_instance() -> Value {
    json!({"name": "delete_instance", "description": "Stop and remove an instance.",
        "inputSchema": {"type": "object", "properties": {"instance": {"type": "string", "description": "Name of the existing instance to remove"}}, "required": ["instance"]}})
}

pub(crate) fn def_start_instance() -> Value {
    json!({"name": "start_instance", "description": "Start a stopped instance.",
        "inputSchema": {"type": "object", "properties": {"instance": {"type": "string", "description": "Name of the existing instance to start"}}, "required": ["instance"]}})
}

pub(crate) fn def_replace_instance() -> Value {
    json!({"name": "replace_instance", "description": "Replace an instance with a fresh one.",
        "inputSchema": {"type": "object", "properties": {"instance": {"type": "string", "description": "Name of the existing instance to replace"}, "reason": {"type": "string"}}, "required": ["instance"]}})
}

pub(crate) fn def_restart_instance() -> Value {
    json!({"name": "restart_instance", "description": "Kill and restart an instance. Default mode 'resume' preserves conversation state; 'fresh' starts clean (like replace_instance).",
        "inputSchema": {"type": "object", "properties": {
            "instance": {"type": "string", "description": "Name of the existing instance to restart"},
            "mode": {"type": "string", "enum": ["resume", "fresh"], "default": "resume", "description": "resume = keep conversation (--continue/--resume); fresh = clean start"},
            "reason": {"type": "string"}
        }, "required": ["instance"]}})
}

pub(crate) fn def_tokens() -> Value {
    json!({"name": "tokens", "description": "On-demand token usage + estimated USD cost from Claude Code + Codex session transcripts. Scans ~/.claude/projects/*.jsonl and ~/.codex/sessions/rollout-*.jsonl at query time, dedups streaming-duplicated rows by message id, and attributes usage to fleet instances by transcript cwd (workspace + worktree paths). action=summary → fleet totals + per-instance table; action=by_instance (requires `instance`) → that instance's per-model breakdown. group_by=task (#1077 slice-1) time-joins each message to whichever task the instance had active at the message's timestamp, with a (no active task) bucket — this is TIME-WINDOW ATTRIBUTION, NOT per-task billing. Cost is an ESTIMATE pending operator pricing calibration; excludes the >200k long-context surcharge tier; OpenCode/Kiro/Gemini are not yet covered.",
    "inputSchema": {"type": "object", "properties": {
        "action": {"type": "string", "enum": ["summary", "by_instance"], "default": "summary"},
        "group_by": {"type": "string", "enum": ["instance", "task"], "default": "instance", "description": "instance (default) → per-instance/per-model; task → per-instance/per-task time-join (#1077). Default is backward-compatible."},
        "since": {"type": "string", "description": "Lookback window: \"24h\" (default) / \"7d\" / \"90m\" / \"all\""},
        "instance": {"type": "string", "description": "Required for action=by_instance; optional filter for action=summary"}
    }}})
}

pub(crate) fn def_interrupt() -> Value {
    json!({"name": "interrupt", "description": "Send ESC byte to target agent's PTY to interrupt current LLM turn. Context preserved, agent accepts next prompt.",
        "inputSchema": {"type": "object", "properties": {
            "instance": {"type": "string", "description": "Name of the existing instance to interrupt"},
            "reason": {"type": "string", "description": "Optional follow-up message to inject after ESC"},
            "snapshot": {"type": "boolean", "default": false, "description": "When true, return a pane snapshot after ESC for closed-loop verification"}
        }, "required": ["instance"]}})
}

pub(crate) fn def_set_display_name() -> Value {
    json!({"name": "set_display_name", "description": "Set your display name. Empty string (or omitting `name`) clears it — the pane falls back to the agent name.",
        "inputSchema": {"type": "object", "properties": {"name": {"type": "string", "description": "Display name. Empty string to clear (reset to the agent name)."}}}})
}

pub(crate) fn def_set_description() -> Value {
    json!({"name": "set_description", "description": "Set a description for this instance. Empty string (or omitting `description`) clears it.",
        "inputSchema": {"type": "object", "properties": {"description": {"type": "string", "description": "Description. Empty string to clear."}}}})
}

pub(crate) fn def_set_waiting_on() -> Value {
    json!({"name": "set_waiting_on", "description": "Declare what this instance is currently waiting for. Set to empty string to clear. Automatically cleared when stale.",
        "inputSchema": {"type": "object", "properties": {"condition": {"type": "string", "description": "What you are waiting for, e.g. 'review from at-dev-4'. Empty string to clear."}}}})
}

pub(crate) fn def_move_pane() -> Value {
    json!({"name": "move_pane", "description": "Move an instance's pane into a different tab in the TUI. If `target_tab` names an existing tab, the pane splits that tab's focused pane; otherwise a new tab with that name is created and the pane becomes its root. Preserves scrollback and PTY state (unlike delete + create).",
        "inputSchema": {"type": "object", "properties": {
            "instance": {"type": "string", "description": "Name of the existing instance whose pane should be moved."},
            "target_tab": {"type": "string", "description": "Destination tab name. Created if not present."},
            "split_dir": {"type": "string", "enum": ["horizontal", "vertical"], "description": "Split direction when the destination tab already exists. Default: horizontal. Ignored when a new tab is created."}
        }, "required": ["instance", "target_tab"]}})
}

pub(crate) fn def_pane_snapshot() -> Value {
    json!({"name": "pane_snapshot", "description": "Read visible text from a target instance's PTY scrollback. Returns plain text (ANSI stripped). Default 100 lines, max 10000.",
        "inputSchema": {"type": "object", "properties": {
            "instance": {"type": "string", "description": "Name of the existing instance to snapshot"},
            "lines": {"type": "integer", "description": "Number of lines to return (default 100, max 10000)"}
        }, "required": ["instance"]}})
}

pub(crate) fn def_tui_screenshot() -> Value {
    json!({"name": "tui_screenshot", "description": "Capture the current TUI state as an SVG image. Only works in TUI mode (not daemon-only). Returns SVG string.",
        "inputSchema": {"type": "object", "properties": {}}})
}

pub(crate) fn def_decision() -> Value {
    json!({"name": "decision", "description": "Manage decisions. Actions: post, list, update.",
        "inputSchema": {"type": "object", "properties": {
            "action": {"type": "string", "enum": ["post", "list", "update"]},
            "title": {"type": "string"}, "content": {"type": "string"},
            "scope": {"type": "string", "enum": ["project", "fleet"]},
            "tags": {"type": "array", "items": {"type": "string"}},
            "ttl_days": {"type": "number"}, "supersedes": {"type": "string"},
            "id": {"type": "string"}, "archive": {"type": "boolean"},
            "include_archived": {"type": "boolean"}
        }, "required": ["action"]}})
}

pub(crate) fn def_task() -> Value {
    json!({"name": "task", "description": "Manage task board. Actions: create, list, claim, done, update, sweep, health, activity, metadata_set, metadata_get. #806: default list trims to actionable statuses (open/claimed/in_progress/blocked); pass include_history=true to surface done/cancelled. `sweep` is operator-triggered manual hygiene (4 stale-task categories with dry-run + confirm_ids round-trip). #830: `health` is a one-shot board-hygiene snapshot — totals + by_status + ghost_owners + stale_claims + age aggregates + recommendations array.",
        "inputSchema": {"type": "object", "properties": {
            "action": {"type": "string", "enum": ["create", "list", "claim", "done", "update", "sweep", "health", "activity", "metadata_set", "metadata_get"]},
            "title": {"type": "string"}, "description": {"type": "string"},
            "priority": {"type": "string", "enum": ["low", "normal", "high", "urgent"]},
            "assignee": {"type": "string"}, "depends_on": {"type": "array", "items": {"type": "string"}}, "parent_id": {"type": "string", "description": "Parent task ID for subtask composition (A is composed of B,C,D). Complementary to depends_on (execution order)."},
            "id": {"type": "string"}, "result": {"type": "string"},
            "status": {"type": "string", "enum": ["backlog", "open", "claimed", "in_progress", "in_review", "blocked", "verified", "done", "cancelled"]},
            "filter_assignee": {"type": "string"}, "filter_status": {"type": "string"}, "filter_tag": {"type": "string", "description": "Filter by tag"}, "tags": {"type": "array", "items": {"type": "string"}, "description": "Task tags (for create/update)"},
            "include_history": {"type": "boolean", "description": "#806: opt in to done/cancelled in `list` response (default trims to actionable)."},
            "limit": {"type": "integer", "description": "#806: cap `list` response to N newest-first entries (sort by updated_at desc)."},
            "due_at": {"type": "string", "description": "ISO 8601 deadline for the task"},
            "branch": {"type": "string", "description": "Git branch the implementer should work on"},
            "force": {"type": "boolean", "description": "#808: bypass ownership ACL on done/update for historical ghost-owned cleanup. Requires non-empty force_reason."},
            "force_reason": {"type": "string", "description": "#808: required when force=true. Logged to event-log.jsonl and embedded in the per-task event's reason field for audit."},
            "apply": {"type": "boolean", "description": "#806 sweep: when false (default), returns dry-run plan; when true, emits Cancelled for the confirm_ids subset."},
            "confirm_ids": {"type": "array", "items": {"type": "string"}, "description": "#806 sweep apply=true: subset of candidate_ids from prior dry-run to actually cancel."},
            "audit_reason": {"type": "string", "description": "#806 sweep apply=true: required audit text recorded in event-log.jsonl + per-task Cancelled.reason."},
            "repository": {"type": "string", "description": "#806 sweep: override GitHub `owner/repo` slug for PR-state queries (defaults to task_sweep.json's repo)."},
            "metadata_key": {"type": "string", "description": "Key for metadata_set action."},
            "metadata_value": {"description": "Value for metadata_set action (any JSON type)."}
        }, "required": ["action"]}})
}

pub(crate) fn def_task_sweep_config() -> Value {
    json!({"name": "task_sweep_config",
    "description": "Configure GitHub-PR auto-close sweep daemon. Sweep polls merged PRs and emits Done events for `Closes t-XXX-N` markers (validated by 5-must-have pipeline).",
    "inputSchema": {"type": "object", "properties": {
        "repository": {"type": "string", "description": "GitHub `owner/repo` slug to sweep (empty string disables)"},
        "pause": {"type": "boolean", "description": "Pause/resume the sweep tick"},
        "dry_run": {"type": "boolean", "description": "Log decisions without emitting events"},
        "api_base_url": {"type": "string", "description": "REST API base URL for self-hosted GitHub Enterprise (e.g. `https://ghe.example.com/api/v3`). Empty string resets to the default `https://api.github.com`."}
    }}})
}

pub(crate) fn def_restart_daemon() -> Value {
    json!({"name": "restart_daemon", "description": "Request graceful daemon restart. Daemon exits with code 42 after shutdown; a supervisor (launchd/systemd/Task Scheduler from `agend-terminal service install`, or `scripts/agend-wrapper.sh` for manual mode) respawns it. Returns ok:false when no supervisor is detected (bare `agend-terminal start`) — operator must install a supervisor before retry. Idempotent.",
        "inputSchema": {"type": "object", "properties": {}}})
}

pub(crate) fn def_team() -> Value {
    json!({"name": "team", "description": "Manage teams. Actions: create, delete, list, update.",
        "inputSchema": {"type": "object", "properties": {
            "action": {"type": "string", "enum": ["create", "delete", "list", "update"]},
            "name": {"type": "string"}, "members": {"type": "array", "items": {"type": "string"}},
            "orchestrator": {"type": "string", "description": "Team orchestrator — must be a member."},
            "description": {"type": "string"},
            "repository_path": {"type": "string", "description": "Local filesystem path to the source repository for the team."},
            "accept_from": {"type": "array", "items": {"type": "string"}, "description": "External agent names allowed to send directly to this team's orchestrator (cross-team allowlist). Empty = deny all cross-team sends (default)."},
            "add": {"type": "array", "items": {"type": "string"}},
            "remove": {"type": "array", "items": {"type": "string"}}
        }, "required": ["action"]}})
}

pub(crate) fn def_schedule() -> Value {
    json!({"name": "schedule", "description": "Manage schedules. Actions: create, list, update, delete.",
        "inputSchema": {"type": "object", "properties": {
            "action": {"type": "string", "enum": ["create", "list", "update", "delete"]},
            "id": {"type": "string"},
            "cron": {"type": "string", "description": "5- or 6-field cron expression (recurring). 5-field layout: `min hour day-of-month month day-of-week`; 6-field prepends seconds. Day-of-week uses Quartz convention: 1=Sun, 2=Mon, 3=Tue, 4=Wed, 5=Thu, 6=Fri, 7=Sat (NOT Unix 0-6). Example: every Wed+Sat at 15:00 → `0 15 * * 4,7`."},
            "run_at": {"type": "string", "description": "ISO 8601 one-shot instant."},
            "message": {"type": "string"}, "instance": {"type": "string", "description": "Name of the existing instance to deliver the scheduled message to."},
            "label": {"type": "string"},
            "timezone": {"type": "string", "description": "IANA zone name."},
            "enabled": {"type": "boolean"},
            "fire_strategy": {"type": "string", "enum": ["always", "until_success"], "description": "Default always (fire every cron match). until_success: skip remaining same-day fires once linked_task_id completes (done); resumes next calendar day. Requires linked_task_id when until_success. See #1521."},
            "linked_task_id": {"type": "string", "description": "Task ID whose done status (today) suppresses further fires under until_success. Missing task disables the schedule with target_task_missing. See #1521."}
        }, "required": ["action"]}})
}

pub(crate) fn def_deployment() -> Value {
    json!({"name": "deployment", "description": "Manage deployments. Actions: deploy, teardown, list.",
        "inputSchema": {"type": "object", "properties": {
            "action": {"type": "string", "enum": ["deploy", "teardown", "list"]},
            "template": {"type": "string", "description": "Template name from fleet.yaml"},
            "directory": {"type": "string", "description": "Working directory for instances"},
            "name": {"type": "string", "description": "Deployment name"},
            "branch": {"type": "string", "description": "Git branch — each instance gets its own worktree"}
        }, "required": ["action"]}})
}

pub(crate) fn def_ci() -> Value {
    json!({"name": "ci", "description": "Manage CI watching. Actions: watch, unwatch, status.",
        "inputSchema": {"type": "object", "properties": {
            "action": {"type": "string", "enum": ["watch", "unwatch", "status"]},
            "repository": {"type": "string", "description": "GitHub `owner/repo` slug. Required for watch/unwatch; optional filter for status."},
            "branch": {"type": "string", "description": "Branch to watch (default: main); optional filter for status."},
            "interval_secs": {"type": "number", "description": "Poll interval in seconds (default: 60)"},
            "next_after_ci": {"type": "string", "description": "Instance to auto-notify when CI passes. Daemon sends [ci-ready-for-action] to this target."},
            "review_class": {"type": "string", "enum": ["single", "dual"], "description": "#972: review threshold for the daemon's PR-state aggregator. `single` (default) — §3.6 one VERIFIED unlocks the merge gate. `dual` — §3.5 two distinct VERIFIED required before `[pr-ready-for-merge]` fires."},
            "ci_provider": {"type": "string", "description": "watch: CI provider override — `github` (default) or `bitbucket_cloud`. `bitbucket_server` is rejected (not yet supported). Persisted on the watch sidecar."},
            "ci_provider_url": {"type": "string", "description": "watch: base URL for a self-hosted CI provider, persisted on the watch sidecar alongside `ci_provider`."}
        }, "required": ["action"]}})
}

pub(crate) fn def_watchdog() -> Value {
    json!({"name": "watchdog", "description": "#1084/#1076: Fleet idle watchdog control. Actions: snooze, resume, status, ack. `ack` suppresses fleet alerts until post-ack agent activity is detected, then auto-clears.",
        "inputSchema": {"type": "object", "properties": {
            "action": {"type": "string", "enum": ["snooze", "resume", "status", "ack"]},
            "duration": {"type": "string", "description": "Snooze duration (e.g. '2h', '30m', '1h30m'). Clamped to max 4h. Default: 1h."}
        }, "required": ["action"]}})
}

pub(crate) fn def_config() -> Value {
    json!({"name": "config", "description": "#1085: Runtime-mutable daemon configuration. Actions: get, set, list. Keys: dev_idle_threshold_secs, fleet_idle_threshold_secs, hang_auto_recovery_enabled.",
    "inputSchema": {"type": "object", "properties": {
        "action": {"type": "string", "enum": ["get", "set", "list"]},
        "key": {"type": "string", "description": "Config key name (required for get/set)"},
        "value": {"type": "string", "description": "New value (required for set)"}
    }, "required": ["action"]}})
}

pub(crate) fn def_mode() -> Value {
    json!({"name": "mode", "description": "#1339: Read the operator availability/authority mode (READ-ONLY for agents). `get` → current mode (active|away|sleep) + delegate. Agents observe this to back off when the operator is away/asleep. SETTING the mode is operator-only via the `agend-terminal mode <active|away|sleep>` CLI — never available to agents.",
    "inputSchema": {"type": "object", "properties": {
        "action": {"type": "string", "enum": ["get"], "default": "get"}
    }}})
}

pub(crate) fn def_health() -> Value {
    json!({"name": "health", "description": "Manage health state. Actions: report, clear.",
        "inputSchema": {"type": "object", "properties": {
            "action": {"type": "string", "enum": ["report", "clear"]},
            "reason": {"type": "string", "enum": ["rate_limit", "quota_exceeded", "awaiting_operator"], "description": "Blocked reason kind (for report)"},
            "retry_after_secs": {"type": "integer", "description": "Seconds until retry (for rate_limit)"},
            "note": {"type": "string", "description": "Optional human-readable note"},
            "instance": {"type": "string", "description": "Target instance name (for clear)"}
        }, "required": ["action"]}})
}

pub(crate) fn def_repo() -> Value {
    json!({"name": "repo", "description": "Manage repo worktrees. Actions: checkout, release, cleanup_init_commits, cleanup_merged_branches, merge. #817: cleanup_merged_branches is operator-triggered local-branch hygiene (4 categories: clean_merged/squash_merged/stale_idle/active_unknown; dry-run by default + confirm_ids + audit_reason required for apply).",
        "inputSchema": {"type": "object", "properties": {
            "action": {"type": "string", "enum": ["checkout", "release", "cleanup_init_commits", "cleanup_merged_branches", "merge"]},
            "pr": {"type": "integer", "description": "PR number for merge action."},
            "repository": {"type": "string", "description": "merge: GitHub `owner/repo` slug to merge the PR in (defaults to the canonical repo). Distinct from `repository_path` (a local checkout path)."},
            "repository_path": {"type": "string", "description": "checkout: local filesystem path to the source repository. Standard cross-tool name (matches bind_self / team update)."},
            "branch": {"type": "string"},
            "path": {"type": "string"},
            "instance": {"type": "string", "description": "#789: name of the existing instance to target for cleanup_init_commits (defaults to caller's instance_name). Cleans empty `init` commits accumulated in the agent's bound worktree by backend session-checkpoint heartbeats. Returns {cleaned_count, [skipped_reason]}. Idempotent — call before push to scrub PR history."},
            "bind": {"type": "boolean", "description": "#778 Option 1: when true on checkout, atomically bind the caller to the just-provisioned worktree (writes binding.json + .agend-managed marker + arms ci_watches) and lands HEAD on the named branch instead of a detached commit. Default false preserves back-compat for inspection-only callers (review pool, operator triage)."},
            "base": {"type": "string", "description": "#817 cleanup_merged_branches: branch to compare against for clean/squash merge detection (default 'main')."},
            "min_age_days": {"type": "integer", "description": "#817 cleanup_merged_branches: stale_idle threshold in days (default 90)."},
            "apply": {"type": "boolean", "description": "#817 cleanup_merged_branches: when false (default), returns dry-run plan; when true, deletes confirm_ids subset."},
            "confirm_ids": {"type": "array", "items": {"type": "string"}, "description": "#817 cleanup_merged_branches apply=true: subset of candidate_ids from prior dry-run to actually delete via `git branch -D`."},
            "audit_reason": {"type": "string", "description": "#817 cleanup_merged_branches apply=true: required audit text recorded in event-log.jsonl per deleted branch with source SHA for restore."},
            "from_ref": {"type": "string", "description": "checkout bind:true: base ref to auto-create `branch` from when it doesn't exist locally (default `origin/main`)."}
        }, "required": ["action"]}})
}

pub(crate) fn def_bind_self() -> Value {
    json!({"name": "bind_self", "description": "Bind the calling agent to a worktree on the named branch. For fresh-task workflows that know the source repo, prefer `repo action=checkout bind:true` (#779 Option 1) — single-step atomic provision + bind. Use `bind_self` when the caller is mid-lifecycle: (a) re-binding a recovered worktree via `rebase_mode=true`, (b) binding via fleet.yaml-resolved source_repo (no explicit source arg), or (c) post-`release_worktree` re-claim of the same branch. Both paths share `dispatch_auto_bind_lease` so binding.json + .agend-managed marker + auto watch_ci all land. Rejects 'main'/'master' (E4.5) and cross-agent branch conflicts. Pair with `release_worktree` to unbind.",
        "inputSchema": {"type": "object", "properties": {
            "repository_path": {"type": "string", "description": "Local filesystem path to source repository. Daemon resolves GitHub owner/repo via `git remote get-url origin`. Sprint 55 P0-B preferred form. Mutually exclusive with `repository` (handler rejects both via `ambiguous_args` code)."},
            "repository": {"type": "string", "description": "GitHub `owner/repo` slug. Legacy form retained for one-Sprint deprecation window — emits warn-log; removal Sprint 57. Mutually exclusive with `repository_path`."},
            "branch": {"type": "string", "description": "Branch to bind (must not be main/master)"},
            "rebase_mode": {"type": "boolean", "description": "Sprint 60 W1 PR-1: atomic recover-and-bind. When true, releases self's stale on-disk worktree dir + binding state before lease — closes the lease_failed recovery path without an explicit release_worktree call. Cross-agent isolation preserved (rejects branches leased by another agent)."}
        }, "required": ["branch"]}})
}

pub(crate) fn def_release_worktree() -> Value {
    json!({"name": "release_worktree", "description": "Release the daemon-managed worktree and clear the binding for the given agent. Idempotent. Only removes worktrees carrying the `.agend-managed` marker — operator-created worktrees are left alone.",
        "inputSchema": {"type": "object", "properties": {
            "instance": {"type": "string", "description": "Name of the existing instance whose worktree + binding to release"}
        }, "required": ["instance"]}})
}

pub(crate) fn def_force_release_worktree() -> Value {
    json!({"name": "force_release_worktree", "description": "Force-release a stale daemon-managed worktree directory — cleans <home>/worktrees/<agent>/<branch>/ on disk + runs the standard release_full to clear any lingering binding state. Idempotent. Refuses to clean paths outside the daemon worktree pool. Sprint 59 Wave 1 PR-5 emergency cherry-pick supporting Q2=(C) bypass-free permanent protocol.",
        "inputSchema": {"type": "object", "properties": {
            "instance": {"type": "string", "description": "Name of the existing instance (worktree owner)"},
            "branch": {"type": "string", "description": "Branch name (worktree subdirectory)"},
            "repository_path": {"type": "string", "description": "#826: optional local filesystem path to the source repository. When given, candidate enumeration is skipped and cleanup targets this repo directly. Standard cross-tool name (matches checkout / bind_self / team update)."}
        }, "required": ["instance", "branch"]}})
}

pub(crate) fn def_binding_state() -> Value {
    json!({"name": "binding_state", "description": "Report the structured daemon-side bind state for an agent: binding.json content, worktree existence + .agend-managed marker, ci-watch subscriptions, bind-in-flight guard, and cross-branch holders. Operator + agent introspection surface; non-destructive. Pairs with release_worktree.",
        "inputSchema": {"type": "object", "properties": {
            "instance": {"type": "string", "description": "Name of the existing instance to inspect"}
        }, "required": ["instance"]}})
}

pub(crate) fn def_gc_dry_run() -> Value {
    json!({"name": "gc_dry_run", "description": "List Phase 4 GC candidates (released, past-grace, daemon-managed worktrees) without deleting them. Non-destructive. Default human format; pass `format=json` for tooling.",
    "inputSchema": {"type": "object", "properties": {
        "format": {"type": "string", "enum": ["human", "json"], "description": "Output format (default: human)"}
    }}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_instance_has_backend_param() {
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().expect("tools array");
        let create = tools
            .iter()
            .find(|t| t["name"] == "create_instance")
            .expect("create_instance tool not found");
        let props = &create["inputSchema"]["properties"];
        assert!(
            props["backend"].is_object(),
            "create_instance should have 'backend' property"
        );
        assert!(
            props["backend"]["description"]
                .as_str()
                .expect("desc")
                .contains("claude"),
            "backend description should list available CLI names"
        );
    }

    #[test]
    fn create_instance_name_not_required_command() {
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().expect("tools array");
        let create = tools
            .iter()
            .find(|t| t["name"] == "create_instance")
            .expect("create_instance tool not found");
        // #1602/#1603 schema-align: `name` is NOT required — the handler
        // defaults it (team mode auto-names; the single-instance path still
        // errors "missing 'name'"). Declaring it required would make the schema
        // lie and the dispatch validator hard-reject a legitimate team create.
        let required_strs: Vec<&str> = create["inputSchema"]["required"]
            .as_array()
            .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        assert!(
            !required_strs.contains(&"name"),
            "name must NOT be required (handler-defaulted — see #1603 audit)"
        );
        assert!(
            !required_strs.contains(&"command"),
            "command should NOT be required (backend is preferred, default is claude)"
        );
    }

    #[test]
    fn create_instance_has_target_pane_param() {
        // target_pane is optional but must be declared so MCP clients surface
        // it to the agent — without it, agents can't place new panes next to
        // a specific peer.
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().expect("tools array");
        let create = tools
            .iter()
            .find(|t| t["name"] == "create_instance")
            .expect("create_instance tool not found");
        let props = &create["inputSchema"]["properties"];
        assert!(
            props["target_pane"].is_object(),
            "create_instance should expose 'target_pane'"
        );
        let required_strs: Vec<&str> = create["inputSchema"]["required"]
            .as_array()
            .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        assert!(
            !required_strs.contains(&"target_pane"),
            "target_pane must stay optional"
        );
    }

    #[test]
    fn create_instance_command_deprecated() {
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().expect("tools array");
        let create = tools
            .iter()
            .find(|t| t["name"] == "create_instance")
            .expect("create_instance tool not found");
        let desc = create["inputSchema"]["properties"]["command"]["description"]
            .as_str()
            .expect("command desc");
        assert!(
            desc.to_lowercase().contains("deprecated"),
            "command should be marked as deprecated"
        );
    }

    #[test]
    fn delete_instance_exists() {
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().expect("tools array");
        let delete = tools
            .iter()
            .find(|t| t["name"] == "delete_instance")
            .expect("delete_instance tool not found");
        let required = delete["inputSchema"]["required"]
            .as_array()
            .expect("required");
        assert!(required.iter().any(|v| v == "instance"));
    }

    #[test]
    fn schedule_exposes_fire_strategy_and_linked_task_id() {
        // #1600: #1521 shipped fire_strategy=until_success end-to-end in the
        // backend (schedules.rs) but def_schedule()'s MCP inputSchema never
        // exposed the two fields, so MCP callers could not reach the feature.
        // #1095 was a prior def_schedule schema-doc gap — this guard keeps the
        // schema and the backend args in sync so the class doesn't recur.
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().expect("tools array");
        let schedule = tools
            .iter()
            .find(|t| t["name"] == "schedule")
            .expect("schedule tool not found");
        let props = &schedule["inputSchema"]["properties"];

        let fire_strategy = &props["fire_strategy"];
        assert!(
            fire_strategy.is_object(),
            "schedule should expose 'fire_strategy'"
        );
        let enum_vals: Vec<&str> = fire_strategy["enum"]
            .as_array()
            .expect("fire_strategy enum")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(
            enum_vals,
            vec!["always", "until_success"],
            "fire_strategy enum must match the backend's fire_strategy_from_args"
        );

        assert!(
            props["linked_task_id"].is_object(),
            "schedule should expose 'linked_task_id'"
        );
    }

    #[test]
    fn tool_count_at_least_35() {
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().expect("tools array");
        assert!(
            tools.len() >= 21,
            "expected at least 21 tools (post-consolidation), got {}",
            tools.len()
        );
    }

    #[test]
    fn all_tools_have_input_schema() {
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().expect("tools array");
        for tool in tools {
            let name = tool["name"].as_str().unwrap_or("?");
            assert!(
                tool["inputSchema"].is_object(),
                "tool '{name}' missing inputSchema"
            );
        }
    }

    /// §3.5.10 invariant: tool count must match Sprint 30 wave-1 final state.
    /// Update this assertion when adding/removing tools.
    #[test]
    fn tool_definitions_count_invariant_post_sprint_30() {
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().expect("tools array");
        assert_eq!(
            tools.len(),
            36,
            "#1400: 34 + tokens (#1077 Phase 1) = 35; + mode (#1339 Operator Mode) = 36. \
             Current tools: {:?}",
            tools
                .iter()
                .filter_map(|t| t["name"].as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn pane_snapshot_tool_registered() {
        let defs = tool_definitions();
        let tools = defs["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(
            names.contains(&"pane_snapshot"),
            "pane_snapshot tool must be registered, got: {names:?}"
        );
    }

    // ── #1505 Invariant B: handler arg reads ⊆ declared-union ∪ allowlist ──

    fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                collect_rs_files(&p, out);
            } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
                out.push(p);
            }
        }
    }

    /// Extract top-level tool-arg keys read on a line: `args["KEY"]` and
    /// `args.get("KEY")`. Only simple snake_case identifiers are returned, so
    /// dynamically-built keys (`args[format!(...)]`, variable indices) and
    /// nested reads (`args["a"]["b"]` exposes only `a` — the `["b"]` has no
    /// `args` prefix) are naturally excluded.
    fn extract_arg_read_keys(line: &str) -> Vec<String> {
        let mut keys = Vec::new();
        for (open, close) in [("args[\"", "\"]"), ("args.get(\"", "\")")] {
            let mut rest = line;
            while let Some(p) = rest.find(open) {
                let after = &rest[p + open.len()..];
                let Some(q) = after.find(close) else { break };
                let key = &after[..q];
                if !key.is_empty()
                    && key
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                {
                    keys.push(key.to_string());
                }
                rest = &after[q + close.len()..];
            }
        }
        keys
    }

    /// #1505 Invariant B: every MCP handler `args["k"]` / `args.get("k")` read of
    /// a top-level tool argument must be either declared in SOME tool's schema
    /// (`inputSchema.properties`) or explicitly allow-listed below. Closes the
    /// silent schema↔handler drift class from the "read side": a handler reading
    /// a key that no schema advertises (typo, undocumented param, stale name) is
    /// invisible to the agent — it can never send that key, so the read always
    /// gets null with no compile error and no test failure.
    ///
    /// Pairs with the retired-name denylist in
    /// `tests/mcp_retired_arg_keys_invariant.rs` (#1502 Part A), which covers the
    /// "wrong name" side.
    ///
    /// LIMITATION (deliberate, for zero false positives): reconciles against the
    /// GLOBAL UNION of all tools' declared keys, NOT per-tool. It does not catch
    /// a key declared in tool X but read by tool Y. Precise per-tool attribution
    /// would need a source-level call graph — handlers read args in shared
    /// helpers (`instance_spawn`, `checkout_source`, `lift_message`) that serve
    /// multiple tools — which is brittle and false-positive-prone. The union
    /// check is the deterministic zero-FP subset that still catches the
    /// high-value case: a read no schema declares at all.
    ///
    /// RED proof: add a non-comment `args["__bogus_undeclared__"]` read to any
    /// non-test source under `src/mcp/` → this test fails naming the file:line.
    #[test]
    fn mcp_handler_arg_reads_are_declared_or_allowlisted() {
        use std::collections::BTreeSet;

        // (1) Declared union: every top-level key across all tools' schemas.
        let defs = tool_definitions();
        let mut declared: BTreeSet<String> = BTreeSet::new();
        for tool in defs["tools"].as_array().expect("tools array") {
            if let Some(props) = tool["inputSchema"]["properties"].as_object() {
                declared.extend(props.keys().cloned());
            }
        }

        // (2) Allowlist: reads that are intentionally NOT agent-facing schema
        //     params. Each entry carries its reason; remove the entry when the
        //     read is removed.
        const ALLOWLIST: &[(&str, &str)] = &[
            // `send` routes by request_kind and calls lift_message(args, dst) to
            // COPY the agent-facing `message` field into these internal keys
            // before handle_report_result / handle_request_information read them.
            // The agent always sends `message` (the schema documents it), so
            // these must stay undeclared — only allow-listed.
            (
                "summary",
                "internal: lift_message copies `message` here for kind=report",
            ),
            (
                "question",
                "internal: lift_message copies `message` here for kind=query",
            ),
            // handle_send_to_instance reads `kind` only as a back-compat fallback
            // when `request_kind` is absent. The current schema name is
            // `request_kind`; `kind` stays undeclared on purpose.
            ("kind", "deprecated back-compat alias for `request_kind`"),
        ];
        let allow: BTreeSet<&str> = ALLOWLIST.iter().map(|(k, _)| *k).collect();

        // (3) Read sites: scan non-test source under src/mcp/ for top-level arg
        //     reads.
        let mcp_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/mcp");
        let mut files = Vec::new();
        collect_rs_files(&mcp_dir, &mut files);
        assert!(!files.is_empty(), "no .rs files under src/mcp/");

        let mut violations = Vec::new();
        for f in &files {
            // Skip test files — fixtures legitimately build arbitrary arg shapes.
            let fname = f.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if fname.ends_with("tests.rs") || fname.ends_with("test.rs") {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(f) else {
                continue;
            };
            for (idx, line) in content.lines().enumerate() {
                let t = line.trim_start();
                if t.starts_with("//") || t.starts_with("*") {
                    continue;
                }
                for key in extract_arg_read_keys(line) {
                    if !declared.contains(&key) && !allow.contains(key.as_str()) {
                        violations.push(format!(
                            "{}:{}: reads undeclared arg key `{}`: {}",
                            f.display(),
                            idx + 1,
                            key,
                            line.trim()
                        ));
                    }
                }
            }
        }

        assert!(
            violations.is_empty(),
            "#1505: MCP handler reads an arg key that no tool schema declares and \
             that is not allow-listed. Either add it to the tool's \
             `inputSchema.properties` (if agent-facing) or to ALLOWLIST with a \
             reason (if internal/deprecated):\n{}",
            violations.join("\n")
        );
    }
}
