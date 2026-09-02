use super::*;

#[test]
fn channel_bridge_queue_without_turn_start_is_truthfully_overdue() {
    let _delivery_hook_guard = super::super::super::registry::test_support::delivery_hook_guard();
    let home = home("queue-without-turn-start");
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
    super::super::super::registry::save_session_locator(&home, "claude-agent", &locator)
        .expect("locator publication");
    let legacy_called = Arc::new(AtomicBool::new(false));
    let legacy_called_by_closure = Arc::clone(&legacy_called);
    let accepted = super::super::super::registry::deliver_self_kick_notification(
        &home,
        "claude-agent",
        "[AGEND-RESUME] id=queue-without-turn-start",
        move |_, _, _| {
            legacy_called_by_closure.store(true, Ordering::Release);
            Ok(())
        },
    )
    .expect("ChannelBridge webhook queue admission");
    assert_eq!(accepted.state, DeliveryState::ProtocolAccepted);
    let store = ReceiptStore::for_instance(&home, "claude-agent").expect("store");
    let mut old = accepted.clone();
    old.recorded_at = (Utc::now()
        - chrono::Duration::seconds(SELF_KICK_ACK_WINDOW.as_secs() as i64 + 1))
    .to_rfc3339();
    assert!(store
        .record_if_latest_state(accepted.delivery_id, DeliveryState::ProtocolAccepted, old)
        .expect("backdate"));
    assert_eq!(
        self_kick_watchdog_pass_at(&home, "claude-agent", Utc::now(), &|_| None)
            .expect("watchdog")
            .len(),
        1
    );
    assert_eq!(
        store
            .latest(accepted.delivery_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::AckOverdue,
        "queue admission without a consumer turn start must not be reported as TurnStarted"
    );
    assert!(!legacy_called.load(Ordering::Acquire));
    stop_instance_state(&home, "claude-agent");
    assert_eq!(channel.stop_and_join(), FakeChannelExit::StopRequested);
    let _ = fs::remove_dir_all(home);
}

fn self_kick_fixture(
    tag: &str,
) -> (
    std::path::PathBuf,
    Arc<ChannelRuntime>,
    DeliveryEnvelope,
    String,
) {
    self_kick_fixture_with_acceptance(tag, true)
}

fn self_kick_fixture_with_acceptance(
    tag: &str,
    accepted: bool,
) -> (
    std::path::PathBuf,
    Arc<ChannelRuntime>,
    DeliveryEnvelope,
    String,
) {
    self_kick_fixture_with_session(tag, accepted, None)
}

fn self_kick_fixture_with_session(
    tag: &str,
    accepted: bool,
    session_id: Option<&str>,
) -> (
    std::path::PathBuf,
    Arc<ChannelRuntime>,
    DeliveryEnvelope,
    String,
) {
    let home = home(tag);
    let locator = test_published_locator(&home, "claude-agent");
    let runtime = Arc::new(ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime"));
    let mut envelope = DeliveryEnvelope::self_kick(
        "claude-agent",
        locator,
        crate::agent::fresh_restart_self_kick_prompt(),
    );
    if let Some(session_id) = session_id {
        envelope.session.session_id = Some(session_id.to_string());
    }
    let chat_id = chat_id_for_delivery("claude-agent", envelope.delivery_id);
    runtime
        .remember_inbound(
            envelope.delivery_id,
            &chat_id,
            Some("agend-terminal"),
            &envelope.body,
        )
        .expect("inbound");
    let store = ReceiptStore::for_instance(&home, "claude-agent").expect("store");
    store.record_queued(&envelope).expect("queued");
    if accepted {
        let mut receipt = DeliveryReceipt::for_state(&envelope, DeliveryState::ProtocolAccepted);
        receipt.protocol_request_id = Some(envelope.delivery_id.to_string());
        receipt.backend_event = Some("webhook_accepted".to_string());
        store.record(receipt).expect("accepted");
    }
    (home, runtime, envelope, chat_id)
}

#[test]
fn self_kick_late_ack_after_four_minutes_is_admissible_and_completes() {
    let (home, runtime, envelope, _chat_id) = self_kick_fixture("late-ack");
    let store = ReceiptStore::for_instance(&home, "claude-agent").expect("store");
    let accepted = store
        .latest(envelope.delivery_id)
        .expect("latest")
        .expect("accepted");
    let mut old = accepted;
    old.recorded_at = (Utc::now()
        - chrono::Duration::minutes(4)
        - chrono::Duration::seconds(SELF_KICK_ACK_WINDOW.as_secs() as i64))
    .to_rfc3339();
    store
        .record_if_latest_state(envelope.delivery_id, DeliveryState::ProtocolAccepted, old)
        .expect("backdate")
        .then_some(());
    assert_eq!(
        self_kick_watchdog_pass_at(&home, "claude-agent", Utc::now(), &|_| None)
            .expect("watchdog")
            .len(),
        1
    );
    let started = mcp_message(
        json!({
            "jsonrpc":"2.0", "id":1, "method":"tools/call",
            "params":{"name":"ack_start","arguments":{"delivery_id":envelope.delivery_id}}
        }),
        &runtime,
    )
    .expect("late start response");
    assert_eq!(
        started["result"]["structuredContent"]["state"], "TurnStarted",
        "an overdue but truthful current-session ack must remain admissible"
    );
    let completed = mcp_message(
        json!({
            "jsonrpc":"2.0", "id":2, "method":"tools/call",
            "params":{"name":"ack_complete","arguments":{"delivery_id":envelope.delivery_id}}
        }),
        &runtime,
    )
    .expect("late completion response");
    assert_eq!(
        completed["result"]["structuredContent"]["state"],
        "Completed"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn self_kick_ack_rejects_predecessor_session_envelope() {
    let (home, runtime, envelope, _chat_id) =
        self_kick_fixture_with_session("predecessor-session", true, Some("old-session"));
    let response = mcp_message(
        json!({
            "jsonrpc":"2.0", "id":1, "method":"tools/call",
            "params":{"name":"ack_start","arguments":{"delivery_id":envelope.delivery_id}}
        }),
        &runtime,
    )
    .expect("predecessor response");
    assert_eq!(response["error"]["code"], -32602);
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("session"),
        "predecessor-session rejection must identify the session mismatch"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn self_kick_ack_linearizes_when_consumer_wins_before_protocol_receipt() {
    let (home, runtime, envelope, _chat_id) =
        self_kick_fixture_with_acceptance("queued-ack", false);
    let started = mcp_message(
        json!({
            "jsonrpc":"2.0", "id":1, "method":"tools/call",
            "params":{"name":"ack_start","arguments":{"delivery_id":envelope.delivery_id}}
        }),
        &runtime,
    )
    .expect("queued start response");
    assert_eq!(
        started["result"]["structuredContent"]["state"],
        "TurnStarted"
    );
    assert_eq!(
        ReceiptStore::for_instance(&home, "claude-agent")
            .expect("store")
            .latest(envelope.delivery_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::TurnStarted
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn self_kick_ack_is_exact_id_and_has_no_reply_or_sse_side_effect() {
    let (home, runtime, envelope, chat_id) = self_kick_fixture("ack");
    let (events, event_replay) = runtime.subscribe(None);
    assert!(event_replay.is_empty());

    let wrong = mcp_message(
        json!({
            "jsonrpc":"2.0", "id":1, "method":"tools/call",
            "params":{"name":"ack_start","arguments":{"delivery_id":Uuid::new_v4()}}
        }),
        &runtime,
    )
    .expect("wrong-id response");
    assert_eq!(wrong["error"]["code"], -32602);
    assert!(
        events.try_recv().is_err(),
        "wrong/stale ack must not publish an SSE event"
    );

    let started = mcp_message(
        json!({
            "jsonrpc":"2.0", "id":2, "method":"tools/call",
            "params":{"name":"ack_start","arguments":{"delivery_id":envelope.delivery_id}}
        }),
        &runtime,
    )
    .expect("start response");
    assert_eq!(
        started["result"]["structuredContent"]["state"],
        "TurnStarted"
    );
    assert!(events.try_recv().is_err(), "start ack must not publish SSE");
    let store = ReceiptStore::for_instance(&home, "claude-agent").expect("store");
    assert_eq!(
        store
            .latest(envelope.delivery_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::TurnStarted
    );

    let reply = mcp_message(
        json!({
            "jsonrpc":"2.0", "id":3, "method":"tools/call",
            "params":{"name":"reply","arguments":{"chat_id":chat_id,"delivery_id":envelope.delivery_id,"text":"wrong side effect"}}
        }),
        &runtime,
    )
    .expect("reply rejection");
    assert_eq!(reply["error"]["code"], -32602);
    assert!(
        events.try_recv().is_err(),
        "rejected reply must not publish SSE"
    );

    let completed = mcp_message(
        json!({
            "jsonrpc":"2.0", "id":4, "method":"tools/call",
            "params":{"name":"ack_complete","arguments":{"delivery_id":envelope.delivery_id}}
        }),
        &runtime,
    )
    .expect("completion response");
    assert_eq!(
        completed["result"]["structuredContent"]["state"],
        "Completed"
    );
    assert!(
        events.try_recv().is_err(),
        "completion ack must not publish SSE"
    );
    assert_eq!(
        store
            .latest(envelope.delivery_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::Completed
    );
    assert_eq!(
        self_kick_watchdog_pass_at(
            &home,
            "claude-agent",
            Utc::now() + chrono::Duration::hours(1),
            &|_| None,
        )
        .expect("successful turn watchdog")
        .len(),
        0,
        "a completed self-kick must not produce a missing-ack alert"
    );
    let _ = fs::remove_dir_all(home);
}

#[test]
fn self_kick_watchdog_replays_durable_acceptance_and_alerts_once() {
    let (home, _runtime, envelope, _chat_id) = self_kick_fixture("watchdog");
    let store = ReceiptStore::for_instance(&home, "claude-agent").expect("store");
    let accepted = store
        .latest(envelope.delivery_id)
        .expect("latest")
        .expect("accepted");
    let mut old = accepted.clone();
    old.recorded_at = (Utc::now()
        - chrono::Duration::seconds(SELF_KICK_ACK_WINDOW.as_secs() as i64 + 1))
    .to_rfc3339();
    store
        .record_if_latest_state(envelope.delivery_id, DeliveryState::ProtocolAccepted, old)
        .expect("backdate")
        .then_some(());

    let now = Utc::now();
    assert_eq!(
        self_kick_watchdog_pass_at(&home, "claude-agent", now, &|_| None)
            .expect("watchdog")
            .len(),
        1
    );
    assert_eq!(
        self_kick_watchdog_pass_at(&home, "claude-agent", now, &|_| None)
            .expect("latch")
            .len(),
        0
    );
    assert_eq!(
        format!(
            "{:?}",
            store
                .latest(envelope.delivery_id)
                .expect("latest")
                .expect("receipt")
                .state
        ),
        "AckOverdue"
    );
    let log = fs::read_to_string(home.join("event-log.jsonl")).expect("event log");
    assert_eq!(log.matches("claude_self_kick_ack_overdue").count(), 1);
    assert!(log.contains("no automatic retry"));
    let _ = fs::remove_dir_all(home);
}

#[test]
fn self_kick_receipt_cas_stress_has_one_terminal_winner() {
    let (home, _runtime, envelope, _chat_id) = self_kick_fixture("cas-stress");
    let store = ReceiptStore::for_instance(&home, "claude-agent").expect("store");
    let barrier = Arc::new(std::sync::Barrier::new(32));
    let mut workers = Vec::new();
    for _ in 0..32 {
        let barrier = Arc::clone(&barrier);
        let store = store.clone();
        let receipt = store
            .latest(envelope.delivery_id)
            .expect("latest")
            .expect("accepted");
        // fire-and-forget: test workers are joined below after the CAS race.
        workers.push(thread::spawn(move || {
            barrier.wait();
            let mut ambiguous = receipt;
            ambiguous.state = DeliveryState::AckOverdue;
            store
                .record_if_latest_state(
                    ambiguous.delivery_id,
                    DeliveryState::ProtocolAccepted,
                    ambiguous,
                )
                .expect("CAS write")
        }));
    }
    let winners = workers
        .into_iter()
        .map(|worker| worker.join().expect("worker"))
        .filter(|won| *won)
        .count();
    assert_eq!(winners, 1, "exactly one watchdog winner may alert");
    assert_eq!(
        store
            .latest(envelope.delivery_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::AckOverdue
    );
    let _ = fs::remove_dir_all(home);
}

/// t-…-82348-105: an `ack_start` that arrives AFTER the watchdog declared
/// `AckOverdue` must leave a durable, typed reconciliation marker so the
/// daemon can tell the operator the earlier alarm is resolved — and must do
/// so exactly once, for the ack and for the watchdog pass that consumes it.
#[test]
fn self_kick_late_ack_after_overdue_marks_and_reconciles_exactly_once() {
    let (home, runtime, envelope, _chat_id) = self_kick_fixture("late-ack-marker");
    let store = ReceiptStore::for_instance(&home, "claude-agent").expect("store");
    let mut old = store
        .latest(envelope.delivery_id)
        .expect("latest")
        .expect("accepted");
    old.recorded_at = (Utc::now()
        - chrono::Duration::seconds(SELF_KICK_ACK_WINDOW.as_secs() as i64 + 14))
    .to_rfc3339();
    assert!(store
        .record_if_latest_state(envelope.delivery_id, DeliveryState::ProtocolAccepted, old)
        .expect("backdate"));
    assert_eq!(
        self_kick_watchdog_pass_at(&home, "claude-agent", Utc::now(), &|_| None)
            .expect("watchdog")
            .len(),
        1
    );

    let ack = json!({
        "jsonrpc":"2.0", "id":1, "method":"tools/call",
        "params":{"name":"ack_start","arguments":{"delivery_id":envelope.delivery_id}}
    });
    let started = mcp_message(ack.clone(), &runtime).expect("late start response");
    assert_eq!(
        started["result"]["structuredContent"]["state"],
        "TurnStarted"
    );

    let latest = store
        .latest(envelope.delivery_id)
        .expect("latest")
        .expect("receipt");
    assert_eq!(
        latest.backend_event.as_deref(),
        Some("claude_channel_turn_started_late"),
        "a post-AckOverdue ack must be marked as the late one"
    );
    let late_by = latest.late_ack_secs.expect("late_ack_secs must be stamped");
    assert!(
        (14..=16).contains(&late_by),
        "late_ack_secs must measure the overshoot past the window, got {late_by}"
    );

    let log = fs::read_to_string(home.join("event-log.jsonl")).expect("event log");
    assert_eq!(log.matches("claude_self_kick_ack_late").count(), 1);

    // A second ack is an idempotent no-op: no second event, no re-stamp.
    mcp_message(ack, &runtime).expect("repeat ack");
    let log = fs::read_to_string(home.join("event-log.jsonl")).expect("event log");
    assert_eq!(
        log.matches("claude_self_kick_ack_late").count(),
        1,
        "a repeated ack must not log a second late-ack event"
    );

    // The watchdog reports the late ack once, then clears the marker.
    let outcomes = self_kick_watchdog_pass_at(&home, "claude-agent", Utc::now(), &|_| None)
        .expect("reconcile pass");
    assert_eq!(
        outcomes.len(),
        1,
        "one reconciliation outcome: {outcomes:?}"
    );
    assert!(
        matches!(
            outcomes[0].kind,
            SelfKickOutcomeKind::AckLate { late_by_secs } if late_by_secs == late_by
        ),
        "the outcome must be AckLate carrying the overshoot: {:?}",
        outcomes[0].kind
    );
    assert_eq!(
        self_kick_watchdog_pass_at(&home, "claude-agent", Utc::now(), &|_| None)
            .expect("second reconcile pass")
            .len(),
        0,
        "the late ack is reconciled exactly once"
    );
    let _ = fs::remove_dir_all(home);
}
