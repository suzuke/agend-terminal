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

/// #3310 seed: one legacy `Inbound` journal row (the pre-settle era shape —
/// this host carried 765 of them with ZERO settle records) whose receipt is
/// still non-terminal, i.e. exactly the backlog the 2026-08-19 restart swept
/// into 1636 Ambiguous receipts fleet-wide in ten seconds.
fn seed_legacy_inbound_row_3310(home: &Path, state: DeliveryState) -> Uuid {
    let mut envelope = DeliveryEnvelope::new(
        "claude-agent",
        SessionLocator::claude(
            "http://127.0.0.1:43123".to_string(),
            "claude-test-session".to_string(),
            "test-token".to_string(),
        ),
        crate::transport::envelope::DeliveryKind::Notification,
        "[AGEND-MSG] id=m-3310 kind=task from=lead inbox=1",
        None,
    );
    let delivery_id = envelope.delivery_id;
    let store = ReceiptStore::for_instance(home, "claude-agent").expect("store");
    store.record_queued(&envelope).expect("queued receipt");
    if state != DeliveryState::Queued {
        store
            .record(DeliveryReceipt::for_state(&envelope, state))
            .expect("advance receipt");
    }
    envelope.delivery_id = delivery_id;
    append_log(
        home,
        "claude-agent",
        &ChannelLogRecord::Inbound {
            delivery_id,
            chat_id: format!("chat-{delivery_id}"),
            sender_id: None,
            content: "[AGEND-MSG] id=m-3310 kind=task from=lead inbox=1".to_string(),
            recorded_at: chrono::Utc::now().to_rfc3339(),
        },
    )
    .expect("legacy journal row");
    delivery_id
}

fn journal_retire_rows_for_3310(home: &Path, delivery_id: Uuid) -> usize {
    load_log(home, "claude-agent")
        .expect("journal readable")
        .into_iter()
        .filter(|record| {
            matches!(
                record,
                ChannelLogRecord::InboundRejected { delivery_id: id, .. } if *id == delivery_id
            )
        })
        .count()
}

/// #3310: the restart sweep stamps a replayed prepared row Ambiguous but
/// writes NOTHING back to the journal, so the same row re-primes `prepared`
/// (and the in-memory inbound maps) on EVERY subsequent restart, forever.
/// The fix: after the sweep decides a row's fate, RETIRE it in the journal
/// (an `InboundRejected` record — its replay semantic, `prepared.remove`, is
/// exactly right and pre-#3310 readers already understand it) so the next
/// load nets it out. Stamp-then-retire order keeps the crash window
/// convergent: a crash between the two leaves an Ambiguous receipt that the
/// next sweep skips-and-retires.
#[test]
fn restart_retires_replayed_prepared_rows_in_the_journal_3310() {
    let home = home("3310-retire");
    let locator = test_published_locator(&home, "claude-agent");
    let delivery_id = seed_legacy_inbound_row_3310(&home, DeliveryState::Queued);

    let _runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime");

    // Existing behavior (sanity): the non-terminal receipt was swept Ambiguous.
    let latest = ReceiptStore::for_instance(&home, "claude-agent")
        .expect("store")
        .latest(delivery_id)
        .expect("latest readable")
        .expect("receipt present");
    assert_eq!(
        latest.state,
        DeliveryState::Ambiguous,
        "precondition: the sweep must stamp the replayed row Ambiguous"
    );

    // #3310 contract: the sweep must also retire the row in the journal.
    assert_eq!(
        journal_retire_rows_for_3310(&home, delivery_id),
        1,
        "the restart sweep must retire the replayed prepared row in the journal \
         so the next restart does not re-prime it"
    );
    fs::remove_dir_all(&home).ok();
}

/// #3310: a receipt already TERMINAL (e.g. Completed) is skipped by the
/// Ambiguous stamp — but its journal row still replayed into `prepared` on
/// every restart. It must be retired too, without touching the receipt.
#[test]
fn restart_retires_terminal_receipt_rows_without_restamping_3310() {
    let home = home("3310-terminal");
    let locator = test_published_locator(&home, "claude-agent");
    let delivery_id = seed_legacy_inbound_row_3310(&home, DeliveryState::Completed);

    let _runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime");

    let latest = ReceiptStore::for_instance(&home, "claude-agent")
        .expect("store")
        .latest(delivery_id)
        .expect("latest readable")
        .expect("receipt present");
    assert_eq!(
        latest.state,
        DeliveryState::Completed,
        "a terminal receipt must never be re-stamped by the sweep"
    );
    assert_eq!(
        journal_retire_rows_for_3310(&home, delivery_id),
        1,
        "a terminal-receipt row must still be retired from the journal"
    );
    fs::remove_dir_all(&home).ok();
}

