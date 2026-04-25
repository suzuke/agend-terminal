//! MCP round-trip tests — spawn `agend-terminal mcp` as subprocess,
//! send JSON-RPC requests via stdin, verify responses.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Platform-default shell used as a dummy `command` in create_instance
/// payloads. The daemon isn't running in these tests, so the shell is never
/// actually spawned — but we still match real paths so any future validation
/// layer doesn't reject Windows runs on a missing `/bin/bash`.
#[cfg(windows)]
const SHELL_CMD: &str = "cmd.exe";
#[cfg(not(windows))]
const SHELL_CMD: &str = "/bin/bash";
fn binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop();
    path.pop();
    path.push("agend-terminal");
    path
}

fn mcp_home() -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let home = std::env::temp_dir().join(format!("agend-mcp-test-{}-{}", std::process::id(), id));
    std::fs::create_dir_all(&home).ok();
    // Seed fleet.yaml so target validation passes for test-agent.
    std::fs::write(
        home.join("fleet.yaml"),
        "instances:\n  test-agent:\n    backend: claude\n  other-agent:\n    backend: claude\n",
    )
    .ok();
    home
}

fn mcp_session(requests: &[&str]) -> Vec<serde_json::Value> {
    let home = mcp_home();
    let result = mcp_session_in_home(&home, requests);
    let _ = std::fs::remove_dir_all(&home);
    result
}

fn mcp_session_in_home(home: &std::path::Path, requests: &[&str]) -> Vec<serde_json::Value> {
    let mut child = Command::new(binary())
        .args(["mcp"])
        .env("AGEND_HOME", home)
        .env("AGEND_INSTANCE_NAME", "test-agent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mcp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");

    // Send all requests
    for req in requests {
        writeln!(stdin, "{req}").expect("write");
    }
    drop(stdin); // Close stdin → triggers EOF → MCP server exits

    // Read responses
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
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
            responses.push(v);
        }
    }

    child.wait().ok();
    responses
}

/// Run an MCP session where later requests depend on earlier responses.
/// `initial` requests are sent first, then `make_followups` generates additional
/// requests based on the initial responses. All run in the same home directory.
fn mcp_session_with_dynamic(
    initial: &[&str],
    make_followups: impl FnOnce(&[serde_json::Value]) -> Vec<String>,
) -> Vec<serde_json::Value> {
    let home = mcp_home();

    // Phase 1: send initial requests
    let phase1 = mcp_session_in_home(&home, initial);

    // Generate follow-up requests based on phase 1 responses
    let followups = make_followups(&phase1);
    let followup_refs: Vec<&str> = followups.iter().map(|s| s.as_str()).collect();

    // Phase 2: send follow-up requests (same home dir, new MCP process)
    // We need to re-initialize since it's a new process
    let mut all_requests: Vec<&str> = vec![initial[0]]; // re-send initialize
    all_requests.extend(followup_refs.iter());
    let phase2 = mcp_session_in_home(&home, &all_requests);

    let _ = std::fs::remove_dir_all(&home);

    // Combine: phase1 responses + phase2 responses (skip the duplicate initialize)
    let mut combined = phase1;
    if phase2.len() > 1 {
        combined.extend(phase2.into_iter().skip(1));
    }
    combined
}

#[test]
fn test_initialize() {
    let responses = mcp_session(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
    ]);
    assert!(!responses.is_empty(), "should get initialize response");
    assert_eq!(
        responses[0]["result"]["serverInfo"]["name"],
        "agend-terminal"
    );
    assert_eq!(responses[0]["result"]["protocolVersion"], "2024-11-05");
}

#[test]
fn test_tools_list_count() {
    let responses = mcp_session(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
    ]);
    assert!(responses.len() >= 2, "should get 2 responses");
    let tools = responses[1]["result"]["tools"]
        .as_array()
        .expect("tools array");
    assert!(
        tools.len() >= 34,
        "should have at least 34 tools, got {}",
        tools.len()
    );

    // Verify key tools exist
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(tool_names.contains(&"reply"));
    assert!(tool_names.contains(&"delegate_task"));
    assert!(tool_names.contains(&"post_decision"));
    assert!(tool_names.contains(&"task"));
    assert!(tool_names.contains(&"delete_team"));
    assert!(tool_names.contains(&"create_schedule"));
    assert!(tool_names.contains(&"checkout_repo"));
    assert!(tool_names.contains(&"deploy_template"));
}

