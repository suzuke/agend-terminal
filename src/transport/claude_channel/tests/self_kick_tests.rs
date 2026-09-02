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
    // PR #3495: the alert is latched by the state CAS, but its durable notice
    // intent is replayed until the caller confirms the notice is enqueued —
    // that replay is the crash-gap guarantee, not a second alert.
    assert_eq!(
        self_kick_watchdog_pass_at(&home, "claude-agent", now, &|_| None)
            .expect("replay")
            .len(),
        1,
        "an unconfirmed notice intent is replayed"
    );
    assert!(clear_self_kick_notice(&home, "claude-agent", envelope.delivery_id).expect("clear"));
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

    // PR #3495 r3: the late ack does NOT supersede the overdue notice the
    // watchdog persisted and nobody has sent yet (the r2 behaviour, ruled a
    // violation: the operator was told to inspect the agent and never told
    // the alarm was raised at all). The pass replays the OVERDUE intent
    // first...
    let outcomes = self_kick_watchdog_pass_at(&home, "claude-agent", Utc::now(), &|_| None)
        .expect("overdue replay pass");
    assert_eq!(outcomes.len(), 1, "one outcome: {outcomes:?}");
    assert!(
        matches!(outcomes[0].kind, SelfKickOutcomeKind::AckOverdue { .. }),
        "the unsent overdue alert comes first: {:?}",
        outcomes[0].kind
    );
    assert!(
        clear_self_kick_notice(&home, "claude-agent", envelope.delivery_id).expect("clear overdue"),
        "the caller clears it once that notice is durably enqueued"
    );

    // ...and only then derives the distinct resolving notice from the
    // `late_ack_secs` the same ack stamped.
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
    // PR #3495: the pass MOVES the marker into a durable notice intent rather
    // than consuming it, so the outcome is replayed until the caller confirms
    // the notice is enqueued. Until then a repeat pass re-offers the SAME
    // outcome (that is the crash-gap retry), and only the clear ends it.
    assert_eq!(
        self_kick_watchdog_pass_at(&home, "claude-agent", Utc::now(), &|_| None)
            .expect("replay pass")
            .len(),
        1,
        "an unconfirmed notice intent is replayed, not dropped"
    );
    assert!(
        clear_self_kick_notice(&home, "claude-agent", envelope.delivery_id).expect("clear"),
        "the caller clears the intent once the notice is durably enqueued"
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

/// PR #3495 (d): two scanners that read the SAME durable snapshot must not
/// both reconcile it. The late-ack transition PRESERVES `TurnStarted`, so a
/// state-only compare-and-append is not a linearization point for it: both
/// scanners pass it — sequentially, under the store's own file lock — and the
/// operator gets the resolving notice twice. The marker-aware CAS makes the
/// scanner holding the stale marker lose.
#[test]
fn concurrent_stale_snapshot_reconciles_once() {
    let (home, runtime, envelope, _chat_id) = self_kick_fixture("stale-snapshot");
    let store = ReceiptStore::for_instance(&home, "claude-agent").expect("store");
    let mut old = store
        .latest(envelope.delivery_id)
        .expect("latest")
        .expect("accepted");
    old.recorded_at = (Utc::now()
        - chrono::Duration::seconds(SELF_KICK_ACK_WINDOW.as_secs() as i64 + 12))
    .to_rfc3339();
    assert!(store
        .record_if_latest_state(envelope.delivery_id, DeliveryState::ProtocolAccepted, old)
        .expect("backdate"));

    // The overdue half: the state CHANGES, so a stale scanner already loses
    // there — pinned below — but the intent must be durable before the notice.
    let overdue_snapshot = store
        .latest(envelope.delivery_id)
        .expect("latest")
        .expect("receipt");
    assert_eq!(
        self_kick_watchdog_pass_at(&home, "claude-agent", Utc::now(), &|_| None)
            .expect("watchdog")
            .len(),
        1
    );
    assert_eq!(
        store
            .latest(envelope.delivery_id)
            .expect("latest")
            .expect("receipt")
            .notice_pending
            .map(|notice| notice.kind),
        Some(crate::transport::PendingNotice::ACK_OVERDUE.to_string()),
        "the overdue notice intent must be persisted by the SAME CAS that moves the state"
    );
    let mut stale_overdue = overdue_snapshot.clone();
    stale_overdue.state = DeliveryState::AckOverdue;
    stale_overdue.notice_pending = Some(crate::transport::PendingNotice::new(
        crate::transport::PendingNotice::ACK_OVERDUE,
        None,
    ));
    assert!(
        !store
            .record_if_marker(
                envelope.delivery_id,
                DeliveryState::ProtocolAccepted,
                overdue_snapshot.late_ack_secs,
                overdue_snapshot.notice_pending.as_ref(),
                stale_overdue,
            )
            .expect("stale overdue CAS"),
        "a second scanner holding the pre-transition snapshot must lose"
    );
    assert!(
        clear_self_kick_notice(&home, "claude-agent", envelope.delivery_id).expect("clear overdue")
    );

    // The late-ack half: the state is PRESERVED, which is where a state-only
    // predicate admits the duplicate.
    let ack = json!({
        "jsonrpc":"2.0", "id":1, "method":"tools/call",
        "params":{"name":"ack_start","arguments":{"delivery_id":envelope.delivery_id}}
    });
    assert_eq!(
        mcp_message(ack, &runtime).expect("late start response")["result"]["structuredContent"]
            ["state"],
        "TurnStarted"
    );
    let snapshot_a = store
        .latest(envelope.delivery_id)
        .expect("latest")
        .expect("receipt");
    let late_by = snapshot_a.late_ack_secs.expect("late_ack_secs");
    // Scanner B reads the identical snapshot before A commits anything.
    let snapshot_b = snapshot_a.clone();

    // A reconciles in full: intent CAS, (the caller's enqueue), clear.
    assert_eq!(
        self_kick_watchdog_pass_at(&home, "claude-agent", Utc::now(), &|_| None)
            .expect("scanner A")
            .len(),
        1,
        "scanner A reconciles the late ack"
    );
    assert!(
        clear_self_kick_notice(&home, "claude-agent", envelope.delivery_id).expect("clear late"),
        "and clears the intent after its notice is durable"
    );

    // B now attempts the same transition from its stale snapshot.
    let mut reconciled = snapshot_b.clone();
    reconciled.late_ack_secs = None;
    reconciled.notice_pending = Some(crate::transport::PendingNotice::new(
        crate::transport::PendingNotice::LATE_ACK,
        Some(late_by),
    ));
    assert!(
        !store
            .record_if_marker(
                envelope.delivery_id,
                DeliveryState::TurnStarted,
                snapshot_b.late_ack_secs,
                snapshot_b.notice_pending.as_ref(),
                reconciled,
            )
            .expect("stale late-ack CAS"),
        "the scanner holding the stale reconciliation marker must lose its CAS"
    );
    assert_eq!(
        self_kick_watchdog_pass_at(&home, "claude-agent", Utc::now(), &|_| None)
            .expect("final pass")
            .len(),
        0,
        "and exactly one reconciliation happened overall"
    );
    let _ = fs::remove_dir_all(home);
}

/// PR #3495 r3 (a): the reviewer's repro. The watchdog persists
/// `state=AckOverdue` together with the durable INTENT to notify; a truthful
/// but LATE `ack_start` that lands before the per-tick caller enqueues that
/// notice must NOT destroy the intent. The overdue alert is owed to the
/// operator whatever happens afterwards — losing it here (a crash between the
/// transition and the enqueue) makes it underivable from the durable log,
/// which can then only produce the later, distinct late-ack notice.
#[test]
fn late_ack_preserves_pending_overdue_notice() {
    let (home, runtime, envelope, _chat_id) = self_kick_fixture("late-ack-preserve-intent");
    let store = ReceiptStore::for_instance(&home, "claude-agent").expect("store");
    let mut old = store
        .latest(envelope.delivery_id)
        .expect("latest")
        .expect("accepted");
    old.recorded_at = (Utc::now()
        - chrono::Duration::seconds(SELF_KICK_ACK_WINDOW.as_secs() as i64 + 7))
    .to_rfc3339();
    assert!(store
        .record_if_latest_state(envelope.delivery_id, DeliveryState::ProtocolAccepted, old)
        .expect("backdate"));
    assert_eq!(
        self_kick_watchdog_pass_at(&home, "claude-agent", Utc::now(), &|_| None)
            .expect("watchdog")
            .len(),
        1,
        "the watchdog persists the overdue intent"
    );
    assert_eq!(
        store
            .latest(envelope.delivery_id)
            .expect("latest")
            .expect("receipt")
            .notice_pending
            .map(|notice| notice.kind),
        Some(crate::transport::PendingNotice::ACK_OVERDUE.to_string()),
        "precondition: the intent is durable and UNSENT"
    );

    // The late ack arrives before the per-tick pass enqueues anything.
    let ack = json!({
        "jsonrpc":"2.0", "id":1, "method":"tools/call",
        "params":{"name":"ack_start","arguments":{"delivery_id":envelope.delivery_id}}
    });
    assert_eq!(
        mcp_message(ack, &runtime).expect("late start response")["result"]["structuredContent"]
            ["state"],
        "TurnStarted"
    );

    let latest = store
        .latest(envelope.delivery_id)
        .expect("latest")
        .expect("receipt");
    assert_eq!(
        latest.state,
        DeliveryState::TurnStarted,
        "the truthful ack still advances the state"
    );
    assert_eq!(
        latest
            .notice_pending
            .as_ref()
            .map(|notice| notice.kind.as_str()),
        Some(crate::transport::PendingNotice::ACK_OVERDUE),
        "and it must CARRY the unsent overdue intent forward: {latest:?}"
    );
    assert!(
        latest.late_ack_secs.is_some(),
        "while still stamping the late-ack overshoot for the second notice: {latest:?}"
    );
    let _ = fs::remove_dir_all(home);
}

/// PR #3495 r3 (d): a transition computed from a STALE `AckOverdue` snapshot
/// must not reintroduce an intent the per-tick pass has already cleared —
/// that would re-deliver the overdue notice under a new key-free window. The
/// marker-aware CAS makes the stale attempt lose; the bounded retry then
/// re-reads and commits against the fresh row.
#[test]
fn stale_ack_transition_cannot_reintroduce_cleared_intent() {
    let (home, _runtime, envelope, _chat_id) = self_kick_fixture("stale-ack-reintroduce");
    let store = ReceiptStore::for_instance(&home, "claude-agent").expect("store");
    let mut old = store
        .latest(envelope.delivery_id)
        .expect("latest")
        .expect("accepted");
    old.recorded_at = (Utc::now()
        - chrono::Duration::seconds(SELF_KICK_ACK_WINDOW.as_secs() as i64 + 5))
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
    // The snapshot the ack's transition is computed from: AckOverdue, intent
    // still pending.
    let stale = store
        .latest(envelope.delivery_id)
        .expect("latest")
        .expect("receipt");
    assert!(stale.notice_pending.is_some());

    // Meanwhile the per-tick pass enqueues the overdue notice and clears it.
    assert!(
        clear_self_kick_notice(&home, "claude-agent", envelope.delivery_id).expect("clear overdue")
    );

    // The ack now applies its transition from the stale snapshot.
    let started = crate::transport::claude_channel::record_self_kick_turn_started(
        &home,
        "claude-agent",
        "self-kick-session",
        &store,
        &envelope,
        stale,
        Utc::now(),
    )
    .expect("late ack from a stale snapshot");
    assert_eq!(started.state, DeliveryState::TurnStarted);
    assert!(
        started.notice_pending.is_none(),
        "the cleared intent must NOT be resurrected: {started:?}"
    );
    assert!(
        started.late_ack_secs.is_some(),
        "and the retry against the fresh row still stamps the overshoot: {started:?}"
    );
    let durable = store
        .latest(envelope.delivery_id)
        .expect("latest")
        .expect("receipt");
    assert!(durable.notice_pending.is_none(), "durably too: {durable:?}");

    // Exactly one further outcome — the late-ack reconciliation — and no
    // second overdue notice.
    let outcomes =
        self_kick_watchdog_pass_at(&home, "claude-agent", Utc::now(), &|_| None).expect("pass");
    assert_eq!(outcomes.len(), 1, "one outcome: {outcomes:?}");
    assert!(
        matches!(outcomes[0].kind, SelfKickOutcomeKind::AckLate { .. }),
        "and it is the late-ack one: {:?}",
        outcomes[0].kind
    );
    let _ = fs::remove_dir_all(home);
}

/// PR #3495 r3, the SIBLING path of (a): `ack_complete` rewrites the receipt
/// too. The consumer normally calls it moments after `ack_start`, i.e. inside
/// the same gap, and `Completed` is TERMINAL — so a completion that dropped
/// the pending intent would put the owed notices permanently out of the
/// watchdog's reach.
#[test]
fn completion_preserves_pending_overdue_notice() {
    let (home, runtime, envelope, _chat_id) = self_kick_fixture("complete-preserve-intent");
    let store = ReceiptStore::for_instance(&home, "claude-agent").expect("store");
    let mut old = store
        .latest(envelope.delivery_id)
        .expect("latest")
        .expect("accepted");
    old.recorded_at = (Utc::now()
        - chrono::Duration::seconds(SELF_KICK_ACK_WINDOW.as_secs() as i64 + 6))
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
    for tool in ["ack_start", "ack_complete"] {
        let call = json!({
            "jsonrpc":"2.0", "id":1, "method":"tools/call",
            "params":{"name":tool,"arguments":{"delivery_id":envelope.delivery_id}}
        });
        assert!(
            mcp_message(call, &runtime).is_some(),
            "{tool} must be accepted"
        );
    }
    let latest = store
        .latest(envelope.delivery_id)
        .expect("latest")
        .expect("receipt");
    assert_eq!(latest.state, DeliveryState::Completed);
    assert_eq!(
        latest
            .notice_pending
            .as_ref()
            .map(|notice| notice.kind.as_str()),
        Some(crate::transport::PendingNotice::ACK_OVERDUE),
        "completion must carry the unsent overdue intent: {latest:?}"
    );
    assert!(
        latest.late_ack_secs.is_some(),
        "and the late-ack overshoot: {latest:?}"
    );

    // A terminal receipt that still owes notices stays visible to the scan.
    let outcomes = self_kick_watchdog_pass_at(&home, "claude-agent", Utc::now(), &|_| None)
        .expect("replay pass");
    assert_eq!(outcomes.len(), 1, "the overdue replay: {outcomes:?}");
    assert!(matches!(
        outcomes[0].kind,
        SelfKickOutcomeKind::AckOverdue { .. }
    ));
    assert!(clear_self_kick_notice(&home, "claude-agent", envelope.delivery_id).expect("clear"));
    let outcomes = self_kick_watchdog_pass_at(&home, "claude-agent", Utc::now(), &|_| None)
        .expect("late pass");
    assert_eq!(outcomes.len(), 1, "then the late-ack one: {outcomes:?}");
    assert!(matches!(
        outcomes[0].kind,
        SelfKickOutcomeKind::AckLate { .. }
    ));
    assert!(clear_self_kick_notice(&home, "claude-agent", envelope.delivery_id).expect("clear"));
    assert_eq!(
        self_kick_watchdog_pass_at(&home, "claude-agent", Utc::now(), &|_| None)
            .expect("final pass")
            .len(),
        0,
        "and nothing is owed after both notices"
    );
    let _ = fs::remove_dir_all(home);
}