/// #3310 the point of the whole fix: the SECOND restart must be a no-op for
/// an already-retired row. The retirement record nets the legacy row out of
/// `prepared` at load, so restart #2 adds zero journal records and leaves the
/// receipt exactly as restart #1 stamped it — instead of re-priming the row
/// (and the in-memory inbound maps) forever, which is what shipped before.
#[test]
fn second_restart_is_a_noop_for_retired_rows_3310() {
    let home = home("3310-idem");
    let locator = test_published_locator(&home, "claude-agent");
    let delivery_id = seed_legacy_inbound_row_3310(&home, DeliveryState::Queued);

    drop(ChannelRuntime::new(&home, "claude-agent", &locator).expect("first restart"));
    let journal_after_first = load_log(&home, "claude-agent").expect("journal").len();
    assert_eq!(
        journal_retire_rows_for_3310(&home, delivery_id),
        1,
        "restart #1 retires the row exactly once"
    );

    drop(ChannelRuntime::new(&home, "claude-agent", &locator).expect("second restart"));
    assert_eq!(
        load_log(&home, "claude-agent").expect("journal").len(),
        journal_after_first,
        "restart #2 must add ZERO journal records for an already-retired row"
    );
    assert_eq!(
        journal_retire_rows_for_3310(&home, delivery_id),
        1,
        "restart #2 must not re-retire"
    );
    let latest = ReceiptStore::for_instance(&home, "claude-agent")
        .expect("store")
        .latest(delivery_id)
        .expect("latest readable")
        .expect("receipt present");
    assert_eq!(
        latest.state,
        DeliveryState::Ambiguous,
        "the receipt keeps restart #1's single Ambiguous stamp"
    );
    fs::remove_dir_all(&home).ok();
}