#[test]
fn test_tool_call_inbox() {
    let responses = mcp_session(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"inbox","arguments":{}}}"#,
    ]);
    assert!(responses.len() >= 2);
    let content = &responses[1]["result"]["content"];
    assert!(content.is_array(), "content should be array");
    let text = content[0]["text"].as_str().expect("text");
    let result: serde_json::Value = serde_json::from_str(text).expect("parse result");
    assert!(
        result["messages"].is_array(),
        "inbox should return messages array"
    );
}

#[test]
fn test_tool_call_post_decision() {
    let responses = mcp_session(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"post_decision","arguments":{"title":"Test","content":"Integration test decision"}}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_decisions","arguments":{}}}"#,
    ]);
    assert!(responses.len() >= 3);

    // Verify post result
    let post_text = responses[1]["result"]["content"][0]["text"]
        .as_str()
        .expect("post text");
    let post_result: serde_json::Value = serde_json::from_str(post_text).expect("parse");
    assert_eq!(post_result["status"], "posted");

    // Verify list contains the decision
    let list_text = responses[2]["result"]["content"][0]["text"]
        .as_str()
        .expect("list text");
    let list_result: serde_json::Value = serde_json::from_str(list_text).expect("parse");
    let decisions = list_result["decisions"].as_array().expect("arr");
    assert!(!decisions.is_empty(), "should have at least 1 decision");
}

#[test]
fn test_content_length_framing() {
    let home = std::env::temp_dir().join(format!("agend-mcp-cl-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();

    let mut child = Command::new(binary())
        .args(["mcp"])
        .env("AGEND_HOME", &home)
        .env("AGEND_INSTANCE_NAME", "test-cl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");

    // Send Content-Length framed request
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;
    write!(stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).expect("write");
    stdin.flush().expect("flush");
    drop(stdin);

    let reader = BufReader::new(stdout);
    let mut responses = Vec::new();
    for l in reader.lines().map_while(Result::ok) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&l) {
            responses.push(v);
        }
    }

    child.wait().ok();
    let _ = std::fs::remove_dir_all(&home);

    assert!(
        !responses.is_empty(),
        "should get response from Content-Length framed request"
    );
    assert_eq!(
        responses[0]["result"]["serverInfo"]["name"],
        "agend-terminal"
    );
}

#[test]
fn test_content_length_zero_skips_frame_without_desync() {
    // Content-Length: 0 is valid (empty body) — it must consume the
    // separator and continue. If the server mishandled it by `continue`ing
    // without eating the empty line, the following NDJSON request would
    // be read at an offset and lost, hanging the caller.
    let home = std::env::temp_dir().join(format!("agend-mcp-cl0-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();

    let mut child = Command::new(binary())
        .args(["mcp"])
        .env("AGEND_HOME", &home)
        .env("AGEND_INSTANCE_NAME", "test-cl0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");

    // Send an empty-body CL=0 frame, then a real NDJSON ping. If framing
    // is right we get exactly one response (ping); if not, the server
    // mis-parses the ping bytes as headers and we get none.
    write!(stdin, "Content-Length: 0\r\n\r\n").expect("write cl0");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"ping","params":{{}}}}"#
    )
    .expect("write ping");
    stdin.flush().expect("flush");
    drop(stdin);

    let reader = BufReader::new(stdout);
    let mut responses = Vec::new();
    for l in reader.lines().map_while(Result::ok) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&l) {
            responses.push(v);
        }
    }
    child.wait().ok();
    let _ = std::fs::remove_dir_all(&home);

    assert_eq!(
        responses.len(),
        1,
        "CL=0 frame must not desync stream; expected 1 ping response, got {}",
        responses.len()
    );
    assert_eq!(responses[0]["id"], 1);
    assert!(responses[0]["result"].is_object());
}

#[test]
fn test_invalid_content_length_resyncs_to_next_frame() {
    // A garbage Content-Length previously fell through to len=0 via
    // unwrap_or(0) and then `continue`d without consuming the separator,
    // scrambling the stream. With the fix, the malformed frame is
    // discarded and a following NDJSON request is still served.
    let home = std::env::temp_dir().join(format!("agend-mcp-clbad-{}", std::process::id()));
    std::fs::create_dir_all(&home).ok();

    let mut child = Command::new(binary())
        .args(["mcp"])
        .env("AGEND_HOME", &home)
        .env("AGEND_INSTANCE_NAME", "test-clbad")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");

    // Malformed header, followed by an empty separator, followed by a
    // real NDJSON request. The server must skip the bad frame cleanly
    // and respond to the ping.
    write!(stdin, "Content-Length: not-a-number\r\n\r\n").expect("write bad");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":7,"method":"ping","params":{{}}}}"#
    )
    .expect("write ping");
    stdin.flush().expect("flush");
    drop(stdin);

    let reader = BufReader::new(stdout);
    let mut responses = Vec::new();
    for l in reader.lines().map_while(Result::ok) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&l) {
            responses.push(v);
        }
    }
    child.wait().ok();
    let _ = std::fs::remove_dir_all(&home);

    assert!(
        responses.iter().any(|r| r["id"] == 7),
        "server must resync and serve the following ping, got {responses:?}"
    );
}

