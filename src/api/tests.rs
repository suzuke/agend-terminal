#![allow(clippy::unwrap_used)]

use super::*;

/// #2453 R2 P0 seam: `maybe_evict_noncacheable_restart` — the handle_session
/// post-flush step — evicts the request_id when the post-flush slot is either
/// ARMED (a `prepared` response) OR MARKED non-cacheable (a transient loser /
/// abort / timeout), and leaves an ordinary (neither) response cached.
#[test]
fn maybe_evict_noncacheable_restart_evicts_armed_and_marked_only() {
    use crate::api::app_restart::PostFlushSlot;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let fp = request_dedup::operation_fingerprint("mcp_tool", &json!({"tool": "restart_daemon"}));

    // Assert a retry re-runs the handler after `slot` (set up by `arm`) is evicted.
    fn evict_then_retry_reruns(fp: u64, id: &str, arm: impl FnOnce(&PostFlushSlot)) -> bool {
        let cache = request_dedup::DedupCache::default();
        cache.dispatch(
            Some(id),
            fp,
            std::time::Duration::from_secs(5),
            || json!({"ok": true, "restart": "prepared"}),
        );
        let slot = PostFlushSlot::new();
        arm(&slot);
        maybe_evict_noncacheable_restart(Some(id), &slot, &cache);
        let reran = Arc::new(AtomicBool::new(false));
        let rr = Arc::clone(&reran);
        cache.dispatch(Some(id), fp, std::time::Duration::from_secs(5), move || {
            rr.store(true, Ordering::SeqCst);
            json!({"ok": false})
        });
        reran.load(Ordering::SeqCst)
    }

    // ARMED (prepared) ⇒ evicted → retry re-runs.
    assert!(
        evict_then_retry_reruns(fp, "armed", |s| {
            assert!(s.register(Box::new(|| {})));
        }),
        "an armed (prepared) response must be evicted so the retry re-runs"
    );
    // MARKED non-cacheable (transient loser/abort/timeout, NOT armed) ⇒ evicted too.
    assert!(
        evict_then_retry_reruns(fp, "marked", |s| s.mark_non_cacheable()),
        "a marked (transient) restart response must be evicted so the retry re-runs"
    );

    // Neither armed nor marked ⇒ an ordinary response stays cached (retry must NOT re-run).
    let cache2 = request_dedup::DedupCache::default();
    cache2.dispatch(
        Some("plain"),
        fp,
        std::time::Duration::from_secs(5),
        || json!({"ok": true, "n": 1}),
    );
    let unarmed = PostFlushSlot::new();
    maybe_evict_noncacheable_restart(Some("plain"), &unarmed, &cache2);
    let reran2 = Arc::new(AtomicBool::new(false));
    let rr2 = Arc::clone(&reran2);
    let cached = cache2.dispatch(
        Some("plain"),
        fp,
        std::time::Duration::from_secs(5),
        move || {
            rr2.store(true, Ordering::SeqCst);
            json!({"ok": true, "n": 2})
        },
    );
    assert!(
        !reran2.load(Ordering::SeqCst),
        "an ordinary (unarmed) response must stay cached — the retry must NOT re-run"
    );
    assert_eq!(cached["n"], 1, "the retry observed the cached response");
}

