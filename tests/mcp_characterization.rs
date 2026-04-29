//! Characterization tests — pin existing MCP tool behavior for safe refactoring.
//!
//! These tests exercise `agend-terminal mcp` as a subprocess (same as
//! `mcp_roundtrip.rs`) but focus on cross-cutting invariants rather than
//! individual tool happy paths.
//!
//! ## Slice 1: Validation Invariants
//!
//! Parameterized tests that verify missing-param rejection, `validate_name`
//! rejection, and self-send prevention across multiple tools sharing the
//! same validation pattern.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

// ---------------------------------------------------------------------------
// Test infrastructure (shared with future slices)
// ---------------------------------------------------------------------------

const INIT_REQUEST: &str = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;

fn binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop();
    path.pop();
    path.push("agend-terminal");
    path
}

fn temp_home(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-char-{}-{}-{}",
        std::process::id(),
        label,
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// Call a single MCP tool and return the result content text (or error).
fn call_tool(home: &std::path::Path, tool: &str, args: &Value) -> Value {
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": args }
    });
    let responses = mcp_session_in(home, "test-agent", &[INIT_REQUEST, &req.to_string()]);
    // responses[0] = initialize, responses[1] = tool call
    if responses.len() < 2 {
        return json!({"error": "no response"});
    }
    parse_tool_result(&responses[1])
}

/// Call a tool with a custom instance name.
fn call_tool_as(home: &std::path::Path, instance_name: &str, tool: &str, args: &Value) -> Value {
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": args }
    });
    let responses = mcp_session_in(home, instance_name, &[INIT_REQUEST, &req.to_string()]);
    if responses.len() < 2 {
        return json!({"error": "no response"});
    }
    parse_tool_result(&responses[1])
}

fn mcp_session_in(home: &std::path::Path, instance_name: &str, requests: &[&str]) -> Vec<Value> {
    let mut child = Command::new(binary())
        .args(["mcp"])
        .env("AGEND_HOME", home)
        .env("AGEND_INSTANCE_NAME", instance_name)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mcp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");

    for req in requests {
        writeln!(stdin, "{req}").expect("write");
    }
    drop(stdin);

    let reader = BufReader::new(stdout);
    let mut responses = Vec::new();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            responses.push(v);
        }
    }
    child.wait().ok();
    responses
}

/// Extract the text content from an MCP tool response, parsing the inner JSON
/// if the content is a JSON string.
fn parse_tool_result(response: &Value) -> Value {
    // MCP tool results come as: {"result":{"content":[{"type":"text","text":"..."}]}}
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    serde_json::from_str(text).unwrap_or_else(|_| json!({"raw": text}))
}

/// Assert that a tool result contains an "error" field with a substring.
fn assert_error_contains(result: &Value, substring: &str) {
    let err = result["error"]
        .as_str()
        .unwrap_or_else(|| panic!("expected error field in {result}, looking for '{substring}'"));
    assert!(
        err.contains(substring),
        "expected error containing '{substring}', got: '{err}'"
    );
}

// ---------------------------------------------------------------------------
// Slice 1: Validation Invariants
// ---------------------------------------------------------------------------

// -- 1a: Missing required parameter --
// Tools that require a specific param should return an error containing "missing".