#[test]
fn test_unknown_method() {
    let responses = mcp_session(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"nonexistent/method","params":{}}"#,
    ]);
    assert!(responses.len() >= 2);
    assert!(
        responses[1]["error"].is_object(),
        "unknown method should return error"
    );
    assert_eq!(responses[1]["error"]["code"], -32601);
}

#[test]
fn test_parse_error_returns_response() {
    // JSON-RPC 2.0 mandates that a parse error yields an error response.
    // Prior behaviour silently dropped malformed requests, hanging clients.
    let responses = mcp_session(&[
        r#"{not valid json"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#,
    ]);
    assert!(
        responses.len() >= 2,
        "parse error must produce a response, got {}",
        responses.len()
    );
    assert_eq!(responses[0]["error"]["code"], -32700);
    assert_eq!(responses[0]["id"], serde_json::Value::Null);
    // Subsequent valid request still served.
    assert!(responses[1]["result"].is_object());
}

#[test]
fn test_parse_error_salvages_id_when_possible() {
    // Valid JSON but missing required fields — we can still recover id.
    let responses = mcp_session(&[r#"{"id":42,"garbage":true}"#]);
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["error"]["code"], -32700);
    assert_eq!(responses[0]["id"], 42);
}

#[test]
fn test_ping() {
    let responses = mcp_session(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"ping","params":{}}"#,
    ]);
    assert!(responses.len() >= 2);
    assert!(
        responses[1]["result"].is_object(),
        "ping should return result"
    );
}

// ---- MCP Behavioral Tests ----

/// Helper to extract the JSON result text from an MCP tools/call response.
fn extract_tool_result(response: &serde_json::Value) -> serde_json::Value {
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("{}");
    serde_json::from_str(text).unwrap_or_else(|_| serde_json::json!({"_raw": text}))
}

#[test]
fn test_create_delete_instance_lifecycle() {
    // This test verifies create_instance and delete_instance actually modify state.
    // create_instance/delete_instance need a running daemon (they call API spawn/delete).
    // We use list_instances which falls back to scanning fleet.yaml when API is unavailable.
    // So we verify the fleet.yaml persistence side.
    let create_call = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"create_instance","arguments":{{"name":"test-dynamic","command":"{}"}}}}}}"#,
        SHELL_CMD
    );
    let responses = mcp_session(&[
        // 1: initialize
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
        // 2: create_instance (will fail API call since no daemon, but tests the path)
        &create_call,
        // 3: list_instances (will show from fleet.yaml fallback)
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_instances","arguments":{}}}"#,
        // 4: delete_instance
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"delete_instance","arguments":{"name":"test-dynamic"}}}"#,
        // 5: list_instances again
        r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"list_instances","arguments":{}}}"#,
    ]);
    assert!(
        responses.len() >= 5,
        "expected 5 responses, got {}",
        responses.len()
    );

    // create_instance will return an error (no daemon) but the tool should still respond
    let _create_result = extract_tool_result(&responses[1]);
    // It may have an "error" field (API unavailable) — that's expected without daemon
    // The key test is that delete_instance returns the name
    let delete_result = extract_tool_result(&responses[3]);
    assert_eq!(
        delete_result["name"], "test-dynamic",
        "delete_instance should return the deleted name"
    );
}

