use super::tests::invoke_runtime_mcp_tool;
use serde_json::json;

// ── #2454 Slice 12: SEND runtime contract RED tests ─────────────

/// #2454 Slice 12 RED (1/3): structural budget + reachability guard.
/// (a) cumulative handler api::call == 5, (b) dispatch_send not adapter!,
/// (c) agent_ops::send_to zero api::call, (d) SEND region bridge == 1.
#[test]
fn send_structural_budget_and_reachability_2454() {
    let needle_call = concat!("crate::", "api::", "call");
    let needle_at = concat!("api::", "call_at");
    let test_mod_marker = "#[cfg(test)]\nmod ";

    // (a) cumulative budget: 8 → 5
    // Three handler-local sites to eliminate: SEND (comms.rs),
    // REPORT (comms.rs), DELEGATE (comms_delegate/mod.rs).
    let files: &[&str] = &[
        include_str!("../../mcp/handlers/comms.rs"),
        include_str!("../../mcp/handlers/comms_delegate/mod.rs"),
        include_str!("../../mcp/handlers/task.rs"),
        include_str!("../../mcp/handlers/restart.rs"),
        include_str!("../../mcp/handlers/instance_state/mod.rs"),
        include_str!("../../mcp/handlers/instance_state/spawn.rs"),
        include_str!("../../mcp/handlers/instance_state/lifecycle.rs"),
        include_str!("../../mcp/handlers/instance_metadata.rs"),
    ];
    let mut handler_count = 0;
    for src in files {
        let boundary = src.rfind(test_mod_marker).unwrap_or(src.len());
        let production = &src[..boundary];
        for line in production.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            if line.contains(needle_call) && !line.contains(needle_at) {
                handler_count += 1;
            }
        }
    }
    assert_eq!(
        handler_count, 4,
        "Slice-13 cumulative api::call budget must be 4; got {handler_count}"
    );

    // (b) dispatch_send must NOT be adapter! — custom fn threads RuntimeContext
    let dispatch_src = include_str!("../../mcp/handlers/dispatch.rs");
    let dispatch_boundary = dispatch_src
        .rfind(test_mod_marker)
        .unwrap_or(dispatch_src.len());
    let dispatch_prod = &dispatch_src[..dispatch_boundary];
    let adapter_send = "adapter!(dispatch_send";
    assert!(
        !dispatch_prod.contains(adapter_send),
        "dispatch_send must be a custom fn threading RuntimeContext, not adapter!"
    );

    // (c) agent_ops::send_to retired — no longer exists.
    let ops_src = include_str!("../../agent_ops.rs");
    assert!(
        !ops_src.contains("pub fn send_to("),
        "agent_ops::send_to must be retired after Slice 12 migration"
    );

    // (d) SEND production region (comms + comms_delegate + agent_ops) must
    // have exactly 1 remaining api::call bridge (the compatibility fallback).
    let send_region_files: &[&str] = &[
        include_str!("../../mcp/handlers/comms.rs"),
        include_str!("../../mcp/handlers/comms_delegate/mod.rs"),
        ops_src,
    ];
    let mut send_region_count = 0;
    for src in send_region_files {
        let boundary = src.rfind(test_mod_marker).unwrap_or(src.len());
        let production = &src[..boundary];
        for line in production.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            if line.contains(needle_call) && !line.contains(needle_at) {
                send_region_count += 1;
            }
        }
    }
    assert_eq!(
        send_region_count, 1,
        "SEND production region must have exactly 1 api::call bridge; got {send_region_count}"
    );
}

/// #2454 Slice 12 RED (2/3): real MCP ingress — a send with
/// RuntimeContext=Some must deliver via the neutral in-process service,
/// not the socket fallback. Currently api::call fails in test (no daemon)
/// → fallback_deliver → delivery_mode="inbox_fallback".
#[test]
#[allow(clippy::unwrap_used)]
fn send_real_entry_runtime_delivery_not_fallback_2454() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let tag = format!(
        "send-rt-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let home = std::env::temp_dir().join(format!("agend-s12-{tag}"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  send-target:\n    backend: claude\n",
    )
    .unwrap();
    let registry: crate::agent::AgentRegistry = Default::default();
    let configs: crate::api::ConfigRegistry = Default::default();
    let externals: crate::agent::ExternalRegistry = Default::default();

    let previous_home = std::env::var_os("AGEND_HOME");
    std::env::set_var("AGEND_HOME", &home);
    let response = invoke_runtime_mcp_tool(
        &home,
        &registry,
        &configs,
        &externals,
        "send",
        "test-sender",
        json!({
            "instance": "send-target",
            "message": "hello from Slice 12 RED",
        }),
    );
    match previous_home {
        Some(value) => std::env::set_var("AGEND_HOME", value),
        None => std::env::remove_var("AGEND_HOME"),
    }
    std::fs::remove_dir_all(&home).ok();

    assert_eq!(
        response["ok"], true,
        "MCP send ingress must return a response: {response}"
    );
    let dm = response["result"]["delivery_mode"]
        .as_str()
        .unwrap_or_default();
    assert_ne!(
        dm, "inbox_fallback",
        "SEND with RuntimeContext must deliver via the neutral service, \
         not the socket fallback (delivery_mode={dm}): {response}"
    );
}

