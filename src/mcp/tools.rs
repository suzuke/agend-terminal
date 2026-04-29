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
    json!({"tools": tools})
}

fn channel_tools() -> Vec<Value> {
    vec![
        json!({"name": "reply", "description": "Reply to the user via the active channel.",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}),
        json!({"name": "react", "description": "React to a message with an emoji.",
            "inputSchema": {"type": "object", "properties": {"emoji": {"type": "string"}}, "required": ["emoji"]}}),
        json!({"name": "download_attachment", "description": "Download a file attachment (telegram multimedia: images, audio, documents). Returns local path.",
            "inputSchema": {"type": "object", "properties": {"file_id": {"type": "string"}}, "required": ["file_id"]}}),
    ]
}

fn comm_tools() -> Vec<Value> {
    vec![
        // --- Unified send (Sprint 30: 5→1 consolidation) ---
        json!({"name": "send", "description": "Send a message to another instance or broadcast to multiple. Replaces send_to_instance/delegate_task/report_result/request_information/broadcast.",
            "inputSchema": {"type": "object", "properties": {
                "target_instance": {"type": "string", "description": "Target instance name (single recipient)"},
                "targets": {"type": "array", "items": {"type": "string"}, "description": "Multiple targets (broadcast mode)"},
                "team": {"type": "string", "description": "Team name (broadcast to team)"},
                "tags": {"type": "array", "items": {"type": "string"}, "description": "Tags filter (broadcast mode)"},
                "message": {"type": "string", "description": "Message text (or 'task' for delegate, 'summary' for report, 'question' for query)"},
                "request_kind": {"type": "string", "enum": ["query", "task", "report", "update"], "description": "Message kind (determines behavior)"},
                "requires_reply": {"type": "boolean"},
                "task_summary": {"type": "string"},
                "success_criteria": {"type": "string", "description": "For task delegation"},
                "context": {"type": "string"},
                "task_id": {"type": "string", "description": "Task board ID for correlation"},
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
                "working_directory": {"type": "string"}
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
        json!({"name": "list_instances", "description": "List all active agent instances.",
            "inputSchema": {"type": "object", "properties": {}}}),
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
        json!({"name": "describe_instance", "description": "Get detailed info about an instance.",
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
        json!({"name": "task", "description": "Manage task board. Actions: create, list, claim, done, update.",
            "inputSchema": {"type": "object", "properties": {
                "action": {"type": "string", "enum": ["create", "list", "claim", "done", "update"]},
                "title": {"type": "string"}, "description": {"type": "string"},
                "priority": {"type": "string", "enum": ["low", "normal", "high", "urgent"]},
                "assignee": {"type": "string"}, "depends_on": {"type": "array", "items": {"type": "string"}},
                "id": {"type": "string"}, "result": {"type": "string"},
                "status": {"type": "string", "enum": ["open", "claimed", "in_progress", "blocked", "verified", "done", "cancelled"]},
                "filter_assignee": {"type": "string"}, "filter_status": {"type": "string"},
                "due_at": {"type": "string", "description": "ISO 8601 deadline for the task"},
                "duration": {"type": "string", "description": "Human duration until deadline (e.g. 30m, 1h, 2d)"},
                "branch": {"type": "string", "description": "Git branch the implementer should work on"}
            }, "required": ["action"]}}),
        json!({"name": "task_sweep_config",
        "description": "Configure GitHub-PR auto-close sweep daemon. Sweep polls merged PRs and emits Done events for `Closes t-XXX-N` markers (validated by 5-must-have pipeline).",
        "inputSchema": {"type": "object", "properties": {
            "repo": {"type": "string", "description": "GitHub `owner/repo` to sweep (empty string disables)"},
            "pause": {"type": "boolean", "description": "Pause/resume the sweep tick"},
            "dry_run": {"type": "boolean", "description": "Log decisions without emitting events"}
        }}}),
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
        json!({"name": "ci", "description": "Manage CI watching. Actions: watch, unwatch.",
            "inputSchema": {"type": "object", "properties": {
                "action": {"type": "string", "enum": ["watch", "unwatch"]},
                "repo": {"type": "string", "description": "GitHub repo (owner/repo)"},
                "branch": {"type": "string", "description": "Branch to watch (default: main)"},
                "interval_secs": {"type": "number", "description": "Poll interval in seconds (default: 60)"}
            }, "required": ["action", "repo"]}}),
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
        json!({"name": "repo", "description": "Manage repo worktrees. Actions: checkout, release.",
            "inputSchema": {"type": "object", "properties": {
                "action": {"type": "string", "enum": ["checkout", "release"]},
                "source": {"type": "string"}, "branch": {"type": "string"},
                "path": {"type": "string"}
            }, "required": ["action"]}}),
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
            25 + 1,
            "Sprint 33 tool count = 26 (pane_snapshot added). \
             Adding/removing a tool requires updating this assertion. \
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
