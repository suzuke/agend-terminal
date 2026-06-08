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

/// Verify the mcp_proxy handler routes through execute_tool (the service
/// boundary). Since the R3#1 candidate-2 refactor, `handle_mcp_tool` passes
/// `crate::mcp::execute_tool` as the injectable executor to
/// `handle_mcp_tool_inner` (a fn pointer, not a direct `execute_tool(` call), so
/// the invariant is "the symbol is referenced as the default executor".
#[test]
fn proxy_handler_calls_handle_tool_directly() {
    let src = std::fs::read_to_string("src/api/handlers/mcp_proxy.rs").expect("read mcp_proxy.rs");
    assert!(
        src.contains("crate::mcp::execute_tool"),
        "mcp_proxy must route through execute_tool (service boundary)"
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
///
/// `handle_tool` routes via two sources since #694 BLOCK 2 first cut:
/// the dispatch table (`dispatch.rs`) plus the fallback inline `match`
/// (`mod.rs`). Both files participate in the route, so a tool name
/// satisfies the "is routed" invariant if it appears as a quoted
/// literal in either one.
#[test]
fn five_tool_sample_all_routed_through_handle_tool() {
    let mod_src = std::fs::read_to_string("src/mcp/handlers/mod.rs").expect("read handlers mod.rs");
    let dispatch_src =
        std::fs::read_to_string("src/mcp/handlers/dispatch.rs").expect("read dispatch.rs");
    let tools = [
        "list_instances",
        "reply",
        "create_instance",
        "task",
        "inbox",
    ];
    for tool in &tools {
        let quoted = format!(r#""{tool}""#);
        assert!(
            mod_src.contains(&quoted) || dispatch_src.contains(&quoted),
            "handle_tool must route '{tool}' (checked mod.rs + dispatch.rs)"
        );
    }
}

// Sprint 56 Track I-Phase2c (#531): the
// `proxy_or_local_tries_daemon_first` test was deleted here when its
// subject (`proxy_or_local` + `is_running_inside_daemon_process` in
// `src/mcp/mod.rs`) was removed alongside the `agend-terminal mcp`
// subcommand. The "daemon-first" semantics it pinned have moved to
// `src/bin/agend-mcp-bridge.rs` (the canonical wire entry point); the
// equivalent invariants are pinned by:
//   - `proxy_handler_calls_handle_tool_directly` (above) — daemon side
//   - `bridge_unwraps_daemon_response_for_tools_call` (above) — wire side
//   - `tests/no_local_mcp_mode_invariant.rs::bridge_emits_daemon_error_when_daemon_down`
//     — runtime invariant that the bridge has NO local-handler fallback