pub(super) fn tmp_home(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-api-test-{}-{}-{}",
        std::process::id(),
        name,
        id
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// Windows-only #893 regression: the path returned by
/// `validate_working_directory` becomes the PTY cwd
/// (`agent::build_command` -> `cmd.cwd`). It MUST NOT carry the `\\?\`
/// UNC verbatim prefix that `std::fs::canonicalize` returns on Windows —
/// a `\\?\`-prefixed cwd makes cmd.exe-based backends warn "UNC paths are
/// not supported" and fall back to C:\Windows. `dunce::canonicalize`
/// strips the prefix when safe; falling back to `std::fs::canonicalize`
/// here would re-introduce the bug. The validation must also still SUCCEED
/// (exercises `is_under_allowed_root`, which must canonicalize the root the
/// same way or the `starts_with` check would spuriously reject).
#[cfg(windows)]
#[test]
fn validate_work_dir_strips_verbatim_prefix_on_windows() {
    let home = tmp_home("validate_verbatim");
    let work = crate::paths::workspace_dir(&home).join("agent");
    std::fs::create_dir_all(&work).expect("create dir");
    let resolved =
        validate_working_directory(&work, &home).expect("existing path under home must validate");
    let resolved_str = resolved.to_string_lossy();
    assert!(
        !resolved_str.starts_with(r"\\?\"),
        "validate_working_directory must strip the Windows `\\\\?\\` verbatim \
             prefix (got {resolved_str:?}); this path becomes the PTY cwd and a \
             `\\\\?\\` cwd breaks cmd.exe-based backends — keep dunce::canonicalize, \
             do not fall back to std::fs::canonicalize"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn validate_work_dir_rejects_outside_roots() {
    let home = tmp_home("validate_outside");
    // /tmp exists but is not under home or workspace
    let outside = std::path::PathBuf::from("/tmp");
    let err = validate_working_directory(&outside, &home).unwrap_err();
    assert!(
        format!("{err}").contains("outside allowed roots"),
        "expected roots rejection, got: {err}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
#[serial_test::serial(env)]
fn validate_work_dir_env_override_accepted() {
    let home = tmp_home("validate_env");
    // Use a sibling dir of home (not under home) as custom root
    let custom = home.parent().unwrap().join("agend-custom-root-707");
    std::fs::create_dir_all(&custom).expect("create custom");
    let sep = if cfg!(windows) { ";" } else { ":" };
    let canonical_custom = std::fs::canonicalize(&custom).unwrap_or_else(|_| custom.clone());
    std::env::set_var(
        "AGEND_ALLOWED_ROOTS",
        canonical_custom.to_str().unwrap_or(""),
    );
    let result = validate_working_directory(&custom, &home);
    std::env::remove_var("AGEND_ALLOWED_ROOTS");
    assert!(result.is_ok(), "env override should allow: {result:?}");
    std::fs::remove_dir_all(&home).ok();
    std::fs::remove_dir_all(&custom).ok();
    let _ = sep; // suppress unused warning
}

#[test]
fn call_fails_without_daemon() {
    let home = tmp_home("call_no_daemon");
    let err = call(&home, &json!({"method": "list"})).unwrap_err();
    // No active daemon → either "no active daemon" or a TCP ConnectionRefused
    let msg = format!("{err:#}");
    assert!(
        msg.to_ascii_lowercase().contains("no active daemon")
            || msg.to_ascii_lowercase().contains("refused"),
        "unexpected error: {msg}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn api_call_read_timeout_is_fixed_90s() {
    // #1492 backstop: the loopback read timeout that converts a wedged
    // self-IPC from a permanent daemon freeze into a recoverable error.
    // #env-cleanup: now a fixed const (the AGEND_API_CALL_TIMEOUT_SECS
    // override + sub-1s clamp were demoted). 90s must still exceed the
    // slowest legit method (create_instance ~60s).
    assert_eq!(
        api_call_read_timeout(),
        std::time::Duration::from_secs(90),
        "fixed default must exceed the slowest legit method (create_instance ~60s)"
    );
}

// -----------------------------------------------------------------------
// ApiNotifier seam tests
// -----------------------------------------------------------------------

/// Test-only notifier that records every event for later assertion.
struct RecordingNotifier {
    events: parking_lot::Mutex<Vec<ApiEvent>>,
}

impl RecordingNotifier {
    fn new() -> Self {
        Self {
            events: parking_lot::Mutex::new(Vec::new()),
        }
    }
    fn take(&self) -> Vec<ApiEvent> {
        std::mem::take(&mut *self.events.lock())
    }
}

impl ApiNotifier for RecordingNotifier {
    fn notify(&self, event: ApiEvent) {
        self.events.lock().push(event);
    }
}

// -- Positive: 4 call-site tests (full payload assertion) --

#[test]
fn notifier_receives_instance_deleted() {
    let rec = RecordingNotifier::new();
    rec.notify(ApiEvent::InstanceDeleted {
        name: "agent-1".into(),
        instance_ref: None,
    });
    let events = rec.take();
    assert_eq!(events.len(), 1);
    let ApiEvent::InstanceDeleted { name, .. } = &events[0] else {
        panic!("wrong variant")
    };
    assert_eq!(name, "agent-1");
}

#[test]
fn notifier_receives_instance_created() {
    let rec = RecordingNotifier::new();
    rec.notify(ApiEvent::InstanceCreated {
        name: "agent-2".into(),
        instance_ref: None,
        layout: LayoutHint::SplitRight,
        spawner: Some("caller".into()),
        target_pane: None,
    });
    let events = rec.take();
    assert_eq!(events.len(), 1);
    let ApiEvent::InstanceCreated {
        name,
        layout,
        spawner,
        target_pane,
        ..
    } = &events[0]
    else {
        panic!("wrong variant")
    };
    assert_eq!(name, "agent-2");
    assert_eq!(*layout, LayoutHint::SplitRight);
    assert_eq!(spawner.as_deref(), Some("caller"));
    assert_eq!(*target_pane, None);
}

#[test]
fn notifier_receives_team_created() {
    let rec = RecordingNotifier::new();
    rec.notify(ApiEvent::TeamCreated {
        name: "team-a".into(),
        members: vec!["m1".into(), "m2".into()],
    });
    let events = rec.take();
    assert_eq!(events.len(), 1);
    let ApiEvent::TeamCreated { name, members } = &events[0] else {
        panic!("wrong variant")
    };
    assert_eq!(name, "team-a");
    assert_eq!(members, &["m1", "m2"]);
}

#[test]
fn notifier_receives_team_members_changed() {
    let rec = RecordingNotifier::new();
    rec.notify(ApiEvent::TeamMembersChanged {
        name: "team-b".into(),
        added: vec!["new".into()],
        removed: vec!["old".into()],
    });
    let events = rec.take();
    assert_eq!(events.len(), 1);
    let ApiEvent::TeamMembersChanged {
        name,
        added,
        removed,
    } = &events[0]
    else {
        panic!("wrong variant")
    };
    assert_eq!(name, "team-b");
    assert_eq!(added, &["new"]);
    assert_eq!(removed, &["old"]);
}

// -- None-path: 4 tests verifying no panic when notifier is None --

#[test]
fn none_notifier_instance_deleted_no_panic() {
    let notifier: Option<&dyn ApiNotifier> = None;
    if let Some(n) = notifier {
        n.notify(ApiEvent::InstanceDeleted {
            name: "x".into(),
            instance_ref: None,
        });
    }
}

#[test]
fn none_notifier_instance_created_no_panic() {
    let notifier: Option<&dyn ApiNotifier> = None;
    if let Some(n) = notifier {
        n.notify(ApiEvent::InstanceCreated {
            name: "x".into(),
            instance_ref: None,
            layout: LayoutHint::Tab,
            spawner: None,
            target_pane: None,
        });
    }
}

#[test]
fn none_notifier_team_created_no_panic() {
    let notifier: Option<&dyn ApiNotifier> = None;
    if let Some(n) = notifier {
        n.notify(ApiEvent::TeamCreated {
            name: "x".into(),
            members: vec![],
        });
    }
}

#[test]
fn none_notifier_team_members_changed_no_panic() {
    let notifier: Option<&dyn ApiNotifier> = None;
    if let Some(n) = notifier {
        n.notify(ApiEvent::TeamMembersChanged {
            name: "x".into(),
            added: vec![],
            removed: vec![],
        });
    }
}

// -- Failure resilience --

/// A notifier that panics on every call — used to verify that a panicking
/// notifier does not silently corrupt state in the RecordingNotifier path.
/// Note: in production, a panic inside `notify()` will unwind through
/// `handle_session`, terminating that API connection. This is acceptable
/// because notifier implementations (TuiNotifier) never panic.
struct PanickingNotifier;

impl ApiNotifier for PanickingNotifier {
    fn notify(&self, _event: ApiEvent) {
        panic!("intentional test panic");
    }
}

#[test]
fn panicking_notifier_unwinds_safely() {
    let result = std::panic::catch_unwind(|| {
        let n: &dyn ApiNotifier = &PanickingNotifier;
        n.notify(ApiEvent::InstanceDeleted {
            name: "x".into(),
            instance_ref: None,
        });
    });
    assert!(result.is_err(), "expected panic to propagate");
}

#[test]
fn notifier_multiple_events_accumulate() {
    let rec = RecordingNotifier::new();
    rec.notify(ApiEvent::InstanceCreated {
        name: "a".into(),
        instance_ref: None,
        layout: LayoutHint::Tab,
        spawner: None,
        target_pane: None,
    });
    rec.notify(ApiEvent::InstanceDeleted {
        name: "a".into(),
        instance_ref: None,
    });
    let events = rec.take();
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0], ApiEvent::InstanceCreated { .. }));
    assert!(matches!(&events[1], ApiEvent::InstanceDeleted { .. }));
}

// -----------------------------------------------------------------------
// Slice 4: Dispatch-Level Notifier Coverage
// -----------------------------------------------------------------------
// These tests exercise handle_session's actual notifier call sites by
// starting a real API server with a RecordingNotifier and sending NDJSON
// requests over TCP.

/// Start an API server on a background thread with a given notifier.
fn start_test_server_with(
    label: &str,
    notifier: Option<Arc<dyn ApiNotifier>>,
) -> (u16, std::path::PathBuf, Arc<AtomicBool>) {
    let home = tmp_home(label);
    let run_dir = crate::daemon::run_dir(&home);
    std::fs::create_dir_all(&run_dir).unwrap();
    crate::auth_cookie::issue(&run_dir).unwrap();

    let registry: AgentRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let configs: ConfigRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let externals: crate::agent::ExternalRegistry =
        Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
    let shutdown = Arc::new(AtomicBool::new(false));

    let h = home.clone();
    let r = Arc::clone(&registry);
    let s = Arc::clone(&shutdown);
    let c = Arc::clone(&configs);
    let e = Arc::clone(&externals);

    std::thread::Builder::new()
        .name(format!("test_api_{label}"))
        .spawn(move || {
            serve(
                &h,
                r,
                s,
                c,
                e,
                notifier,
                crate::api::RestartCapability::Unsupported,
                None,
            );
        })
        .unwrap();

    let mut port = 0u16;
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if let Ok(contents) = std::fs::read_to_string(run_dir.join("api.port")) {
            if let Ok(p) = contents.trim().parse::<u16>() {
                port = p;
                break;
            }
        }
    }
    assert!(port > 0, "API server did not publish port");
    (port, home, shutdown)
}

/// Start an API server with a RecordingNotifier.
fn start_test_server(
    label: &str,
) -> (
    u16,
    std::path::PathBuf,
    Arc<RecordingNotifier>,
    Arc<AtomicBool>,
) {
    let rec = Arc::new(RecordingNotifier::new());
    let n: Arc<dyn ApiNotifier> = Arc::clone(&rec) as Arc<dyn ApiNotifier>;
    let (port, home, shutdown) = start_test_server_with(label, Some(n));
    (port, home, rec, shutdown)
}

/// Send an NDJSON request to the API server and read one response using a
/// caller-supplied authenticated principal token.
fn api_request_with_auth(port: u16, request: &Value, auth: &crate::auth_cookie::Cookie) -> Value {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let stream =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2)).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = std::io::BufReader::new(stream);

    crate::auth_cookie::client_handshake_ndjson(&mut reader, &mut writer, auth).unwrap();

    writeln!(writer, "{}", request).unwrap();
    writer.flush().unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(line.trim()).unwrap_or(json!({"error": "parse failed"}))
}

/// Send an NDJSON request on the operator-capability surface.
fn api_request(port: u16, home: &std::path::Path, request: &Value) -> Value {
    let run_dir = crate::daemon::run_dir(home);
    let operator_token = crate::auth_cookie::read_operator_token(&run_dir).unwrap();
    api_request_with_auth(port, request, &operator_token)
}

fn stop_server(shutdown: &Arc<AtomicBool>, home: &std::path::Path) {
    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    // Connect to unblock the accept() loop
    let run_dir = crate::daemon::run_dir(home);
    if let Ok(contents) = std::fs::read_to_string(run_dir.join("api.port")) {
        if let Ok(port) = contents.trim().parse::<u16>() {
            let _ = std::net::TcpStream::connect_timeout(
                &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                std::time::Duration::from_millis(100),
            );
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    std::fs::remove_dir_all(home).ok();
}

mod job_recovery;

#[test]
fn dispatch_delete_emits_instance_deleted() {
    let (port, home, notifier, shutdown) = start_test_server("dispatch-del");
    let resp = api_request(
        port,
        &home,
        &json!({"method": "delete", "params": {"name": "agent-x"}}),
    );
    assert_eq!(resp["ok"], true);
    let events = notifier.take();
    assert_eq!(events.len(), 1, "expected 1 event, got {events:?}");
    let ApiEvent::InstanceDeleted { name, .. } = &events[0] else {
        panic!("expected InstanceDeleted, got {:?}", events[0])
    };
    assert_eq!(name, "agent-x");
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_create_team_emits_team_created() {
    let (port, home, notifier, shutdown) = start_test_server("dispatch-team");
    // CREATE_TEAM with existing members (no spawns) now emits TeamCreated
    // with the full member roster.
    let resp = api_request(
        port,
        &home,
        &json!({
            "method": "create_team",
            "params": {"name": "test-team", "members": ["a", "b"]}
        }),
    );
    assert_eq!(resp["ok"], true);
    let events = notifier.take();
    assert_eq!(
        events.len(),
        1,
        "expected TeamCreated for existing members, got {events:?}"
    );
    let ApiEvent::TeamCreated { name, members } = &events[0] else {
        panic!("expected TeamCreated, got {:?}", events[0])
    };
    assert_eq!(name, "test-team");
    assert_eq!(members, &["a", "b"]);
    stop_server(&shutdown, &home);
}

/// Positive-pin companion to `dispatch_create_team_emits_team_created`:
/// uses `true` (resolved via PATH — `/usr/bin/true` on macOS,
/// `/bin/true` or `/usr/bin/true` on Linux) as a harmless real
/// backend so `spawn_one` actually succeeds, exercising the
/// spawn + emit path of `handle_create_team` end-to-end.
/// `TeamCreated.members` now carries the full roster (all_members),
/// not just spawned names.
///
/// Context (see `LESSONS-04-21.md` open items): headless daemon mode
/// passes `notifier = None`, so the emission block in
/// `handlers/team.rs:153-161` is unreachable by the standard E2E
/// smoke. Prior coverage used a three-piece equivalence bracket —
/// (a) negative pin via the sibling test above, (b) byte-identical
/// refactor verdict from at-dev-3 on the emission block, (c) runtime
/// abort-point evidence from smoke logs. This positive pin replaces
/// (a)+(c) with a direct in-process assertion that a successful
/// spawn results in the expected `ApiEvent::TeamCreated` payload.
///
/// `#[cfg(unix)]` — `true(1)` is a universal harmless real backend
/// on Unix; Windows lacks a directly equivalent short-lived builtin
/// and the LESSONS open item is scoped to a proof-of-concept. A
/// Windows-specific positive pin can be added as a follow-up
/// without changing this test.
#[cfg(unix)]
#[test]
fn dispatch_create_team_emits_team_created_positive() {
    let (port, home, notifier, shutdown) = start_test_server("dispatch-team-pos");
    let resp = api_request(
        port,
        &home,
        &json!({
            "method": "create_team",
            "params": {
                "name": "positive_pin",
                "backend": "true",
                "count": 1
            }
        }),
    );
    assert_eq!(resp["ok"], true, "create_team failed: {resp:?}");
    assert_eq!(
        resp["spawned"].as_array().map(|a| a.len()),
        Some(1),
        "expected 1 spawned agent, got {resp:?}"
    );
    // The notifier emission in `handle_create_team` happens synchronously
    // before the response is written to the wire (see handlers/team.rs
    // L153-161), so by the time `api_request` returns, the event is
    // already in `RecordingNotifier`'s buffer — no polling needed.
    let events = notifier.take();
    assert_eq!(events.len(), 1, "expected 1 event, got {events:?}");
    let ApiEvent::TeamCreated { name, members } = &events[0] else {
        panic!("expected TeamCreated, got {:?}", events[0])
    };
    assert_eq!(name, "positive_pin");
    assert_eq!(members, &["positive_pin-1"]);
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_update_team_emits_members_changed() {
    let (port, home, notifier, shutdown) = start_test_server("dispatch-update-team");
    // First create a team via the teams store
    // Sprint 54 fleet-yaml unification: teams live in fleet.yaml.
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "teams:\n  t1:\n    members: [m1]\n    created_at: \"2026-01-01T00:00:00Z\"\n",
    )
    .unwrap();

    let resp = api_request(
        port,
        &home,
        &json!({
            "method": "update_team",
            "params": {"name": "t1", "add": ["m2"]}
        }),
    );
    assert_eq!(resp["ok"], true);
    let events = notifier.take();
    assert_eq!(events.len(), 1, "expected 1 event, got {events:?}");
    let ApiEvent::TeamMembersChanged {
        name,
        added,
        removed,
    } = &events[0]
    else {
        panic!("expected TeamMembersChanged, got {:?}", events[0])
    };
    assert_eq!(name, "t1");
    assert_eq!(added, &["m2"]);
    assert!(removed.is_empty());
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_delete_with_none_notifier_no_panic() {
    let (port, home, shutdown) = start_test_server_with("dispatch-none", None);
    let resp = api_request(
        port,
        &home,
        &json!({"method": "delete", "params": {"name": "ghost"}}),
    );
    assert_eq!(resp["ok"], true);
    stop_server(&shutdown, &home);
}

// -----------------------------------------------------------------------
// Slice A characterization: LIST + STATUS response shapes
// -----------------------------------------------------------------------

#[test]
fn dispatch_list_returns_agents_array_and_protocol_version() {
    let (port, home, _notifier, shutdown) = start_test_server("list-shape");
    let resp = api_request(port, &home, &json!({"method": "list"}));
    assert_eq!(resp["ok"], true);
    let result = &resp["result"];
    assert!(
        result["protocol_version"].is_number(),
        "expected protocol_version number, got: {result}"
    );
    let agents = result["agents"].as_array().expect("agents array");
    assert_eq!(agents.len(), 0, "empty registry should yield 0 agents");
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_status_returns_agents_and_timestamp() {
    let (port, home, _notifier, shutdown) = start_test_server("status-shape");
    let resp = api_request(port, &home, &json!({"method": "status"}));
    assert_eq!(resp["ok"], true);
    let result = &resp["result"];
    let agents = result["agents"].as_array().expect("agents array");
    assert_eq!(agents.len(), 0, "no snapshot should yield 0 agents");
    assert_eq!(result["timestamp"], serde_json::Value::Null);
    stop_server(&shutdown, &home);
}

// -----------------------------------------------------------------------
// Slice D characterization: SEND + REGISTER/DEREGISTER_EXTERNAL
// -----------------------------------------------------------------------

#[test]
fn dispatch_send_delivers_to_inbox() {
    let (port, home, _notifier, shutdown) = start_test_server("send-char");
    // Target must exist in fleet.yaml for validation to pass.
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  receiver:\n    backend: claude\n",
    )
    .ok();
    let resp = api_request(
        port,
        &home,
        &json!({
            "method": "send",
            "params": {"from": "sender", "target": "receiver", "text": "hello"}
        }),
    );
    assert_eq!(resp["ok"], true);
    // Verify message was enqueued to receiver's inbox
    let inbox_file = crate::inbox::inbox_path_resolved(&home, "receiver");
    assert!(
        inbox_file.exists(),
        "expected inbox file at {}",
        inbox_file.display()
    );
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_send_rejects_self_send() {
    let (port, home, _notifier, shutdown) = start_test_server("send-self");
    let resp = api_request(
        port,
        &home,
        &json!({
            "method": "send",
            "params": {"from": "agent-x", "target": "agent-x", "text": "hi"}
        }),
    );
    assert_eq!(resp["ok"], false);
    assert!(resp["error"]
        .as_str()
        .is_some_and(|e| e.contains("cannot send to self")),);
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_register_and_deregister_external() {
    let (port, home, _notifier, shutdown) = start_test_server("ext-reg");
    // Register
    let resp = api_request(
        port,
        &home,
        &json!({
            "method": "register_external",
            "params": {"name": "ext-agent", "backend": "custom", "pid": 12345}
        }),
    );
    assert_eq!(resp["ok"], true);

    // Verify it shows up in LIST
    let list_resp = api_request(port, &home, &json!({"method": "list"}));
    let agents = list_resp["result"]["agents"].as_array().expect("agents");
    assert!(
        agents
            .iter()
            .any(|a| a["name"] == "ext-agent" && a["kind"] == "external"),
        "expected ext-agent in list, got: {agents:?}"
    );

    // Deregister
    let resp = api_request(
        port,
        &home,
        &json!({"method": "deregister_external", "params": {"name": "ext-agent"}}),
    );
    assert_eq!(resp["ok"], true);

    // Verify it's gone from LIST
    let list_resp = api_request(port, &home, &json!({"method": "list"}));
    let agents = list_resp["result"]["agents"].as_array().expect("agents");
    assert!(
        !agents.iter().any(|a| a["name"] == "ext-agent"),
        "ext-agent should be gone after deregister"
    );

    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_deregister_nonexistent_returns_error() {
    let (port, home, _notifier, shutdown) = start_test_server("ext-dereg-miss");
    let resp = api_request(
        port,
        &home,
        &json!({"method": "deregister_external", "params": {"name": "ghost"}}),
    );
    assert_eq!(resp["ok"], false);
    assert!(resp["error"]
        .as_str()
        .is_some_and(|e| e.contains("not found")));
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_register_duplicate_returns_error() {
    let (port, home, _notifier, shutdown) = start_test_server("ext-dup");
    // Register first time
    let resp = api_request(
        port,
        &home,
        &json!({
            "method": "register_external",
            "params": {"name": "dup-agent", "backend": "custom", "pid": 1}
        }),
    );
    assert_eq!(resp["ok"], true);

    // Register same name again
    let resp = api_request(
        port,
        &home,
        &json!({
            "method": "register_external",
            "params": {"name": "dup-agent", "backend": "custom", "pid": 2}
        }),
    );
    assert_eq!(resp["ok"], false);
    assert!(resp["error"]
        .as_str()
        .is_some_and(|e| e.contains("already exists")));

    stop_server(&shutdown, &home);
}

// -----------------------------------------------------------------------
// Slice B characterization: INJECT + KILL + DELETE + SPAWN error branches
// -----------------------------------------------------------------------
// Convention: every writeln+continue → return conversion must have its
// early-error branch pinned here.

// -- INJECT --

#[test]
fn dispatch_inject_validate_name_fail() {
    let (port, home, _n, shutdown) = start_test_server("inject-badname");
    let resp = api_request(
        port,
        &home,
        &json!({"method": "inject", "params": {"name": "../escape", "data": "x"}}),
    );
    assert_eq!(resp["ok"], false);
    assert!(resp["error"].as_str().is_some());
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_inject_agent_not_found() {
    let (port, home, _n, shutdown) = start_test_server("inject-notfound");
    let resp = api_request(
        port,
        &home,
        &json!({"method": "inject", "params": {"name": "ghost", "data": "x"}}),
    );
    assert_eq!(resp["ok"], false);
    assert!(resp["error"]
        .as_str()
        .is_some_and(|e| e.contains("not found")));
    stop_server(&shutdown, &home);
}

// -- KILL --

#[test]
fn dispatch_kill_validate_name_fail() {
    let (port, home, _n, shutdown) = start_test_server("kill-badname");
    let resp = api_request(
        port,
        &home,
        &json!({"method": "kill", "params": {"name": "../escape"}}),
    );
    assert_eq!(resp["ok"], false);
    assert!(resp["error"].as_str().is_some());
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_kill_agent_not_found() {
    let (port, home, _n, shutdown) = start_test_server("kill-notfound");
    let resp = api_request(
        port,
        &home,
        &json!({"method": "kill", "params": {"name": "ghost"}}),
    );
    assert_eq!(resp["ok"], false);
    assert!(resp["error"]
        .as_str()
        .is_some_and(|e| e.contains("not found")));
    stop_server(&shutdown, &home);
}

// -- DELETE --

#[test]
fn dispatch_delete_validate_name_fail() {
    let (port, home, _n, shutdown) = start_test_server("delete-badname");
    let resp = api_request(
        port,
        &home,
        &json!({"method": "delete", "params": {"name": "../escape"}}),
    );
    assert_eq!(resp["ok"], false);
    assert!(resp["error"].as_str().is_some());
    stop_server(&shutdown, &home);
}

// DELETE happy path already covered by dispatch_delete_emits_instance_deleted (Slice 4)

// -- SPAWN --

#[test]
fn dispatch_spawn_missing_name() {
    let (port, home, _n, shutdown) = start_test_server("spawn-noname");
    let resp = api_request(port, &home, &json!({"method": "spawn", "params": {}}));
    assert_eq!(resp["ok"], false);
    assert!(resp["error"]
        .as_str()
        .is_some_and(|e| e.contains("missing")));
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_spawn_validate_name_fail() {
    let (port, home, _n, shutdown) = start_test_server("spawn-badname");
    let resp = api_request(
        port,
        &home,
        &json!({"method": "spawn", "params": {"name": "../escape"}}),
    );
    assert_eq!(resp["ok"], false);
    assert!(resp["error"].as_str().is_some());
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_spawn_backend_not_found() {
    let (port, home, _n, shutdown) = start_test_server("spawn-badbinary");
    let resp = api_request(
        port,
        &home,
        &json!({"method": "spawn", "params": {"name": "test-agent", "backend": "nonexistent-binary-xyz"}}),
    );
    assert_eq!(resp["ok"], false);
    assert!(resp["error"].as_str().is_some());
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_spawn_working_directory_rejected() {
    let (port, home, _n, shutdown) = start_test_server("spawn-badwd");
    let resp = api_request(
        port,
        &home,
        &json!({"method": "spawn", "params": {"name": "test-wd", "working_directory": "/tmp/../etc/foo"}}),
    );
    assert_eq!(resp["ok"], false);
    assert!(resp["error"].as_str().is_some());
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_delete_external_success() {
    let (port, home, _n, shutdown) = start_test_server("del-ext");
    // Register an external agent
    let _ = api_request(
        port,
        &home,
        &json!({"method": "register_external", "params": {"name": "ext-1", "backend": "x", "pid": 1}}),
    );
    // Delete it — exercises the external early-return success path
    let resp = api_request(
        port,
        &home,
        &json!({"method": "delete", "params": {"name": "ext-1"}}),
    );
    assert_eq!(resp["ok"], true);
    stop_server(&shutdown, &home);
}

// SPAWN happy path + dedup are disclosed known gaps — require real agent spawn

// -----------------------------------------------------------------------
// Slice C1 characterization: UPDATE_TEAM
// -----------------------------------------------------------------------

#[test]
fn dispatch_update_team_missing_name() {
    let (port, home, _n, shutdown) = start_test_server("ut-noname");
    let resp = api_request(port, &home, &json!({"method": "update_team", "params": {}}));
    assert_eq!(resp["ok"], false);
    assert!(resp["error"]
        .as_str()
        .is_some_and(|e| e.contains("missing")));
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_update_team_remove_member() {
    let (port, home, notifier, shutdown) = start_test_server("ut-remove");
    // Pre-create team with members
    // Sprint 54 fleet-yaml unification: teams live in fleet.yaml.
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "teams:\n  t1:\n    members: [m1, m2]\n    created_at: \"2026-01-01T00:00:00Z\"\n",
    )
    .unwrap();

    let resp = api_request(
        port,
        &home,
        &json!({"method": "update_team", "params": {"name": "t1", "remove": ["m2"]}}),
    );
    assert_eq!(resp["ok"], true);
    let events = notifier.take();
    assert_eq!(events.len(), 1);
    let ApiEvent::TeamMembersChanged {
        name,
        added,
        removed,
    } = &events[0]
    else {
        panic!("expected TeamMembersChanged, got {:?}", events[0])
    };
    assert_eq!(name, "t1");
    assert!(added.is_empty());
    assert_eq!(removed, &["m2"]);
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_update_team_noop_no_event() {
    let (port, home, notifier, shutdown) = start_test_server("ut-noop");
    // Pre-create team
    // Sprint 54 fleet-yaml unification: teams live in fleet.yaml.
    std::fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "teams:\n  t1:\n    members: [m1]\n    created_at: \"2026-01-01T00:00:00Z\"\n",
    )
    .unwrap();

    // Re-add existing member → noop diff → no event
    let resp = api_request(
        port,
        &home,
        &json!({"method": "update_team", "params": {"name": "t1", "add": ["m1"]}}),
    );
    assert_eq!(resp["ok"], true);
    let events = notifier.take();
    assert_eq!(events.len(), 0, "noop diff should not emit event");
    stop_server(&shutdown, &home);
}

// -----------------------------------------------------------------------
// Slice C2 characterization: CREATE_TEAM
// -----------------------------------------------------------------------

#[test]
fn dispatch_create_team_missing_name() {
    let (port, home, _n, shutdown) = start_test_server("ct-noname");
    let resp = api_request(port, &home, &json!({"method": "create_team", "params": {}}));
    assert_eq!(resp["ok"], false);
    assert!(resp["error"]
        .as_str()
        .is_some_and(|e| e.contains("missing")));
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_create_team_all_spawns_failed() {
    let (port, home, _n, shutdown) = start_test_server("ct-allfail");
    let resp = api_request(
        port,
        &home,
        &json!({
            "method": "create_team",
            "params": {
                "name": "fail-team",
                "backend": "nonexistent-binary-xyz",
                "count": 2
            }
        }),
    );
    assert_eq!(resp["ok"], false);
    assert!(resp["error"].as_str().is_some_and(|e| e.contains("failed")));
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_create_team_zero_count_succeeds() {
    let (port, home, notifier, shutdown) = start_test_server("ct-zero");
    let resp = api_request(
        port,
        &home,
        &json!({
            "method": "create_team",
            "params": {"name": "empty-team"}
        }),
    );
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["spawned"], json!([]));
    assert!(resp.get("failed").is_none(), "no failed field expected");
    // Empty team (no members) → no TeamCreated event
    let events = notifier.take();
    assert_eq!(events.len(), 0, "empty team should not emit TeamCreated");
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_create_team_with_existing_members_only() {
    let (port, home, notifier, shutdown) = start_test_server("ct-members");
    let resp = api_request(
        port,
        &home,
        &json!({
            "method": "create_team",
            "params": {"name": "ref-team", "members": ["a", "b"]}
        }),
    );
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["spawned"], json!([]));
    // Existing members now emit TeamCreated with full roster.
    let events = notifier.take();
    assert_eq!(
        events.len(),
        1,
        "expected TeamCreated for existing members, got {events:?}"
    );
    let ApiEvent::TeamCreated { name, members } = &events[0] else {
        panic!("expected TeamCreated, got {:?}", events[0])
    };
    assert_eq!(name, "ref-team");
    assert_eq!(members, &["a", "b"]);
    stop_server(&shutdown, &home);
}

// -----------------------------------------------------------------------
// MOVE_PANE dispatch coverage
// -----------------------------------------------------------------------

#[test]
fn dispatch_move_pane_missing_agent() {
    let (port, home, _n, shutdown) = start_test_server("mp-noagent");
    let resp = api_request(
        port,
        &home,
        &json!({"method": "move_pane", "params": {"target_tab": "t"}}),
    );
    assert_eq!(resp["ok"], false);
    assert!(resp["error"]
        .as_str()
        .is_some_and(|e| e.contains("missing agent")));
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_move_pane_missing_target_tab() {
    let (port, home, _n, shutdown) = start_test_server("mp-notab");
    let resp = api_request(
        port,
        &home,
        &json!({"method": "move_pane", "params": {"agent": "a"}}),
    );
    assert_eq!(resp["ok"], false);
    assert!(resp["error"]
        .as_str()
        .is_some_and(|e| e.contains("missing target_tab")));
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_move_pane_emits_pane_moved_default_horizontal() {
    let (port, home, notifier, shutdown) = start_test_server("mp-emit-h");
    let resp = api_request(
        port,
        &home,
        &json!({"method": "move_pane", "params": {"agent": "a1", "target_tab": "team-x"}}),
    );
    assert_eq!(resp["ok"], true);
    let events = notifier.take();
    assert_eq!(events.len(), 1, "expected 1 event, got {events:?}");
    let ApiEvent::PaneMoved {
        agent,
        target_tab,
        split_dir,
    } = &events[0]
    else {
        panic!("expected PaneMoved, got {:?}", events[0])
    };
    assert_eq!(agent, "a1");
    assert_eq!(target_tab, "team-x");
    assert_eq!(*split_dir, PaneMoveSplitDir::Horizontal);
    stop_server(&shutdown, &home);
}

#[test]
fn dispatch_move_pane_parses_vertical_split() {
    let (port, home, notifier, shutdown) = start_test_server("mp-emit-v");
    let resp = api_request(
        port,
        &home,
        &json!({"method": "move_pane", "params": {
            "agent": "a2",
            "target_tab": "team-y",
            "split_dir": "vertical"
        }}),
    );
    assert_eq!(resp["ok"], true);
    let events = notifier.take();
    assert_eq!(events.len(), 1);
    let ApiEvent::PaneMoved { split_dir, .. } = &events[0] else {
        panic!("expected PaneMoved");
    };
    assert_eq!(*split_dir, PaneMoveSplitDir::Vertical);
    stop_server(&shutdown, &home);
}

// -----------------------------------------------------------------------
// spawn_one preset resolution — regression pin for per-backend submit_key
// -----------------------------------------------------------------------

#[test]
fn spawn_one_resolves_preset_submit_key() {
    // Verify that Backend::from_command returns the correct preset
    // submit_key for each known backend. spawn_one uses this to avoid
    // hardcoding "\r" where a backend needs a different submit sequence.
    use crate::backend::Backend;
    let cases = [
        ("claude", "\r"),
        ("kiro-cli", "\r"),
        ("codex", "\r"),
        ("agy", "\r"),
    ];
    for (cmd, expected) in cases {
        let key = Backend::from_command(cmd)
            .map(|b| b.preset().submit_key)
            .unwrap_or("\r");
        assert_eq!(
            key, expected,
            "backend '{cmd}' should have submit_key {expected:?}, got {key:?}"
        );
    }
    // Unknown backend falls back to "\r"
    let unknown = Backend::from_command("unknown-backend")
        .map(|b| b.preset().submit_key)
        .unwrap_or("\r");
    assert_eq!(unknown, "\r");
}

// ── #bughunt-r1 #4: accept-loop error disposition ──────────────────────

#[test]
fn accept_error_disposition_logs_first_then_rate_limits() {
    // First error in a streak always logs; subsequent ones are suppressed
    // until the next `ACCEPT_ERROR_LOG_EVERY` multiple — so a persistent
    // failure produces a bounded log rate, not one line per spin.
    assert_eq!(accept_error_disposition(1), (true, false), "1st error logs");
    assert_eq!(
        accept_error_disposition(2),
        (false, false),
        "2nd suppressed"
    );
    assert_eq!(
        accept_error_disposition(ACCEPT_ERROR_LOG_EVERY),
        (true, false),
        "every Nth logs"
    );
    assert_eq!(
        accept_error_disposition(ACCEPT_ERROR_LOG_EVERY + 1),
        (false, false),
        "N+1 suppressed"
    );
}

#[test]
fn accept_error_disposition_breaks_after_sustained_failure() {
    // Below the cap → keep going; at/over the cap → give up the loop.
    assert!(
        !accept_error_disposition(MAX_CONSECUTIVE_ACCEPT_ERRORS - 1).1,
        "just under cap keeps accepting"
    );
    assert!(
        accept_error_disposition(MAX_CONSECUTIVE_ACCEPT_ERRORS).1,
        "at cap breaks the accept loop"
    );
}

// ── #bughunt-r1 #2: api::call port+cookie single run-dir resolution ────

/// Source-scan invariant (the runtime TOCTOU needs a daemon-restart race
/// that's impractical to drive in a unit test): `api::call` MUST resolve the
/// active run dir exactly ONCE and feed that same dir to BOTH the port
/// connect (`connect_run_dir_api`) and the cookie read (`read_cookie`). It
/// must NOT use `connect_api` (which does its OWN internal resolution) — that
/// was the bug: a second `find_active_run_dir` for the cookie could land on a
/// different run dir mid-restart.
#[test]
fn call_resolves_run_dir_once_for_port_and_cookie_bughunt_r1() {
    let src = include_str!("mod.rs");
    // Scope to the `pub fn call(` body. Stop at the next top-level item
    // (`\nfn ` — the following `api_call_read_timeout`) so the scan can NOT
    // reach the `#[cfg(test)]` module, whose own source contains the literal
    // `connect_api(` in this test's assert message (#1593 self-match trap).
    let start = src
        .find("pub fn call(home: &Path")
        .expect("call fn present");
    let after = &src[start..];
    let end = after[1..]
        .find("\nfn ")
        .map(|i| i + 1)
        .unwrap_or(after.len());
    let body = &after[..end];

    assert!(
        !body.contains("connect_api("),
        "#bughunt-r1 #2: api::call must NOT use connect_api (it re-resolves the \
             run dir internally → port/cookie TOCTOU); use connect_run_dir_api on a \
             single resolution"
    );
    assert!(
        body.contains("connect_run_dir_api("),
        "api::call must connect via connect_run_dir_api on the resolved run dir"
    );
    assert_eq!(
        body.matches("find_active_run_dir(").count(),
        1,
        "api::call must resolve the active run dir exactly ONCE (port + cookie \
             from the same dir)"
    );
}

/// bug-audit Rank1 regression: a `handle_session` PANIC must NOT leak the
/// `active_conns` reservation. Pre-fix the manual `fetch_sub` sat AFTER
/// `handle_session`, so an unwind skipped it and leaked the slot (leaks
/// accumulate to `API_MAX_CONNS` → control-plane lockup). The `ConnSlot`
/// `Drop` releases the slot on the unwind path, returning the count to
/// baseline.
#[test]
fn conn_slot_releases_reservation_on_panic_unwind() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let counter = Arc::new(AtomicUsize::new(0));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let (_slot, prev) = ConnSlot::reserve(&counter);
        assert_eq!(prev, 0, "first reservation sees prev=0");
        assert_eq!(counter.load(Ordering::SeqCst), 1, "slot reserved");
        panic!("simulated handle_session panic");
    }));
    assert!(result.is_err(), "panic must propagate out of catch_unwind");
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "ConnSlot::drop must release the slot on panic unwind (no leak)"
    );
}

/// bug-audit Rank1 regression (sibling counter): a `handle_session` panic
/// must also decrement the in-flight session count via `SessionCount::drop`
/// instead of leaking it.
#[test]
fn session_count_decrements_on_panic_unwind() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static TEST_SESSIONS: AtomicUsize = AtomicUsize::new(0);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _session = SessionCount::enter(&TEST_SESSIONS);
        assert_eq!(TEST_SESSIONS.load(Ordering::Relaxed), 1, "session entered");
        panic!("simulated handle_session panic");
    }));
    assert!(result.is_err(), "panic must propagate");
    assert_eq!(
        TEST_SESSIONS.load(Ordering::Relaxed),
        0,
        "SessionCount::drop must decrement on panic unwind"
    );
}