/// #2454 Slice 12 RED (3/3): real MCP ingress — the neutral service's
/// team-isolation gate must reject a cross-team send with no delivery
/// side-effect. Currently api::call fails → fallback_deliver bypasses
/// the API's policy gates → message silently delivered.
#[test]
#[allow(clippy::unwrap_used)]
fn send_real_entry_runtime_team_isolation_gate_2454() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let tag = format!(
        "send-xteam-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let home = std::env::temp_dir().join(format!("agend-s12-{tag}"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  \
           alpha-1:\n    backend: claude\n  \
           beta-1:\n    backend: claude\n\
         teams:\n  \
           alpha:\n    members: [alpha-1]\n  \
           beta:\n    members: [beta-1]\n",
    )
    .unwrap();
    let registry: crate::agent::AgentRegistry = Default::default();
    let configs: crate::api::ConfigRegistry = Default::default();
    let externals: crate::agent::ExternalRegistry = Default::default();

    let previous_home = std::env::var_os("AGEND_HOME");
    std::env::set_var("AGEND_HOME", &home);
    let response = invoke_runtime_mcp_tool(
        &home,
        &registry,
        &configs,
        &externals,
        "send",
        "alpha-1",
        json!({
            "instance": "beta-1",
            "message": "cross-team probe",
        }),
    );
    match previous_home {
        Some(value) => std::env::set_var("AGEND_HOME", value),
        None => std::env::remove_var("AGEND_HOME"),
    }

    // Verify no delivery side-effect: inbox must be empty for the target
    let inbox_dir = home.join("inbox").join("beta-1");
    let inbox_has_message = inbox_dir.exists()
        && std::fs::read_dir(&inbox_dir)
            .map(|d| d.count() > 0)
            .unwrap_or(false);
    std::fs::remove_dir_all(&home).ok();

    let result_str = response.to_string();
    assert!(
        result_str.contains("cross") && result_str.contains("team"),
        "cross-team send must be rejected by the neutral service's team-isolation \
         gate, not silently delivered via the socket fallback: {response}"
    );
    assert!(
        !inbox_has_message,
        "rejected cross-team send must not produce a delivery side-effect in the target inbox"
    );
}

// ── #2454 Slice 12 supplemental RED ───────────────────────────────

/// #2454 Slice 12 supplemental RED (1/3): RuntimeContext=None (standalone
/// mode) with daemon unreachable must NOT silently succeed via inbox
/// fallback. The _with_runtime_or_legacy bridge must surface the failure.
#[test]
#[allow(clippy::unwrap_used)]
fn send_runtime_none_no_silent_fallback_2454() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let tag = format!(
        "send-none-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let home = std::env::temp_dir().join(format!("agend-s12-{tag}"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  none-target:\n    backend: claude\n",
    )
    .unwrap();

    let previous_home = std::env::var_os("AGEND_HOME");
    std::env::set_var("AGEND_HOME", &home);
    let response = crate::mcp::handlers::handle_tool_with_runtime(
        "send",
        &json!({
            "instance": "none-target",
            "message": "hello from runtime=None test",
        }),
        "test-sender",
        None,
    );
    match previous_home {
        Some(value) => std::env::set_var("AGEND_HOME", value),
        None => std::env::remove_var("AGEND_HOME"),
    }
    std::fs::remove_dir_all(&home).ok();

    let dm = response["delivery_mode"].as_str().unwrap_or_default();
    assert_ne!(
        dm, "inbox_fallback",
        "runtime=None send must NOT silently succeed via inbox_fallback — \
         daemon-unreachable must be an honest failure: {response}"
    );
}

/// #2454 Slice 12 supplemental RED (2/3): a report (request_kind=report)
/// via MCP ingress with RuntimeContext=Some must deliver through the
/// neutral service, not the socket fallback. Proves the report path's
/// API-level logic (authorize_report, process_verdicts, track_dispatch)
/// runs through the in-process path.
#[test]
#[allow(clippy::unwrap_used)]
fn send_report_through_service_not_fallback_2454() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let tag = format!(
        "send-rpt-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let home = std::env::temp_dir().join(format!("agend-s12-{tag}"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  rpt-target:\n    backend: claude\n",
    )
    .unwrap();
    let registry: crate::agent::AgentRegistry = Default::default();
    let configs: crate::api::ConfigRegistry = Default::default();
    let externals: crate::agent::ExternalRegistry = Default::default();

    let previous_home = std::env::var_os("AGEND_HOME");
    std::env::set_var("AGEND_HOME", &home);
    let response = invoke_runtime_mcp_tool(
        &home,
        &registry,
        &configs,
        &externals,
        "send",
        "rpt-sender",
        json!({
            "instance": "rpt-target",
            "request_kind": "report",
            "message": "report summary text",
            "correlation_id": "t-test-rpt-corr",
        }),
    );
    match previous_home {
        Some(value) => std::env::set_var("AGEND_HOME", value),
        None => std::env::remove_var("AGEND_HOME"),
    }
    std::fs::remove_dir_all(&home).ok();

    let dm = response["result"]["delivery_mode"]
        .as_str()
        .or_else(|| response["result"]["note"].as_str())
        .unwrap_or_default();
    assert!(
        !dm.contains("inbox_fallback") && !dm.contains("API unavailable"),
        "report via MCP must deliver through the neutral service, \
         not the socket fallback: {response}"
    );
}

