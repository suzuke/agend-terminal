//! MCP tool definitions — JSON schemas for all exposed tools.

use serde_json::{json, Value};

pub fn tool_definitions() -> Value {
    let tools: Vec<Value> = crate::mcp::registry::all()
        .iter()
        .map(|entry| (entry.definition)())
        .collect();
    json!({"tools": tools})
}

/// #2300 P0 / #2344: tool definitions VISIBLE to a typed [`crate::fleet::RoleKind`]
/// (per [`crate::mcp::registry::tool_subset_for_role`]). Default-all-open — `None`
/// (no `role_kind` declared) or a full-capability role → the full set,
/// byte-identical to [`tool_definitions`]. Used by the daemon `tools/list` handler
/// to subset the surface a read/report role advertises.
pub fn tool_definitions_for_role(role_kind: Option<crate::fleet::RoleKind>) -> Value {
    let tools: Vec<Value> = crate::mcp::registry::tool_subset_for_role(role_kind)
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
            "sequencing": {"type": "string", "enum": ["parallel", "sequential", "sequential-merge-only"], "description": "Task execution order constraint"},
            "eta_minutes": {"type": "integer", "description": "Expected completion time in minutes"},
            "reporting_cadence": {"type": "string", "enum": ["per-pr", "wave-end", "both"], "description": "When implementer should report back"},
            "worktree_binding_required": {"type": "boolean", "description": "Whether target must bind to a worktree before starting"},
            "expect_reply_within_secs": {"type": "integer", "description": "Opt-in dispatch-idle watchdog (PR1). When set on a kind=task/query send, the daemon records a pending-dispatch sidecar and fires `dispatch_idle_threshold_exceeded` to the dispatcher's inbox if the matching kind=report (correlation_id-keyed) hasn't arrived within this many seconds. Default unset = no tracking (cross-team-safe). Fixup-team dispatches inherit a 10-min default automatically; other teams must opt in explicitly."},
            "next_after_ci": {"type": "string", "description": "#931 Fix 2 (H5a): when dispatching kind=task with a `branch`, set this to the agent that should receive `[ci-ready-for-action]` after CI passes on that branch. The daemon's auto-armed ci-watch carries the chain target so the handoff fires without a manual follow-up `ci action=watch next_after_ci=…`. Example: lead dispatches dev with `next_after_ci=reviewer` — reviewer is auto-notified when dev's PR goes green."},
            "terminal": {"type": "boolean", "description": "Set true on kind=report to signal task completion. When correlation_id matches a task and reporter is the assignee, the task is auto-closed. Default false — progress reports and review verdicts do not trigger auto-close."},
            "no_report_expected": {"type": "boolean", "description": "#2099: set true on a fire-and-forget kind=task dispatch that intentionally expects NO kind=report back. The dispatch is recorded with a terminal-like status so the 30-min dispatch-stuck sweep never false-fires a 'dispatch stuck check' for it (the audit row is kept). Default false — every normal dispatch stays stuck-tracked. Distinct from `terminal`, which is the report-side auto-close signal."}
        }, "required": ["message"]}})
}

pub(crate) fn def_inbox() -> Value {
    json!({"name": "inbox", "description": "Check pending messages, OR look up a single message by ID, OR fetch a thread's messages, OR quietly clear a backlog, OR confirm messages processed. No params = drain pending (returns messages, now marked `delivering`). With message_id = describe message status (read/unread/expired/notfound). With thread_id = fetch all messages in thread ordered by timestamp. With action=\"clear\" = quiet compact-clear: marks non-obligation messages read and returns a BOUNDED summary {cleared_count, kept_unread_count, summaries[], requires_response[]} — unanswered queries + unsettled tasks stay UNREAD and surface in requires_response (never silently swallowed). Use clear (not drain) to dismiss a large stale backlog without flooding your context. With action=\"ack\" (#2299) = confirm you have HANDLED what you drained: transitions delivering→processed so the reclaim-TTL never re-delivers it. Pass message_id to ack one message, or omit it to ack your whole in-flight batch. Acking is best-effort: a re-drain implicitly acks the prior batch and a 10-min reclaim-TTL re-delivers anything left unconfirmed — but acking promptly avoids a redundant re-delivery if your turn is interrupted.",
    "inputSchema": {"type": "object", "properties": {
        "action": {"type": "string", "enum": ["clear", "ack"], "description": "\"clear\" = quiet compact-clear (obligations kept unread). \"ack\" = confirm delivering→processed (#2299; message_id targets one, omit to ack the whole drained batch). Omit for normal drain."},
        "message_id": {"type": "string", "description": "Look up message status by ID (describe), or with action=\"ack\" the message to confirm processed"},
        "thread_id": {"type": "string", "description": "Fetch all messages in a thread"},
        "instance": {"type": "string", "description": "For message_id: target instance (defaults to caller). For thread_id: filter to a specific instance's inbox (optional, scans all if omitted)"}
    }}})
}