/// #3310: 1636 receipts flipped fleet-wide in ten seconds with not one log
/// line. The sweep must say what it did — one summary per restore pass.
#[test]
#[tracing_test::traced_test]
fn restart_sweep_logs_one_summary_line_3310() {
    let home = home("3310-summary");
    let locator = test_published_locator(&home, "claude-agent");
    let _delivery_id = seed_legacy_inbound_row_3310(&home, DeliveryState::Queued);

    let _runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime");

    assert!(
        logs_contain("bridge restart retired"),
        "#3310: the restart sweep must log a summary of what it swept"
    );
    fs::remove_dir_all(&home).ok();
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
fn foreign_backend_locator_is_recovered_at_bridge_start() {
    let home = home("foreign-recover");
    let foreign = SessionLocator::opencode(
        "http://127.0.0.1:43123".to_string(),
        Some("opencode-session".to_string()),
        "agend".to_string(),
        "secret".to_string(),
    );
    super::super::registry::save_session_locator(&home, "claude-agent", &foreign)
        .expect("plant foreign locator");

    // A backend switch (opencode→claude) leaves a foreign locator behind. The
    // Claude bridge must recover it — publish a fresh claude locator instead
    // of hard-failing on the stale artifact (root cause #3307).
    let (fresh, listener) =
        bind_and_publish_channel(&home, "claude-agent").expect("recover and publish");
    assert_eq!(fresh.backend, "claude");
    assert!(fresh.session_id.is_some());
    let stored = super::super::registry::load_session_locator(&home, "claude-agent")
        .expect("stored locator");
    assert_eq!(stored.backend, "claude");
    assert_eq!(stored.session_id, fresh.session_id);
    drop(listener);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn stale_claude_locator_is_recovered_at_bridge_start() {
    let home = home("stale-claude-recover");
    let mut stale = SessionLocator::claude(
        "http://127.0.0.1:43123".to_string(),
        "stale-session".to_string(),
        "stale-token".to_string(),
    );
    // A dead bridge identity (pid 0 has no start token) makes this stale.
    stale.server_pid = Some(0);
    stale.server_start_token = Some(1);
    super::super::registry::save_session_locator(&home, "claude-agent", &stale)
        .expect("plant stale claude locator");

    // A locator whose bridge process is gone must be recovered by the new
    // bridge, not carried forward as an unreachable target.
    let (fresh, listener) =
        bind_and_publish_channel(&home, "claude-agent").expect("recover stale claude locator");
    assert_eq!(fresh.backend, "claude");
    let stored = super::super::registry::load_session_locator(&home, "claude-agent")
        .expect("stored locator");
    assert_eq!(stored.backend, "claude");
    assert_ne!(stored.password, stale.password);
    drop(listener);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn foreign_recover_is_instance_keyed_and_preserves_other_live_locator() {
    let home = home("foreign-isolation");
    let foreign = SessionLocator::opencode(
        "http://127.0.0.1:43123".to_string(),
        Some("opencode-session".to_string()),
        "agend".to_string(),
        "secret".to_string(),
    );
    super::super::registry::save_session_locator(&home, "claude-agent", &foreign)
        .expect("plant foreign locator");
    // A live claude-owned locator on a DIFFERENT instance must be untouched by
    // recovering claude-agent's own stale locator (instance-keyed isolation).
    let live_other = test_published_locator(&home, "other-agent");

    let (fresh, listener) =
        bind_and_publish_channel(&home, "claude-agent").expect("recover own locator");
    assert_eq!(fresh.backend, "claude");
    let other = super::super::registry::load_session_locator(&home, "other-agent")
        .expect("other live locator");
    assert_eq!(other, live_other);
    drop(listener);
    let _ = fs::remove_dir_all(home);
}

#[test]
fn live_claude_locator_is_never_recovered_away() {
    let home = home("live-safety");
    // A live claude-owned locator: same-process pid + start token, backend claude.
    let live = test_published_locator(&home, "claude-agent");
    assert_eq!(live.backend, "claude");
    assert!(super::super::registry::load_session_locator(&home, "claude-agent").is_ok());

    // Re-publishing a fresh bridge on a live claude locator must NOT remove the
    // live one first — the fresh listener atomically supersedes it, and the
    // recover path leaves live claude locators alone.
    let (fresh, listener) =
        bind_and_publish_channel(&home, "claude-agent").expect("re-publish over live");
    assert_eq!(fresh.backend, "claude");
    let stored = super::super::registry::load_session_locator(&home, "claude-agent")
        .expect("stored locator");
    assert_eq!(stored.backend, "claude");
    assert_eq!(stored.server_pid, Some(std::process::id()));
    drop(listener);
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
fn prepared_prefix_reserves_same_id_and_marks_receipt_ambiguous() {
    let home = home("prepared-prefix");
    let locator = test_published_locator(&home, "claude-agent");
    let envelope = DeliveryEnvelope::self_kick(
        "claude-agent",
        locator.clone(),
        crate::agent::fresh_restart_self_kick_prompt(),
    );
    let delivery_id = envelope.delivery_id;
    let chat_id = chat_id_for_delivery("claude-agent", delivery_id);
    let store = ReceiptStore::for_instance(&home, "claude-agent").expect("store");
    store.record_queued(&envelope).expect("queued receipt");
    append_log(
        &home,
        "claude-agent",
        &ChannelLogRecord::InboundPrepared {
            delivery_id,
            chat_id: chat_id.clone(),
            sender_id: Some("agend-terminal".to_string()),
            content: envelope.body.clone(),
            recorded_at: Utc::now().to_rfc3339(),
        },
    )
    .expect("prepared prefix");

    let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("restart runtime");
    let (sender, receiver) = mpsc::sync_channel(1);
    runtime.set_sender(sender);
    assert_eq!(
        runtime
            .admit_channel_notification(
                delivery_id,
                &chat_id,
                Some("agend-terminal"),
                &envelope.body,
            )
            .expect("same-id replay"),
        NotificationAdmission::Duplicate
    );
    assert_eq!(
        runtime
            .admit_channel_notification(delivery_id, &chat_id, Some("agend-terminal"), "different",)
            .expect("same-id conflict"),
        NotificationAdmission::Conflict
    );
    assert!(receiver.try_recv().is_err());
    let receipt = store
        .latest(delivery_id)
        .expect("latest receipt")
        .expect("ambiguous receipt");
    assert_eq!(receipt.state, DeliveryState::Ambiguous);
    assert!(receipt
        .detail
        .as_deref()
        .is_some_and(|detail| detail.contains("prepared admission")));

    let retry = store
        .record_queued(&envelope)
        .expect("same-id retry bookkeeping");
    assert_eq!(retry.state, DeliveryState::Ambiguous);
    assert_eq!(
        store
            .latest(delivery_id)
            .expect("latest retry receipt")
            .expect("preserved ambiguous receipt")
            .state,
        DeliveryState::Ambiguous
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn prepared_prefix_ambiguous_accepts_exact_start_ack() {
    let home = home("prepared-prefix-start-ack");
    let locator = test_published_locator(&home, "claude-agent");
    let envelope = DeliveryEnvelope::self_kick(
        "claude-agent",
        locator.clone(),
        crate::agent::fresh_restart_self_kick_prompt(),
    );
    let delivery_id = envelope.delivery_id;
    let chat_id = chat_id_for_delivery("claude-agent", delivery_id);
    let store = ReceiptStore::for_instance(&home, "claude-agent").expect("store");
    store.record_queued(&envelope).expect("queued receipt");
    append_log(
        &home,
        "claude-agent",
        &ChannelLogRecord::InboundPrepared {
            delivery_id,
            chat_id,
            sender_id: Some("agend-terminal".to_string()),
            content: envelope.body.clone(),
            recorded_at: Utc::now().to_rfc3339(),
        },
    )
    .expect("prepared prefix");

    let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("restart runtime");
    assert_eq!(
        store
            .latest(delivery_id)
            .expect("latest receipt")
            .expect("ambiguous receipt")
            .state,
        DeliveryState::Ambiguous
    );
    assert_eq!(
        runtime
            .acknowledge_self_kick(delivery_id)
            .expect("exact start ack")
            .state,
        DeliveryState::TurnStarted
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn rejected_suffix_reopens_prepared_id_across_restart() {
    let home = home("rejected-suffix");
    let locator = test_published_locator(&home, "claude-agent");
    let delivery_id = Uuid::new_v4();
    append_log(
        &home,
        "claude-agent",
        &ChannelLogRecord::InboundPrepared {
            delivery_id,
            chat_id: "chat-1".to_string(),
            sender_id: None,
            content: "hello".to_string(),
            recorded_at: Utc::now().to_rfc3339(),
        },
    )
    .expect("prepared prefix");
    append_log(
        &home,
        "claude-agent",
        &ChannelLogRecord::InboundRejected {
            delivery_id,
            recorded_at: Utc::now().to_rfc3339(),
        },
    )
    .expect("rejected suffix");

    let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("restart runtime");
    let (sender, receiver) = mpsc::sync_channel(1);
    runtime.set_sender(sender);
    assert_eq!(
        runtime
            .admit_channel_notification(delivery_id, "chat-1", None, "hello")
            .expect("retry after rejection"),
        NotificationAdmission::Accepted
    );
    let notification = receiver.recv().expect("retry notification");
    assert_eq!(
        notification.pointer("/params/meta/delivery_id"),
        Some(&Value::String(delivery_id.to_string()))
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn legacy_inbound_prefix_is_reserved_and_marked_ambiguous() {
    let home = home("legacy-inbound-prefix");
    let locator = test_published_locator(&home, "claude-agent");
    let envelope = DeliveryEnvelope::self_kick(
        "claude-agent",
        locator.clone(),
        crate::agent::fresh_restart_self_kick_prompt(),
    );
    let delivery_id = envelope.delivery_id;
    let chat_id = chat_id_for_delivery("claude-agent", delivery_id);
    let store = ReceiptStore::for_instance(&home, "claude-agent").expect("store");
    store.record_queued(&envelope).expect("queued receipt");
    append_log(
        &home,
        "claude-agent",
        &ChannelLogRecord::Inbound {
            delivery_id,
            chat_id: chat_id.clone(),
            sender_id: Some("agend-terminal".to_string()),
            content: envelope.body.clone(),
            recorded_at: Utc::now().to_rfc3339(),
        },
    )
    .expect("legacy inbound prefix");

    let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("restart runtime");
    let (sender, receiver) = mpsc::sync_channel(1);
    runtime.set_sender(sender);
    assert_eq!(
        runtime
            .admit_channel_notification(
                delivery_id,
                &chat_id,
                Some("agend-terminal"),
                &envelope.body,
            )
            .expect("legacy same-id replay"),
        NotificationAdmission::Duplicate
    );
    assert!(receiver.try_recv().is_err());
    assert_eq!(
        store
            .latest(delivery_id)
            .expect("latest receipt")
            .expect("ambiguous receipt")
            .state,
        DeliveryState::Ambiguous
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn legacy_inbound_ambiguous_accepts_exact_start_ack() {
    let home = home("legacy-inbound-start-ack");
    let locator = test_published_locator(&home, "claude-agent");
    let envelope = DeliveryEnvelope::self_kick(
        "claude-agent",
        locator.clone(),
        crate::agent::fresh_restart_self_kick_prompt(),
    );
    let delivery_id = envelope.delivery_id;
    let chat_id = chat_id_for_delivery("claude-agent", delivery_id);
    let store = ReceiptStore::for_instance(&home, "claude-agent").expect("store");
    store.record_queued(&envelope).expect("queued receipt");
    let mut accepted = DeliveryReceipt::for_state(&envelope, DeliveryState::ProtocolAccepted);
    accepted.protocol_request_id = Some(delivery_id.to_string());
    store.record(accepted).expect("protocol accepted receipt");
    append_log(
        &home,
        "claude-agent",
        &ChannelLogRecord::Inbound {
            delivery_id,
            chat_id,
            sender_id: Some("agend-terminal".to_string()),
            content: envelope.body.clone(),
            recorded_at: Utc::now().to_rfc3339(),
        },
    )
    .expect("legacy inbound prefix");

    let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("restart runtime");
    assert_eq!(
        store
            .latest(delivery_id)
            .expect("latest receipt")
            .expect("ambiguous receipt")
            .state,
        DeliveryState::Ambiguous
    );
    assert_eq!(
        runtime
            .acknowledge_self_kick(delivery_id)
            .expect("exact start ack")
            .state,
        DeliveryState::TurnStarted
    );
    let _ = fs::remove_dir_all(home);
}

/// #3324: the bridge's own MCP instructions are one of the two signals that
/// sent the agent to the wrong tool, and BOTH halves were wrong.
///
/// It described the envelope as `source="agend-terminal"` when the real wrapper
/// is `source="agend-claude-channel" ... sender_id="agend-terminal"` — the two
/// fields transposed — and then said "reply with the reply tool", which names
/// two different tools in this environment. An agent reading it reasons
/// correctly to the wrong conclusion.
#[test]
fn bridge_instructions_describe_the_real_envelope_and_the_exact_tool_3324() {
    let home = home("instructions-3324");
    let locator = test_published_locator(&home, "claude-agent");
    let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime");
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
    let text = response["result"]["instructions"]
        .as_str()
        .expect("instructions")
        .to_string();
    let _ = fs::remove_dir_all(home);
    assert!(
        text.contains("source=\"agend-claude-channel\""),
        "#3324: instructions must describe the envelope the agent actually \
         receives; got {text:?}"
    );
    assert!(
        !text.contains("source=\"agend-terminal\""),
        "#3324: the transposed source/sender_id description must be gone; got {text:?}"
    );
    assert!(
        text.contains("mcp__agend-terminal__reply"),
        "#3324: instructions must name the tool that actually reaches an \
         external channel, since this server's own `reply` does not; got {text:?}"
    );
}

/// #3324: seed one delivery exactly as the daemon would, and hand the runtime
/// the inbound mapping the bridge webhook records.
fn seed_delivery(
    home: &Path,
    locator: &SessionLocator,
    runtime: &ChannelRuntime,
    body: &str,
    origin: Option<crate::channel::ChannelKind>,
) -> (Uuid, String) {
    let mut envelope = DeliveryEnvelope::new(
        "claude-agent",
        locator.clone(),
        crate::transport::envelope::DeliveryKind::Notification,
        body,
        None,
    );
    envelope.channel_origin = origin;
    envelope.logical_delivery_id = Some("m-3324".to_string());
    let delivery_id = envelope.delivery_id;
    let chat_id = chat_id_for_delivery("claude-agent", delivery_id);
    let store = ReceiptStore::for_instance(home, "claude-agent").expect("store");
    store.record_queued(&envelope).expect("queued receipt");
    runtime
        .remember_inbound(delivery_id, &chat_id, Some("agend-terminal"), body)
        .expect("inbound mapping");
    (delivery_id, chat_id)
}

fn call_bridge_reply(runtime: &ChannelRuntime, chat_id: &str, delivery_id: Uuid) -> Value {
    mcp_message(
        json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"tools/call",
            "params": {
                "name":"reply",
                "arguments": {
                    "chat_id": chat_id,
                    "delivery_id": delivery_id,
                    "text":"the answer the user is waiting for"
                }
            }
        }),
        runtime,
    )
    .expect("tool response")
}

/// #3324 RED: a delivery that ORIGINATED on an external channel must not be
/// replied to through the bridge.
///
/// The bridge is an inbound transport. Its `reply` records a transport
/// acknowledgement and never reaches Telegram — and the environment exposes a
/// SECOND tool also called `reply` that does. The observed incident: the agent
/// called this one twice, got success twice, and the user received nothing for
/// eleven minutes while the escalation ladder ran.
///
/// Fail CLOSED, and fail BEFORE the Reply record: a recorded reply is what makes
/// the loss invisible afterwards.
#[test]
fn bridge_reply_is_refused_for_an_external_channel_origin_3324() {
    let home = home("external-origin-refused-3324");
    let locator = test_published_locator(&home, "claude-agent");
    let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime");
    let (delivery_id, chat_id) = seed_delivery(
        &home,
        &locator,
        &runtime,
        "[user:chiachenghuang via telegram] please research this",
        Some(crate::channel::ChannelKind::Telegram),
    );

    let response = call_bridge_reply(&runtime, &chat_id, delivery_id);

    let message = response["error"]["message"].as_str().unwrap_or_default();
    assert_eq!(
        response["error"]["code"], -32602,
        "#3324: an external-origin delivery must be REFUSED, not acknowledged; got {response}"
    );
    assert!(
        message.contains("mcp__agend-terminal__reply"),
        "#3324: the refusal must name the exact tool that can deliver, or the \
         agent picks the wrong `reply` again; got {message:?}"
    );
    assert!(
        message.contains("m-3324"),
        "#3324: the refusal must name the target identity so the agent can \
         address the right message; got {message:?}"
    );
    assert!(
        runtime.reply_for(delivery_id).is_none(),
        "#3324: nothing may be recorded as replied — the obligation stays armed"
    );
    let _ = fs::remove_dir_all(home);
}

/// #3324: a delivery with NO receipt is served, deliberately.
///
/// Unknown origin is not external origin. `deliver_resident` records the
/// envelope before posting the webhook, so a live delivery has one; the absent
/// case is a pruned or pre-settle row (#3310), and refusing those would break
/// internal replies with no way for the agent to tell why. Pinned so the choice
/// is a decision rather than an accident — a later "fail closed on anything
/// unknown" would break legacy rows and this test says so.
#[test]
fn bridge_reply_serves_a_delivery_with_no_receipt_3324() {
    let home = home("no-receipt-served-3324");
    let locator = test_published_locator(&home, "claude-agent");
    let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime");
    let delivery_id = Uuid::new_v4();
    let chat_id = chat_id_for_delivery("claude-agent", delivery_id);
    // Inbound mapping only — no receipt is ever written for this delivery.
    runtime
        .remember_inbound(
            delivery_id,
            &chat_id,
            Some("agend-terminal"),
            "orphaned row",
        )
        .expect("inbound mapping");

    let response = call_bridge_reply(&runtime, &chat_id, delivery_id);

    assert!(
        response.get("error").is_none(),
        "#3324: an unknown-origin delivery must still be answerable; got {response}"
    );
    assert!(
        runtime.reply_for(delivery_id).is_some(),
        "#3324: and its reply must be recorded"
    );
    let _ = fs::remove_dir_all(home);
}

/// #3324: the guard must not overreach. A delivery that originated INSIDE AgEnD
/// is the bridge's own to answer, and refusing it would break every internal
/// query.
#[test]
fn bridge_reply_still_serves_an_internal_delivery_3324() {
    let home = home("internal-origin-served-3324");
    let locator = test_published_locator(&home, "claude-agent");
    let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime");
    let (delivery_id, chat_id) = seed_delivery(
        &home,
        &locator,
        &runtime,
        "[from:codex-125550] status?",
        None,
    );

    let response = call_bridge_reply(&runtime, &chat_id, delivery_id);

    assert!(
        response.get("error").is_none(),
        "#3324: an internal delivery must still be accepted; got {response}"
    );
    assert!(
        runtime.reply_for(delivery_id).is_some(),
        "#3324: the internal reply must still be recorded"
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
