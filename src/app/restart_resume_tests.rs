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
        instance_name: "lead",
        sender: &EMPTY_SENDER,
        runtime: Some(&runtime),
    };
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

#[test]
fn successor_argv_contract_is_hidden_and_commit_only() {
    let app = include_str!("mod.rs");
    let main = include_str!("../main.rs");
    assert!(
        app.contains("--app-restart-requester"),
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
        app.contains("if attached_mode") && app.contains("spawn_self_kick_bootstrap"),
        "resume scheduling must be owned-app-only and use the existing bootstrap"
    );
    assert!(
        app.contains("resolve_name_by_uuid"),
        "restore scheduling must reverse-resolve the exact UUID, with no name fallback"
    );
}

#[test]
fn restore_resume_scheduling_has_one_production_call_site() {
    let app = include_str!("mod.rs");
    assert_eq!(
        app.matches("spawn_self_kick_bootstrap").count(),
        1,
        "app restore must arm the existing self-kick exactly once"
    );
}