/// #2454 Slice 12 supplemental RED (3/3): a delegate (request_kind=task)
/// via MCP ingress with RuntimeContext=Some must deliver through the
/// neutral service. Proves dispatch tracking, auto-bind, and task
/// correlation fire through the in-process path.
#[test]
#[allow(clippy::unwrap_used)]
fn send_delegate_through_service_not_fallback_2454() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let tag = format!(
        "send-del-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let home = std::env::temp_dir().join(format!("agend-s12-{tag}"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  del-target:\n    backend: claude\n",
    )
    .unwrap();
    let registry: crate::agent::AgentRegistry = Default::default();
    let configs: crate::api::ConfigRegistry = Default::default();
    let externals: crate::agent::ExternalRegistry = Default::default();

    let previous_home = std::env::var_os("AGEND_HOME");
    std::env::set_var("AGEND_HOME", &home);
    let response = invoke_runtime_mcp_tool(
        &home,
        &registry,
        &configs,
        &externals,
        "send",
        "del-sender",
        json!({
            "instance": "del-target",
            "request_kind": "task",
            "message": "test task delegation",
            "task_id": "t-test-delegate-001",
        }),
    );
    match previous_home {
        Some(value) => std::env::set_var("AGEND_HOME", value),
        None => std::env::remove_var("AGEND_HOME"),
    }
    std::fs::remove_dir_all(&home).ok();

    let result_str = response["result"].to_string();
    assert!(
        !result_str.contains("inbox_fallback") && !result_str.contains("API unavailable"),
        "delegate via MCP must deliver through the neutral service, \
         not the socket fallback: {response}"
    );
}

// ── #2454 Slice 12 supplemental RED batch 2 (d-20260719053726713862-4) ──
//
// Three invariant families:
//   1. Directive integrity (broadcast + query)
//   2. runtime=None zero side effects
//   3. Post-success exactly-once (report + delegate + source-owner guard)
//
// Post-success effect ownership (reference for tests 9-11):
//   SERVICE level (handle_send in messaging.rs):
//     settle_parent_after_successful_send, inject_provenance,
//     checkout_branch_if_requested, process_verdicts, track_dispatch
//   MCP DECORATOR level (comms.rs handlers):
//     dispatch_tracking::mark_completed, ack_by_correlation,
//     record_triaged_if_present, UxEvent emission

/// d-...-4 invariant 1a: broadcast must preserve the full advertised
/// directive set in EVERY per-target delivered InboxMessage. Currently
/// handle_broadcast → send_to only passes from/target/text/kind/
/// broadcast_context, silently dropping all dispatch directives.
/// Checks BOTH targets and all legal directives that InboxMessage carries.
#[test]
#[allow(clippy::unwrap_used)]
fn send_broadcast_directive_integrity_2454() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let tag = format!(
        "bcast-dir-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let home = std::env::temp_dir().join(format!("agend-s12b2-{tag}"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  bc-sender:\n    backend: claude\n  \
         bc-target-a:\n    backend: claude\n  \
         bc-target-b:\n    backend: claude\n",
    )
    .unwrap();
    let registry: crate::agent::AgentRegistry = Default::default();
    let configs: crate::api::ConfigRegistry = Default::default();
    let externals: crate::agent::ExternalRegistry = Default::default();

    let previous_home = std::env::var_os("AGEND_HOME");
    std::env::set_var("AGEND_HOME", &home);
    let _response = invoke_runtime_mcp_tool(
        &home,
        &registry,
        &configs,
        &externals,
        "send",
        "bc-sender",
        json!({
            "instances": ["bc-target-a", "bc-target-b"],
            "message": "broadcast with directives",
            "thread_id": "thread-bc-001",
            "parent_id": "parent-bc-001",
            "correlation_id": "corr-bc-001",
            "eta_minutes": 30,
            "reporting_cadence": "per-pr",
            "worktree_binding_required": true,
            "terminal": true,
        }),
    );
    match previous_home {
        Some(value) => std::env::set_var("AGEND_HOME", value),
        None => std::env::remove_var("AGEND_HOME"),
    }

    let msgs_a = crate::inbox::drain(&home, "bc-target-a");
    let msgs_b = crate::inbox::drain(&home, "bc-target-b");
    std::fs::remove_dir_all(&home).ok();

    // Both targets must receive a message.
    assert!(!msgs_a.is_empty(), "broadcast must deliver to bc-target-a");
    assert!(!msgs_b.is_empty(), "broadcast must deliver to bc-target-b");

    // Check full directive set on BOTH targets.
    for (label, msg) in [("target-a", &msgs_a[0]), ("target-b", &msgs_b[0])] {
        assert_eq!(
            msg.thread_id.as_deref(),
            Some("thread-bc-001"),
            "{label}: broadcast must preserve thread_id; got {:?}",
            msg.thread_id
        );
        assert_eq!(
            msg.parent_id.as_deref(),
            Some("parent-bc-001"),
            "{label}: broadcast must preserve parent_id; got {:?}",
            msg.parent_id
        );
        assert_eq!(
            msg.correlation_id.as_deref(),
            Some("corr-bc-001"),
            "{label}: broadcast must preserve correlation_id; got {:?}",
            msg.correlation_id
        );
        assert_eq!(
            msg.eta_minutes,
            Some(30),
            "{label}: broadcast must preserve eta_minutes; got {:?}",
            msg.eta_minutes
        );
        assert_eq!(
            msg.reporting_cadence.as_deref(),
            Some("per-pr"),
            "{label}: broadcast must preserve reporting_cadence; got {:?}",
            msg.reporting_cadence
        );
        assert_eq!(
            msg.worktree_binding_required,
            Some(true),
            "{label}: broadcast must preserve worktree_binding_required; got {:?}",
            msg.worktree_binding_required
        );
        assert_eq!(
            msg.terminal,
            Some(true),
            "{label}: broadcast must preserve terminal; got {:?}",
            msg.terminal
        );
    }
}