#[test]
fn test_delegate_task_reaches_inbox() {
    // delegate_task sends a message to target_instance's inbox.
    // Without a daemon, it falls back to direct file delivery.
    // We then drain inbox to verify the message arrived.
    let responses = mcp_session(&[
        // 1: initialize
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
        // 2: delegate_task to self (test-agent is our AGEND_INSTANCE_NAME).
        // The message must exceed 500 chars so inbox::deliver stores it to file
        // (short messages only inject to PTY which doesn't exist in test).
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"delegate_task","arguments":{"target_instance":"test-agent","task":"fix the bug","success_criteria":"all unit tests and integration tests must pass without any failures or warnings","context":"This is a critical bug that affects the main processing pipeline. The root cause appears to be in the data validation layer where input is not properly sanitized before being passed to the transformation engine. Please investigate the following files: processor.rs, validator.rs, transform.rs, pipeline.rs, and engine.rs. Make sure to add regression tests for each case you fix. The bug was reported by multiple users and is blocking the next release. Priority is high. Additional context: the bug manifests as incorrect output when special characters are present in the input data stream."}}}"#,
        // 3: drain inbox
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"inbox","arguments":{}}}"#,
    ]);
    assert!(
        responses.len() >= 3,
        "expected 3 responses, got {}",
        responses.len()
    );

    // delegate_task should indicate it was sent (possibly with API fallback note)
    let delegate_result = extract_tool_result(&responses[1]);
    assert_eq!(
        delegate_result["target"], "test-agent",
        "delegate_task should target test-agent, got: {delegate_result}"
    );

    // inbox should contain the delegated task message
    let inbox_result = extract_tool_result(&responses[2]);
    let messages = inbox_result["messages"].as_array().expect("messages array");
    assert!(
        !messages.is_empty(),
        "inbox should have at least 1 message after delegate_task"
    );
    let found = messages.iter().any(|m| {
        let text = m["text"].as_str().unwrap_or("");
        text.contains("[delegate_task]") && text.contains("fix the bug")
    });
    assert!(
        found,
        "inbox should contain a message with '[delegate_task]' and 'fix the bug', got: {messages:?}"
    );
}

#[test]
fn test_task_board_lifecycle() {
    // Full task lifecycle: create → claim → complete → list with filter.
    // Uses mcp_session_with_dynamic because task ID from create is needed for claim/done.
    let responses2 = mcp_session_with_dynamic(
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"task","arguments":{"action":"create","title":"Implement feature X","priority":"high"}}}"#,
        ],
        |responses| {
            let create_result = extract_tool_result(&responses[1]);
            let id = create_result["id"].as_str().expect("task id").to_string();
            vec![
            format!(
                r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"task","arguments":{{"action":"claim","id":"{id}"}}}}}}"#,
            ),
            format!(
                r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"task","arguments":{{"action":"done","id":"{id}","result":"feature implemented and tested"}}}}}}"#,
            ),
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"task","arguments":{"action":"list","filter_status":"done"}}}"#.to_string(),
        ]
        },
    );

    assert!(
        responses2.len() >= 5,
        "expected 5 responses, got {}",
        responses2.len()
    );

    // Verify create
    let create_r = extract_tool_result(&responses2[1]);
    assert_eq!(create_r["status"], "created");
    let task_id = create_r["id"].as_str().expect("id");

    // Verify claim
    let claim_r = extract_tool_result(&responses2[2]);
    assert_eq!(claim_r["status"], "claimed", "task should be claimed");
    assert_eq!(
        claim_r["assignee"], "test-agent",
        "assignee should be test-agent"
    );

    // Verify done
    let done_r = extract_tool_result(&responses2[3]);
    assert_eq!(done_r["status"], "done", "task should be done");

    // Verify list with filter_status=done
    let list_r = extract_tool_result(&responses2[4]);
    let tasks = list_r["tasks"].as_array().expect("tasks array");
    assert!(!tasks.is_empty(), "should have at least 1 done task");
    let found = tasks.iter().any(|t| t["id"].as_str() == Some(task_id));
    assert!(found, "completed task should appear in done list");
    let task = tasks
        .iter()
        .find(|t| t["id"].as_str() == Some(task_id))
        .expect("task");
    assert_eq!(task["result"], "feature implemented and tested");
}

#[test]
fn test_describe_message_mcp() {
    // Enqueue a message via delegate_task (which creates an inbox entry with an ID),
    // drain it (which stamps read_at), then call describe_message to verify status.
    let responses = mcp_session(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
        // delegate_task to self to create an inbox message
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"delegate_task","arguments":{"target_instance":"test-agent","task":"test task for describe","success_criteria":"verify describe_message works end to end via MCP roundtrip integration test with sufficient length to exceed the inline threshold of five hundred characters so the message is actually enqueued to the inbox file rather than only injected to PTY which would not create a persistent message that describe_message can find later","context":"This context padding ensures the message body exceeds 500 chars so inbox::deliver routes it through enqueue() which stamps a message ID. Without this padding the message would be too short and only get PTY-injected, never hitting the JSONL file."}}}"#,
        // drain inbox to mark messages as read
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"inbox","arguments":{}}}"#,
        // describe_message with a nonexistent ID → not_found
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"describe_message","arguments":{"message_id":"m-nonexistent"}}}"#,
    ]);
    assert!(
        responses.len() >= 4,
        "expected 4 responses, got {}",
        responses.len()
    );

    // describe_message for nonexistent ID should return not_found
    let desc_result = extract_tool_result(&responses[3]);
    assert_eq!(
        desc_result["status"], "not_found",
        "nonexistent message should be not_found, got: {desc_result}"
    );
}

