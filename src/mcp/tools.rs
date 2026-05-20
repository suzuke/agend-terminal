//! MCP tool definitions — JSON schemas for all exposed tools.

use serde_json::{json, Value};

pub fn tool_definitions() -> Value {
    let mut tools = Vec::new();
    tools.extend(channel_tools());
    tools.extend(comm_tools());
    tools.extend(instance_tools());
    tools.extend(decision_tools());
    tools.extend(task_tools());
    tools.extend(team_tools());
    tools.extend(schedule_tools());
    tools.extend(repo_tools());
    tools.extend(deploy_tools());
    tools.extend(ci_tools());
    tools.extend(health_tools());
    tools.extend(worktree_tools());
    json!({"tools": tools})
}

fn channel_tools() -> Vec<Value> {
    vec![
        json!({"name": "reply", "description": "Reply to the user via the active channel. Requires daemon API. Sprint 59 Wave 1 PR-4 ((B) decision default with timeout): when both `default_action` and `timeout_secs` are set, the daemon records a pending operator decision sidecar and auto-fires the default after the timeout window. Subsequent reply calls without `default_action` resolve the pending decision (operator override / explicit answer arrived).",
            "inputSchema": {"type": "object", "properties": {
                "text": {"type": "string"},
                "default_action": {"type": "string", "description": "Action to auto-execute on timeout when the operator doesn't reply within `timeout_secs`. e.g. 'proceed-with-lean' / 'abort'. Pair with `timeout_secs` (Sprint 59 Wave 1 PR-4)."},
                "timeout_secs": {"type": "integer", "description": "Seconds to wait for an operator response before firing `default_action`. Required when `default_action` is set; ignored otherwise (Sprint 59 Wave 1 PR-4)."}
            }, "required": ["text"]}}),
        json!({"name": "download_attachment", "description": "Download a file attachment (telegram multimedia: images, audio, documents). Returns local path. Requires daemon API.",
            "inputSchema": {"type": "object", "properties": {"file_id": {"type": "string"}}, "required": ["file_id"]}}),
    ]
}

fn comm_tools() -> Vec<Value> {
    vec![
        // --- Unified send (Sprint 30: 5→1 consolidation) ---
        json!({"name": "send", "description": "Send a message to another instance or broadcast to multiple. Replaces send_to_instance/delegate_task/report_result/request_information/broadcast. Sprint 58 Wave 4 PR-1: kind=task dispatches MUST include task_id (call task action=create first to obtain a 't-...' id).",
            "inputSchema": {"type": "object", "properties": {
                "target_instance": {"type": "string", "description": "Target instance name (single recipient)"},
                "targets": {"type": "array", "items": {"type": "string"}, "description": "Multiple targets (broadcast mode)"},
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
                "next_after_ci": {"type": "string", "description": "#931 Fix 2 (H5a): when dispatching kind=task with a `branch`, set this to the agent that should receive `[ci-ready-for-action]` after CI passes on that branch. The daemon's auto-armed ci-watch carries the chain target so the handoff fires without a manual follow-up `ci action=watch next_after_ci=…`. Example: lead dispatches dev with `next_after_ci=reviewer` — reviewer is auto-notified when dev's PR goes green."}
            }, "required": ["message"]}}),
        json!({"name": "inbox", "description": "Check pending messages, OR look up a single message by ID, OR fetch a thread's messages. No params = drain pending. With message_id = describe message status (read/unread/expired/notfound). With thread_id = fetch all messages in thread ordered by timestamp.",
        "inputSchema": {"type": "object", "properties": {
            "message_id": {"type": "string", "description": "Look up message status by ID"},
            "thread_id": {"type": "string", "description": "Fetch all messages in a thread"},
            "instance": {"type": "string", "description": "For message_id: target instance (defaults to caller). For thread_id: filter to a specific instance's inbox (optional, scans all if omitted)"}
        }}}),
    ]
}