/// d-...-4 invariant 1b: query (request_kind=query) must preserve
/// directive set in the delivered message. Currently
/// handle_request_information → send_to drops all directives.
#[test]
#[allow(clippy::unwrap_used)]
fn send_query_directive_integrity_2454() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let tag = format!(
        "query-dir-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let home = std::env::temp_dir().join(format!("agend-s12b2-{tag}"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  q-sender:\n    backend: claude\n  \
         q-target:\n    backend: claude\n",
    )
    .unwrap();
    let registry: crate::agent::AgentRegistry = Default::default();
    let configs: crate::api::ConfigRegistry = Default::default();
    let externals: crate::agent::ExternalRegistry = Default::default();

    let previous_home = std::env::var_os("AGEND_HOME");
    std::env::set_var("AGEND_HOME", &home);
    let _response = invoke_runtime_mcp_tool(
        &home,
        &registry,
        &configs,
        &externals,
        "send",
        "q-sender",
        json!({
            "instance": "q-target",
            "request_kind": "query",
            "message": "what is the status?",
            "thread_id": "thread-q-001",
            "parent_id": "parent-q-001",
            "correlation_id": "corr-q-001",
            "eta_minutes": 15,
            "reporting_cadence": "both",
            "worktree_binding_required": true,
            "terminal": true,
        }),
    );
    match previous_home {
        Some(value) => std::env::set_var("AGEND_HOME", value),
        None => std::env::remove_var("AGEND_HOME"),
    }

    let msgs = crate::inbox::drain(&home, "q-target");
    std::fs::remove_dir_all(&home).ok();

    assert!(!msgs.is_empty(), "query must deliver to q-target");
    let msg = &msgs[0];
    assert_eq!(
        msg.thread_id.as_deref(),
        Some("thread-q-001"),
        "query must preserve thread_id; got {:?}",
        msg.thread_id
    );
    assert_eq!(
        msg.parent_id.as_deref(),
        Some("parent-q-001"),
        "query must preserve parent_id; got {:?}",
        msg.parent_id
    );
    assert_eq!(
        msg.correlation_id.as_deref(),
        Some("corr-q-001"),
        "query must preserve correlation_id; got {:?}",
        msg.correlation_id
    );
    assert_eq!(
        msg.eta_minutes,
        Some(15),
        "query must preserve eta_minutes; got {:?}",
        msg.eta_minutes
    );
    assert_eq!(
        msg.reporting_cadence.as_deref(),
        Some("both"),
        "query must preserve reporting_cadence; got {:?}",
        msg.reporting_cadence
    );
    assert_eq!(
        msg.worktree_binding_required,
        Some(true),
        "query must preserve worktree_binding_required; got {:?}",
        msg.worktree_binding_required
    );
    assert_eq!(
        msg.terminal,
        Some(true),
        "query must preserve terminal; got {:?}",
        msg.terminal
    );
}

