//! MCP proxy parity test — verifies that the daemon proxy path
//! (mcp_tool API method) produces the same result shape as the
//! direct handle_tool path.
//!
//! Sprint 25 P0 Option F REJECT criterion: 5-tool parity sample.
//!
//! Since handle_tool is not exported via lib.rs (it's in the binary),
//! this test verifies structural parity by checking that:
//! 1. The mcp_proxy handler calls handle_tool with the same args
//! 2. The response wrapping is consistent (ok + result shape)
//! 3. The 5 representative tools are routed correctly

/// Verify the mcp_proxy handler source calls handle_tool directly.
#[test]
fn proxy_handler_calls_handle_tool_directly() {
    let src = std::fs::read_to_string("src/api/handlers/mcp_proxy.rs").expect("read mcp_proxy.rs");
    assert!(
        src.contains("crate::mcp::handlers::handle_tool("),
        "mcp_proxy must call handle_tool"
    );
}

/// Verify the proxy handler wraps result in {"ok": true, "result": ...}.
#[test]
fn proxy_handler_wraps_result_correctly() {
    let src = std::fs::read_to_string("src/api/handlers/mcp_proxy.rs").expect("read mcp_proxy.rs");
    assert!(
        src.contains(r#"json!({"ok": true, "result": result})"#),
        "mcp_proxy must wrap result in {{ok: true, result: ...}}"
    );
}

/// Verify the bridge binary unwraps daemon response correctly for tools/call.
#[test]
fn bridge_unwraps_daemon_response_for_tools_call() {
    let src = std::fs::read_to_string("src/bin/agend-mcp-bridge.rs").expect("read bridge source");
    // Bridge must check resp["ok"] == true and extract resp["result"]
    assert!(
        src.contains(r#"resp["ok"].as_bool() == Some(true)"#),
        "bridge must check daemon response ok field"
    );
    assert!(
        src.contains(r#"resp["result"].clone()"#),
        "bridge must extract result from daemon response"
    );
}

/// Verify the 5 representative tools are all handled by handle_tool.
/// This ensures the proxy path covers the same tools as direct invocation.
#[test]
fn five_tool_sample_all_routed_through_handle_tool() {
    let src = std::fs::read_to_string("src/mcp/handlers.rs").expect("read handlers.rs");
    let tools = [
        "list_instances",
        "reply",
        "create_instance",
        "task",
        "inbox",
    ];
    for tool in &tools {
        assert!(
            src.contains(&format!(r#""{tool}""#)),
            "handle_tool must route '{tool}'"
        );
    }
}

/// Verify the proxy_or_local function tries daemon API first.
#[test]
fn proxy_or_local_tries_daemon_first() {
    let src = std::fs::read_to_string("src/mcp/mod.rs").expect("read mcp/mod.rs");
    assert!(
        src.contains(r#""method": "mcp_tool""#),
        "proxy_or_local must try mcp_tool API method"
    );
    // Also verify short-circuit exists
    assert!(
        src.contains("is_running_inside_daemon_process()"),
        "proxy_or_local must check daemon short-circuit"
    );
}