pub(crate) fn def_list_instances() -> Value {
    json!({"name": "list_instances", "description": "List all active agent instances. Pass optional `instance` for detailed info on a single instance.",
        "inputSchema": {"type": "object", "properties": {"instance": {"type": "string", "description": "Optional: name of an existing instance for detailed info"}}}})
}

pub(crate) fn def_create_instance() -> Value {
    json!({"name": "create_instance", "description": "Create agent instance(s). Team modes: (a) homogeneous — count:3, backend:\"claude\", team:\"dev\" → dev-1..dev-3 all claude; (b) heterogeneous — backends:[\"codex\",\"kiro-cli\",\"agy\"], team:\"mixed\" → mixed-1=codex, mixed-2=kiro-cli, mixed-3=agy, all grouped in one tab.",
    "inputSchema": {"type": "object", "properties": {
        "name": {"type": "string", "description": "Instance name (single instance) or base name (ignored when team is set — team name is used as prefix)"},
        "backend": {"type": "string", "description": "Backend CLI name: claude, agy, kiro-cli, codex, opencode"},
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
    json!({"name": "decision", "description": "Manage decisions. Actions: post, list, update, answer. #2305: `post` with `needs_answer:true` (+ `options`, `allow_free_text`) creates an async pending QUESTION the operator answers later; `answer` (id + answer) records the operator's choice and notifies the decision author.",
        "inputSchema": {"type": "object", "properties": {
            "action": {"type": "string", "enum": ["post", "list", "update", "answer"]},
            "title": {"type": "string"}, "content": {"type": "string", "description": "Decision body (alias: text — #2037). For a question, the prompt text."}, "text": {"type": "string", "description": "Alias of `content` (#2037)."},
            "scope": {"type": "string", "enum": ["project", "fleet"]},
            "tags": {"type": "array", "items": {"type": "string"}},
            "ttl_days": {"type": "number"}, "supersedes": {"type": "string"},
            "id": {"type": "string"}, "archive": {"type": "boolean"},
            "include_archived": {"type": "boolean"},
            "needs_answer": {"type": "boolean", "description": "#2305 post: mark this decision a pending question awaiting an operator answer."},
            "options": {"type": "array", "description": "#2305 post: suggested answer options (recommended-first). Each is `{label, recommended}` or a bare string.", "items": {"type": ["object", "string"]}},
            "allow_free_text": {"type": "boolean", "description": "#2305 post: accept a free-text answer not matching any option."},
            "answer": {"type": "string", "description": "#2305 answer action: the chosen option label or free-text answer for decision `id`."}
        }, "required": ["action"]}})
}

pub(crate) fn def_task() -> Value {
    json!({"name": "task", "description": "Manage task board. Actions: create, list, claim, done, update, sweep, health, activity, metadata_set, metadata_get. #806: default list trims to actionable statuses (open/claimed/in_progress/blocked); pass include_history=true to surface done/cancelled. `sweep` is operator-triggered manual hygiene (5 stale-task categories with dry-run + confirm_ids round-trip). #830: `health` is a one-shot board-hygiene snapshot — totals + by_status + ghost_owners + stale_claims + age aggregates + recommendations array.",
        "inputSchema": {"type": "object", "properties": {
            "action": {"type": "string", "enum": ["create", "list", "claim", "done", "update", "sweep", "health", "activity", "metadata_set", "metadata_get"]},
            "title": {"type": "string"}, "description": {"type": "string"},
            "priority": {"type": "string", "enum": ["low", "normal", "high", "urgent"]},
            "assignee": {"type": "string"}, "depends_on": {"type": "array", "items": {"type": "string"}}, "parent_id": {"type": "string", "description": "Parent task ID for subtask composition (A is composed of B,C,D). Complementary to depends_on (execution order)."},
            "id": {"type": "string", "description": "Task id (alias: task_id — #2037)."}, "task_id": {"type": "string", "description": "Alias of `id` (#2037: send uses task_id, so the cross-tool slip is accepted)."}, "result": {"type": "string"},
            "status": {"type": "string", "enum": ["backlog", "open", "claimed", "in_progress", "in_review", "blocked", "verified", "done", "cancelled"]},
            "filter_assignee": {"type": "string", "description": "list: filter by assignee (alias: assignee)."}, "filter_status": {"type": "string", "description": "list: filter by status (alias: status)."}, "filter_tag": {"type": "string", "description": "Filter by tag (alias: tag)"}, "tag": {"type": "string", "description": "Alias of filter_tag (#2037)."}, "tags": {"type": "array", "items": {"type": "string"}, "description": "Task tags (for create/update)"},
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
            "metadata_value": {"description": "Value for metadata_set action (any JSON type)."},
            "bind": {"type": "boolean", "description": "#1933: create only — opt OUT of the daemon's auto-bind-on-dispatch (default true). Set false to leave the assignee unbound. Consumed in tasks/handler.rs (TaskEvent::Created.bind)."},
            "eta_secs": {"type": "integer", "description": "#1933: create only — task stall-watchdog ETA in seconds; the daemon flags the task as stalled once this budget elapses. Consumed in tasks/handler.rs (TaskEvent::Created.eta_secs)."},
            "project": {"type": "string", "description": "#2117 P1: target project board. create: route the task to this project (default: the caller's current project, derived from its team's source_repo). list: show this project, or `all` to aggregate every board."},
            "scope": {"type": "string", "enum": ["fleet"], "description": "#2117 P1: list scope. `fleet` aggregates tasks across ALL project boards (each task tagged with its project id). Equivalent to `project=all`."}
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
    json!({"name": "restart_daemon", "description": "Request a graceful daemon restart. Default (#1814): the daemon self-respawns — it spawns a successor, health-gates it, and only then exits(0) so the successor takes over; NO external supervisor required. Opt-out `AGEND_RESTART_HANDOFF=0` takes the legacy path (exit code 42 + a launchd/systemd/Task-Scheduler supervisor from `agend-terminal service install`, or `scripts/agend-wrapper.sh`, respawns it; returns ok:false if no supervisor is detected). Returns ok:false in `agend-terminal app` (combined TUI+daemon) mode — that process has no in-process restart consumer, so quit and relaunch the app, or SIGTERM + restart. Idempotent.",
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
            "full_history": {"type": "boolean", "description": "#2037: `list` trims run_history to the newest 3 per schedule (row carries runs_total); pass true for the full stored history (up to 50)."},
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
    // Keys are derived from runtime_config's serialized fields so this list can
    // never go stale as config keys are added (it previously omitted several,
    // incl. show_pane_state).
    let keys = crate::runtime_config::keys().join(", ");
    json!({"name": "config", "description": format!("#1085: Runtime-mutable daemon configuration. Actions: get, set, list. Keys: {keys}."),
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
            "instance": {"type": "string", "description": "Name of the existing instance whose worktree + binding to release"},
            "dry_run": {"type": "boolean", "description": "#1933: when true, preview what would be released WITHOUT releasing (no binding/worktree mutation). Default false. Consumed at mcp/handlers/worktree.rs."}
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
    /// helpers (`instance_state::spawn`, `checkout_source`, `lift_message`) that serve
    /// multiple tools — which is brittle and false-positive-prone. The union
    /// check is the deterministic zero-FP subset that still catches the
    /// high-value case: a read no schema declares at all.
    ///
    /// SCANNED (#1933): `src/mcp/` + `src/tasks/` + `src/{schedules,teams,decisions,
    /// deployments}.rs` — the coordination-tool handlers that live outside src/mcp/.
    ///
    /// RED proof: add a non-comment `args["__bogus_undeclared__"]` read to any
    /// non-test source under the scanned dirs → this test fails naming the file:line.
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
            // #1933: task `done` honors a caller-provided `done_source` for the
            // audit trail (tasks/handler.rs:358/568), but it is a daemon/audit
            // concern — agent-FORGEABLE source would be an audit-integrity hole, so
            // it stays INTERNAL-only (deliberately undeclared). Read multi-line
            // (`args\n.get("done_source")`) so the line-oriented extractor does not
            // currently flag it; allow-listed defensively against a future reflow.
            (
                "done_source",
                "internal: daemon-set audit source; agent-forgeable = integrity risk (#1933)",
            ),
            // NOTE: bind_self's `task_id` (mcp/handlers/worktree.rs) is dispatch-path-
            // set and intentionally NOT a bind_self schema field; it needs no entry
            // here because `task_id` IS declared on the `task`/`send` tools, so the
            // union check already passes it (kept undeclared on bind_self by design).
        ];
        let allow: BTreeSet<&str> = ALLOWLIST.iter().map(|(k, _)| *k).collect();

        // (3) Read sites: scan non-test source under src/mcp/ AND the coordination-
        //     tool handler modules that live OUTSIDE src/mcp/ (#1933: src/mcp/ alone
        //     missed task/schedule/team/decision/deployment handlers — the
        //     eta_secs/done_source-class undeclared-read blind spot from the
        //     #1911-followup audit).
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        for dir in ["src/mcp", "src/tasks"] {
            collect_rs_files(&manifest.join(dir), &mut files);
        }
        for file in [
            "src/schedules.rs",
            "src/teams.rs",
            "src/decisions.rs",
            "src/deployments.rs",
        ] {
            files.push(manifest.join(file));
        }
        assert!(
            !files.is_empty(),
            "no .rs files under the scanned handler dirs"
        );

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

    /// #1933 generalized ghost-directive guard (the #1911 send-only guard,
    /// extended to ALL coordination tools). The COMPLEMENT of #1505's Invariant B:
    /// #1505 catches a handler READ with no schema declaration; this catches a
    /// schema DECLARATION with no consumer — a field advertised to agents that
    /// silently evaporates because no mechanism reads it (the
    /// `working_directory`/`requires_reply`/`task_summary` class, and the
    /// `health.note` ghost surfaced by the #1933 schema-consumption audit).
    ///
    /// Every coordination tool's schema field must be either:
    ///   (a) `CONSUMED` — with a consumption-point cite (a real read site). The
    ///       cite makes the table a living doc and forces a real cite on additions.
    ///   (b) `DEFERRED` — an intentionally-deferred passthrough with a roadmap ref
    ///       (#649 trio), honestly recorded rather than dying silently like a ghost.
    /// And every tool in `tool_definitions()` must be either a classified
    /// coordination tool OR in `NON_COORDINATION` (simple query/display/control
    /// tools, not directive-bearing) — so a NEW tool can't escape classification
    /// (the #1907/#1911 curated-list-drift guard).
    ///
    /// RED proof: add a `"ghost"` field to any coordination tool's schema (and no
    /// consumer) → this test fails naming `<tool>.ghost` as unclassified.
    #[test]
    fn coordination_tool_schema_fields_are_consumed_or_deferred() {
        use std::collections::BTreeSet;

        // (tool, field, consumption-point cite). `send` keeps the #1911 per-field
        // cites (its consumption is field-specific); other tools route all declared
        // fields through one action-dispatch handler, so the handler is the cite.
        const CONSUMED: &[(&str, &str, &str)] = &[
            // ── send (verbatim from #1911) ──
            ("send", "instance", "comms.rs resolve_instance → single-recipient routing"),
            ("send", "instances", "comms.rs handle_broadcast recipient list"),
            ("send", "team", "comms.rs handle_broadcast → teams::get_members"),
            ("send", "tags", "comms.rs is_some() triggers broadcast MODE; value-based tag-targeting NOT implemented"),
            ("send", "message", "comms.rs task body / lift_message for kind=report|query"),
            ("send", "request_kind", "comms.rs handler routing + auto_close kind gate"),
            ("send", "success_criteria", "comms.rs appended to delivered message body"),
            ("send", "context", "comms.rs appended to delivered message body"),
            ("send", "task_id", "comms.rs enriching-gate + dispatch_hook ci_watch correlation"),
            ("send", "correlation_id", "messaging.rs envelope → auto_close_on_report + dispatch_idle"),
            ("send", "parent_id", "messaging.rs InboxMessage envelope → threading"),
            ("send", "thread_id", "inbox/storage.rs get_thread grouping"),
            ("send", "force", "comms.rs busy-gate override"),
            ("send", "force_reason", "comms.rs force validation + force_meta"),
            ("send", "second_reviewer", "comms.rs → review_class=dual on auto-armed watch"),
            ("send", "second_reviewer_reason", "comms.rs dual-review reason validation"),
            ("send", "reviewed_head", "messaging.rs sha-gate / auto-release verdict"),
            ("send", "artifacts", "comms.rs report body + evidence gate"),
            ("send", "branch", "comms.rs dispatch_auto_bind_lease → worktree + ci_watch"),
            ("send", "worktree_binding_required", "messaging.rs check_worktree_enforcement gate"),
            ("send", "expect_reply_within_secs", "messaging.rs → dispatch_idle threshold watchdog"),
            ("send", "next_after_ci", "comms.rs → ci_watch poller fires [ci-ready-for-action]"),
            ("send", "terminal", "messaging.rs msg.terminal → auto_close_on_report"),
            ("send", "no_report_expected", "comms.rs track step → DispatchEntry status=no_report_expected (skips sweep_stuck/sweep_orphans) + messaging.rs track_dispatch skips the dispatch_idle sidecar record"),
            // ── task (all fields consumed per action; #1933 audit) ──
            ("task", "action", "tasks/handler.rs action routing"),
            ("task", "title", "tasks/handler.rs handle_create"),
            ("task", "description", "tasks/handler.rs create/update"),
            ("task", "priority", "tasks/handler.rs create/update"),
            ("task", "assignee", "tasks/handler.rs create/update owner + routed_to"),
            ("task", "depends_on", "tasks/handler.rs create dep-blocking"),
            ("task", "parent_id", "tasks/handler.rs create subtask composition"),
            ("task", "id", "tasks/handler.rs claim/done/update/metadata target"),
            ("task", "task_id", "tasks/handler.rs id_arg alias of `id` (#2037)"),
            ("task", "tag", "tasks/handler.rs list alias of filter_tag (#2037)"),
            ("decision", "text", "decisions.rs post alias of `content` (#2037)"),
            ("schedule", "full_history", "schedules.rs list run_history opt-in (#2037)"),
            ("task", "result", "tasks/handler.rs handle_done"),
            ("task", "status", "tasks/handler.rs update transition gate"),
            ("task", "filter_assignee", "tasks/handler.rs list filter"),
            ("task", "filter_status", "tasks/handler.rs list filter"),
            ("task", "filter_tag", "tasks/handler.rs list filter"),
            ("task", "tags", "tasks/handler.rs create/update"),
            ("task", "include_history", "tasks/handler.rs list opt-in"),
            ("task", "limit", "tasks/handler.rs list cap"),
            ("task", "due_at", "tasks/handler.rs create deadline"),
            ("task", "branch", "tasks/handler.rs create event"),
            ("task", "force", "tasks/handler.rs done/update ACL bypass"),
            ("task", "force_reason", "tasks/handler.rs force audit"),
            ("task", "apply", "tasks/sweep.rs dry-run vs apply"),
            ("task", "confirm_ids", "tasks/sweep.rs apply subset"),
            ("task", "audit_reason", "tasks/sweep.rs apply audit"),
            ("task", "repository", "tasks/sweep.rs PR-state repo override"),
            ("task", "metadata_key", "tasks/handler.rs metadata_set"),
            ("task", "metadata_value", "tasks/handler.rs metadata_set"),
            ("task", "bind", "tasks/handler.rs create → TaskEvent::Created.bind (#1933 declared)"),
            ("task", "eta_secs", "tasks/handler.rs create → TaskEvent::Created.eta_secs (#1933 declared)"),
            ("task", "project", "tasks/handler.rs create board route (:136) + list project select (:221) (#2117 P1)"),
            ("task", "scope", "tasks/handler.rs list fleet_scope aggregate (:208) (#2117 P1)"),
            // ── decision ──
            ("decision", "action", "decisions.rs routing"),
            ("decision", "title", "decisions.rs post"),
            ("decision", "content", "decisions.rs post/update"),
            ("decision", "scope", "decisions.rs post"),
            ("decision", "tags", "decisions.rs post/list-filter/update"),
            ("decision", "ttl_days", "decisions.rs post/update"),
            ("decision", "supersedes", "decisions.rs post archives prior"),
            ("decision", "id", "decisions.rs update/answer target"),
            ("decision", "archive", "decisions.rs update flag"),
            ("decision", "include_archived", "decisions.rs list"),
            ("decision", "needs_answer", "decisions.rs post → pending question (#2305)"),
            ("decision", "options", "decisions.rs post question options (#2305)"),
            ("decision", "allow_free_text", "decisions.rs post question free-text gate (#2305)"),
            ("decision", "answer", "decisions.rs answer action (#2305)"),
            // ── team ──
            ("team", "action", "teams.rs routing"),
            ("team", "name", "teams.rs create/delete/update target"),
            ("team", "members", "teams.rs create"),
            ("team", "orchestrator", "teams.rs create/update validated member"),
            ("team", "description", "teams.rs create/update"),
            ("team", "repository_path", "teams.rs create/update source_repo"),
            ("team", "accept_from", "teams.rs create/update cross-team allowlist"),
            ("team", "add", "teams.rs update add-members"),
            ("team", "remove", "teams.rs update remove-members"),
            // ── schedule ──
            ("schedule", "action", "schedules.rs routing"),
            ("schedule", "id", "schedules.rs update/delete target"),
            ("schedule", "cron", "schedules.rs trigger_from_args"),
            ("schedule", "run_at", "schedules.rs trigger_from_args one-shot"),
            ("schedule", "message", "schedules.rs create/update"),
            ("schedule", "instance", "schedules.rs create target / list filter"),
            ("schedule", "label", "schedules.rs create/update"),
            ("schedule", "timezone", "schedules.rs cron eval zone"),
            ("schedule", "enabled", "schedules.rs update"),
            ("schedule", "fire_strategy", "schedules.rs create/update (#1521)"),
            ("schedule", "linked_task_id", "schedules.rs until_success gate (#1521)"),
            // ── deployment ──
            ("deployment", "action", "deployments.rs routing"),
            ("deployment", "template", "deployments.rs validate_deploy_args"),
            ("deployment", "directory", "deployments.rs working-dir override"),
            ("deployment", "name", "deployments.rs deployment name"),
            ("deployment", "branch", "deployments.rs per-instance worktree"),
            // ── ci ──
            ("ci", "action", "ci/mod.rs routing"),
            ("ci", "repository", "ci/mod.rs watch/unwatch/status"),
            ("ci", "branch", "ci/mod.rs handle_watch_ci"),
            ("ci", "interval_secs", "ci/mod.rs poll interval"),
            ("ci", "next_after_ci", "ci/mod.rs persisted to watch sidecar"),
            ("ci", "review_class", "ci/mod.rs dual-review gate (#972)"),
            ("ci", "ci_provider", "ci/mod.rs provider override"),
            ("ci", "ci_provider_url", "ci/mod.rs self-hosted base URL"),
            // ── repo ──
            ("repo", "action", "ci/mod.rs routing"),
            ("repo", "pr", "ci/mod.rs handle_merge_repo"),
            ("repo", "repository", "ci/mod.rs merge slug"),
            ("repo", "repository_path", "ci/mod.rs checkout source"),
            ("repo", "branch", "ci/mod.rs checkout worktree target"),
            ("repo", "path", "ci/mod.rs handle_release_repo (release path)"),
            ("repo", "instance", "ci/mod.rs cleanup_init_commits target"),
            ("repo", "bind", "ci/mod.rs checkout atomic-bind gate"),
            ("repo", "base", "ci/mod.rs cleanup_merged_branches compare ref"),
            ("repo", "min_age_days", "ci/mod.rs stale_idle threshold"),
            ("repo", "apply", "ci/mod.rs cleanup_merged dry-run/apply"),
            ("repo", "confirm_ids", "ci/mod.rs cleanup_merged subset"),
            ("repo", "audit_reason", "ci/mod.rs cleanup_merged audit"),
            ("repo", "from_ref", "ci/mod.rs checkout auto-create base"),
            // ── bind_self ──
            ("bind_self", "repository_path", "mcp/handlers/worktree.rs preferred source"),
            ("bind_self", "repository", "mcp/handlers/worktree.rs legacy slug"),
            ("bind_self", "branch", "mcp/handlers/worktree.rs validated bind branch"),
            ("bind_self", "rebase_mode", "mcp/handlers/worktree.rs recover-and-bind gate"),
            // ── release_worktree / force_release_worktree ──
            ("release_worktree", "instance", "mcp/handlers/worktree.rs release target"),
            ("release_worktree", "dry_run", "mcp/handlers/worktree.rs preview gate (#1933 declared)"),
            ("force_release_worktree", "instance", "force_release/mod.rs worktree owner"),
            ("force_release_worktree", "branch", "force_release/mod.rs worktree subdir"),
            ("force_release_worktree", "repository_path", "force_release/mod.rs GC enumeration hint"),
            // ── health ──
            ("health", "action", "instance.rs report/clear routing"),
            ("health", "reason", "instance.rs handle_report_health → set_blocked_reason"),
            ("health", "retry_after_secs", "instance.rs parse_kind → BlockedReason"),
            ("health", "note", "instance.rs set_blocked_note + query.rs blocked_note (#1933 wired)"),
            ("health", "instance", "instance.rs handle_clear_blocked_reason target"),
            // ── set_waiting_on ──
            ("set_waiting_on", "condition", "api/handlers/instance.rs waiting_on store/clear"),
            // ── create_instance ──
            ("create_instance", "name", "instance_state/spawn.rs spawn name"),
            ("create_instance", "backend", "instance_state/spawn.rs spawn backend"),
            ("create_instance", "args", "instance_state/spawn.rs extra cmd args"),
            ("create_instance", "model", "instance_state/spawn.rs --model flag"),
            ("create_instance", "working_directory", "instance_state/spawn.rs validated wd"),
            ("create_instance", "branch", "instance_state/spawn.rs worktree::create"),
            ("create_instance", "task", "instance_state/spawn.rs delayed inject"),
            ("create_instance", "layout", "instance_state/spawn.rs resolve_team_layout"),
            ("create_instance", "target_pane", "instance_state/spawn.rs resolve_team_layout"),
            ("create_instance", "count", "mcp/handlers/instance.rs team-mode count"),
            ("create_instance", "team", "mcp/handlers/instance.rs team-mode prefix"),
            ("create_instance", "backends", "mcp/handlers/instance.rs per-member backends"),
            ("create_instance", "command", "instance_state/spawn.rs deprecated backend alias"),
            ("create_instance", "role", "instance_state/spawn.rs fleet.yaml role"),
            ("create_instance", "env", "instance_state/spawn.rs per-instance env (#900)"),
            ("create_instance", "topic_binding", "instance_state/spawn.rs telegram topic binding"),
            // ── replace_instance / restart_instance ──
            ("replace_instance", "instance", "mcp/handlers/instance.rs replace target"),
            ("replace_instance", "reason", "mcp/handlers/instance.rs handover message + event"),
            ("restart_instance", "instance", "mcp/handlers/instance.rs restart target"),
            ("restart_instance", "mode", "mcp/handlers/instance.rs restart_spawn_params --continue/--resume"),
            ("restart_instance", "reason", "mcp/handlers/instance.rs restart event"),
            // ── watchdog / config / mode ──
            ("watchdog", "action", "mcp/handlers/dispatch.rs snooze/resume/status/ack"),
            ("watchdog", "duration", "mcp/handlers/dispatch.rs parse_duration_secs (snooze)"),
            ("config", "action", "mcp/handlers/dispatch.rs get/set/list"),
            ("config", "key", "mcp/handlers/dispatch.rs runtime_config::get_key/set"),
            ("config", "value", "mcp/handlers/dispatch.rs runtime_config::set"),
            ("mode", "action", "mcp/handlers/dispatch.rs get (read-only #1339)"),
        ];

        // Intentionally-deferred passthrough: advertised by design, Phase-2
        // consuming mechanism deferred. MUST carry a roadmap ref.
        const DEFERRED: &[(&str, &str, &str)] = &[
            (
                "send",
                "sequencing",
                "#649 Phase-1 passthrough; Phase-2 task-ordering scheduler deferred",
            ),
            (
                "send",
                "eta_minutes",
                "#649 Phase-1 passthrough; Phase-2 ETA scheduler deferred",
            ),
            (
                "send",
                "reporting_cadence",
                "#649 Phase-1 passthrough; Phase-2 cadence scheduler deferred",
            ),
        ];

        // The coordination tools this guard enforces (every declared field must be
        // classified above).
        const COORDINATION_TOOLS: &[&str] = &[
            "send",
            "task",
            "decision",
            "team",
            "schedule",
            "deployment",
            "ci",
            "repo",
            "bind_self",
            "release_worktree",
            "force_release_worktree",
            "health",
            "set_waiting_on",
            "create_instance",
            "replace_instance",
            "restart_instance",
            "watchdog",
            "config",
            "mode",
        ];

        // Simple query/display/control tools — not directive-bearing, so their
        // fields are not classified here. A NEW tool must be added to EXACTLY ONE
        // of COORDINATION_TOOLS or NON_COORDINATION (drift guard below).
        const NON_COORDINATION: &[&str] = &[
            "reply",
            "download_attachment",
            "inbox",
            "list_instances",
            "delete_instance",
            "start_instance",
            "tokens",
            "interrupt",
            "set_display_name",
            "set_description",
            "move_pane",
            "pane_snapshot",
            "tui_screenshot",
            "restart_daemon",
            "task_sweep_config",
            "binding_state",
            "gc_dry_run",
        ];

        let defs = tool_definitions();
        let tools = defs["tools"].as_array().expect("tools array");

        // Drift guard: every registered tool is classified or explicitly excluded.
        let all_tool_names: BTreeSet<&str> =
            tools.iter().filter_map(|t| t["name"].as_str()).collect();
        let classified: BTreeSet<&str> = COORDINATION_TOOLS
            .iter()
            .chain(NON_COORDINATION.iter())
            .copied()
            .collect();
        let unclassified_tools: Vec<&str> = all_tool_names
            .iter()
            .filter(|t| !classified.contains(*t))
            .copied()
            .collect();
        assert!(
            unclassified_tools.is_empty(),
            "#1933: new tool(s) {unclassified_tools:?} are in neither COORDINATION_TOOLS nor \
             NON_COORDINATION. Add to COORDINATION_TOOLS (and classify every field in CONSUMED/\
             DEFERRED) if directive-bearing, else NON_COORDINATION."
        );

        let consumed: BTreeSet<(&str, &str)> = CONSUMED.iter().map(|(t, f, _)| (*t, *f)).collect();
        let deferred: BTreeSet<(&str, &str)> = DEFERRED.iter().map(|(t, f, _)| (*t, *f)).collect();

        // Forward: every declared field of every coordination tool is classified.
        let mut ghosts = Vec::new();
        for tool in tools {
            let Some(name) = tool["name"].as_str() else {
                continue;
            };
            if !COORDINATION_TOOLS.contains(&name) {
                continue;
            }
            if let Some(props) = tool["inputSchema"]["properties"].as_object() {
                for field in props.keys() {
                    let key = (name, field.as_str());
                    if !consumed.contains(&key) && !deferred.contains(&key) {
                        ghosts.push(format!("{name}.{field}"));
                    }
                }
            }
        }
        assert!(
            ghosts.is_empty(),
            "#1933 ghost-guard: coordination-tool schema field(s) {ghosts:?} have no consumer and \
             aren't deferred-allowlisted — advertised to agents but silently dropped (the \
             health.note class). Wire a consumer + add to CONSUMED with a cite, or add to DEFERRED \
             with a roadmap ref."
        );

        // Reverse: no stale CONSUMED/DEFERRED entry (a classified field the schema
        // no longer declares).
        let declared: BTreeSet<(String, String)> = tools
            .iter()
            .filter_map(|t| {
                let name = t["name"].as_str()?;
                let props = t["inputSchema"]["properties"].as_object()?;
                Some(props.keys().map(move |f| (name.to_string(), f.clone())))
            })
            .flatten()
            .collect();
        for (t, f, _) in CONSUMED.iter().chain(DEFERRED.iter()) {
            assert!(
                declared.contains(&(t.to_string(), f.to_string())),
                "#1933: `{t}.{f}` is classified but no longer a declared schema field — remove the \
                 stale entry."
            );
        }
    }
}