/// d-...-4 invariant 2: runtime=None with API unavailable must produce
/// ZERO side effects across ALL durable stores: no target inbox entry,
/// no dispatch tracking, no dispatch-idle sidecar, no task auto-creation,
/// sender's pre-seeded dispatch row remains unsettled, no discharge
/// ledger entry. Currently the fallback path creates an inbox entry
/// and fires post-success decorations.
#[test]
#[allow(clippy::unwrap_used)]
fn send_runtime_none_failure_zero_side_effects_2454() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let tag = format!(
        "none-fx-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let home = std::env::temp_dir().join(format!("agend-s12b2-{tag}"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  fx-sender:\n    backend: claude\n  \
         fx-target:\n    backend: claude\n",
    )
    .unwrap();

    // Pre-seed a dispatch row in sender's inbox (task dispatch from target).
    // After drain it enters "delivering" state. A failed send must NOT
    // settle this row (ack_by_correlation must not fire).
    crate::inbox::enqueue(
        &home,
        "fx-sender",
        crate::inbox::InboxMessage {
            schema_version: 1,
            id: Some("m-fx-parent".into()),
            from: "fx-target".into(),
            text: "[task] pre-seeded dispatch".into(),
            kind: Some("task".into()),
            task_id: Some("t-fx-parent-task".into()),
            timestamp: "2026-07-19T00:00:00Z".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let pre_drain = crate::inbox::drain(&home, "fx-sender");
    assert_eq!(
        pre_drain.len(),
        1,
        "pre-seeded dispatch drained → delivering"
    );

    // Register UX Recorder — must record 0 events on failure.
    use crate::channel::sink_registry::registry as ux_sink_registry;
    use crate::channel::ux_event::{UxEvent, UxEventSink};
    let rec = {
        struct Rec(parking_lot::Mutex<Vec<UxEvent>>);
        impl UxEventSink for Rec {
            fn emit(&self, event: &UxEvent) {
                self.0.lock().push(event.clone());
            }
        }
        std::sync::Arc::new(Rec(parking_lot::Mutex::new(Vec::new())))
    };
    ux_sink_registry().clear_for_test();
    ux_sink_registry().register(rec.clone() as std::sync::Arc<dyn UxEventSink>);

    let previous_home = std::env::var_os("AGEND_HOME");
    std::env::set_var("AGEND_HOME", &home);
    let response = crate::mcp::handlers::handle_tool_with_runtime(
        "send",
        &json!({
            "instance": "fx-target",
            "message": "this should leave no trace",
            "request_kind": "task",
            "task_id": "t-fx-zero-side-effects",
            "correlation_id": "t-fx-parent-task",
            "triaged": {"head": "fx-test-head-sha", "job": "fx-test-job"},
        }),
        "fx-sender",
        None,
    );
    match previous_home {
        Some(value) => std::env::set_var("AGEND_HOME", value),
        None => std::env::remove_var("AGEND_HOME"),
    }

    // (a) Response must indicate failure explicitly.
    let resp_str = response.to_string();
    assert!(
        resp_str.contains("error")
            || resp_str.contains("fail")
            || resp_str.contains("unavailable")
            || response["ok"] == false,
        "runtime=None must return an explicit failure response; got: {response}"
    );

    // (b) No inbox entry for the target.
    let target_msgs = crate::inbox::drain(&home, "fx-target");
    // (c) No dispatch tracking record for the target.
    let has_tracking = crate::dispatch_tracking::has_for_instance(&home, "fx-target");
    // (d) No dispatch-idle sidecar.
    let has_idle = crate::daemon::dispatch_idle::has_pending_for_instance(&home, "fx-target");
    // (e) Sender's pre-seeded parent remains unsettled (Delivering, not
    // ReadAt). describe_message is the direct status probe — drain would
    // miss a delivering row because drain only returns unread messages.
    let parent_status = crate::inbox::describe_message(&home, "m-fx-parent", "fx-sender");
    // (f) No task auto-created on the board for the sent task_id.
    let board = crate::tasks::handle(&home, "fx-sender", &json!({"action": "list"}));
    let has_auto_task = board["tasks"]
        .as_array()
        .map(|arr| arr.iter().any(|t| t["id"] == "t-fx-zero-side-effects"))
        .unwrap_or(false);
    // (g) No discharge ledger entry.
    let has_discharge =
        crate::daemon::discharge_ledger::lookup_discharge(&home, "fx-test-head-sha", "fx-test-job")
            .is_some();
    // (h) No PR-state residue.
    let pr_dir = crate::daemon::pr_state::pr_state_dir(&home);
    let has_pr_state = pr_dir.exists()
        && std::fs::read_dir(&pr_dir)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
    std::fs::remove_dir_all(&home).ok();

    assert!(
        target_msgs.is_empty(),
        "runtime=None failure must not leave an inbox entry; found {} messages",
        target_msgs.len()
    );
    assert!(
        !has_tracking,
        "runtime=None failure must not create a dispatch tracking record"
    );
    assert!(
        !has_idle,
        "runtime=None failure must not create a dispatch-idle sidecar"
    );
    assert!(
        matches!(
            parent_status,
            crate::inbox::MessageStatus::Delivering { .. }
        ),
        "runtime=None failure must not settle the pre-seeded parent — \
         expected Delivering, got {:?}",
        parent_status
    );
    assert!(
        !has_auto_task,
        "runtime=None failure must not auto-create a task on the board"
    );
    assert!(
        !has_discharge,
        "runtime=None failure must not write a discharge ledger entry"
    );
    assert!(
        !has_pr_state,
        "runtime=None failure must not create any PR-state residue"
    );
    // (i) UX: zero events on failure.
    let ux_events = rec.0.lock().clone();
    assert!(
        ux_events.is_empty(),
        "runtime=None failure must not emit any UX events; got {} events",
        ux_events.len()
    );
}

/// d-...-4 invariant 3a: a successful report via runtime=Some must
/// deliver through the neutral service (not fallback) AND fire MCP
/// post-success decorations (ack_by_correlation settles sender's
/// dispatch row). Exactly 1 message must appear in the target's inbox.
/// Currently adapter! strips runtime → delivery via inbox_fallback.
#[test]
#[allow(clippy::unwrap_used)]
fn send_report_service_delivery_with_post_effects_2454() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let tag = format!(
        "rpt-fx-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let home = std::env::temp_dir().join(format!("agend-s12b2-{tag}"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  rpt-fx-sender:\n    backend: claude\n  \
         rpt-fx-target:\n    backend: claude\n",
    )
    .unwrap();
    let registry: crate::agent::AgentRegistry = Default::default();
    let configs: crate::api::ConfigRegistry = Default::default();
    let externals: crate::agent::ExternalRegistry = Default::default();

    // Pre-seed an inbox dispatch row so ack_by_correlation has something to settle.
    crate::inbox::enqueue(
        &home,
        "rpt-fx-sender",
        crate::inbox::InboxMessage {
            schema_version: 1,
            id: Some("m-dispatch-fx".into()),
            from: "rpt-fx-target".into(),
            text: "[task] do it".into(),
            kind: Some("task".into()),
            task_id: Some("t-rpt-fx-corr".into()),
            timestamp: "2026-07-19T00:00:00Z".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        crate::inbox::drain(&home, "rpt-fx-sender").len(),
        1,
        "pre-seeded dispatch row drained → delivering"
    );

    // Pre-seed a DispatchEntry so mark_completed has something to clear.
    crate::dispatch_tracking::track_dispatch(
        &home,
        crate::dispatch_tracking::DispatchEntry {
            task_id: Some("t-rpt-fx-corr".into()),
            from: "rpt-fx-target".into(),
            to: "rpt-fx-sender".into(),
            from_id: None,
            to_id: None,
            delegated_at: "2026-07-19T00:00:00Z".into(),
            status: "pending".into(),
        },
    );
    assert!(
        crate::dispatch_tracking::has_for_instance(&home, "rpt-fx-sender"),
        "pre-seeded DispatchEntry must exist"
    );

    // Register UX Recorder.
    use crate::channel::sink_registry::registry as ux_sink_registry;
    use crate::channel::ux_event::{UxEvent, UxEventSink};
    let rec = {
        struct Rec(parking_lot::Mutex<Vec<UxEvent>>);
        impl UxEventSink for Rec {
            fn emit(&self, event: &UxEvent) {
                self.0.lock().push(event.clone());
            }
        }
        std::sync::Arc::new(Rec(parking_lot::Mutex::new(Vec::new())))
    };
    ux_sink_registry().clear_for_test();
    ux_sink_registry().register(rec.clone() as std::sync::Arc<dyn UxEventSink>);

    let previous_home = std::env::var_os("AGEND_HOME");
    std::env::set_var("AGEND_HOME", &home);
    let response = invoke_runtime_mcp_tool(
        &home,
        &registry,
        &configs,
        &externals,
        "send",
        "rpt-fx-sender",
        json!({
            "instance": "rpt-fx-target",
            "request_kind": "report",
            "message": "report with post-effects",
            "correlation_id": "t-rpt-fx-corr",
            "triaged": {"head": "rpt-fx-head-sha", "job": "rpt-fx-job"},
        }),
    );
    match previous_home {
        Some(value) => std::env::set_var("AGEND_HOME", value),
        None => std::env::remove_var("AGEND_HOME"),
    }

    // (a) Delivery must NOT be via inbox_fallback.
    let dm = response["result"]["delivery_mode"]
        .as_str()
        .or_else(|| response["result"]["note"].as_str())
        .unwrap_or_default();
    // (b) Exactly 1 message in target's inbox (not 0, not 2).
    let target_msgs = crate::inbox::drain(&home, "rpt-fx-target");
    // (c) ack_by_correlation must have settled the sender's dispatch row.
    // describe_message is the direct status probe — drain would miss a
    // delivering row. ReadAt = settled.
    let parent_status = crate::inbox::describe_message(&home, "m-dispatch-fx", "rpt-fx-sender");
    // (d) mark_completed must have cleared the seeded DispatchEntry.
    let remaining_tracking =
        crate::dispatch_tracking::take_pending_dispatchers_to(&home, "rpt-fx-sender");
    // (e) UX: exactly one total event, and it must be ReportResult.
    let ux_events = rec.0.lock().clone();
    let report_events: Vec<_> = ux_events
        .iter()
        .filter(|e| {
            matches!(
                e,
                UxEvent::Fleet(crate::channel::ux_event::FleetEvent::ReportResult { .. })
            )
        })
        .collect();
    // (f) record_triaged_if_present must have written a discharge ledger entry.
    let has_discharge =
        crate::daemon::discharge_ledger::lookup_discharge(&home, "rpt-fx-head-sha", "rpt-fx-job")
            .is_some();
    std::fs::remove_dir_all(&home).ok();

    assert!(
        !dm.contains("inbox_fallback") && !dm.contains("API unavailable"),
        "report via runtime=Some must deliver through the neutral service \
         (handle_send → settle_parent_after_successful_send, process_verdicts, \
         track_dispatch), not the socket fallback (delivery_mode={dm}): {response}"
    );
    assert_eq!(
        target_msgs.len(),
        1,
        "report must produce exactly 1 inbox message for the target; got {}",
        target_msgs.len()
    );
    assert!(
        matches!(parent_status, crate::inbox::MessageStatus::ReadAt(..)),
        "ack_by_correlation must settle the sender's dispatch row to ReadAt; \
         got {:?}",
        parent_status
    );
    assert!(
        remaining_tracking.is_empty(),
        "mark_completed must clear the DispatchEntry for the sender; \
         {} rows remain",
        remaining_tracking.len()
    );
    assert_eq!(
        ux_events.len(),
        1,
        "report must emit exactly 1 total UX event; got {}",
        ux_events.len()
    );
    assert_eq!(
        report_events.len(),
        1,
        "the single UX event must be ReportResult; got {} ReportResult events",
        report_events.len()
    );
    assert!(
        has_discharge,
        "record_triaged_if_present must write a discharge ledger entry \
         for the triaged head/job"
    );
}

/// d-...-4 invariant 3b: a successful delegate (request_kind=task) via
/// runtime=Some must deliver through the neutral service (not fallback),
/// produce exactly 1 inbox message for the target, and create a
/// dispatch tracking record. Currently adapter! strips runtime →
/// delivery via inbox_fallback.
#[test]
#[allow(clippy::unwrap_used)]
fn send_delegate_service_delivery_with_post_effects_2454() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let tag = format!(
        "del-fx-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let home = std::env::temp_dir().join(format!("agend-s12b2-{tag}"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  del-fx-sender:\n    backend: claude\n  \
         del-fx-target:\n    backend: claude\n",
    )
    .unwrap();
    let registry: crate::agent::AgentRegistry = Default::default();
    let configs: crate::api::ConfigRegistry = Default::default();
    let externals: crate::agent::ExternalRegistry = Default::default();

    // Register UX Recorder.
    use crate::channel::sink_registry::registry as ux_sink_registry;
    use crate::channel::ux_event::{UxEvent, UxEventSink};
    let rec = {
        struct Rec(parking_lot::Mutex<Vec<UxEvent>>);
        impl UxEventSink for Rec {
            fn emit(&self, event: &UxEvent) {
                self.0.lock().push(event.clone());
            }
        }
        std::sync::Arc::new(Rec(parking_lot::Mutex::new(Vec::new())))
    };
    ux_sink_registry().clear_for_test();
    ux_sink_registry().register(rec.clone() as std::sync::Arc<dyn UxEventSink>);

    let previous_home = std::env::var_os("AGEND_HOME");
    std::env::set_var("AGEND_HOME", &home);
    let response = invoke_runtime_mcp_tool(
        &home,
        &registry,
        &configs,
        &externals,
        "send",
        "del-fx-sender",
        json!({
            "instance": "del-fx-target",
            "request_kind": "task",
            "message": "delegate with post-effects",
            "task_id": "t-del-fx-001",
        }),
    );
    match previous_home {
        Some(value) => std::env::set_var("AGEND_HOME", value),
        None => std::env::remove_var("AGEND_HOME"),
    }

    // (a) Delivery must NOT be via inbox_fallback.
    let result_str = response["result"].to_string();
    // (b) Exactly 1 message in target's inbox.
    let target_msgs = crate::inbox::drain(&home, "del-fx-target");
    // (c) Dispatch tracking: extract exact rows (take_pending_dispatchers_to
    // returns AND removes them, so len is the definitive count).
    let tracking_rows =
        crate::dispatch_tracking::take_pending_dispatchers_to(&home, "del-fx-target");
    // (d) UX: exactly one DelegateTask event.
    let ux_events = rec.0.lock().clone();
    let delegate_events: Vec<_> = ux_events
        .iter()
        .filter(|e| {
            matches!(
                e,
                UxEvent::Fleet(crate::channel::ux_event::FleetEvent::DelegateTask { .. })
            )
        })
        .collect();
    std::fs::remove_dir_all(&home).ok();

    assert!(
        !result_str.contains("inbox_fallback") && !result_str.contains("API unavailable"),
        "delegate via runtime=Some must deliver through the neutral service \
         (handle_send → track_dispatch), not the socket fallback: {response}"
    );
    assert_eq!(
        target_msgs.len(),
        1,
        "delegate must produce exactly 1 inbox message for the target; got {}",
        target_msgs.len()
    );
    assert_eq!(
        tracking_rows.len(),
        1,
        "delegate must create exactly 1 dispatch tracking row for the target; got {}",
        tracking_rows.len()
    );
    assert_eq!(
        ux_events.len(),
        1,
        "delegate must emit exactly 1 total UX event; got {}",
        ux_events.len()
    );
    assert_eq!(
        delegate_events.len(),
        1,
        "the single UX event must be DelegateTask; got {} DelegateTask events",
        delegate_events.len()
    );
}

