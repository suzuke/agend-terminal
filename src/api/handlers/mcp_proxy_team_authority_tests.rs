use super::{handle_mcp_tool, HandlerCtx};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

struct TestHome {
    path: PathBuf,
    previous: Option<std::ffi::OsString>,
}

impl TestHome {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "agend-team-authority-{tag}-{}",
            crate::types::InstanceId::new().short()
        ));
        std::fs::create_dir_all(&path).expect("create test home");
        let previous = std::env::var_os("AGEND_HOME");
        std::env::set_var("AGEND_HOME", &path);
        Self { path, previous }
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var("AGEND_HOME", value),
            None => std::env::remove_var("AGEND_HOME"),
        }
        std::fs::remove_dir_all(&self.path).ok();
    }
}

fn seed_team(home: &Path) {
    let result = crate::teams::create(
        home,
        &json!({
            "name": "devs",
            "members": ["lead", "member"],
            "orchestrator": "lead",
        }),
    );
    assert_eq!(result["status"], "created", "seed failed: {result}");
}

fn insert_live(registry: &crate::agent::AgentRegistry, name: &str) {
    let id = crate::types::InstanceId::new();
    registry
        .lock()
        .insert(id, crate::agent::mk_test_handle(name, id));
}

fn test_ctx<'a>(
    home: &'a Path,
    registry: &'a crate::agent::AgentRegistry,
    configs: &'a crate::api::ConfigRegistry,
    externals: &'a crate::agent::ExternalRegistry,
) -> HandlerCtx<'a> {
    HandlerCtx {
        registry,
        configs,
        externals,
        notifier: None,
        home,
        capability: crate::api::RestartCapability::Unsupported,
        app_restart: None,
        post_flush: crate::api::app_restart::PostFlushSlot::new(),
        shutdown: None,
    }
}

fn call_team_update(ctx: &HandlerCtx<'_>, instance: &str, new_orchestrator: &str) -> Value {
    handle_mcp_tool(
        &json!({
            "instance": instance,
            "tool": "team",
            "arguments": {
                "action": "update",
                "name": "devs",
                "orchestrator": new_orchestrator,
            },
        }),
        ctx,
    )
}

#[test]
fn non_orchestrator_cannot_promote_itself_and_fleet_is_unchanged() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let home = TestHome::new("member-denied");
    seed_team(&home.path);
    let before = std::fs::read(crate::fleet::fleet_yaml_path(&home.path)).unwrap();
    let registry: crate::agent::AgentRegistry = Default::default();
    insert_live(&registry, "member");
    let configs = Default::default();
    let externals = Default::default();
    let response = call_team_update(
        &test_ctx(&home.path, &registry, &configs, &externals),
        "member",
        "member",
    );

    assert!(
        response["result"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("current orchestrator")),
        "ordinary member must be refused: {response}"
    );
    assert_eq!(
        std::fs::read(crate::fleet::fleet_yaml_path(&home.path)).unwrap(),
        before,
        "a refused promotion must leave fleet.yaml byte-identical"
    );
}

#[test]
fn current_orchestrator_can_hand_over_through_real_mcp_ingress() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let home = TestHome::new("orchestrator-handover");
    seed_team(&home.path);
    let registry: crate::agent::AgentRegistry = Default::default();
    insert_live(&registry, "lead");
    let configs = Default::default();
    let externals = Default::default();
    let response = call_team_update(
        &test_ctx(&home.path, &registry, &configs, &externals),
        "lead",
        "member",
    );

    assert_eq!(response["result"]["status"], "updated", "{response}");
    assert_eq!(
        crate::teams::list(&home.path)["teams"][0]["orchestrator"],
        "member"
    );
}

#[test]
fn operator_api_channel_can_hand_over() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let home = TestHome::new("operator-handover");
    seed_team(&home.path);
    let registry = Default::default();
    let configs = Default::default();
    let externals = Default::default();
    let response = crate::api::handlers::team::handle_update_team(
        &json!({"name": "devs", "orchestrator": "member"}),
        &test_ctx(&home.path, &registry, &configs, &externals),
    );

    assert_eq!(response["result"]["status"], "updated", "{response}");
    assert_eq!(
        crate::teams::list(&home.path)["teams"][0]["orchestrator"],
        "member"
    );
}

