//! Deterministic RED witnesses for app-restart requester resume.
//!
//! These tests are intentionally committed before the production wiring. The
//! dispatch test crosses the real restart handler entry; the structural guards
//! pin the successor-argv and restore scheduling contract without requiring a
//! TTY or a live backend.

#[cfg(unix)]
#[test]
fn restart_request_real_dispatch_carries_stable_instance_id() {
    use crate::api::app_restart::{AppRestart, AppRestartGate, AppRestartRequest};
    use crate::identity::Sender;
    use crate::mcp::handlers::dispatch::{dispatch_restart_daemon, HandlerCtx, RuntimeContext};
    use crate::types::InstanceId;
    use serde_json::json;
    use std::path::Path;
    use std::time::Duration;

    let home = std::env::temp_dir().join(format!(
        "agend-app-restart-requester-red-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).expect("create test home");
    let id = InstanceId::new();
    std::fs::write(
        home.join("fleet.yaml"),
        format!(
            "instances:\n  lead:\n    id: {}\n    backend: codex\n",
            id.full()
        ),
    )
    .expect("write fleet");

    let gate = AppRestartGate::new();
    let (tx, rx) = crossbeam_channel::bounded::<AppRestartRequest>(1);
    let registry: crate::agent::AgentRegistry = Default::default();
    registry
        .lock()
        .insert(id, crate::agent::mk_test_handle("lead", id));
    let runtime = RuntimeContext {
        registry,
        configs: Default::default(),
        externals: Default::default(),
        capability: crate::api::RestartCapability::App,
        app_restart: Some(AppRestart { tx, gate }),
        post_flush: Some(crate::api::app_restart::PostFlushSlot::new()),
        notifier: None,
        shutdown: None,
    };
    static EMPTY_SENDER: Option<Sender> = None;
    let args = json!({});
    let ctx = HandlerCtx {
        home: Path::new(&home),
        args: &args,
        instance_name: "lead",
        sender: &EMPTY_SENDER,
        runtime: Some(&runtime),
    };
    // fire-and-forget: test observer is joined below after the handler response.
    let request_debug = std::thread::spawn(move || {
        let req = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("restart request delivered");
        let debug = format!("{req:?}");
        req.reply
            .send(crate::api::app_restart::AppRestartVerdict::Prepared)
            .expect("prepared verdict delivered");
        debug
    });

    let response = dispatch_restart_daemon(&ctx);
    assert_eq!(
        response["restart"], "prepared",
        "real app restart entry must prepare"
    );
    let debug = request_debug.join().expect("request observer joined");
    assert!(
        debug.contains(&id.full()),
        "request must carry the stable fleet InstanceId, got {debug}"
    );
    std::fs::remove_dir_all(home).ok();
}

#[cfg(unix)]
#[test]
fn unresolved_managed_caller_fails_before_restart_gate() {
    use crate::api::app_restart::{AppRestart, AppRestartGate, AppRestartRequest};
    use crate::identity::Sender;
    use crate::mcp::handlers::dispatch::{dispatch_restart_daemon, HandlerCtx, RuntimeContext};
    use serde_json::json;
    use std::path::Path;
    use std::time::Duration;

    let home = std::env::temp_dir().join(format!(
        "agend-app-restart-unresolved-red-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).expect("create test home");
    let gate = AppRestartGate::new();
    let gate_view = gate.clone();
    let (tx, rx) = crossbeam_channel::bounded::<AppRestartRequest>(1);
    let runtime = RuntimeContext {
        registry: Default::default(),
        configs: Default::default(),
        externals: Default::default(),
        capability: crate::api::RestartCapability::App,
        app_restart: Some(AppRestart { tx, gate }),
        post_flush: Some(crate::api::app_restart::PostFlushSlot::new()),
        notifier: None,
        shutdown: None,
    };
    static EMPTY_SENDER: Option<Sender> = None;
    let args = json!({});
    let ctx = HandlerCtx {
        home: Path::new(&home),
        args: &args,
        instance_name: "missing-managed-caller",
        sender: &EMPTY_SENDER,
        runtime: Some(&runtime),
    };
    // fire-and-forget: test observer is joined below to prove no request arrived.
    let saw_request = std::thread::spawn(move || {
        let Ok(req) = rx.recv_timeout(Duration::from_secs(2)) else {
            return false;
        };
        let _ = req
            .reply
            .send(crate::api::app_restart::AppRestartVerdict::Aborted(
                "unexpected restart request".into(),
            ));
        true
    });

    let response = dispatch_restart_daemon(&ctx);
    let saw_request = saw_request.join().expect("request observer joined");
    assert!(
        !saw_request,
        "unresolved managed callers must not enter the restart gate"
    );
    assert!(
        !gate_view.is_committing(),
        "unresolved caller must not commit"
    );
    assert!(
        response["error"]
            .as_str()
            .unwrap_or_default()
            .contains("stable InstanceId"),
        "failure must identify the missing stable requester identity: {response}"
    );
    std::fs::remove_dir_all(home).ok();
}

#[cfg(unix)]
#[test]
fn unresolved_managed_caller_fails_at_mcp_ingress_before_worker() {
    use crate::api::app_restart::{AppRestart, AppRestartGate, AppRestartRequest};
    use crate::api::handlers::{mcp_proxy, HandlerCtx};
    use serde_json::json;

    let _guard = crate::mcp::handlers::fleet_test_guard();
    let home = std::env::temp_dir().join(format!(
        "agend-app-restart-ingress-unresolved-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).expect("create test home");
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  managed-caller:\n    id: not-a-stable-uuid\n    backend: codex\n",
    )
    .expect("write fleet");

    let gate = AppRestartGate::new();
    let gate_view = gate.clone();
    let (tx, rx) = crossbeam_channel::bounded::<AppRestartRequest>(1);
    let app_restart = AppRestart { tx, gate };
    let registry: crate::agent::AgentRegistry = Default::default();
    let configs: crate::api::ConfigRegistry = Default::default();
    let externals: crate::agent::ExternalRegistry = Default::default();
    let ctx = HandlerCtx {
        registry: &registry,
        configs: &configs,
        externals: &externals,
        notifier: None,
        home: &home,
        capability: crate::api::RestartCapability::App,
        app_restart: Some(&app_restart),
        post_flush: crate::api::app_restart::PostFlushSlot::new(),
        shutdown: None,
    };

    let response = mcp_proxy::handle_mcp_tool(
        &json!({
            "tool": "restart_daemon",
            "instance": "managed-caller",
            "arguments": {}
        }),
        &ctx,
    );

    assert_eq!(
        response["ok"], false,
        "ingress must reject unresolved caller"
    );
    assert!(
        response["error"]
            .as_str()
            .unwrap_or_default()
            .contains("stable InstanceId"),
        "ingress failure must identify the stable requester requirement: {response}"
    );
    assert!(
        rx.try_recv().is_err(),
        "unresolved caller must not start an MCP worker or enqueue restart"
    );
    assert!(
        !gate_view.is_committing(),
        "unresolved caller must leave the restart gate untouched"
    );
    std::fs::remove_dir_all(home).ok();
}

#[cfg(unix)]
#[test]
fn live_mcp_ingress_uses_registry_identity_not_default_fleet() {
    use crate::api::app_restart::{AppRestart, AppRestartGate, AppRestartRequest};
    use crate::api::handlers::{mcp_proxy, HandlerCtx};
    use crate::types::InstanceId;
    use serde_json::json;
    use std::time::Duration;

    let _guard = crate::mcp::handlers::fleet_test_guard();
    let home = std::env::temp_dir().join(format!(
        "agend-app-restart-ingress-override-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).expect("create test home");
    let requester_id = InstanceId::new();
    let conflicting_id = InstanceId::new();
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        format!(
            "instances:\n  lead:\n    id: {}\n    backend: codex\n",
            conflicting_id.full()
        ),
    )
    .expect("write conflicting default fleet");
    std::fs::write(
        home.join("override-fleet.yaml"),
        format!(
            "instances:\n  lead:\n    id: {}\n    backend: codex\n",
            requester_id.full()
        ),
    )
    .expect("write override fleet");

    let registry: crate::agent::AgentRegistry = Default::default();
    registry.lock().insert(
        requester_id,
        crate::agent::mk_test_handle("lead", requester_id),
    );
    let configs: crate::api::ConfigRegistry = Default::default();
    let externals: crate::agent::ExternalRegistry = Default::default();
    let gate = AppRestartGate::new();
    let (tx, rx) = crossbeam_channel::bounded::<AppRestartRequest>(1);
    let app_restart = AppRestart { tx, gate };
    let ctx = HandlerCtx {
        registry: &registry,
        configs: &configs,
        externals: &externals,
        notifier: None,
        home: &home,
        capability: crate::api::RestartCapability::App,
        app_restart: Some(&app_restart),
        post_flush: crate::api::app_restart::PostFlushSlot::new(),
        shutdown: None,
    };

    let previous_home = std::env::var_os("AGEND_HOME");
    std::env::set_var("AGEND_HOME", &home);
    // fire-and-forget: test observer is retained and joined below.
    let observer = std::thread::spawn(move || {
        let request = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("restart request enqueued");
        let debug = format!("{request:?}");
        request
            .reply
            .send(crate::api::app_restart::AppRestartVerdict::Prepared)
            .expect("prepared verdict delivered");
        debug
    });
    let response = mcp_proxy::handle_mcp_tool(
        &json!({
            "tool": "restart_daemon",
            "instance": "lead",
            "arguments": {}
        }),
        &ctx,
    );
    let debug = observer.join().expect("request observer joined");

    match previous_home {
        Some(value) => std::env::set_var("AGEND_HOME", value),
        None => std::env::remove_var("AGEND_HOME"),
    }
    registry.lock().remove(&requester_id);
    std::fs::remove_dir_all(home).ok();

    assert_eq!(
        response["ok"], true,
        "live requester must proceed: {response}"
    );
    assert_eq!(response["result"]["restart"], "prepared");
    assert!(
        debug.contains(&requester_id.full()) && !debug.contains(&conflicting_id.full()),
        "request must carry the live override-fleet UUID only: {debug}"
    );
}

#[test]
fn successor_argv_contract_is_hidden_and_commit_only() {
    let app = include_str!("mod.rs");
    let resume = include_str!("restart_resume.rs");
    let main = include_str!("../main.rs");
    assert!(
        resume.contains("--app-restart-requester"),
        "app restart argv must carry a hidden requester value"
    );
    assert!(
        main.contains("app_restart_requester"),
        "successor CLI must parse the hidden requester value"
    );
    assert!(
        app.contains("commit_app_restart(fleet_path_override, requester_id)"),
        "only the committed restart outcome may pass requester argv"
    );
}

#[test]
fn cold_boot_and_uuid_mismatch_do_not_schedule_resume() {
    let app = include_str!("mod.rs");
    assert!(
        !app.contains("spawn_self_kick_bootstrap"),
        "permanent thin-client app must not schedule daemon-owner resume work"
    );
}

#[test]
fn restore_resume_scheduling_has_no_app_production_call_site() {
    let app = include_str!("mod.rs");
    assert_eq!(
        app.matches("spawn_self_kick_bootstrap").count(),
        0,
        "permanent thin-client app must leave resume ownership to the daemon"
    );
}

#[cfg(unix)]
#[test]
fn successor_argv_behavior_is_cold_boot_or_exact_requester() {
    use super::restart_argv;
    use crate::types::InstanceId;

    let id = InstanceId::new();
    let cold = restart_argv(Some("fleet.yaml"), None);
    assert!(!cold.contains(&"--app-restart-requester".to_string()));
    let committed = restart_argv(Some("fleet.yaml"), Some(id));
    assert_eq!(
        committed,
        vec![
            "app".to_string(),
            "--fleet".to_string(),
            "fleet.yaml".to_string(),
            "--app-restart-requester".to_string(),
            id.full(),
        ]
    );
}