/// d-...-4 invariant 3c: source-owner guard — MCP adapter production
/// code (comms.rs, comms_delegate/mod.rs) must NOT call API-service-
/// level post-success functions, and API service (messaging.rs) must
/// NOT call MCP-decorator functions. Prevents adapter+service duplicate
/// tracking/settlement/receipt effects when the in-process path runs.
#[test]
fn send_post_success_owner_source_guard_2454() {
    let test_mod_marker = "#[cfg(test)]\nmod ";

    // SERVICE-level functions (owned by messaging.rs handle_send):
    // MCP adapters must NOT call these directly.
    let service_fns = [
        concat!("settle_parent_after", "_successful_send"),
        concat!("inject_", "provenance"),
        concat!("checkout_branch_if", "_requested"),
        concat!("process_", "verdicts"),
        concat!("track_", "dispatch"),
    ];

    // MCP DECORATOR functions (owned by comms.rs handlers):
    // API service must NOT call these.
    let decorator_fns = [
        concat!("dispatch_tracking::", "mark_completed"),
        concat!("ack_by_", "correlation"),
        concat!("record_triaged_if", "_present"),
    ];

    // Check: MCP adapters do NOT call service-level functions.
    let adapter_sources: &[(&str, &str)] = &[
        ("comms.rs", include_str!("../../mcp/handlers/comms.rs")),
        (
            "comms_delegate/mod.rs",
            include_str!("../../mcp/handlers/comms_delegate/mod.rs"),
        ),
    ];
    for (file, src) in adapter_sources {
        let boundary = src.rfind(test_mod_marker).unwrap_or(src.len());
        let production = &src[..boundary];
        for needle in &service_fns {
            for (ln, line) in production.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") || trimmed.starts_with("///") {
                    continue;
                }
                assert!(
                    !line.contains(needle),
                    "MCP adapter {file}:{} calls service-level function `{needle}` — \
                     this belongs to handle_send, not the MCP adapter. \
                     Duplicate effects will fire when the in-process path runs.",
                    ln + 1
                );
            }
        }
    }

    // Check: API service does NOT call MCP-decorator functions.
    let svc_src = include_str!("messaging.rs");
    let svc_boundary = svc_src.rfind(test_mod_marker).unwrap_or(svc_src.len());
    let svc_production = &svc_src[..svc_boundary];
    for needle in &decorator_fns {
        for (ln, line) in svc_production.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            assert!(
                !line.contains(needle),
                "API service messaging.rs:{} calls MCP-decorator function `{needle}` — \
                 this belongs to the MCP comms adapter, not handle_send. \
                 Duplicate effects will fire when the in-process path runs.",
                ln + 1
            );
        }
    }
}

