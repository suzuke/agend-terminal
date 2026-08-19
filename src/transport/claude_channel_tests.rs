use super::*;
use crate::backend::Backend;
use crate::transport::{mode_for_backend, mode_for_instance};
use std::fs;
use std::sync::atomic::AtomicUsize;

fn home(tag: &str) -> std::path::PathBuf {
    let home =
        std::env::temp_dir().join(format!("agend-claude-channel-{}-{}", tag, Uuid::new_v4()));
    fs::create_dir_all(&home).expect("home");
    home
}

fn test_published_locator(home: &Path, instance: &str) -> SessionLocator {
    let mut locator = SessionLocator::claude(
        "http://127.0.0.1:43123".to_string(),
        "claude-test-session".to_string(),
        "test-token".to_string(),
    );
    locator.managed = true;
    locator.server_pid = Some(std::process::id());
    locator.server_start_token = crate::process::process_start_token(std::process::id());
    super::super::registry::save_session_locator(home, instance, &locator)
        .expect("published test locator");
    locator
}

#[test]
fn claude_uses_channel_bridge_by_default() {
    assert_eq!(
        mode_for_backend(&Backend::ClaudeCode),
        TransportMode::ChannelBridge
    );
}

#[test]
fn explicit_legacy_pty_is_the_only_claude_fallback() {
    let home = home("legacy");
    fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  claude-agent:\n    backend: claude\n    env:\n      AGEND_TRANSPORT_MODE: legacy_pty\n",
        )
        .expect("fleet");
    assert_eq!(
        mode_for_instance(&home, "claude-agent"),
        TransportMode::LegacyPty
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn channel_locator_persists_across_daemon_restart() {
    let home = home("locator");
    let (first, first_listener) =
        bind_and_publish_channel(&home, "claude-agent").expect("initial bridge");
    assert!(first.managed);
    assert!(
        TcpListener::bind(endpoint_address(&first).expect("first endpoint")).is_err(),
        "the published endpoint must remain owned by the bridge"
    );
    assert_eq!(
        prepare_claude_channel(&home, "claude-agent").expect("first published locator"),
        first
    );
    drop(first_listener);
    let (second, second_listener) =
        bind_and_publish_channel(&home, "claude-agent").expect("restart bridge");
    assert!(second.managed);
    assert_ne!(second.password, first.password);
    assert_ne!(second.session_id, first.session_id);
    assert_eq!(
        prepare_claude_channel(&home, "claude-agent").expect("second published locator"),
        second
    );
    drop(second_listener);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn existing_non_claude_locator_is_not_replaced() {
    let home = home("foreign-locator");
    let locator = SessionLocator::opencode(
        "http://127.0.0.1:43123".to_string(),
        Some("opencode-session".to_string()),
        "agend".to_string(),
        "secret".to_string(),
    );
    super::super::registry::save_session_locator(&home, "claude-agent", &locator)
        .expect("foreign locator");
    assert!(prepare_claude_channel(&home, "claude-agent").is_err());
    let stored = super::super::registry::load_session_locator(&home, "claude-agent")
        .expect("stored locator");
    assert_eq!(stored.backend, "opencode");
    stop_instance_state(&home, "claude-agent");
    assert!(
        super::super::registry::load_session_locator(&home, "claude-agent").is_ok(),
        "Claude cleanup must not remove another backend's locator"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn stale_locator_rejects_port_squatter_without_sending_bearer() {
    let home = home("port-squatter");
    let listener = TcpListener::bind("127.0.0.1:0").expect("squatter listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking squatter listener");
    let port = listener.local_addr().expect("squatter address").port();
    let mut stale = SessionLocator::claude(
        format!("http://127.0.0.1:{port}"),
        "stale-session".to_string(),
        "SUPER-SECRET-BEARER".to_string(),
    );
    stale.server_pid = Some(0);
    stale.server_start_token = Some(1);
    super::super::registry::save_session_locator(&home, "claude-agent", &stale)
        .expect("stale locator");

    assert!(prepare_claude_channel(&home, "claude-agent").is_err());
    assert!(client_request(&stale, "GET", "/health", &[], "application/json").is_err());
    assert!(matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock));
    let _ = fs::remove_dir_all(home);
}

#[test]
fn channel_ready_wait_is_bounded_when_locator_never_publishes() {
    let home = home("bounded-ready-wait");
    let started = Instant::now();
    let error =
        wait_for_ready_claude_channel_until(&home, "claude-agent", Duration::from_millis(50))
            .expect_err("missing locator must time out");
    assert!(
        error.to_string().contains("did not become ready within"),
        "unexpected readiness error: {error:#}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "bounded readiness wait must not hang"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn health_probe_rejects_mismatched_session_identity() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("health listener");
    let port = listener.local_addr().expect("health address").port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("health request");
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut chunk).expect("health request read");
            assert!(read > 0, "health request closed before headers");
            request.extend_from_slice(&chunk[..read]);
        }
        let body = json!({
            "ready": true,
            "session_id": "wrong-session",
            "backend_version": "2.1.89",
            "capabilities": {"claude/channel": true, "tools": true}
        })
        .to_string();
        let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        stream
            .write_all(response.as_bytes())
            .expect("health response");
    });
    let mut locator = SessionLocator::claude(
        format!("http://127.0.0.1:{port}"),
        "expected-session".to_string(),
        "health-token".to_string(),
    );
    locator.managed = true;
    locator.server_pid = Some(std::process::id());
    locator.server_start_token = crate::process::process_start_token(std::process::id());
    let error = health_probe(&locator).expect_err("mismatched session must fail closed");
    assert!(
        error.to_string().contains("session identity mismatch"),
        "unexpected health probe error: {error:#}"
    );
    server.join().expect("health server");
}

