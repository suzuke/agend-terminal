//! Sprint 30 — CRUD consolidation routing tests.
//! Verifies action-based dispatch routes to correct handlers.

#![allow(clippy::unwrap_used)]

/// Helper: call handle_tool via the MCP roundtrip test infrastructure.
/// Since handle_tool is binary-internal, these tests verify via the
/// daemon API proxy path (mcp_tool method).
///
/// For now, verify structurally that the consolidated tool names exist
/// in the handler dispatch and route correctly by checking the handler
/// mod.rs source for the dispatch arms.

use std::path::Path;

fn handler_source() -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/mcp/handlers/mod.rs"),
    )
    .expect("read handlers/mod.rs")
}

fn prod_section(src: &str) -> &str {
    let cutoff = src.find("#[cfg(test)]").unwrap_or(src.len());
    &src[..cutoff]
}

#[test]
fn decision_routes_post_list_update() {
    let src = handler_source();
    let prod = prod_section(&src);
    assert!(prod.contains(r#""decision" =>"#), "decision consolidated tool must exist");
    assert!(prod.contains(r#""post" => task::handle_post_decision"#));
    assert!(prod.contains(r#""list" => task::handle_list_decisions"#));
    assert!(prod.contains(r#""update" => task::handle_update_decision"#));
}

#[test]
fn team_routes_create_delete_list_update() {
    let src = handler_source();
    let prod = prod_section(&src);
    assert!(prod.contains(r#""team" =>"#));
    assert!(prod.contains(r#""create" => task::handle_create_team"#));
    assert!(prod.contains(r#""delete" => task::handle_delete_team"#));
    assert!(prod.contains(r#""list" => task::handle_list_teams"#));
    assert!(prod.contains(r#""update" => task::handle_update_team"#));
}

#[test]
fn schedule_routes_create_list_update_delete() {
    let src = handler_source();
    let prod = prod_section(&src);
    assert!(prod.contains(r#""schedule" =>"#));
    assert!(prod.contains(r#""create" => schedule::handle_create_schedule"#));
    assert!(prod.contains(r#""list" => schedule::handle_list_schedules"#));
    assert!(prod.contains(r#""update" => schedule::handle_update_schedule"#));
    assert!(prod.contains(r#""delete" => schedule::handle_delete_schedule"#));
}

#[test]
fn deployment_routes_deploy_teardown_list() {
    let src = handler_source();
    let prod = prod_section(&src);
    assert!(prod.contains(r#""deployment" =>"#));
    assert!(prod.contains(r#""deploy" => schedule::handle_deploy_template"#));
    assert!(prod.contains(r#""teardown" => schedule::handle_teardown_deployment"#));
    assert!(prod.contains(r#""list" => schedule::handle_list_deployments"#));
}

#[test]
fn repo_routes_checkout_release() {
    let src = handler_source();
    let prod = prod_section(&src);
    assert!(prod.contains(r#""repo" =>"#));
    assert!(prod.contains(r#""checkout" => ci::handle_checkout_repo"#));
    assert!(prod.contains(r#""release" => ci::handle_release_repo"#));
}

#[test]
fn ci_routes_watch_unwatch() {
    let src = handler_source();
    let prod = prod_section(&src);
    assert!(prod.contains(r#""ci" =>"#));
    assert!(prod.contains(r#""watch" => ci::handle_watch_ci"#));
    assert!(prod.contains(r#""unwatch" => ci::handle_unwatch_ci"#));
}

#[test]
fn health_routes_report_clear() {
    let src = handler_source();
    let prod = prod_section(&src);
    assert!(prod.contains(r#""health" =>"#));
    assert!(prod.contains(r#""report" => instance::handle_report_health"#));
    assert!(prod.contains(r#""clear" => instance::handle_clear_blocked_reason"#));
}

#[test]
fn unknown_action_returns_error_pattern() {
    let src = handler_source();
    let prod = prod_section(&src);
    // Each consolidated tool must have an error arm for unknown actions
    for tool in ["decision", "team", "schedule", "deployment", "repo", "ci", "health"] {
        assert!(
            prod.contains(&format!("unknown {tool} action")),
            "consolidated tool '{tool}' must have unknown-action error arm"
        );
    }
}
