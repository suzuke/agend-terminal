//! MCP round-trip tests — spawn `agend-terminal mcp` as subprocess,
//! send JSON-RPC requests via stdin, verify responses.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
fn binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop();
    path.pop();
    path.push("agend-terminal");
    path
}

fn mcp_session(requests: &[&str]) -> Vec<serde_json::Value> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let home = std::env::temp_dir().join(format!("agend-mcp-test-{}-{}", std::process::id(), id));
    std::fs::create_dir_all(&home).ok();

    let mut child = Command::new(binary())
        .args(["mcp"])
        .env("AGEND_TERMINAL_HOME", &home)
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
    let _ = std::fs::remove_dir_all(&home);
    responses
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
        tools.len() >= 35,
        "should have at least 35 tools, got {}",
        tools.len()
    );

    // Verify key tools exist
    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(tool_names.contains(&"reply"));
    assert!(tool_names.contains(&"delegate_task"));
    assert!(tool_names.contains(&"post_decision"));
    assert!(tool_names.contains(&"task"));
    assert!(tool_names.contains(&"create_team"));
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
        .env("AGEND_TERMINAL_HOME", &home)
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
    for line in reader.lines() {
        if let Ok(l) = line {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&l) {
                responses.push(v);
            }
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