#[test]
fn persisted_claude_locator_keeps_channel_mode() {
    let home = home("mode");
    let locator = test_published_locator(&home, "claude-agent");
    fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  claude-agent:\n    backend: claude\n",
    )
    .expect("fleet");
    assert_eq!(
        mode_for_instance(&home, "claude-agent"),
        TransportMode::ChannelBridge
    );
    assert_eq!(locator.backend, "claude");
    let _ = fs::remove_dir_all(home);
}

/// Why the fake channel server's accept loop ended. The fixture used to
/// `break` on ANY non-`WouldBlock` accept error and leave no trace, so a
/// transient `Interrupted`/`ConnectionAborted` silently killed the helper and
/// every later request failed with `Broken pipe (os error 32)` — a dead test
/// server that read as a product failure. The reason is recorded so the test
/// can assert the loop ended because we stopped it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FakeChannelExit {
    StopRequested,
    AcceptFailed(String),
}

/// `WouldBlock` is the nonblocking-accept idle case; `Interrupted` and
/// `ConnectionAborted` are transient kernel conditions that say nothing about
/// the listener's health. Only something else is a reason to stop serving.
fn accept_error_is_fatal(kind: io::ErrorKind) -> bool {
    !matches!(
        kind,
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
    )
}

struct FakeChannel {
    port: u16,
    stop: Arc<AtomicBool>,
    health_probes: Arc<AtomicUsize>,
    not_ready_answers: Arc<AtomicUsize>,
    exit: Arc<Mutex<Option<FakeChannelExit>>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl FakeChannel {
    /// Readiness is an ORDERING fact here, not a delay: `/health` answers
    /// `ready: false` until it has served `ready_from_probe` probes, so
    /// readiness arrives strictly AFTER the waiter has observed not-ready.
    /// The old fixture simulated lateness with `sleep(150ms)` + `sleep(200ms)`
    /// inside a 1-second budget, which proved nothing about ordering and
    /// failed outright when the machine was loaded.
    fn spawn(ready_from_probe: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("fake channel listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let port = listener.local_addr().expect("listener address").port();
        let stop = Arc::new(AtomicBool::new(false));
        let health_probes = Arc::new(AtomicUsize::new(0));
        let not_ready_answers = Arc::new(AtomicUsize::new(0));
        let exit = Arc::new(Mutex::new(None));
        let thread_stop = Arc::clone(&stop);
        let thread_probes = Arc::clone(&health_probes);
        let thread_not_ready = Arc::clone(&not_ready_answers);
        let thread_exit = Arc::clone(&exit);
        let handle = thread::spawn(move || {
            let reason = loop {
                if thread_stop.load(Ordering::Acquire) {
                    break FakeChannelExit::StopRequested;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut request = [0_u8; 4096];
                        let read = stream.read(&mut request).unwrap_or(0);
                        let request = String::from_utf8_lossy(&request[..read]);
                        let path = request.split_whitespace().nth(1).unwrap_or("/");
                        let (status, body) = if path == "/health" {
                            let served = thread_probes.fetch_add(1, Ordering::AcqRel) + 1;
                            let ready = served >= ready_from_probe;
                            if !ready {
                                thread_not_ready.fetch_add(1, Ordering::AcqRel);
                            }
                            (
                                "200 OK",
                                json!({
                                    "ready": ready,
                                    "session_id": "claude-registry-session",
                                    "backend_version": "2.1.89",
                                    "capabilities": {
                                        "claude/channel": ready,
                                        "tools": ready
                                    }
                                })
                                .to_string(),
                            )
                        } else if path == "/webhook" {
                            ("202 Accepted", json!({"accepted": true}).to_string())
                        } else {
                            ("404 Not Found", json!({"reply": null}).to_string())
                        };
                        let response = format!(
                                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(error) if !accept_error_is_fatal(error.kind()) => {
                        // Idle or transient: keep serving. The 5 ms backoff is
                        // an accept poll, not a readiness budget.
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => break FakeChannelExit::AcceptFailed(error.kind().to_string()),
                }
            };
            *thread_exit.lock() = Some(reason);
        });
        Self {
            port,
            stop,
            health_probes,
            not_ready_answers,
            exit,
            handle: Some(handle),
        }
    }

    fn stop_and_join(&mut self) -> FakeChannelExit {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.exit
            .lock()
            .clone()
            .expect("fake channel must record why its accept loop ended")
    }
}

#[test]
fn accept_loop_treats_transient_errors_as_nonfatal() {
    for kind in [
        io::ErrorKind::WouldBlock,
        io::ErrorKind::Interrupted,
        io::ErrorKind::ConnectionAborted,
    ] {
        assert!(
            !accept_error_is_fatal(kind),
            "{kind:?} is transient: killing the helper on it is what produced \
                 the historical Broken pipe"
        );
    }
    for kind in [io::ErrorKind::InvalidInput, io::ErrorKind::PermissionDenied] {
        assert!(
            accept_error_is_fatal(kind),
            "{kind:?} must still end the loop, with the reason recorded"
        );
    }
}

#[test]
fn registry_delivery_waits_for_delayed_channel_bridge_without_pty_fallback() {
    let _delivery_hook_guard = super::super::registry::test_support::delivery_hook_guard();
    let home = home("registry-delivery");
    // Ready only from the SECOND probe: the waiter must see not-ready once.
    let mut channel = FakeChannel::spawn(2);
    let port = channel.port;
    let mut locator = SessionLocator::claude(
        format!("http://127.0.0.1:{port}"),
        "claude-registry-session".to_string(),
        "registry-token".to_string(),
    );
    locator.managed = true;
    locator.server_pid = Some(std::process::id());
    locator.server_start_token = crate::process::process_start_token(std::process::id());
    fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  claude-agent:\n    backend: claude\n",
    )
    .expect("fleet");
    // The locator is published up front and readiness is withheld by the
    // server instead. The delay under test is READINESS, and gating it on the
    // probe count makes "the bridge became ready after we started waiting" an
    // observable ordering fact rather than a race against two sleeps.
    super::super::registry::save_session_locator(&home, "claude-agent", &locator)
        .expect("locator publication");
    let legacy_called = Arc::new(AtomicBool::new(false));
    let legacy_called_by_closure = Arc::clone(&legacy_called);
    let result = super::super::registry::wait_for_notification_readiness(
        &home,
        "claude-agent",
        Duration::from_secs(1),
    )
    .and_then(|()| {
        super::super::registry::deliver_notification(
            &home,
            "claude-agent",
            "registry delivery",
            move |_, _, _| {
                legacy_called_by_closure.store(true, Ordering::Release);
                Ok(())
            },
        )
    });
    if let Ok(receipt) = &result {
        assert_eq!(receipt.state, DeliveryState::ProtocolAccepted);
        assert!(!legacy_called.load(Ordering::Acquire));
        let stored = ReceiptStore::for_instance(&home, "claude-agent")
            .expect("receipt store")
            .latest(receipt.delivery_id)
            .expect("receipt lookup")
            .expect("stored receipt");
        assert_eq!(stored.state, DeliveryState::ProtocolAccepted);
    }
    stop_instance_state(&home, "claude-agent");
    let probes = channel.health_probes.load(Ordering::Acquire);
    let not_ready = channel.not_ready_answers.load(Ordering::Acquire);
    let exit = channel.stop_and_join();
    let _ = fs::remove_dir_all(home);
    result.expect("ChannelBridge delivery must wait for delayed readiness");
    // The delivery only proves WAITING if a probe was actually answered
    // not-ready. Counting probes alone does not: the delivery path probes
    // `/health` again after the wait, so `probes >= 2` holds even when
    // readiness was there from the first answer — a vacuous assertion that a
    // spawn(1) mutant passed.
    assert!(
        not_ready >= 1,
        "readiness must have been withheld for at least one probe \
             (probes={probes}, not_ready={not_ready})"
    );
    // And the helper must have outlived the delivery: an accept-loop exit for
    // any other reason is the historical failure mode, where the dead server
    // turned into `Broken pipe` on the next request.
    assert_eq!(
        exit,
        FakeChannelExit::StopRequested,
        "fake channel must end because the test stopped it"
    );
}

/// The historical shape, pinned deterministically: when the helper dies for
/// good, delivery fails with a transport error rather than falling back or
/// hanging. This is what the old fixture produced by accident whenever a
/// transient accept error broke its loop.
#[test]
fn permanently_terminated_channel_helper_surfaces_transport_error() {
    let _delivery_hook_guard = super::super::registry::test_support::delivery_hook_guard();
    let home = home("registry-delivery-dead");
    let mut channel = FakeChannel::spawn(1);
    let port = channel.port;
    let mut locator = SessionLocator::claude(
        format!("http://127.0.0.1:{port}"),
        "claude-registry-session".to_string(),
        "registry-token".to_string(),
    );
    locator.managed = true;
    locator.server_pid = Some(std::process::id());
    locator.server_start_token = crate::process::process_start_token(std::process::id());
    fs::write(
        crate::fleet::fleet_yaml_path(&home),
        "instances:\n  claude-agent:\n    backend: claude\n",
    )
    .expect("fleet");
    super::super::registry::save_session_locator(&home, "claude-agent", &locator)
        .expect("locator publication");
    let exit = channel.stop_and_join();
    assert_eq!(
        exit,
        FakeChannelExit::StopRequested,
        "helper stopped on purpose"
    );
    let legacy_called = Arc::new(AtomicBool::new(false));
    let legacy_called_by_closure = Arc::clone(&legacy_called);
    let error = super::super::registry::wait_for_notification_readiness(
        &home,
        "claude-agent",
        Duration::from_secs(1),
    )
    .and_then(|()| {
        super::super::registry::deliver_notification(
            &home,
            "claude-agent",
            "registry delivery",
            move |_, _, _| {
                legacy_called_by_closure.store(true, Ordering::Release);
                Ok(())
            },
        )
    })
    .expect_err("a dead channel helper cannot accept a delivery");
    stop_instance_state(&home, "claude-agent");
    let _ = fs::remove_dir_all(home);
    // Assert the PRODUCT'S failure path, never the platform's vocabulary for
    // it. The errno is the part that differs — `Broken pipe (os error 32)`,
    // `Connection refused (os error 61)`, and on Windows `No connection could
    // be made because the target machine actively refused it. (os error
    // 10061)`. An allowlist of the strings one happens to have seen is a
    // platform-shaped assertion wearing a portability comment: this fixture
    // exists because of a flaky failure and must not become one. What is
    // stable is the product's own message, which every platform reaches by
    // the same route when the bridge cannot be contacted. The full error is
    // kept in the failure message so a real divergence stays diagnosable.
    let rendered = format!("{error:#}").to_ascii_lowercase();
    assert!(
        rendered.contains("did not become ready") || rendered.contains("channelbridge"),
        "dead helper must fail the readiness/transport path, got: {error:#}"
    );
    assert!(
        !legacy_called.load(Ordering::Acquire),
        "a dead bridge must not silently fall back to the legacy path"
    );
}

#[test]
fn channel_server_entry_declares_authenticated_local_bridge() {
    let home = home("entry");
    let entry = channel_server_entry(&home, "claude-agent").expect("server entry");
    assert_eq!(entry["env"]["AGEND_INSTANCE_NAME"], "claude-agent");
    assert_eq!(entry["args"][0], "channel-bridge");
    assert_eq!(entry["env"]["AGEND_HOME"], home.display().to_string());
    let _ = fs::remove_dir_all(home);
}

#[test]
fn channel_wire_notification_uses_delivery_and_chat_metadata() {
    let delivery_id = Uuid::new_v4();
    let value = json!({
        "jsonrpc":"2.0",
        "method":"notifications/claude/channel",
        "params": {
            "content":"hello",
            "meta": {"delivery_id":delivery_id.to_string(),"chat_id":"chat-1","sender_id":"agend-terminal"}
        }
    });
    assert_eq!(value["method"], "notifications/claude/channel");
    assert_eq!(
        value["params"]["meta"]["delivery_id"],
        delivery_id.to_string()
    );
    assert_eq!(value["params"]["meta"]["chat_id"], "chat-1");
}

#[test]
fn channel_initialize_rejects_old_or_missing_client_version() {
    let home = home("version");
    let locator = test_published_locator(&home, "claude-agent");
    let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime");
    let (sender, _receiver) = mpsc::sync_channel(1);
    runtime.set_sender(sender);
    let missing = mcp_initialize(&json!({"jsonrpc":"2.0","id":1,"params":{}}), &runtime);
    assert_eq!(missing["error"]["code"], -32001);
    mcp_initialize(
        &json!({"jsonrpc":"2.0","id":2,"params":{"clientInfo":{"version":"2.1.80"}}}),
        &runtime,
    );
    runtime.mark_initialized();
    assert!(runtime.is_ready());
    let old = mcp_initialize(
        &json!({"jsonrpc":"2.0","id":3,"params":{"clientInfo":{"version":"2.1.79"}}}),
        &runtime,
    );
    assert_eq!(old["error"]["code"], -32001);
    assert!(!runtime.is_ready());
    assert_eq!(runtime.client_version(), None);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn channel_initialize_does_not_claim_consumer_readiness_before_initialized_notification() {
    let home = home("consumer-readiness");
    let locator = test_published_locator(&home, "claude-agent");
    let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime");
    let (sender, _receiver) = mpsc::sync_channel(1);
    runtime.set_sender(sender);

    let response = mcp_message(
        json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{
                "protocolVersion":"2025-06-18",
                "clientInfo":{"name":"claude-code","version":"2.1.80"}
            }
        }),
        &runtime,
    )
    .expect("initialize response");
    assert!(response.get("result").is_some());
    assert!(
            !runtime.is_ready(),
            "initialize response proves protocol compatibility, not that the consumer installed its notification handler"
        );

    assert!(mcp_message(
        json!({
            "jsonrpc":"2.0",
            "method":"notifications/initialized"
        }),
        &runtime,
    )
    .is_none());
    assert!(
        runtime.is_ready(),
        "the standard initialized notification is the consumer-readiness boundary"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn initialized_notification_without_supported_initialize_stays_unready() {
    let home = home("initialized-without-handshake");
    let locator = test_published_locator(&home, "claude-agent");
    let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime");
    let (sender, _receiver) = mpsc::sync_channel(1);
    runtime.set_sender(sender);

    assert!(mcp_message(
        json!({
            "jsonrpc":"2.0",
            "method":"notifications/initialized"
        }),
        &runtime,
    )
    .is_none());
    assert!(
        !runtime.is_ready(),
        "an uncorrelated initialized notification must not open the delivery lane"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn late_initialized_notification_cannot_revive_a_disconnected_generation() {
    let home = home("initialized-after-disconnect");
    let locator = test_published_locator(&home, "claude-agent");
    let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime");
    let (sender, _receiver) = mpsc::sync_channel(1);
    runtime.set_sender(sender);

    mcp_message(
        json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{"clientInfo":{"version":"2.1.80"}}
        }),
        &runtime,
    )
    .expect("initialize response");
    runtime.clear_sender();

    assert!(mcp_message(
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        &runtime,
    )
    .is_none());
    assert!(
        !runtime.is_ready(),
        "a disconnected generation must not be revived by a late initialized notification"
    );
    assert_eq!(runtime.client_version(), None);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn late_consumer_readiness_admits_exactly_one_same_id_notification() {
    let home = home("late-consumer-readiness");
    let (locator, listener) =
        bind_and_publish_channel(&home, "claude-agent").expect("channel listener");
    let runtime =
        Arc::new(ChannelRuntime::new(&home, "claude-agent", &locator).expect("channel runtime"));
    let (sender, receiver) = mpsc::sync_channel(4);
    runtime.set_sender(sender);
    let delivery_id = Uuid::new_v4();
    let payload = serde_json::to_vec(&json!({
        "delivery_id":delivery_id,
        "chat_id":"self-kick-late-ready",
        "sender_id":"agend-terminal",
        "text":"[AGEND-RESUME]"
    }))
    .expect("payload");

    let before_runtime = Arc::clone(&runtime);
    let before_listener = listener.try_clone().expect("clone listener");
    let before = thread::spawn(move || {
        let (stream, _) = before_listener.accept().expect("pre-ready accept");
        handle_http(stream, before_runtime).expect("pre-ready response");
    });
    let rejected = client_request(&locator, "POST", HTTP_PATH, &payload, "application/json")
        .expect("pre-ready request");
    before.join().expect("pre-ready server");
    assert_eq!(rejected.status, 503);
    assert!(receiver.try_recv().is_err());

    mcp_message(
        json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":{"clientInfo":{"version":"2.1.80"}}
        }),
        &runtime,
    )
    .expect("initialize response");
    assert!(!runtime.is_ready());

    let after_initialize_runtime = Arc::clone(&runtime);
    let after_initialize_listener = listener.try_clone().expect("clone listener");
    let after_initialize = thread::spawn(move || {
        let (stream, _) = after_initialize_listener
            .accept()
            .expect("post-initialize accept");
        handle_http(stream, after_initialize_runtime).expect("post-initialize response");
    });
    let rejected_after_initialize =
        client_request(&locator, "POST", HTTP_PATH, &payload, "application/json")
            .expect("post-initialize request");
    after_initialize.join().expect("post-initialize server");
    assert_eq!(rejected_after_initialize.status, 503);
    assert!(receiver.try_recv().is_err());

    assert!(mcp_message(
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        &runtime,
    )
    .is_none());

    let ready_runtime = Arc::clone(&runtime);
    let ready_listener = listener.try_clone().expect("clone listener");
    let ready = thread::spawn(move || {
        let (stream, _) = ready_listener.accept().expect("ready accept");
        handle_http(stream, ready_runtime).expect("ready response");
    });
    let accepted = client_request(&locator, "POST", HTTP_PATH, &payload, "application/json")
        .expect("ready request");
    ready.join().expect("ready server");
    assert_eq!(accepted.status, 202);
    let notification = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("one channel notification");
    assert_eq!(
        notification.pointer("/params/meta/delivery_id"),
        Some(&Value::String(delivery_id.to_string()))
    );
    assert!(
        receiver.try_recv().is_err(),
        "the rejected pre-ready attempt must not leave a duplicate notification"
    );

    let retry_runtime = Arc::clone(&runtime);
    let retry_listener = listener.try_clone().expect("clone listener");
    let retry = thread::spawn(move || {
        let (stream, _) = retry_listener.accept().expect("retry accept");
        handle_http(stream, retry_runtime).expect("retry response");
    });
    let retried = client_request(&locator, "POST", HTTP_PATH, &payload, "application/json")
        .expect("same-id retry");
    retry.join().expect("retry server");
    assert_eq!(retried.status, 202);
    assert!(
        receiver.try_recv().is_err(),
        "an accepted same-id retry must not emit a second notification"
    );

    let conflicting_payload = serde_json::to_vec(&json!({
        "delivery_id":delivery_id,
        "chat_id":"self-kick-late-ready",
        "sender_id":"agend-terminal",
        "text":"different payload"
    }))
    .expect("conflicting payload");
    let conflict_runtime = Arc::clone(&runtime);
    let conflict_listener = listener.try_clone().expect("clone listener");
    let conflict = thread::spawn(move || {
        let (stream, _) = conflict_listener.accept().expect("conflict accept");
        handle_http(stream, conflict_runtime).expect("conflict response");
    });
    let conflicted = client_request(
        &locator,
        "POST",
        HTTP_PATH,
        &conflicting_payload,
        "application/json",
    )
    .expect("same-id conflict");
    conflict.join().expect("conflict server");
    assert_eq!(conflicted.status, 409);
    assert!(receiver.try_recv().is_err());

    runtime.clear_sender();
    assert!(!runtime.is_ready());
    drop(listener);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn unavailable_queue_does_not_poison_same_id_retry_or_restart_state() {
    let home = home("queue-retry");
    let locator = test_published_locator(&home, "claude-agent");
    let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime");
    let (sender, receiver) = mpsc::sync_channel(1);
    sender.send(json!({"occupied":true})).expect("fill queue");
    runtime.set_sender(sender);
    let delivery_id = Uuid::new_v4();

    assert_eq!(
        runtime
            .admit_channel_notification(delivery_id, "chat-1", None, "hello")
            .expect("full queue result"),
        NotificationAdmission::Unavailable
    );
    receiver.recv().expect("drain queue");
    assert_eq!(
        runtime
            .admit_channel_notification(delivery_id, "chat-1", None, "hello")
            .expect("retry result"),
        NotificationAdmission::Accepted
    );
    receiver.recv().expect("accepted notification");

    let restarted = ChannelRuntime::new(&home, "claude-agent", &locator).expect("restart runtime");
    assert_eq!(
        restarted
            .admit_channel_notification(delivery_id, "chat-1", None, "hello")
            .expect("durable retry result"),
        NotificationAdmission::Duplicate
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn reply_requires_delivery_and_chat_correlation() {
    let home = home("correlation");
    let locator = test_published_locator(&home, "claude-agent");
    let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime");
    let response = mcp_message(
        json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"tools/call",
            "params": {
                "name":"reply",
                "arguments": {
                    "chat_id":"unknown-chat",
                    "delivery_id":Uuid::new_v4(),
                    "text":"must be rejected"
                }
            }
        }),
        &runtime,
    )
    .expect("tool response");
    assert_eq!(response["error"]["code"], -32602);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn concurrent_deliveries_keep_distinct_chat_correlations() {
    let home = home("two-in-flight");
    let locator = test_published_locator(&home, "claude-agent");
    let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime");
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let first_chat = chat_id_for_delivery("claude-agent", first);
    let second_chat = chat_id_for_delivery("claude-agent", second);
    assert_ne!(first_chat, second_chat);
    runtime
        .remember_inbound(first, &first_chat, None, "first")
        .expect("first inbound");
    runtime
        .remember_inbound(second, &second_chat, None, "second")
        .expect("second inbound");
    assert_eq!(runtime.delivery_for_chat(&first_chat), Some(first));
    assert_eq!(runtime.delivery_for_chat(&second_chat), Some(second));
    let _ = fs::remove_dir_all(home);
}

#[test]
fn event_worker_switches_to_a_restarted_bridge_locator() {
    let home = home("event-worker-restart");
    let first = test_published_locator(&home, "claude-agent");
    let mut second = first.clone();
    second.endpoint_url = Some("http://127.0.0.1:43124".to_string());
    second.session_id = Some("claude-restarted-session".to_string());
    second.password = Some("restarted-token".to_string());

    ensure_event_worker(&home, "claude-agent", &first);
    ensure_event_worker(&home, "claude-agent", &second);
    let key = worker_key(&home, "claude-agent");
    assert_eq!(
        event_workers()
            .lock()
            .get(&key)
            .expect("restarted event worker")
            .locator,
        second
    );
    stop_instance_state(&home, "claude-agent");
    let _ = fs::remove_dir_all(home);
}

#[path = "claude_channel/tests/self_kick_tests.rs"]
mod self_kick_tests;
