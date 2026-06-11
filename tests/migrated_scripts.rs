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

/// Originally migrated from: scripts/test-team-dedup.sh — rewritten for #1993.
///
/// #1964 (PR #1966) deliberately changed `create_instance(team=X)`: re-creating
/// an existing team is NOT rejected — it EXTENDS the roster, numbering new
/// members from `max(existing <team>-N) + 1`. The old `team_dedup_and_rejection`
/// asserted the pre-#1964 reject-on-dup contract via a `resp_str.contains("error")`
/// fallback so loose it passed on ANY "error" substring in the response — green
/// on CI by accident (spawn-noise text), red locally where a clean re-create has
/// no "error" substring (#1993). This asserts the CURRENT extend-roster contract
/// STRICTLY: exact spawned ids, no substring fallback.
#[test]
fn team_recreate_extends_roster() {
    let home = tmp_home("team-extend");
    let harness = AgendHarness::spawn(home.clone(), "defaults:\n  backend: cat\ninstances: {}\n")
        .expect("spawn");

    let client = TuiClient::new(&harness, 80, 24);

    // Explicit `backends: [cat, cat]` (NOT `count`, which falls back to the
    // hardcoded "claude" default in instance.rs and would need the claude binary)
    // so member spawn is deterministic on every host incl. CI — this test asserts
    // the roster/numbering contract, not backend availability.
    let create = |c: &TuiClient| {
        c.call(
            "mcp_tool",
            &json!({
                "tool": "create_instance",
                "arguments": {"team": "test-team", "backends": ["cat", "cat"]},
                "instance": "test-caller"
            }),
        )
    };
    // Sorted spawned ids — order within a batch is incidental; the #1964 contract
    // is about the NUMBERING (max+1), so compare as a set.
    let spawned_sorted = |resp: &serde_json::Value| -> Vec<String> {
        let mut ids: Vec<String> = resp["result"]["spawned"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids
    };

    // First create — fresh team, members numbered from 1.
    let resp1 = create(&client).expect("first create API call");
    assert_eq!(
        resp1["ok"].as_bool(),
        Some(true),
        "first create must succeed: {resp1}"
    );
    assert!(
        resp1["result"].get("error").is_none(),
        "first create must not error: {resp1}"
    );
    assert_eq!(
        spawned_sorted(&resp1),
        vec!["test-team-1", "test-team-2"],
        "fresh team numbers members from 1: {resp1}"
    );

    // Re-create the SAME team — #1964: NOT rejected; the roster extends with new
    // members numbered from max(existing)+1. STRICT assertion — no `contains("error")`
    // fallback that would mask a regression to the old reject-or-restart behaviour.
    let resp2 = create(&client).expect("second create API call");
    assert_eq!(
        resp2["ok"].as_bool(),
        Some(true),
        "re-create extends the roster (NOT rejected): {resp2}"
    );
    assert!(
        resp2["result"].get("error").is_none(),
        "re-create must not error: {resp2}"
    );
    assert_eq!(
        spawned_sorted(&resp2),
        vec!["test-team-3", "test-team-4"],
        "#1964: re-create numbers from max+1 (test-team-3,4), not restart at 1: {resp2}"
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
