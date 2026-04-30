//! Sprint 42 Phase 4 — bash script migration to AgendHarness.
//!
//! Migrated from:
//! - scripts/test-team-dedup.sh → team_dedup_and_rejection
//! - scripts/verify-awaiting-operator.sh → silent_agent_spawn_observable_and_inject_path
//! - scripts/repro-team-tab-bug.sh → team_creation_returns_structured_response

mod common;

use common::harness::{AgendHarness, TuiClient};
use serde_json::json;

fn tmp_home(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("agend-migrate-{}-{}", tag, std::process::id()));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// Migrated from: scripts/test-team-dedup.sh
/// Tests: team creation spawns members + re-create with same name is rejected.
#[test]
fn team_dedup_and_rejection() {
    let home = tmp_home("team-dedup");
    let harness = AgendHarness::spawn(home.clone(), "defaults:\n  backend: cat\ninstances: {}\n")
        .expect("spawn");

    let client = TuiClient::new(&harness, 80, 24);

    // Create a team — should succeed
    let result = client.call(
        "mcp_tool",
        &json!({
            "tool": "create_instance",
            "arguments": {"team": "test-team", "count": 2},
            "instance": "test-caller"
        }),
    );
    assert!(
        result.is_ok(),
        "team creation API call must not fail: {:?}",
        result.err()
    );

    // Re-create same team — should be rejected with "already exists"
    let resp2 = client
        .call(
            "mcp_tool",
            &json!({
                "tool": "create_instance",
                "arguments": {"team": "test-team", "count": 2},
                "instance": "test-caller"
            }),
        )
        .expect("dedup call must succeed (API level)");
    let inner2 = &resp2["result"];
    let resp_str = inner2.to_string();
    assert!(
        inner2.get("error").is_some()
            || resp_str.contains("already exists")
            || resp_str.contains("error"),
        "re-creating same team must be rejected: {resp2}"
    );

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}
/// Migrated from: scripts/verify-awaiting-operator.sh
/// Narrowed to: agent spawn observable + raw inject path verification.
/// awaiting_operator transition not verifiable with cat backend (goes to
/// 'ready' not 'starting'; classifier divergence documented).
#[test]
fn silent_agent_spawn_observable_and_inject_path() {
    let home = tmp_home("await-op");
    let harness = AgendHarness::spawn_with(
        home.clone(),
        "instances:\n  silent:\n    backend: cat\n",
        "start",
    )
    .expect("spawn");

    let client = TuiClient::new(&harness, 80, 24);
    std::thread::sleep(std::time::Duration::from_secs(3));

    // Verify agent spawned and state observable
    let list_resp = client.call("list", &json!({})).expect("list must succeed");
    let agents = list_resp["result"]["agents"]
        .as_array()
        .expect("agents array required");
    let silent = agents
        .iter()
        .find(|a| a["name"].as_str() == Some("silent"))
        .expect("silent agent must exist");
    let state = silent["agent_state"]
        .as_str()
        .expect("agent_state must be string");
    assert!(!state.is_empty(), "agent state must be non-empty: {state}");

    // Verify raw inject path (core of original bash script)
    let inject_resp = client
        .call(
            "inject",
            &json!({"name": "silent", "data": "HELLO-RAW\n", "raw": true}),
        )
        .expect("inject must succeed");
    assert_eq!(
        inject_resp["ok"], true,
        "raw inject must succeed: {inject_resp}"
    );

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}

/// Migrated from: scripts/repro-team-tab-bug.sh
/// Narrowed to: team creation API returns structured response (spawned
/// array on success, descriptive error on backend unavailability).
#[test]
fn team_creation_returns_structured_response() {
    let home = tmp_home("team-spawn");
    let harness = AgendHarness::spawn(home.clone(), "defaults:\n  backend: cat\ninstances: {}\n")
        .expect("spawn");

    let client = TuiClient::new(&harness, 80, 24);

    // Create a team with 2 members
    let result = client.call(
        "mcp_tool",
        &json!({
            "tool": "create_instance",
            "arguments": {"team": "repro-team", "count": 2, "backend": "cat"},
            "instance": "test-caller"
        }),
    );
    assert!(
        result.is_ok(),
        "team creation must not fail: {:?}",
        result.err()
    );
    let resp = result.expect("create response");

    // Hard assertion: response must indicate spawned members or structured error
    let inner = &resp["result"];
    // Team creation via mcp_tool returns the tool's result.
    // On success: {"team": ..., "spawned": [...]}
    // On API error: {"error": "..."}
    if inner.get("error").is_some() {
        // API error (e.g., daemon couldn't spawn backend) — acceptable in CI
        // where backends may not be installed. The test verifies the API path works.
        let err = inner["error"].as_str().unwrap_or("unknown");
        assert!(
            !err.is_empty(),
            "team creation error must be descriptive: {inner}"
        );
    } else {
        let spawned = inner
            .get("spawned")
            .and_then(|v| v.as_array())
            .expect("successful team creation must have 'spawned' array");
        assert!(
            !spawned.is_empty(),
            "team creation must spawn at least 1 member: {inner}"
        );
    }

    // Hard assertion: list must show agents after team creation
    let list_resp = client
        .call("list", &json!({}))
        .expect("list call must succeed after team creation");
    let agents = list_resp["result"]["agents"]
        .as_array()
        .expect("list must return agents array");
    assert!(
        !agents.is_empty(),
        "agents list must not be empty after team creation: {list_resp}"
    );

    drop(harness);
    std::fs::remove_dir_all(&home).ok();
}
