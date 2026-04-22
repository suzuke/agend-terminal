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
    json!({"tools": tools})
}

fn channel_tools() -> Vec<Value> {
    vec![
        json!({"name": "reply", "description": "Reply to the user via Telegram.",
            "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}}),
        json!({"name": "react", "description": "React to a message with an emoji.",
            "inputSchema": {"type": "object", "properties": {"emoji": {"type": "string"}}, "required": ["emoji"]}}),
        json!({"name": "edit_message", "description": "Edit a previously sent message.",
            "inputSchema": {"type": "object", "properties": {"message_id": {"type": "string"}, "text": {"type": "string"}}, "required": ["message_id", "text"]}}),
        json!({"name": "download_attachment", "description": "Download a file attachment. Returns local path.",
            "inputSchema": {"type": "object", "properties": {"file_id": {"type": "string"}}, "required": ["file_id"]}}),
    ]
}

fn comm_tools() -> Vec<Value> {
    vec![
        json!({"name": "send_to_instance", "description": "Send a message to another instance.",
            "inputSchema": {"type": "object", "properties": {
                "instance_name": {"type": "string"}, "message": {"type": "string"},
                "request_kind": {"type": "string", "enum": ["query", "task", "report", "update"]},
                "requires_reply": {"type": "boolean"}, "task_summary": {"type": "string"},
                "correlation_id": {"type": "string"}, "working_directory": {"type": "string"}, "branch": {"type": "string"}
            }, "required": ["instance_name", "message"]}}),
        json!({"name": "delegate_task", "description": "Delegate a task to another instance (expects result report back).",
            "inputSchema": {"type": "object", "properties": {
                "target_instance": {"type": "string"}, "task": {"type": "string"},
                "success_criteria": {"type": "string"}, "context": {"type": "string"}
            }, "required": ["target_instance", "task"]}}),
        json!({"name": "report_result", "description": "Report results back to the delegating instance.",
            "inputSchema": {"type": "object", "properties": {
                "target_instance": {"type": "string"}, "summary": {"type": "string"},
                "correlation_id": {"type": "string"}, "artifacts": {"type": "string"}
            }, "required": ["target_instance", "summary"]}}),
        json!({"name": "request_information", "description": "Ask another instance a question (expects reply).",
            "inputSchema": {"type": "object", "properties": {
                "target_instance": {"type": "string"}, "question": {"type": "string"}, "context": {"type": "string"}
            }, "required": ["target_instance", "question"]}}),
        json!({"name": "broadcast", "description": "Send a message to multiple instances. Priority: team > targets > tags > all.",
            "inputSchema": {"type": "object", "properties": {
                "message": {"type": "string"}, "targets": {"type": "array", "items": {"type": "string"}},
                "team": {"type": "string"}, "tags": {"type": "array", "items": {"type": "string"}},
                "task_summary": {"type": "string"}, "request_kind": {"type": "string", "enum": ["query", "task", "update"]},
                "requires_reply": {"type": "boolean"}
            }, "required": ["message"]}}),
        json!({"name": "inbox", "description": "Check pending messages.",
            "inputSchema": {"type": "object", "properties": {}}}),
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
    ]
}

fn decision_tools() -> Vec<Value> {
    vec![
        json!({"name": "post_decision", "description": "Record a decision. scope='fleet' visible to all, 'project' to same working dir.",
            "inputSchema": {"type": "object", "properties": {
                "title": {"type": "string"}, "content": {"type": "string"},
                "scope": {"type": "string", "enum": ["project", "fleet"]},
                "tags": {"type": "array", "items": {"type": "string"}},
                "ttl_days": {"type": "number"}, "supersedes": {"type": "string"}
            }, "required": ["title", "content"]}}),
        json!({"name": "list_decisions", "description": "List active decisions.",
        "inputSchema": {"type": "object", "properties": {
            "include_archived": {"type": "boolean"}, "tags": {"type": "array", "items": {"type": "string"}}
        }}}),
        json!({"name": "update_decision", "description": "Update or archive a decision.",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "string"}, "content": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "ttl_days": {"type": "number"}, "archive": {"type": "boolean"}
            }, "required": ["id"]}}),
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
                "status": {"type": "string", "enum": ["open", "claimed", "done", "blocked", "cancelled"]},
                "filter_assignee": {"type": "string"}, "filter_status": {"type": "string"}
            }, "required": ["action"]}}),
    ]
}