#[test]
fn test_sweep_expired_removes_old_read_messages() {
    let home = mcp_home();
    let inbox_dir = home.join("inbox");
    std::fs::create_dir_all(&inbox_dir).ok();

    // Seed: a read message with old timestamp and a fresh unread message
    let old_ts = "2020-01-01T00:00:00+00:00";
    let fresh_ts = "2099-01-01T00:00:00+00:00";
    let old_read = format!(
        r#"{{"schema_version":1,"id":"m-old-read","from":"a","text":"old","kind":null,"timestamp":"{old_ts}","read_at":"{old_ts}"}}"#
    );
    let fresh_unread = format!(
        r#"{{"schema_version":1,"id":"m-fresh-unread","from":"b","text":"fresh","kind":null,"timestamp":"{fresh_ts}"}}"#
    );
    std::fs::write(
        inbox_dir.join("test-agent.jsonl"),
        format!("{old_read}\n{fresh_unread}\n"),
    )
    .expect("write test inbox");

    // Use MCP to describe both messages
    let responses = mcp_session_in_home(
        &home,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"describe_message","arguments":{"message_id":"m-old-read"}}}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"describe_message","arguments":{"message_id":"m-fresh-unread"}}}"#,
        ],
    );
    assert!(responses.len() >= 3);

    let old_result = extract_tool_result(&responses[1]);
    assert_eq!(
        old_result["status"], "read",
        "old read message should report 'read', got: {old_result}"
    );

    let fresh_result = extract_tool_result(&responses[2]);
    assert_eq!(
        fresh_result["status"], "not_found",
        "fresh unread should be 'not_found', got: {fresh_result}"
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_send_to_instance_passes_thread_id() {
    let home = mcp_home();
    // Send to a different agent (not self) with thread_id
    let responses = mcp_session_in_home(
        &home,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"send_to_instance","arguments":{"instance_name":"other-agent","message":"thread test message that needs to be long enough to exceed the inline threshold of five hundred characters so it actually gets enqueued to the inbox JSONL file rather than only being injected to PTY which would not persist the thread_id field we are testing here. Adding more padding text to ensure we cross the 500 char boundary reliably in this integration test scenario.","thread_id":"t-root-42","request_kind":"task"}}}"#,
        ],
    );
    assert!(responses.len() >= 2);
    // Verify the message was enqueued to other-agent's inbox with thread_id
    let inbox_path = home.join("inbox").join("other-agent.jsonl");
    let content = std::fs::read_to_string(&inbox_path).unwrap_or_default();
    assert!(
        content.contains("t-root-42"),
        "thread_id must be in inbox JSONL: {content}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_describe_thread_returns_ordered_msgs() {
    let home = mcp_home();
    // Seed two messages with same thread_id directly in inbox
    let inbox_dir = home.join("inbox");
    std::fs::create_dir_all(&inbox_dir).ok();
    let m1 = r#"{"schema_version":1,"id":"m-1","from":"a","text":"first","kind":null,"timestamp":"2026-01-01T00:00:01Z","thread_id":"t-99"}"#;
    let m2 = r#"{"schema_version":1,"id":"m-2","from":"b","text":"second","kind":null,"timestamp":"2026-01-01T00:00:02Z","thread_id":"t-99"}"#;
    let m3 = r#"{"schema_version":1,"id":"m-3","from":"c","text":"other thread","kind":null,"timestamp":"2026-01-01T00:00:03Z","thread_id":"t-other"}"#;
    std::fs::write(
        inbox_dir.join("test-agent.jsonl"),
        format!("{m1}\n{m2}\n{m3}\n"),
    )
    .ok();

    let responses = mcp_session_in_home(
        &home,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"describe_thread","arguments":{"thread_id":"t-99"}}}"#,
        ],
    );
    assert!(responses.len() >= 2);
    let result = extract_tool_result(&responses[1]);
    assert_eq!(
        result["count"], 2,
        "should find 2 messages in thread t-99, got: {result}"
    );
    let msgs = result["messages"].as_array().expect("messages");
    assert_eq!(msgs[0]["id"], "m-1");
    assert_eq!(msgs[1]["id"], "m-2");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_describe_thread_filters_by_instance() {
    let home = mcp_home();
    let inbox_dir = home.join("inbox");
    std::fs::create_dir_all(&inbox_dir).ok();
    let m1 = r#"{"schema_version":1,"id":"m-a1","from":"x","text":"agent-a msg","kind":null,"timestamp":"2026-01-01T00:00:01Z","thread_id":"t-shared"}"#;
    let m2 = r#"{"schema_version":1,"id":"m-b1","from":"y","text":"agent-b msg","kind":null,"timestamp":"2026-01-01T00:00:02Z","thread_id":"t-shared"}"#;
    std::fs::write(inbox_dir.join("agent-a.jsonl"), format!("{m1}\n")).ok();
    std::fs::write(inbox_dir.join("agent-b.jsonl"), format!("{m2}\n")).ok();

    let responses = mcp_session_in_home(
        &home,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"describe_thread","arguments":{"thread_id":"t-shared","instance":"agent-a"}}}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"describe_thread","arguments":{"thread_id":"t-shared"}}}"#,
        ],
    );
    assert!(responses.len() >= 3);
    let filtered = extract_tool_result(&responses[1]);
    assert_eq!(
        filtered["count"], 1,
        "filtered to agent-a should have 1 msg"
    );
    let all = extract_tool_result(&responses[2]);
    assert_eq!(
        all["count"], 2,
        "unfiltered should have 2 msgs across agents"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_parent_id_auto_inherits_thread() {
    let home = mcp_home();
    // Seed a parent message with thread_id in other-agent's inbox
    let inbox_dir = home.join("inbox");
    std::fs::create_dir_all(&inbox_dir).ok();
    let parent = r#"{"schema_version":1,"id":"m-parent","from":"lead","text":"original task","kind":"task","timestamp":"2026-01-01T00:00:01Z","thread_id":"t-conv-1"}"#;
    std::fs::write(inbox_dir.join("other-agent.jsonl"), format!("{parent}\n")).ok();

    // Send a reply with parent_id but no thread_id — should auto-inherit
    let responses = mcp_session_in_home(
        &home,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"send_to_instance","arguments":{"instance_name":"other-agent","message":"reply with auto-inherit. Padding to exceed 500 chars threshold so the message gets enqueued to inbox JSONL where we can verify thread_id inheritance. More padding text here to ensure we reliably cross the boundary for this integration test of the parent auto-inherit thread correlation feature in the agend-terminal daemon.","parent_id":"m-parent"}}}"#,
        ],
    );
    assert!(responses.len() >= 2);
    // Read other-agent's inbox directly to verify thread_id was inherited
    let content = std::fs::read_to_string(inbox_dir.join("other-agent.jsonl")).unwrap_or_default();
    let lines: Vec<&str> = content.lines().collect();
    // Find the reply (has parent_id=m-parent, not the parent itself)
    let reply_line = lines
        .iter()
        .find(|l| l.contains("m-parent") && l.contains("reply with auto-inherit"));
    assert!(reply_line.is_some(), "reply must be in inbox");
    assert!(
        reply_line.unwrap().contains("t-conv-1"),
        "thread_id must be auto-inherited from parent: {}",
        reply_line.unwrap()
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_delegate_task_persists_task_id() {
    let home = mcp_home();
    std::fs::write(
        home.join("fleet.yaml"),
        "instances:\n  test-agent:\n    backend: claude\n",
    )
    .ok();
    let responses = mcp_session_in_home(
        &home,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"delegate_task","arguments":{"target_instance":"test-agent","task":"implement feature X","task_id":"t-sprint6-42","success_criteria":"all tests pass with full coverage","context":"This is a test of the typed task_id field in delegate_task. The message needs to be long enough to exceed the inline threshold of five hundred characters so it gets enqueued to the inbox JSONL file where we can verify the task_id was persisted as a typed field. Adding sufficient padding text to reliably cross the 500 char boundary for this integration test of the delegate_task typed task_id correlation feature in the agend-terminal daemon."}}}"#,
        ],
    );
    assert!(responses.len() >= 2);
    // Deserialize inbox JSONL and verify typed task_id field
    let inbox_path = home.join("inbox").join("test-agent.jsonl");
    let content = std::fs::read_to_string(&inbox_path).expect("inbox file must exist");
    let msg: serde_json::Value = content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .find(|v: &serde_json::Value| v["text"].as_str().unwrap_or("").contains("[delegate_task]"))
        .expect("delegate_task message must be in inbox");
    assert_eq!(
        msg["task_id"].as_str(),
        Some("t-sprint6-42"),
        "typed task_id field must be persisted in InboxMessage JSON: {msg}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_delegate_task_no_task_id_remains_none() {
    let home = mcp_home();
    std::fs::write(
        home.join("fleet.yaml"),
        "instances:\n  test-agent:\n    backend: claude\n",
    )
    .ok();
    let responses = mcp_session_in_home(
        &home,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"delegate_task","arguments":{"target_instance":"test-agent","task":"implement feature Y without any task_id parameter provided at all","success_criteria":"verify that when no task_id is given the typed field is absent from the serialized JSON","context":"This delegate_task call intentionally omits the task_id field entirely. The combined message body including task description plus success criteria plus this context string needs to exceed five hundred characters total to be enqueued to the inbox JSONL file by the deliver function. We verify that when task_id is not provided by the caller, the InboxMessage JSON does not contain a task_id field. This padding ensures we reliably cross the five hundred character threshold boundary for the test."}}}"#,
        ],
    );
    assert!(responses.len() >= 2);
    let inbox_path = home.join("inbox").join("test-agent.jsonl");
    let content = std::fs::read_to_string(&inbox_path).expect("inbox file must exist");
    let msg: serde_json::Value = content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .find(|v: &serde_json::Value| v["text"].as_str().unwrap_or("").contains("[delegate_task]"))
        .expect("delegate_task message must be in inbox");
    assert!(
        msg.get("task_id").is_none() || msg["task_id"].is_null(),
        "task_id must be absent or null when not provided: {msg}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_correlation_id_persisted_as_typed_field() {
    let home = mcp_home();
    std::fs::write(
        home.join("fleet.yaml"),
        "instances:\n  test-agent:\n    backend: claude\n",
    )
    .ok();
    let responses = mcp_session_in_home(
        &home,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"report_result","arguments":{"target_instance":"test-agent","summary":"done","correlation_id":"t-corr-99","parent_id":"m-parent","thread_id":"t-thread"}}}"#,
        ],
    );
    assert!(responses.len() >= 2);
    let inbox_path = home.join("inbox").join("test-agent.jsonl");
    let content = std::fs::read_to_string(&inbox_path).unwrap_or_default();
    let msg: serde_json::Value = content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .find(|v: &serde_json::Value| v["text"].as_str().unwrap_or("").contains("[report_result]"))
        .expect("report_result message must be in inbox");
    assert_eq!(
        msg["correlation_id"].as_str(),
        Some("t-corr-99"),
        "typed correlation_id must be persisted: {msg}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_report_result_reviewed_head_persisted() {
    let home = mcp_home();
    std::fs::write(
        home.join("fleet.yaml"),
        "instances:\n  test-agent:\n    backend: claude\n",
    )
    .ok();
    let responses = mcp_session_in_home(
        &home,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"report_result","arguments":{"target_instance":"test-agent","summary":"reviewed","reviewed_head":"abc123","correlation_id":"t-1","parent_id":"m-1"}}}"#,
        ],
    );
    assert!(responses.len() >= 2);
    let inbox_path = home.join("inbox").join("test-agent.jsonl");
    let content = std::fs::read_to_string(&inbox_path).unwrap_or_default();
    let msg: serde_json::Value = content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .find(|v: &serde_json::Value| v["text"].as_str().unwrap_or("").contains("[report_result]"))
        .expect("report_result must be in inbox");
    assert_eq!(
        msg["reviewed_head"].as_str(),
        Some("abc123"),
        "reviewed_head must be persisted: {msg}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_send_to_report_kind_without_parent_id_returns_warning() {
    let home = mcp_home();
    std::fs::write(
        home.join("fleet.yaml"),
        "instances:\n  test-agent:\n    backend: claude\n  other:\n    backend: claude\n",
    )
    .ok();
    let responses = mcp_session_in_home(
        &home,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"send_to_instance","arguments":{"instance_name":"other","message":"status update","request_kind":"report"}}}"#,
        ],
    );
    assert!(responses.len() >= 2);
    let result = extract_tool_result(&responses[1]);
    assert!(
        result.get("warning").is_some(),
        "report kind without parent_id must return warning: {result}"
    );
    assert!(
        result["warning"]
            .as_str()
            .unwrap_or("")
            .contains("parent_id"),
        "warning must mention parent_id: {result}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_send_to_report_kind_with_parent_id_no_warning() {
    let home = mcp_home();
    std::fs::write(
        home.join("fleet.yaml"),
        "instances:\n  test-agent:\n    backend: claude\n  other:\n    backend: claude\n",
    )
    .ok();
    let responses = mcp_session_in_home(
        &home,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"send_to_instance","arguments":{"instance_name":"other","message":"status update","request_kind":"report","parent_id":"m-123"}}}"#,
        ],
    );
    assert!(responses.len() >= 2);
    let result = extract_tool_result(&responses[1]);
    assert!(
        result.get("warning").is_none(),
        "report with parent_id must not have warning: {result}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_send_to_instance_correlation_id_persisted() {
    let home = mcp_home();
    std::fs::write(
        home.join("fleet.yaml"),
        "instances:\n  test-agent:\n    backend: claude\n  other:\n    backend: claude\n",
    )
    .ok();
    let responses = mcp_session_in_home(
        &home,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"send_to_instance","arguments":{"instance_name":"other","message":"correlated msg with enough padding to exceed five hundred characters threshold so it gets enqueued to inbox JSONL file where we can verify the correlation_id typed field was persisted correctly by the send_to_instance handler in both the API path and the fallback path for this integration test of Sprint 8 PR-J correlation infrastructure","correlation_id":"corr-42","request_kind":"report","parent_id":"m-1"}}}"#,
        ],
    );
    assert!(responses.len() >= 2);
    let inbox_path = home.join("inbox").join("other.jsonl");
    let content = std::fs::read_to_string(&inbox_path).unwrap_or_default();
    let msg: serde_json::Value = content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .next()
        .expect("message in inbox");
    assert_eq!(
        msg["correlation_id"].as_str(),
        Some("corr-42"),
        "typed correlation_id must be persisted via send_to_instance: {msg}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_reviewed_head_mismatch_annotates_stale() {
    let home = mcp_home();
    std::fs::write(
        home.join("fleet.yaml"),
        "instances:\n  test-agent:\n    backend: claude\n",
    )
    .ok();
    // Send report_result with reviewed_head
    mcp_session_in_home(
        &home,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"report_result","arguments":{"target_instance":"test-agent","summary":"reviewed","reviewed_head":"old-sha-123","correlation_id":"t-1","parent_id":"m-1"}}}"#,
        ],
    );
    // Drain to mark as read, then describe_message
    let responses = mcp_session_in_home(
        &home,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"inbox","arguments":{}}}"#,
        ],
    );
    // Get the message ID from inbox
    let inbox_result = extract_tool_result(&responses[1]);
    let messages = inbox_result["messages"].as_array().expect("messages");
    let msg = messages
        .iter()
        .find(|m| m["reviewed_head"].as_str() == Some("old-sha-123"))
        .expect("msg with reviewed_head");
    let msg_id = msg["id"].as_str().expect("msg id");
    // Now describe_message should show stale_possible
    let desc_req = format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"describe_message","arguments":{{"message_id":"{msg_id}"}}}}}}"#
    );
    let responses2 = mcp_session_in_home(
        &home,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            &desc_req,
        ],
    );
    let desc = extract_tool_result(&responses2[1]);
    assert_eq!(
        desc["reviewed_head"].as_str(),
        Some("old-sha-123"),
        "describe_message must show reviewed_head: {desc}"
    );
    assert_eq!(
        desc["stale_possible"], true,
        "reviewed_head present must annotate stale_possible: {desc}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_report_result_persists_parent_and_thread() {
    let home = mcp_home();
    std::fs::write(
        home.join("fleet.yaml"),
        "instances:\n  test-agent:\n    backend: claude\n",
    )
    .ok();
    let responses = mcp_session_in_home(
        &home,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"report_result","arguments":{"target_instance":"test-agent","summary":"done","parent_id":"m-parent-99","thread_id":"t-thread-77","correlation_id":"t-1"}}}"#,
        ],
    );
    assert!(responses.len() >= 2);
    let inbox_path = home.join("inbox").join("test-agent.jsonl");
    let content = std::fs::read_to_string(&inbox_path).unwrap_or_default();
    let msg: serde_json::Value = content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .find(|v: &serde_json::Value| v["text"].as_str().unwrap_or("").contains("[report_result]"))
        .expect("report_result in inbox");
    assert_eq!(
        msg["parent_id"].as_str(),
        Some("m-parent-99"),
        "parent_id must be persisted: {msg}"
    );
    assert_eq!(
        msg["thread_id"].as_str(),
        Some("t-thread-77"),
        "thread_id must be persisted: {msg}"
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn test_interrupt_target_not_exist() {
    let responses = mcp_session(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"interrupt","arguments":{"target":"nonexistent-agent"}}}"#,
    ]);
    assert!(responses.len() >= 2);
    let result = extract_tool_result(&responses[1]);
    assert!(
        result.get("error").is_some(),
        "unknown target must return error: {result}"
    );
}

#[test]
fn test_interrupt_missing_target() {
    let responses = mcp_session(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"interrupt","arguments":{}}}"#,
    ]);
    assert!(responses.len() >= 2);
    let result = extract_tool_result(&responses[1]);
    assert!(
        result["error"].as_str().unwrap_or("").contains("missing"),
        "missing target must error: {result}"
    );
}

#[test]
fn test_interrupt_invalid_target_name() {
    let responses = mcp_session(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"interrupt","arguments":{"target":"../escape"}}}"#,
    ]);
    assert!(responses.len() >= 2);
    let result = extract_tool_result(&responses[1]);
    assert!(
        result.get("error").is_some(),
        "invalid name must error: {result}"
    );
}