/// (tool_name, args_without_required_param, expected_error_substring)
/// Some tools use `unwrap_or("")` + validate_name instead of explicit "missing"
/// checks, so we accept either "missing" or "empty" or "cannot be empty".
const MISSING_PARAM_CASES: &[(&str, &str, &str)] = &[
    ("send_to_instance", r#"{}"#, "missing"),
    ("send_to_instance", r#"{"message":"hi"}"#, "missing"),
    ("delegate_task", r#"{}"#, "missing"),
    ("delegate_task", r#"{"target_instance":"x"}"#, "missing"),
    ("report_result", r#"{}"#, "missing"),
    ("report_result", r#"{"target_instance":"x"}"#, "missing"),
    ("request_information", r#"{}"#, "missing"),
    (
        "request_information",
        r#"{"target_instance":"x"}"#,
        "missing",
    ),
    ("broadcast", r#"{}"#, "missing"),
    ("delete_instance", r#"{}"#, "missing"),
    ("start_instance", r#"{}"#, "missing"),
    ("download_attachment", r#"{}"#, "missing"),
    ("checkout_repo", r#"{}"#, "missing"),
    ("release_repo", r#"{}"#, "missing"),
    ("watch_ci", r#"{}"#, "missing"),
    ("unwatch_ci", r#"{}"#, "missing"),
];

#[test]
fn missing_required_param_returns_error() {
    let home = temp_home("missing-param");
    for (tool, args_str, expected) in MISSING_PARAM_CASES {
        let args: Value = serde_json::from_str(args_str).expect("parse args");
        let result = call_tool(&home, tool, &args);
        assert!(
            result["error"]
                .as_str()
                .is_some_and(|e| e.to_lowercase().contains(expected)),
            "tool={tool} args={args_str}: expected error containing '{expected}', got: {result}"
        );
    }
    std::fs::remove_dir_all(&home).ok();
}

/// Tools that use unwrap_or("") + validate_name: empty args → "cannot be empty"
#[test]
fn empty_name_param_returns_validation_error() {
    let home = temp_home("empty-name");
    let tools_with_default_empty = &[("describe_instance", "name"), ("replace_instance", "name")];
    for (tool, param) in tools_with_default_empty {
        let result = call_tool(&home, tool, &json!({}));
        assert!(
            result.get("error").is_some(),
            "tool={tool} param={param}: expected error for empty {param}, got: {result}"
        );
    }
    std::fs::remove_dir_all(&home).ok();
}

// -- 1b: validate_name rejection --
// Tools that call validate_name should reject names with path traversal or
// invalid characters.

const VALIDATE_NAME_TOOLS: &[(&str, &str)] = &[
    ("send_to_instance", "target"),
    ("delegate_task", "target_instance"),
    ("report_result", "target_instance"),
    ("request_information", "target_instance"),
    ("delete_instance", "name"),
    ("start_instance", "name"),
    ("describe_instance", "name"),
    ("replace_instance", "name"),
];

const BAD_NAMES: &[&str] = &["../escape", "a/b", ""];

#[test]
fn validate_name_rejects_bad_names() {
    let home = temp_home("bad-names");
    for (tool, param) in VALIDATE_NAME_TOOLS {
        for bad_name in BAD_NAMES {
            let args = json!({ *param: *bad_name });
            let result = call_tool(&home, tool, &args);
            assert!(
                result.get("error").is_some(),
                "tool={tool} param={param} name={bad_name}: expected error, got: {result}"
            );
        }
    }
    std::fs::remove_dir_all(&home).ok();
}

// -- 1c: Self-send prevention --
// send_to_instance should reject sending to the caller's own instance name.

#[test]
fn send_to_self_rejected() {
    let home = temp_home("self-send");
    let result = call_tool_as(
        &home,
        "my-agent",
        "send_to_instance",
        &json!({"target": "my-agent", "message": "hello"}),
    );
    assert_error_contains(&result, "cannot send to self");
    std::fs::remove_dir_all(&home).ok();
}

// -- 1d: validate_name accepts valid names --

#[test]
fn validate_name_accepts_good_names() {
    let home = temp_home("good-names");
    // These should NOT fail with a name validation error.
    // They may fail for other reasons (e.g., agent not found), but the error
    // should not be about the name itself.
    let good_names = &["agent-1", "my_agent", "dev"];
    for name in good_names {
        let result = call_tool(&home, "describe_instance", &json!({"name": name}));
        // describe_instance on a non-existent agent may return an error,
        // but it should NOT be a name validation error.
        if let Some(err) = result["error"].as_str() {
            assert!(
                !err.contains("invalid") && !err.contains("..") && !err.contains("traversal"),
                "name={name}: got unexpected validation error: {err}"
            );
        }
    }
    std::fs::remove_dir_all(&home).ok();
}

// -- 1e: Structural validation — working_directory and branch --

#[test]
fn working_directory_rejects_path_traversal() {
    let home = temp_home("wd-traversal");
    let result = call_tool(
        &home,
        "create_instance",
        &json!({"name": "test-wd", "backend": "claude", "working_directory": "../escape"}),
    );
    assert_error_contains(&result, "..");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn working_directory_rejects_relative_path() {
    let home = temp_home("wd-relative");
    let result = call_tool(
        &home,
        "create_instance",
        &json!({"name": "test-wd", "backend": "claude", "working_directory": "relative/path"}),
    );
    assert_error_contains(&result, "absolute");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn branch_rejects_invalid_names() {
    let home = temp_home("bad-branch");
    let bad_branches = &["../escape", "-leading-dash", "has space"];
    for bad in bad_branches {
        let result = call_tool(
            &home,
            "checkout_repo",
            &json!({"source": "/tmp/fake-repo", "branch": bad}),
        );
        assert!(
            result.get("error").is_some(),
            "branch={bad}: expected error, got: {result}"
        );
    }
    std::fs::remove_dir_all(&home).ok();
}

// ---------------------------------------------------------------------------
// Slice 2: API Unavailable Fallback
// ---------------------------------------------------------------------------
// When the daemon is not running, tools that call api::call should either
// fall back gracefully or return a clear error. No panics, no hangs.

#[test]
fn list_instances_falls_back_when_daemon_down() {
    let home = temp_home("no-daemon");
    let result = call_tool(&home, "list_instances", &json!({}));
    std::fs::remove_dir_all(&home).ok();
    // Should fall back to file-based list, returning an "instances" array
    assert!(
        result.get("instances").is_some(),
        "expected instances array fallback, got: {result}"
    );
}

#[test]
fn send_to_instance_falls_back_when_daemon_down() {
    let home = temp_home("send-fallback");
    // Target must exist in fleet.yaml for validation to pass.
    std::fs::write(
        home.join("fleet.yaml"),
        "instances:\n  other-agent:\n    backend: claude\n  sender:\n    backend: claude\n",
    )
    .ok();
    let result = call_tool_as(
        &home,
        "sender",
        "send_to_instance",
        &json!({"target": "other-agent", "message": "hello"}),
    );
    // Fallback: direct inbox delivery returns target + note about API unavailable
    assert_eq!(
        result["target"].as_str(),
        Some("other-agent"),
        "expected target in fallback response, got: {result}"
    );
    let note = result["note"]
        .as_str()
        .expect("expected 'note' field in fallback response");
    assert!(
        note.to_lowercase().contains("unavailable") || note.to_lowercase().contains("direct"),
        "expected unavailable/direct note, got: {note}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// Per-tool daemon-down behavior pinned to specific response shapes.
#[test]
fn describe_instance_returns_error_when_daemon_down() {
    let home = temp_home("desc-no-daemon");
    let result = call_tool(&home, "describe_instance", &json!({"name": "nonexistent"}));
    assert!(
        result.get("error").is_some(),
        "describe_instance should return error when daemon down, got: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn delete_instance_succeeds_via_mcp_handler_when_daemon_down() {
    let home = temp_home("del-no-daemon");
    let result = call_tool(&home, "delete_instance", &json!({"name": "nonexistent"}));
    // The MCP `delete_instance` handler's `api::call(DELETE)` fails silently
    // when the daemon is unreachable; the handler continues via direct disk
    // operations (fleet.yaml removal + `cleanup_working_dir`) which succeed
    // regardless of daemon state, so the call still reports success.
    assert_eq!(
        result["name"].as_str(),
        Some("nonexistent"),
        "delete_instance should succeed via MCP handler disk fallback, got: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn replace_instance_returns_error_when_daemon_down() {
    let home = temp_home("replace-no-daemon");
    let result = call_tool(&home, "replace_instance", &json!({"name": "nonexistent"}));
    // replace_instance needs daemon to list agents; without it, returns error or note
    assert!(
        result.get("error").is_some() || result.get("note").is_some(),
        "replace_instance should return error/note when daemon down, got: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

// ---------------------------------------------------------------------------
// Slice 3: Broadcast Target Resolution
// ---------------------------------------------------------------------------
// Priority: team > targets > (all). Self is always excluded.

/// Helper: create a team by writing directly to the teams store file.
fn setup_team(home: &std::path::Path, name: &str, members: &[&str]) {
    let store_path = home.join("teams.json");
    let members_json: Vec<Value> = members.iter().map(|m| json!(m)).collect();
    let store = if store_path.exists() {
        let content = std::fs::read_to_string(&store_path).unwrap_or_default();
        serde_json::from_str::<Value>(&content).unwrap_or(json!({"schema_version": 1, "teams": []}))
    } else {
        json!({"schema_version": 1, "teams": []})
    };
    let mut teams = store["teams"].as_array().cloned().unwrap_or_default();
    teams.push(json!({
        "name": name,
        "members": members_json,
        "created_at": "2026-01-01T00:00:00Z"
    }));
    let new_store = json!({"schema_version": 1, "teams": teams});
    std::fs::write(
        &store_path,
        serde_json::to_string_pretty(&new_store).expect("json"),
    )
    .expect("write teams.json");
}

#[test]
fn broadcast_with_team_resolves_to_team_members() {
    let home = temp_home("bcast-team");
    setup_team(&home, "my-team", &["agent-a", "agent-b"]);
    let result = call_tool_as(
        &home,
        "sender",
        "broadcast",
        &json!({"team": "my-team", "message": "hello team"}),
    );
    // Should resolve to team members, excluding self
    let sent = result["sent_to"].as_array();
    assert!(sent.is_some(), "expected sent_to array, got: {result}");
    let sent = sent.expect("sent_to");
    // sender is not in the team, so all members should be included
    let names: Vec<&str> = sent.iter().filter_map(|v| v.as_str()).collect();
    assert!(names.contains(&"agent-a"), "expected agent-a in {names:?}");
    assert!(names.contains(&"agent-b"), "expected agent-b in {names:?}");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn broadcast_with_targets_uses_explicit_list() {
    let home = temp_home("bcast-targets");
    let result = call_tool_as(
        &home,
        "sender",
        "broadcast",
        &json!({"targets": ["target-1", "target-2"], "message": "hello"}),
    );
    let sent = result["sent_to"].as_array().expect("sent_to array");
    let names: Vec<&str> = sent.iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"target-1"));
    assert!(names.contains(&"target-2"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn broadcast_excludes_self_from_targets() {
    let home = temp_home("bcast-self");
    let result = call_tool_as(
        &home,
        "me",
        "broadcast",
        &json!({"targets": ["me", "other"], "message": "hello"}),
    );
    let sent = result["sent_to"].as_array().expect("sent_to array");
    let names: Vec<&str> = sent.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        !names.contains(&"me"),
        "self should be excluded, got: {names:?}"
    );
    assert!(names.contains(&"other"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn broadcast_team_takes_priority_over_targets() {
    let home = temp_home("bcast-priority");
    setup_team(&home, "priority-team", &["team-member"]);
    // Pass both team and targets — team should win
    let result = call_tool_as(
        &home,
        "sender",
        "broadcast",
        &json!({
            "team": "priority-team",
            "targets": ["explicit-target"],
            "message": "hello"
        }),
    );
    let sent = result["sent_to"].as_array().expect("sent_to array");
    let names: Vec<&str> = sent.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        names.contains(&"team-member"),
        "team should take priority, got: {names:?}"
    );
    assert!(
        !names.contains(&"explicit-target"),
        "explicit target should be ignored when team is set, got: {names:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn broadcast_without_team_or_targets_returns_valid_response() {
    let home = temp_home("bcast-all");
    // No daemon running → list_agents returns empty → sent_to should be empty
    let result = call_tool_as(
        &home,
        "sender",
        "broadcast",
        &json!({"message": "hello everyone"}),
    );
    let sent = result["sent_to"].as_array().expect("sent_to array");
    // With no daemon, list_agents() returns empty, so sent_to is empty
    // The key invariant: it doesn't panic and returns a valid response
    assert!(
        result.get("count").is_some(),
        "expected count field, got: {result}"
    );
    // Self should never appear even if list_agents somehow included it
    let names: Vec<&str> = sent.iter().filter_map(|v| v.as_str()).collect();
    assert!(!names.contains(&"sender"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn broadcast_with_tags_falls_through_to_all() {
    // tags param is mentioned in code comment but not implemented.
    // When only tags is provided (no team, no targets), broadcast falls
    // through to the all-agents path. This test pins that behavior.
    let home = temp_home("bcast-tags");
    let result = call_tool_as(
        &home,
        "sender",
        "broadcast",
        &json!({"tags": ["backend"], "message": "hello tagged"}),
    );
    let sent = result["sent_to"].as_array().expect("sent_to array");
    // No daemon → list_agents empty → sent_to empty (tags ignored)
    assert!(
        result.get("count").is_some(),
        "expected count field, got: {result}"
    );
    // tags param had no effect — same as no-filter broadcast
    let names: Vec<&str> = sent.iter().filter_map(|v| v.as_str()).collect();
    assert!(!names.contains(&"sender"), "self should be excluded");
    std::fs::remove_dir_all(&home).ok();
}

// Regression: cross-instance tools must refuse to run without a resolvable
// sender identity, else receivers see `[from:]` with no originator.
#[test]
fn cross_instance_tools_reject_empty_sender() {
    let home = temp_home("no-sender");
    let cases: &[(&str, Value)] = &[
        (
            "send_to_instance",
            json!({"instance_name": "peer", "message": "hi"}),
        ),
        (
            "delegate_task",
            json!({"target_instance": "peer", "task": "x"}),
        ),
        (
            "report_result",
            json!({"target_instance": "peer", "summary": "done"}),
        ),
        (
            "request_information",
            json!({"target_instance": "peer", "question": "why"}),
        ),
        ("broadcast", json!({"message": "hi"})),
    ];
    for (tool, args) in cases {
        let result = call_tool_as(&home, "", tool, args);
        let err = result["error"].as_str().unwrap_or("");
        assert!(
            err.contains("AGEND_INSTANCE_NAME"),
            "tool={tool}: expected sender-identity error, got: {result}"
        );
    }
    std::fs::remove_dir_all(&home).ok();
}

// ---------------------------------------------------------------------------
// create_team MCP tool
// ---------------------------------------------------------------------------

#[test]
fn create_team_with_existing_instances() {
    let home = temp_home("create_team_ok");
    let result = call_tool(
        &home,
        "create_team",
        &json!({"name": "devs", "members": ["alice", "bob"]}),
    );
    assert_eq!(result["status"], "created");
    assert_eq!(result["name"], "devs");

    let listed = call_tool(&home, "list_teams", &json!({}));
    let teams = listed["teams"].as_array().expect("teams array");
    assert_eq!(teams.len(), 1);
    assert_eq!(teams[0]["name"], "devs");
    let members: Vec<&str> = teams[0]["members"]
        .as_array()
        .expect("members")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(members, vec!["alice", "bob"]);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn create_team_duplicate_rejects() {
    let home = temp_home("create_team_dup");
    call_tool(
        &home,
        "create_team",
        &json!({"name": "t", "members": ["a"]}),
    );
    let result = call_tool(
        &home,
        "create_team",
        &json!({"name": "t", "members": ["b"]}),
    );
    assert_error_contains(&result, "already exists");
    std::fs::remove_dir_all(&home).ok();
}

// ---------------------------------------------------------------------------
// delete_instance team cleanup + replace_instance preservation
// ---------------------------------------------------------------------------

#[test]
fn delete_instance_removes_from_team() {
    let home = temp_home("del_team_member");
    setup_team(&home, "devs", &["alice", "bob"]);
    call_tool(&home, "delete_instance", &json!({"name": "alice"}));
    let listed = call_tool(&home, "list_teams", &json!({}));
    let teams = listed["teams"].as_array().expect("teams");
    assert_eq!(teams.len(), 1);
    let members: Vec<&str> = teams[0]["members"]
        .as_array()
        .expect("members")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(members, vec!["bob"]);
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn delete_last_member_auto_deletes_team() {
    let home = temp_home("del_last_member");
    setup_team(&home, "solo", &["only-one"]);
    call_tool(&home, "delete_instance", &json!({"name": "only-one"}));
    let listed = call_tool(&home, "list_teams", &json!({}));
    let teams = listed["teams"].as_array().expect("teams");
    assert!(teams.is_empty(), "empty team should be auto-deleted");
    std::fs::remove_dir_all(&home).ok();
}

/// Regression: zombie instances (in the runtime registry but absent from
/// fleet.yaml) must be deletable even when fleet.yaml is down to a single
/// real instance under a configured channel. The original guard blocked
/// all deletes in that state, making torn-down deployment members
/// un-cleanable without a daemon restart.
#[test]
fn delete_zombie_succeeds_when_fleet_has_only_one_survivor() {
    let home = temp_home("del_zombie_with_channel");
    let fleet_yaml = r#"
channel:
  type: telegram
  bot_token_env: TOKEN
  group_id: -1
instances:
  survivor:
    backend: claude
"#;
    std::fs::write(home.join("fleet.yaml"), fleet_yaml).expect("write fleet.yaml");

    let result = call_tool(&home, "delete_instance", &json!({"name": "zombie"}));
    assert!(
        result.get("error").is_none(),
        "zombie (not in fleet.yaml) must be deletable, got: {result}"
    );
    assert_eq!(
        result["name"].as_str(),
        Some("zombie"),
        "handler must return the deleted name, got: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

/// The guard still protects real fleet members: deleting the last
/// fleet.yaml-tracked instance when a channel is configured must be
/// rejected, otherwise there's nothing left to receive channel traffic.
#[test]
fn delete_blocks_last_real_instance_when_channel_configured() {
    let home = temp_home("del_last_real");
    let fleet_yaml = r#"
channel:
  type: telegram
  bot_token_env: TOKEN
  group_id: -1
instances:
  survivor:
    backend: claude
"#;
    std::fs::write(home.join("fleet.yaml"), fleet_yaml).expect("write fleet.yaml");

    let result = call_tool(&home, "delete_instance", &json!({"name": "survivor"}));
    let err = result["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("last instance"),
        "guard must reject, got: {result}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn replace_instance_preserves_team_membership() {
    let home = temp_home("replace_team");
    setup_team(&home, "devs", &["alice"]);
    // replace_instance kills + respawns same name — team membership stays
    // because teams.json references by name, not by process identity.
    // The API call will fail (no daemon), but that's fine — we just verify
    // teams.json is untouched.
    call_tool(
        &home,
        "replace_instance",
        &json!({"name": "alice", "reason": "test"}),
    );
    let listed = call_tool(&home, "list_teams", &json!({}));
    let teams = listed["teams"].as_array().expect("teams");
    assert_eq!(teams.len(), 1);
    let members: Vec<&str> = teams[0]["members"]
        .as_array()
        .expect("members")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(members, vec!["alice"]);
    std::fs::remove_dir_all(&home).ok();
}

/// Regression pin: a freshly-created watch must record
/// `last_polled_at: null` so `check_ci_watches` treats it as
/// "never polled" and fires on the next daemon tick rather than
/// waiting the full `interval_secs`. PR #119 solved the same lag by
/// backdating file mtime — that was a workaround; the elegant fix
/// moves throttle state into the schema itself so external writers
/// (migration, hand-edit) can't silently re-introduce the lag by
/// stamping the mtime.
#[test]
fn watch_ci_writes_null_last_polled_at_for_prompt_first_poll() {
    let home = temp_home("watch-ci-polled-null");
    let resp = call_tool(
        &home,
        "watch_ci",
        &json!({
            "repo": "owner/repo",
            "branch": "main",
            "interval_secs": 60
        }),
    );
    assert_eq!(resp["watching"], true, "watch_ci must succeed: {resp:?}");

    // Handler writes under a hashed filename — enumerate rather than guess.
    let entries: Vec<_> = std::fs::read_dir(home.join("ci-watches"))
        .expect("ci-watches dir must exist")
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "expected exactly one watch file, got {entries:?}"
    );
    let content = std::fs::read_to_string(entries[0].path()).expect("watch file must be readable");
    let watch: serde_json::Value =
        serde_json::from_str(&content).expect("watch file must parse as JSON");
    assert!(
        watch["last_polled_at"].is_null(),
        "freshly-created watch must record last_polled_at: null (drives \
         first-poll-immediate in check_ci_watches), got: {watch:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}