fn team_tools() -> Vec<Value> {
    vec![
        json!({"name": "create_team", "description": "Create a named team from existing instances.",
            "inputSchema": {"type": "object", "properties": {
                "name": {"type": "string"}, "members": {"type": "array", "items": {"type": "string"}},
                "orchestrator": {"type": "string", "description": "Team orchestrator — must be a member. Receives team-level task routing."},
                "description": {"type": "string"}
            }, "required": ["name", "members"]}}),
        json!({"name": "delete_team", "description": "Delete a team.",
            "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]}}),
        json!({"name": "list_teams", "description": "List all teams.",
            "inputSchema": {"type": "object", "properties": {}}}),
        json!({"name": "update_team", "description": "Add or remove team members.",
            "inputSchema": {"type": "object", "properties": {
                "name": {"type": "string"}, "add": {"type": "array", "items": {"type": "string"}},
                "remove": {"type": "array", "items": {"type": "string"}},
                "orchestrator": {"type": "string", "description": "Change team orchestrator — must be a current member."}
            }, "required": ["name"]}}),
    ]
}

fn schedule_tools() -> Vec<Value> {
    vec![
        json!({"name": "create_schedule", "description": "Create a schedule to inject messages. Supply 'cron' for recurring or 'run_at' for one-shot (mutually exclusive). A one-shot auto-disables after firing or being detected as missed.",
            "inputSchema": {"type": "object", "properties": {
                "cron": {"type": "string", "description": "5- or 6-field cron expression (recurring)."},
                "run_at": {"type": "string", "description": "ISO 8601 one-shot instant. Either with offset ('2026-04-21T15:30:00+08:00') or naive ('2026-04-21T15:30:00') combined with 'timezone'. Must be in the future."},
                "message": {"type": "string"},
                "target": {"type": "string"},
                "label": {"type": "string"},
                "timezone": {"type": "string", "description": "IANA zone name. Defaults to the detected system timezone."}
            }, "required": ["message"]}}),
        json!({"name": "list_schedules", "description": "List all schedules. Each row carries a 'trigger' object: {kind:'cron',expr} or {kind:'once',at}.",
            "inputSchema": {"type": "object", "properties": {"target": {"type": "string"}}}}),
        json!({"name": "update_schedule", "description": "Update a schedule. 'cron' and 'run_at' remain mutually exclusive; supplying either replaces the trigger kind.",
            "inputSchema": {"type": "object", "properties": {
                "id": {"type": "string"},
                "cron": {"type": "string"},
                "run_at": {"type": "string"},
                "message": {"type": "string"},
                "target": {"type": "string"},
                "label": {"type": "string"},
                "timezone": {"type": "string"},
                "enabled": {"type": "boolean"}
            }, "required": ["id"]}}),
        json!({"name": "delete_schedule", "description": "Delete a schedule.",
            "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}}),
    ]
}

fn deploy_tools() -> Vec<Value> {
    vec![
        json!({"name": "deploy_template", "description": "Deploy a fleet template — creates instances and optionally a team.",
            "inputSchema": {"type": "object", "properties": {
                "template": {"type": "string", "description": "Template name from fleet.yaml"},
                "directory": {"type": "string", "description": "Working directory for instances"},
                "name": {"type": "string", "description": "Deployment name (defaults to template name)"},
                "branch": {"type": "string", "description": "Git branch — each instance gets its own worktree"}
            }, "required": ["template", "directory"]}}),
        json!({"name": "teardown_deployment", "description": "Tear down a deployment — stops instances and team.",
            "inputSchema": {"type": "object", "properties": {
                "name": {"type": "string"}
            }, "required": ["name"]}}),
        json!({"name": "list_deployments", "description": "List active template deployments.",
            "inputSchema": {"type": "object", "properties": {}}}),
    ]
}

fn ci_tools() -> Vec<Value> {
    vec![
        json!({"name": "watch_ci", "description": "Watch GitHub Actions CI for a repo. When CI fails, the error log is automatically injected to this agent.",
            "inputSchema": {"type": "object", "properties": {
                "repo": {"type": "string", "description": "GitHub repo (owner/repo)"},
                "branch": {"type": "string", "description": "Branch to watch (default: main)"},
                "interval_secs": {"type": "number", "description": "Poll interval in seconds (default: 60)"}
            }, "required": ["repo"]}}),
        json!({"name": "unwatch_ci", "description": "Stop watching CI for a repo.",
            "inputSchema": {"type": "object", "properties": {
                "repo": {"type": "string"}
            }, "required": ["repo"]}}),
    ]
}

fn repo_tools() -> Vec<Value> {
    vec![
        json!({"name": "checkout_repo", "description": "Mount another repo as read-only worktree.",
            "inputSchema": {"type": "object", "properties": {
                "source": {"type": "string"}, "branch": {"type": "string"}
            }, "required": ["source"]}}),
        json!({"name": "release_repo", "description": "Remove a checked-out repo worktree.",
            "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}),
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
            tools.len() >= 35,
            "expected at least 35 tools, got {}",
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
}