/// d-...-4 invariant 4: typed shared service boundary guard.
/// The neutral typed SendRequest/SendOutcome service must live in
/// src/agent_ops/messaging.rs, with a typed (non-raw-Value) entry point.
/// Both the thin API SEND adapter (messaging.rs) and the MCP runtime
/// SEND family must converge on this module. The neutral module must
/// NOT depend on API-layer types (HandlerCtx, ConfigRegistry,
/// ExternalRegistry, raw serde_json::Value service entry, crate::api).
/// RED while the neutral service source is absent.
#[test]
fn send_typed_shared_service_boundary_guard_2454() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let neutral_path = std::path::Path::new(manifest_dir).join("src/agent_ops/messaging.rs");

    // Gate: neutral service source must exist.
    assert!(
        neutral_path.exists(),
        "neutral typed service source src/agent_ops/messaging.rs must exist; \
         d-...-4 requires a typed SendRequest/SendOutcome service below \
         both the API and MCP adapters"
    );

    let src =
        std::fs::read_to_string(&neutral_path).expect("neutral service source must be readable");
    let test_mod_marker = "#[cfg(test)]\nmod ";
    let boundary = src.rfind(test_mod_marker).unwrap_or(src.len());
    let production = &src[..boundary];

    // Must expose typed request/outcome boundary (not raw Value).
    assert!(
        production.contains("SendRequest"),
        "neutral service must define SendRequest typed boundary"
    );
    assert!(
        production.contains("SendOutcome"),
        "neutral service must define SendOutcome typed boundary"
    );

    // Must have a typed service entry function (not raw Value → Value).
    // Handles multiline signatures: scan for fn...SendRequest blocks
    // ending with -> SendOutcome within a bounded window.
    let has_typed_entry = {
        let lines: Vec<&str> = production.lines().collect();
        let mut found = false;
        for (i, line) in lines.iter().enumerate() {
            let t = line.trim();
            if t.starts_with("//") || t.starts_with("///") {
                continue;
            }
            if !t.contains("fn ") {
                continue;
            }
            let window_end = (i + 6).min(lines.len());
            let window: String = lines[i..window_end].join(" ");
            if window.contains("SendRequest") && window.contains("SendOutcome") {
                found = true;
                break;
            }
        }
        found
    };
    assert!(
        has_typed_entry,
        "neutral service must have a typed entry fn(SendRequest) -> SendOutcome; \
         a raw-Value send_with_runtime_or_legacy approach must not pass"
    );

    // Forbidden API-layer types — the neutral service must be
    // dependency-free from the API adapter layer.
    let forbidden = [
        "HandlerCtx",
        "ConfigRegistry",
        "ExternalRegistry",
        "crate::api",
    ];
    for needle in &forbidden {
        for (ln, line) in production.lines().enumerate() {
            let t = line.trim();
            if t.starts_with("//") || t.starts_with("///") {
                continue;
            }
            assert!(
                !line.contains(needle),
                "neutral service src/agent_ops/messaging.rs:{} contains \
                 forbidden API-layer dependency `{needle}` — the neutral \
                 service must be below both API and MCP adapters",
                ln + 1
            );
        }
    }

    // Forbidden: raw serde_json::Value as service entry parameter.
    let has_raw_value_entry = production.lines().any(|line| {
        let t = line.trim();
        !t.starts_with("//")
            && !t.starts_with("///")
            && t.contains("fn ")
            && (t.contains("Value") || t.contains("&serde_json"))
            && !t.contains("SendRequest")
            && !t.contains("SendOutcome")
    });
    assert!(
        !has_raw_value_entry,
        "neutral service must not have a raw Value / serde_json service \
         entry — use typed SendRequest/SendOutcome boundary"
    );

    // ── Convergence proof: both adapters reference this neutral owner ──

    let neutral_mod = concat!("crate::agent_ops::", "messaging");

    // (a) agent_ops.rs must register the messaging submodule.
    let ops_src = include_str!("../../agent_ops.rs");
    let ops_boundary = ops_src.rfind(test_mod_marker).unwrap_or(ops_src.len());
    let ops_production = &ops_src[..ops_boundary];
    let has_mod_decl = ops_production.lines().any(|line| {
        let t = line.trim();
        !t.starts_with("//")
            && !t.starts_with("///")
            && (t == "mod messaging;"
                || t == "pub mod messaging;"
                || t == "pub(crate) mod messaging;")
    });
    assert!(
        has_mod_decl,
        "src/agent_ops.rs must register `mod messaging` submodule \
         to expose the neutral typed service"
    );

    // (b) API SEND adapter (src/api/handlers/messaging.rs) must
    // reference the neutral owner.
    let api_svc_src = include_str!("messaging.rs");
    let api_boundary = api_svc_src
        .rfind(test_mod_marker)
        .unwrap_or(api_svc_src.len());
    let api_production = &api_svc_src[..api_boundary];
    let api_refs_neutral = api_production.lines().any(|line| {
        let t = line.trim();
        !t.starts_with("//") && !t.starts_with("///") && line.contains(neutral_mod)
    });
    assert!(
        api_refs_neutral,
        "API SEND adapter (api/handlers/messaging.rs) must reference \
         {neutral_mod} — both adapters must converge on the neutral owner"
    );

    // (c) MCP runtime SEND adapter sources must reference the neutral
    // owner at least once (behavioral REDs already prove report/delegate
    // paths, so this is a structural convergence check only).
    let mcp_adapter_sources: &[(&str, &str)] = &[
        ("comms.rs", include_str!("../../mcp/handlers/comms.rs")),
        (
            "comms_delegate/mod.rs",
            include_str!("../../mcp/handlers/comms_delegate/mod.rs"),
        ),
        (
            "dispatch.rs",
            include_str!("../../mcp/handlers/dispatch.rs"),
        ),
    ];
    let mut mcp_refs_neutral = false;
    for (_file, src) in mcp_adapter_sources {
        let b = src.rfind(test_mod_marker).unwrap_or(src.len());
        let prod = &src[..b];
        if prod.lines().any(|line| {
            let t = line.trim();
            !t.starts_with("//") && !t.starts_with("///") && line.contains(neutral_mod)
        }) {
            mcp_refs_neutral = true;
            break;
        }
    }
    assert!(
        mcp_refs_neutral,
        "MCP SEND adapter sources (comms.rs, comms_delegate/mod.rs, dispatch.rs) \
         must reference {neutral_mod} at least once — both adapters must converge \
         on the neutral owner"
    );
}