#[test]
fn unknown_or_dead_claimed_instance_is_refused() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let home = TestHome::new("dead-refused");
    seed_team(&home.path);
    let registry = Default::default();
    let configs = Default::default();
    let externals = Default::default();
    let response = call_team_update(
        &test_ctx(&home.path, &registry, &configs, &externals),
        "lead",
        "member",
    );

    assert_eq!(
        response["ok"], false,
        "dead claimed identity must fail: {response}"
    );
    assert!(response["error"]
        .as_str()
        .is_some_and(|error| error.contains("live")));
    assert_eq!(
        crate::teams::list(&home.path)["teams"][0]["orchestrator"],
        "lead"
    );
}

#[test]
fn orchestrator_handover_writes_old_new_and_caller_audit() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let home = TestHome::new("handover-audit");
    seed_team(&home.path);
    let registry: crate::agent::AgentRegistry = Default::default();
    insert_live(&registry, "lead");
    let configs = Default::default();
    let externals = Default::default();
    let response = call_team_update(
        &test_ctx(&home.path, &registry, &configs, &externals),
        "lead",
        "member",
    );
    assert_eq!(response["result"]["status"], "updated", "{response}");

    let audit = std::fs::read_to_string(home.path.join("event-log.jsonl"))
        .expect("handover must create durable audit log");
    assert!(audit.contains("team_orchestrator_change"), "{audit}");
    assert!(audit.contains("old=lead"), "{audit}");
    assert!(audit.contains("new=member"), "{audit}");
    assert!(audit.contains("caller=lead"), "{audit}");
}

#[test]
fn only_operator_handover_clears_missing_orchestrator_task() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let home = TestHome::new("operator-only-cleanup");
    seed_team(&home.path);
    let created = crate::tasks::handle(
        &home.path,
        "system",
        &json!({
            "action": "create",
            "title": "Team 'devs' needs new orchestrator (test)",
            "priority": "urgent",
        }),
    );
    assert!(created.get("error").is_none(), "{created}");

    let registry: crate::agent::AgentRegistry = Default::default();
    insert_live(&registry, "lead");
    let configs = Default::default();
    let externals = Default::default();
    let ctx = test_ctx(&home.path, &registry, &configs, &externals);
    let response = call_team_update(&ctx, "lead", "member");
    assert_eq!(response["result"]["status"], "updated", "{response}");
    assert!(crate::tasks::list_all(&home.path)
        .iter()
        .any(|task| !task.status.is_terminal()));

    let response = crate::api::handlers::team::handle_update_team(
        &json!({"name": "devs", "orchestrator": "lead"}),
        &ctx,
    );
    assert_eq!(response["result"]["status"], "updated", "{response}");
    assert!(crate::tasks::list_all(&home.path)
        .iter()
        .all(|task| task.status.is_terminal()));
}

/// The API cookie authenticates same-uid daemon access, not a distinct agent.
/// `mcp_proxy.rs::live_requester_id` deliberately trusts an unambiguous live
/// name claim; `auth_cookie::SAME_UID_OPERATOR_ISOLATION` documents the wider
/// isolation boundary. This test pins that limitation instead of claiming the
/// gate prevents deliberate same-uid impersonation.
#[test]
fn same_uid_live_orchestrator_name_claim_is_trusted_by_design() {
    let _guard = crate::mcp::handlers::fleet_test_guard();
    let home = TestHome::new("same-uid-boundary");
    seed_team(&home.path);
    let registry: crate::agent::AgentRegistry = Default::default();
    insert_live(&registry, "lead");
    let configs = Default::default();
    let externals = Default::default();
    let response = call_team_update(
        &test_ctx(&home.path, &registry, &configs, &externals),
        "lead",
        "member",
    );

    assert_eq!(response["result"]["status"], "updated", "{response}");
}