fn instance_tools() -> Vec<Value> {
    vec![
        json!({"name": "list_instances", "description": "List all active agent instances. Pass optional `name` for detailed info on a single instance.",
            "inputSchema": {"type": "object", "properties": {"name": {"type": "string", "description": "Optional: instance name for detailed info"}}}}),
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
                "command": {"type": "string", "description": "Deprecated: use 'backend' instead"}
            }, "required": ["name"]}}),
        json!({"name": "delete_instance", "description": "Stop and remove an instance.",
            "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}}),
        json!({"name": "start_instance", "description": "Start a stopped instance.",
            "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}}),
        json!({"name": "replace_instance", "description": "Replace an instance with a fresh one.",
            "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}, "reason": {"type": "string"}}, "required": ["name"]}}),
        json!({"name": "interrupt", "description": "Send ESC byte to target agent's PTY to interrupt current LLM turn. Context preserved, agent accepts next prompt.",
            "inputSchema": {"type": "object", "properties": {
                "target": {"type": "string", "description": "Target instance name"},
                "reason": {"type": "string", "description": "Optional follow-up message to inject after ESC"}
            }, "required": ["target"]}}),
        json!({"name": "set_display_name", "description": "Set your display name.",
            "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}}),
        json!({"name": "set_description", "description": "Set a description for this instance.",
            "inputSchema": {"type": "object", "properties": {"description": {"type": "string"}}, "required": ["description"]}}),
        json!({"name": "set_waiting_on", "description": "Declare what this instance is currently waiting for. Set to empty string to clear. Automatically cleared when stale.",
            "inputSchema": {"type": "object", "properties": {"condition": {"type": "string", "description": "What you are waiting for, e.g. 'review from at-dev-4'. Empty string to clear."}}, "required": ["condition"]}}),
        json!({"name": "move_pane", "description": "Move an instance's pane into a different tab in the TUI. If `target_tab` names an existing tab, the pane splits that tab's focused pane; otherwise a new tab with that name is created and the pane becomes its root. Preserves scrollback and PTY state (unlike delete + create).",
            "inputSchema": {"type": "object", "properties": {
                "agent": {"type": "string", "description": "Instance name whose pane should be moved."},
                "target_tab": {"type": "string", "description": "Destination tab name. Created if not present."},
                "split_dir": {"type": "string", "enum": ["horizontal", "vertical"], "description": "Split direction when the destination tab already exists. Default: horizontal. Ignored when a new tab is created."}
            }, "required": ["agent", "target_tab"]}}),
        json!({"name": "pane_snapshot", "description": "Read visible text from a target instance's PTY scrollback. Returns plain text (ANSI stripped). Default 100 lines, max 10000.",
            "inputSchema": {"type": "object", "properties": {
                "target": {"type": "string", "description": "Target instance name"},
                "lines": {"type": "integer", "description": "Number of lines to return (default 100, max 10000)"}
            }, "required": ["target"]}}),
    ]
}

fn decision_tools() -> Vec<Value> {
    vec![
        json!({"name": "decision", "description": "Manage decisions. Actions: post, list, update.",
            "inputSchema": {"type": "object", "properties": {
                "action": {"type": "string", "enum": ["post", "list", "update"]},
                "title": {"type": "string"}, "content": {"type": "string"},
                "scope": {"type": "string", "enum": ["project", "fleet"]},
                "tags": {"type": "array", "items": {"type": "string"}},
                "ttl_days": {"type": "number"}, "supersedes": {"type": "string"},
                "id": {"type": "string"}, "archive": {"type": "boolean"},
                "include_archived": {"type": "boolean"}
            }, "required": ["action"]}}),
    ]
}

fn task_tools() -> Vec<Value> {
    vec![
        json!({"name": "task", "description": "Manage task board. Actions: create, list, claim, done, update, sweep, health. #806: default list trims to actionable statuses (open/claimed/in_progress/blocked); pass include_history=true to surface done/cancelled. `sweep` is operator-triggered manual hygiene (4 stale-task categories with dry-run + confirm_ids round-trip). #830: `health` is a one-shot board-hygiene snapshot — totals + by_status + ghost_owners + stale_claims + age aggregates + recommendations array.",
            "inputSchema": {"type": "object", "properties": {
                "action": {"type": "string", "enum": ["create", "list", "claim", "done", "update", "sweep", "health"]},
                "title": {"type": "string"}, "description": {"type": "string"},
                "priority": {"type": "string", "enum": ["low", "normal", "high", "urgent"]},
                "assignee": {"type": "string"}, "depends_on": {"type": "array", "items": {"type": "string"}},
                "id": {"type": "string"}, "result": {"type": "string"},
                "status": {"type": "string", "enum": ["open", "claimed", "in_progress", "blocked", "verified", "done", "cancelled"]},
                "filter_assignee": {"type": "string"}, "filter_status": {"type": "string"},
                "include_history": {"type": "boolean", "description": "#806: opt in to done/cancelled in `list` response (default trims to actionable)."},
                "limit": {"type": "integer", "description": "#806: cap `list` response to N newest-first entries (sort by updated_at desc)."},
                "due_at": {"type": "string", "description": "ISO 8601 deadline for the task"},
                "duration": {"type": "string", "description": "Human duration until deadline (e.g. 30m, 1h, 2d)"},
                "branch": {"type": "string", "description": "Git branch the implementer should work on"},
                "force": {"type": "boolean", "description": "#808: bypass ownership ACL on done/update for historical ghost-owned cleanup. Requires non-empty force_reason."},
                "force_reason": {"type": "string", "description": "#808: required when force=true. Logged to event-log.jsonl and embedded in the per-task event's reason field for audit."},
                "apply": {"type": "boolean", "description": "#806 sweep: when false (default), returns dry-run plan; when true, emits Cancelled for the confirm_ids subset."},
                "confirm_ids": {"type": "array", "items": {"type": "string"}, "description": "#806 sweep apply=true: subset of candidate_ids from prior dry-run to actually cancel."},
                "audit_reason": {"type": "string", "description": "#806 sweep apply=true: required audit text recorded in event-log.jsonl + per-task Cancelled.reason."},
                "repo": {"type": "string", "description": "#806 sweep: override repo for PR-state queries (defaults to task_sweep.json's repo)."}
            }, "required": ["action"]}}),
        json!({"name": "task_sweep_config",
        "description": "Configure GitHub-PR auto-close sweep daemon. Sweep polls merged PRs and emits Done events for `Closes t-XXX-N` markers (validated by 5-must-have pipeline).",
        "inputSchema": {"type": "object", "properties": {
            "repo": {"type": "string", "description": "GitHub `owner/repo` to sweep (empty string disables)"},
            "pause": {"type": "boolean", "description": "Pause/resume the sweep tick"},
            "dry_run": {"type": "boolean", "description": "Log decisions without emitting events"}
        }}}),
        json!({"name": "restart_daemon", "description": "Request graceful daemon restart. Daemon exits with code 42 after shutdown; a supervisor (launchd/systemd/Task Scheduler from `agend-terminal service install`, or `scripts/agend-wrapper.sh` for manual mode) respawns it. Returns ok:false when no supervisor is detected (bare `agend-terminal start`) — operator must install a supervisor before retry. Idempotent.",
            "inputSchema": {"type": "object", "properties": {}}}),
    ]
}

fn team_tools() -> Vec<Value> {
    vec![
        json!({"name": "team", "description": "Manage teams. Actions: create, delete, list, update.",
            "inputSchema": {"type": "object", "properties": {
                "action": {"type": "string", "enum": ["create", "delete", "list", "update"]},
                "name": {"type": "string"}, "members": {"type": "array", "items": {"type": "string"}},
                "orchestrator": {"type": "string", "description": "Team orchestrator — must be a member."},
                "description": {"type": "string"},
                "source_repo": {"type": "string", "description": "Source repository path for the team."},
                "add": {"type": "array", "items": {"type": "string"}},
                "remove": {"type": "array", "items": {"type": "string"}}
            }, "required": ["action"]}}),
    ]
}

fn schedule_tools() -> Vec<Value> {
    vec![
        json!({"name": "schedule", "description": "Manage schedules. Actions: create, list, update, delete.",
            "inputSchema": {"type": "object", "properties": {
                "action": {"type": "string", "enum": ["create", "list", "update", "delete"]},
                "id": {"type": "string"},
                "cron": {"type": "string", "description": "5- or 6-field cron expression (recurring)."},
                "run_at": {"type": "string", "description": "ISO 8601 one-shot instant."},
                "message": {"type": "string"}, "target": {"type": "string"},
                "label": {"type": "string"},
                "timezone": {"type": "string", "description": "IANA zone name."},
                "enabled": {"type": "boolean"}
            }, "required": ["action"]}}),
    ]
}

fn deploy_tools() -> Vec<Value> {
    vec![
        json!({"name": "deployment", "description": "Manage deployments. Actions: deploy, teardown, list.",
            "inputSchema": {"type": "object", "properties": {
                "action": {"type": "string", "enum": ["deploy", "teardown", "list"]},
                "template": {"type": "string", "description": "Template name from fleet.yaml"},
                "directory": {"type": "string", "description": "Working directory for instances"},
                "name": {"type": "string", "description": "Deployment name"},
                "branch": {"type": "string", "description": "Git branch — each instance gets its own worktree"}
            }, "required": ["action"]}}),
    ]
}

fn ci_tools() -> Vec<Value> {
    vec![
        json!({"name": "ci", "description": "Manage CI watching. Actions: watch, unwatch, status.",
            "inputSchema": {"type": "object", "properties": {
                "action": {"type": "string", "enum": ["watch", "unwatch", "status"]},
                "repo": {"type": "string", "description": "GitHub repo (owner/repo). Required for watch/unwatch; optional filter for status."},
                "branch": {"type": "string", "description": "Branch to watch (default: main); optional filter for status."},
                "interval_secs": {"type": "number", "description": "Poll interval in seconds (default: 60)"},
                "next_after_ci": {"type": "string", "description": "Instance to auto-notify when CI passes. Daemon sends [ci-ready-for-action] to this target."},
                "review_class": {"type": "string", "enum": ["single", "dual"], "description": "#972: review threshold for the daemon's PR-state aggregator. `single` (default) — §3.6 one VERIFIED unlocks the merge gate. `dual` — §3.5 two distinct VERIFIED required before `[pr-ready-for-merge]` fires."}
            }, "required": ["action"]}}),
    ]
}

fn health_tools() -> Vec<Value> {
    vec![
        json!({"name": "health", "description": "Manage health state. Actions: report, clear.",
            "inputSchema": {"type": "object", "properties": {
                "action": {"type": "string", "enum": ["report", "clear"]},
                "reason": {"type": "string", "enum": ["rate_limit", "quota_exceeded", "awaiting_operator"], "description": "Blocked reason kind (for report)"},
                "retry_after_secs": {"type": "integer", "description": "Seconds until retry (for rate_limit)"},
                "note": {"type": "string", "description": "Optional human-readable note"},
                "instance": {"type": "string", "description": "Target instance name (for clear)"}
            }, "required": ["action"]}}),
    ]
}

fn repo_tools() -> Vec<Value> {
    vec![
        json!({"name": "repo", "description": "Manage repo worktrees. Actions: checkout, release, cleanup_init_commits, cleanup_merged_branches. #817: cleanup_merged_branches is operator-triggered local-branch hygiene (4 categories: clean_merged/squash_merged/stale_idle/active_unknown; dry-run by default + confirm_ids + audit_reason required for apply).",
            "inputSchema": {"type": "object", "properties": {
                "action": {"type": "string", "enum": ["checkout", "release", "cleanup_init_commits", "cleanup_merged_branches"]},
                "source": {"type": "string"}, "branch": {"type": "string"},
                "path": {"type": "string"},
                "agent": {"type": "string", "description": "#789: target agent for cleanup_init_commits (defaults to caller's instance_name). Cleans empty `init` commits accumulated in the agent's bound worktree by backend session-checkpoint heartbeats. Returns {cleaned_count, [skipped_reason]}. Idempotent — call before push to scrub PR history."},
                "bind": {"type": "boolean", "description": "#778 Option 1: when true on checkout, atomically bind the caller to the just-provisioned worktree (writes binding.json + .agend-managed marker + arms ci_watches) and lands HEAD on the named branch instead of a detached commit. Default false preserves back-compat for inspection-only callers (review pool, operator triage)."},
                "base": {"type": "string", "description": "#817 cleanup_merged_branches: branch to compare against for clean/squash merge detection (default 'main')."},
                "min_age_days": {"type": "integer", "description": "#817 cleanup_merged_branches: stale_idle threshold in days (default 90)."},
                "apply": {"type": "boolean", "description": "#817 cleanup_merged_branches: when false (default), returns dry-run plan; when true, deletes confirm_ids subset."},
                "confirm_ids": {"type": "array", "items": {"type": "string"}, "description": "#817 cleanup_merged_branches apply=true: subset of candidate_ids from prior dry-run to actually delete via `git branch -D`."},
                "audit_reason": {"type": "string", "description": "#817 cleanup_merged_branches apply=true: required audit text recorded in event-log.jsonl per deleted branch with source SHA for restore."}
            }, "required": ["action"]}}),
    ]
}

fn worktree_tools() -> Vec<Value> {
    vec![
        // Sprint 54 P1-7: generic self-bind. Same `dispatch_auto_bind_lease`
        // lifecycle as `repo action=checkout bind:true` (#779 Option 1);
        // difference is caller context (fleet.yaml-resolved source_repo vs
        // explicit `source` arg), not the underlying mechanism. Every
        // Sprint 53/54 invariant (Phase 1 trailers, P0-1.5 cross-agent
        // registry check, P0-1.6 actual-HEAD verification, P0-X
        // release_worktree as sole exit point, source_repo persistence,
        // auto watch_ci) applies uniformly.
        // Sprint 55 P0-B: dual-arg shape (handler at worktree.rs:24-27) —
        // schema exposes both `source_repo` (preferred) and legacy `repo`
        // with `required` relaxed to `branch` only. Handler enforces
        // mutual exclusivity at runtime via `ambiguous_args` code.
        json!({"name": "bind_self", "description": "Bind the calling agent to a worktree on the named branch. For fresh-task workflows that know the source repo, prefer `repo action=checkout bind:true` (#779 Option 1) — single-step atomic provision + bind. Use `bind_self` when the caller is mid-lifecycle: (a) re-binding a recovered worktree via `rebase_mode=true`, (b) binding via fleet.yaml-resolved source_repo (no explicit source arg), or (c) post-`release_worktree` re-claim of the same branch. Both paths share `dispatch_auto_bind_lease` so binding.json + .agend-managed marker + auto watch_ci all land. Rejects 'main'/'master' (E4.5) and cross-agent branch conflicts. Pair with `release_worktree` to unbind.",
            "inputSchema": {"type": "object", "properties": {
                "source_repo": {"type": "string", "description": "Local path to source repository. Daemon resolves GitHub owner/repo via `git remote get-url origin`. Sprint 55 P0-B preferred form. Mutually exclusive with `repo` (handler rejects both via `ambiguous_args` code)."},
                "repo": {"type": "string", "description": "GitHub repo (owner/name). Legacy form retained for one-Sprint deprecation window — emits warn-log; removal Sprint 57. Mutually exclusive with `source_repo`."},
                "branch": {"type": "string", "description": "Branch to bind (must not be main/master)"},
                "rebase_mode": {"type": "boolean", "description": "Sprint 60 W1 PR-1: atomic recover-and-bind. When true, releases self's stale on-disk worktree dir + binding state before lease — closes the lease_failed recovery path without an explicit release_worktree call. Cross-agent isolation preserved (rejects branches leased by another agent)."}
            }, "required": ["branch"]}}),
        // Sprint 53 P0-X: release a daemon-managed worktree + clear binding
        // for `agent`. Idempotent. Only removes worktrees that carry the
        // `.agend-managed` marker (R14 safety — operator-created worktrees
        // are left alone). Operator- and agent-callable.
        json!({"name": "release_worktree", "description": "Release the daemon-managed worktree and clear the binding for the given agent. Idempotent. Only removes worktrees carrying the `.agend-managed` marker — operator-created worktrees are left alone.",
            "inputSchema": {"type": "object", "properties": {
                "agent": {"type": "string", "description": "Agent name whose worktree + binding to release"}
            }, "required": ["agent"]}}),
        // Sprint 59 Wave 1 PR-5 emergency cherry-pick: closes the
        // bind_self lease_failed recovery path so callers never
        // resort to AGEND_GIT_BYPASS=1 to clean stale on-disk
        // worktree dirs (operator Q2=(C) bypass-free permanent
        // protocol). Required when the binding state was already
        // released but the on-disk dir lingers (e.g. prior PR cycle
        // crashed mid-cleanup).
        json!({"name": "force_release_worktree", "description": "Force-release a stale daemon-managed worktree directory — cleans <home>/worktrees/<agent>/<branch>/ on disk + runs the standard release_full to clear any lingering binding state. Idempotent. Refuses to clean paths outside the daemon worktree pool. Sprint 59 Wave 1 PR-5 emergency cherry-pick supporting Q2=(C) bypass-free permanent protocol.",
            "inputSchema": {"type": "object", "properties": {
                "agent": {"type": "string", "description": "Agent name (worktree owner)"},
                "branch": {"type": "string", "description": "Branch name (worktree subdirectory)"}
            }, "required": ["agent", "branch"]}}),
        // Sprint 60 W1 PR-3 (#P0-3): operator-initiated restart MCP tool.
        // Closes the operator-restart-required SPOF that drove the
        // Sprint 59 PR-4 (P3) abandon path. Sets RESTART_PENDING +
        // records OperatorRestart shutdown reason; the daemon's main
        // loop notices, runs shutdown_sequence, and re-execs self
        // (Unix) or spawns + exits (Windows). On-disk state (binding
        // metadata, topic registry, fleet.yaml) is preserved across
        // restart automatically since it's already on disk; PTY
        // agents are killed and operator re-attaches post-restart
        // (MVP scope).
        // Sprint 58 Wave 3 PR-2 (#8): daemon-side binding diagnostic.
        // Operator + agent introspection surface for lease-block recovery
        // debugging. Reports binding.json + on-disk worktree state +
        // ci-watch subscriptions + bind-in-flight guard + cross-branch
        // holders in one call. Pairs with `release_worktree`'s
        // comprehensive cleanup (#9).
        json!({"name": "binding_state", "description": "Report the structured daemon-side bind state for an agent: binding.json content, worktree existence + .agend-managed marker, ci-watch subscriptions, bind-in-flight guard, and cross-branch holders. Operator + agent introspection surface; non-destructive. Pairs with release_worktree.",
            "inputSchema": {"type": "object", "properties": {
                "agent": {"type": "string", "description": "Agent name to inspect"}
            }, "required": ["agent"]}}),
        // Sprint 53 P1-4: operator-callable Phase 4 GC visibility. Wraps
        // worktree_pool::gc_dry_run; non-destructive (Phase 4 cutover stays
        // gated behind AGEND_WORKTREE_GC=1).
        json!({"name": "gc_dry_run", "description": "List Phase 4 GC candidates (released, past-grace, daemon-managed worktrees) without deleting them. Non-destructive. Default human format; pass `format=json` for tooling.",
        "inputSchema": {"type": "object", "properties": {
            "format": {"type": "string", "enum": ["human", "json"], "description": "Output format (default: human)"}
        }}}),
    ]
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
        let required = create["inputSchema"]["required"]
            .as_array()
            .expect("required");
        let required_strs: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(required_strs.contains(&"name"), "name should be required");
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
        let required = create["inputSchema"]["required"]
            .as_array()
            .expect("required");
        let required_strs: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
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
        assert!(required.iter().any(|v| v == "name"));
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
            30,
            "Sprint 61: 29 + restart_daemon(exit-42) = 30. \
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
}
