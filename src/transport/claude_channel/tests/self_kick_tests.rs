use super::*;

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
    let home = home(tag);
    let locator = test_published_locator(&home, "claude-agent");
    let runtime = Arc::new(ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime"));
    let envelope = DeliveryEnvelope::self_kick(
        "claude-agent",
        locator,
        crate::agent::fresh_restart_self_kick_prompt(),
    );
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
        )
        .expect("successful turn watchdog"),
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
        self_kick_watchdog_pass_at(&home, "claude-agent", now).expect("watchdog"),
        1
    );
    assert_eq!(
        self_kick_watchdog_pass_at(&home, "claude-agent", now).expect("latch"),
        0
    );
    assert_eq!(
        store
            .latest(envelope.delivery_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::Ambiguous
    );
    let log = fs::read_to_string(home.join("event-log.jsonl")).expect("event log");
    assert_eq!(log.matches("claude_self_kick_ambiguous").count(), 1);
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
        workers.push(thread::spawn(move || {
            barrier.wait();
            let mut ambiguous = receipt;
            ambiguous.state = DeliveryState::Ambiguous;
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
        DeliveryState::Ambiguous
    );
    let _ = fs::remove_dir_all(home);
}
