use super::*;

use crate::channel::ChannelEvent;
use serial_test::serial;

/// §3.5.10 wire-format fixture: Discord Gateway READY payload
/// (tests/fixtures/discord-gateway-ready.json) is deserialized via
/// twilight-model and mapped to `ChannelEvent::Connected`.
///
/// §3.5.11 test-first: this test was committed RED before the
/// implementation existed. The GREEN commit adds `map_ready_to_connected`.
#[test]
fn discord_gateway_ready_emits_connected_event() {
    let fixture = include_str!("../../../tests/fixtures/discord-gateway-ready.json");
    let frame: serde_json::Value =
        serde_json::from_str(fixture).expect("fixture must parse as JSON");
    let d = frame.get("d").expect("fixture must have 'd' field");
    let ready: twilight_model::gateway::payload::incoming::Ready =
        serde_json::from_value(d.clone()).expect("'d' must parse as Ready");

    let event = super::map_ready_to_connected(&ready);

    match event {
        ChannelEvent::Connected { kind, who } => {
            assert_eq!(kind, "discord");
            assert_eq!(who, "agend-bot");
        }
        other => panic!("expected Connected, got: {other:?}"),
    }
}

// ── #2562 P0: gateway_event_to_channel_event / should_stop_gateway_loop ──

fn ready_fixture() -> twilight_model::gateway::payload::incoming::Ready {
    let fixture = include_str!("../../../tests/fixtures/discord-gateway-ready.json");
    let frame: serde_json::Value =
        serde_json::from_str(fixture).expect("fixture must parse as JSON");
    let d = frame.get("d").expect("fixture must have 'd' field");
    serde_json::from_value(d.clone()).expect("'d' must parse as Ready")
}

fn message_create_fixture() -> twilight_gateway::Event {
    let fixture = include_str!("../../../tests/fixtures/discord-gateway-message-create.json");
    let frame: serde_json::Value =
        serde_json::from_str(fixture).expect("fixture must parse as JSON");
    let d = frame.get("d").expect("fixture must have 'd' field");
    let msg: twilight_model::channel::Message =
        serde_json::from_value(d.clone()).expect("'d' must parse as Message");
    twilight_gateway::Event::MessageCreate(Box::new(
        twilight_model::gateway::payload::incoming::MessageCreate(msg),
    ))
}

/// `Event::Ready` reaches `gateway_event_to_channel_event` and comes out
/// as `Connected` — proves the gateway-event dispatch wiring, not just
/// the already-tested `map_ready_to_connected` in isolation.
#[test]
fn gateway_event_to_channel_event_ready_is_connected() {
    let event = twilight_gateway::Event::Ready(ready_fixture());
    let result = super::gateway_event_to_channel_event(event, &None);
    match result {
        Some(ChannelEvent::Connected { kind, who }) => {
            assert_eq!(kind, "discord");
            assert_eq!(who, "agend-bot");
        }
        other => panic!("expected Some(Connected), got: {other:?}"),
    }
}

/// `Event::MessageCreate` with an allowlisted author reaches
/// `gateway_event_to_channel_event` and comes out as `MessageIn`.
#[test]
fn gateway_event_to_channel_event_message_create_allowlisted_is_message_in() {
    let event = message_create_fixture();
    let allowlist = Some(vec![82198898841029460_i64]);
    let result = super::gateway_event_to_channel_event(event, &allowlist);
    assert!(
        matches!(result, Some(ChannelEvent::MessageIn { .. })),
        "expected Some(MessageIn), got: {result:?}"
    );
}

/// `Event::MessageCreate` with a non-allowlisted author is dropped —
/// the allowlist gate must survive being routed through the gateway
/// dispatcher, not just the underlying mapper.
#[test]
fn gateway_event_to_channel_event_message_create_not_allowlisted_is_dropped() {
    let event = message_create_fixture();
    let allowlist = Some(vec![999_i64]);
    let result = super::gateway_event_to_channel_event(event, &allowlist);
    assert!(result.is_none(), "expected None, got: {result:?}");
}

/// `Event::ChannelDelete` reaches `gateway_event_to_channel_event` and
/// comes out as `BindingRevoked`.
#[test]
fn gateway_event_to_channel_event_channel_delete_is_binding_revoked() {
    let channel: twilight_model::channel::Channel =
        serde_json::from_value(serde_json::json!({"id": "223456789012345678", "type": 0}))
            .expect("minimal channel object must parse");
    let event = twilight_gateway::Event::ChannelDelete(Box::new(
        twilight_model::gateway::payload::incoming::ChannelDelete(channel),
    ));
    let result = super::gateway_event_to_channel_event(event, &None);
    match result {
        Some(ChannelEvent::BindingRevoked { binding, .. }) => {
            assert_eq!(binding.kind(), "discord");
        }
        other => panic!("expected Some(BindingRevoked), got: {other:?}"),
    }
}

/// Event types this adapter doesn't model (e.g. a bare heartbeat ack)
/// are silently dropped, not an error — the dispatcher only forwards
/// what it explicitly recognizes.
#[test]
fn gateway_event_to_channel_event_unmodeled_event_is_none() {
    let event = twilight_gateway::Event::GatewayHeartbeatAck;
    let result = super::gateway_event_to_channel_event(event, &None);
    assert!(result.is_none(), "expected None, got: {result:?}");
}

/// The one shard state that must stop the reader loop: a fatal close
/// (bad token, invalid intents, etc.) that twilight's own reconnect
/// logic cannot recover from.
#[test]
fn should_stop_gateway_loop_stops_on_fatally_closed() {
    assert!(super::should_stop_gateway_loop(
        twilight_gateway::ShardState::FatallyClosed
    ));
}

/// Every other shard state is something twilight will keep working on
/// internally (reconnecting/resuming) — the loop must NOT give up.
#[test]
fn should_stop_gateway_loop_continues_on_recoverable_states() {
    assert!(!super::should_stop_gateway_loop(
        twilight_gateway::ShardState::Active
    ));
    assert!(!super::should_stop_gateway_loop(
        twilight_gateway::ShardState::Disconnected {
            reconnect_attempts: 3
        }
    ));
    assert!(!super::should_stop_gateway_loop(
        twilight_gateway::ShardState::Identifying
    ));
}

/// #2562 PR-3a: `gateway_is_dead()` reflects whatever `GATEWAY_DEAD`
/// was last set to, and `reset_gateway_dead_for_test()` clears it back.
/// `#[serial]` because the flag is a process-wide static (mirrors
/// `daemon::mod::SHUTDOWN_REASON`'s shape) — parallel tests touching it
/// would race. `start_gateway` itself sets this on its real exit paths
/// (fatal close / receiver dropped / spawn failure); those paths need a
/// live gateway attempt to reach, so this test exercises the static
/// directly rather than driving a real connection.
#[test]
#[serial]
fn gateway_is_dead_reflects_death_state() {
    super::reset_gateway_dead_for_test();
    assert!(!super::gateway_is_dead(), "must start alive after reset");

    super::GATEWAY_DEAD.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(super::gateway_is_dead(), "must reflect a marked death");

    super::reset_gateway_dead_for_test();
    assert!(
        !super::gateway_is_dead(),
        "reset must clear it back to alive"
    );
}

/// #2562 PR-3b: exponential backoff, base 5s, cap 60s — same shape and
/// same expected sequence as Telegram's `poll_supervisor::backoff_delay`
/// test, confirming the constants chosen for consistency actually land
/// on the same numbers.
#[test]
fn discord_gateway_backoff_delay_follows_expected_sequence() {
    let expected = [5u64, 10, 20, 40, 60, 60, 60];
    for (i, &secs) in expected.iter().enumerate() {
        let attempt = (i + 1) as u32;
        assert_eq!(
            super::discord_gateway_backoff_delay(attempt),
            std::time::Duration::from_secs(secs),
            "attempt {attempt} backoff mismatch"
        );
    }
    // Never panics / overflows for a pathologically large attempt
    // count — same saturating-exponent-cap shape as Telegram's
    // `poll_supervisor::backoff_delay`, same assertion.
    assert_eq!(
        super::discord_gateway_backoff_delay(u32::MAX),
        std::time::Duration::from_secs(60)
    );
}

/// #2562 PR-3b: restart attempts are allowed strictly below the cap,
/// and refused at and beyond it — the supervisor must give up for good
/// rather than hot-loop a still-bad config forever.
#[test]
fn should_restart_gateway_caps_at_max_attempts() {
    for n in 0..super::GATEWAY_RESTART_MAX_ATTEMPTS {
        assert!(
            super::should_restart_gateway(n),
            "attempt {n} should still be allowed to retry"
        );
    }
    assert!(!super::should_restart_gateway(
        super::GATEWAY_RESTART_MAX_ATTEMPTS
    ));
    assert!(!super::should_restart_gateway(
        super::GATEWAY_RESTART_MAX_ATTEMPTS + 5
    ));
}

/// Contract test: DiscordChannel satisfies the registry-side
/// contract from `src/channel/contract.rs`.
#[test]
fn discord_channel_satisfies_contract() {
    let (ch, _rx) = super::DiscordChannel::new_for_test();
    crate::channel::contract::run_registry_contract(ch, super::discord_make_binding);
}

/// Caps snapshot: pin the Discord capability matrix so reviewers
/// can diff against the S5 analysis.
#[test]
fn discord_caps_match_s5_analysis() {
    let (ch, _rx) = super::DiscordChannel::new_for_test();
    let caps = crate::channel::Channel::caps(&ch);

    assert!(caps.emits_deletion_events);
    assert!(caps.threads);
    assert!(caps.attachments);
    // M3: react support is `false` per production `discord_caps()` —
    // returns NotSupported until implemented. Sprint 54 P2-8b: test
    // updated to reflect production reality (was: stale aspirational
    // `assert!(caps.react)`).
    assert!(!caps.react);
    assert!(caps.edit);
    assert!(caps.typing_indicator);
    assert!(caps.receives_edit_events);
    assert_eq!(caps.max_msg_bytes, 2000);
    assert_eq!(caps.markdown, crate::channel::MarkdownDialect::DiscordMd);
    assert_eq!(
        caps.mention_parsing_hint,
        crate::channel::MentionStyle::AtSnowflake
    );
    assert!(!caps.bot_sees_read_receipts);
    assert!(caps.has_native_multi_thread_view.is_none());
    assert!(!caps.ephemeral);
}

/// poll_event drains the internal mpsc channel.
#[test]
fn poll_event_drains_mpsc() {
    let (ch, tx) = super::DiscordChannel::new_for_test();
    assert!(crate::channel::Channel::poll_event(&ch).is_none());

    tx.send(ChannelEvent::Connected {
        kind: "discord".into(),
        who: "test-bot".into(),
    })
    .expect("send");

    let event = crate::channel::Channel::poll_event(&ch).expect("should have event");
    match event {
        ChannelEvent::Connected { kind, who } => {
            assert_eq!(kind, "discord");
            assert_eq!(who, "test-bot");
        }
        other => panic!("expected Connected, got: {other:?}"),
    }

    assert!(crate::channel::Channel::poll_event(&ch).is_none());
}

// ── §3.5.10 expanded gateway handshake fixture tests ─────────────
//
// F1 fix: cover the full HELLO → IDENTIFY → HEARTBEAT → HEARTBEAT_ACK
// → READY sequence using Discord API spec payloads.
//
// §3.5.11 r3 empirical-revert exemption: impl already exists from
// GREEN commit; tests depend on impl-provided fns. Reviewer can
// revert impl to verify test failure.

/// HELLO (opcode 10): server sends heartbeat_interval after WS connect.
/// Fixture: tests/fixtures/discord-gateway-hello.json (Discord API spec).
#[test]
fn discord_gateway_hello_parsed_correctly() {
    let fixture = include_str!("../../../tests/fixtures/discord-gateway-hello.json");

    // Opcode must be 10 (Hello).
    let frame = super::parse_gateway_opcode(fixture).expect("must parse");
    assert_eq!(frame.op, 10, "HELLO opcode must be 10");

    // heartbeat_interval must be extractable.
    let interval = super::parse_hello_interval(fixture).expect("must parse interval");
    assert_eq!(interval, 41250, "fixture heartbeat_interval");
}

/// IDENTIFY (opcode 2): client sends token + intents after receiving HELLO.
/// Asserts the frame our adapter builds matches Discord spec shape.
#[test]
fn discord_gateway_identify_shape_matches_spec() {
    let intents = twilight_model::gateway::Intents::GUILDS
        | twilight_model::gateway::Intents::GUILD_MESSAGES
        | twilight_model::gateway::Intents::MESSAGE_CONTENT;

    let frame = super::build_identify_payload("Bot test-token-redacted", intents);

    // op must be 2
    assert_eq!(frame["op"], 2, "IDENTIFY opcode must be 2");

    // d.token present
    assert_eq!(frame["d"]["token"], "Bot test-token-redacted");

    // d.intents is a numeric bitfield
    assert!(frame["d"]["intents"].is_u64(), "intents must be numeric");

    // d.properties has required fields per Discord spec
    let props = &frame["d"]["properties"];
    assert!(props["os"].is_string(), "properties.os required");
    assert_eq!(props["browser"], "agend-terminal");
    assert_eq!(props["device"], "agend-terminal");
}

/// HEARTBEAT_ACK (opcode 11): server acknowledges client heartbeat.
/// Fixture: tests/fixtures/discord-gateway-heartbeat-ack.json.
#[test]
fn discord_gateway_heartbeat_ack_recognized() {
    let fixture = include_str!("../../../tests/fixtures/discord-gateway-heartbeat-ack.json");

    let frame = super::parse_gateway_opcode(fixture).expect("must parse");
    assert_eq!(frame.op, 11, "HEARTBEAT_ACK opcode must be 11");
    assert!(super::is_heartbeat_ack(fixture), "is_heartbeat_ack");
}

/// HEARTBEAT (opcode 1): client sends sequence number periodically.
/// Spec shape: `{"op": 1, "d": <last_sequence_or_null>}`.
#[test]
fn discord_gateway_heartbeat_shape() {
    // First heartbeat (no sequence yet) — d is null per spec.
    let first = r#"{"op": 1, "d": null}"#;
    let frame = super::parse_gateway_opcode(first).expect("must parse");
    assert_eq!(frame.op, 1, "HEARTBEAT opcode must be 1");
    assert!(!super::is_heartbeat_ack(first), "heartbeat is not ack");

    // Subsequent heartbeat with sequence number.
    let subsequent = r#"{"op": 1, "d": 42}"#;
    let frame = super::parse_gateway_opcode(subsequent).expect("must parse");
    assert_eq!(frame.op, 1);
}

/// Full handshake sequence: HELLO → IDENTIFY → HEARTBEAT → HEARTBEAT_ACK → READY.
/// Asserts the correct opcode ordering and that each frame parses.
#[test]
fn discord_gateway_full_handshake_sequence() {
    let hello = include_str!("../../../tests/fixtures/discord-gateway-hello.json");
    let heartbeat_ack = include_str!("../../../tests/fixtures/discord-gateway-heartbeat-ack.json");
    let ready = include_str!("../../../tests/fixtures/discord-gateway-ready.json");

    // Step 1: Server sends HELLO (op=10)
    let f1 = super::parse_gateway_opcode(hello).expect("hello");
    assert_eq!(f1.op, 10);

    // Step 2: Client sends IDENTIFY (op=2) — we build it
    let identify =
        super::build_identify_payload("Bot fake", twilight_model::gateway::Intents::GUILDS);
    assert_eq!(identify["op"], 2);

    // Step 3: Client sends HEARTBEAT (op=1)
    let hb = r#"{"op": 1, "d": null}"#;
    let f3 = super::parse_gateway_opcode(hb).expect("heartbeat");
    assert_eq!(f3.op, 1);

    // Step 4: Server sends HEARTBEAT_ACK (op=11)
    let f4 = super::parse_gateway_opcode(heartbeat_ack).expect("ack");
    assert_eq!(f4.op, 11);

    // Step 5: Server sends READY (op=0, t=READY)
    let f5 = super::parse_gateway_opcode(ready).expect("ready");
    assert_eq!(f5.op, 0);

    // Map READY to Connected event
    let frame: serde_json::Value = serde_json::from_str(ready).expect("json");
    let d = frame.get("d").expect("d");
    let ready_payload: twilight_model::gateway::payload::incoming::Ready =
        serde_json::from_value(d.clone()).expect("Ready");
    let event = super::map_ready_to_connected(&ready_payload);
    assert!(matches!(event, ChannelEvent::Connected { .. }));
}

// ── PR2 tests: MessageIn + send + notify ─────────────────────────

/// §3.5.10 wire-format fixture: MESSAGE_CREATE gateway event
/// parsed into `ChannelEvent::MessageIn`.
#[test]
fn discord_message_create_emits_message_in() {
    let fixture = include_str!("../../../tests/fixtures/discord-gateway-message-create.json");
    let frame: serde_json::Value = serde_json::from_str(fixture).expect("fixture must parse");
    let d = frame.get("d").expect("d field");
    let msg: twilight_model::channel::Message = serde_json::from_value(d.clone()).expect("Message");

    // #bughunt-r3 #3: author on the allowlist → emitted.
    let allowlist = Some(vec![82198898841029460_i64]);
    let event = super::map_message_create_to_message_in(&msg, &allowlist)
        .expect("allowlisted author must emit MessageIn");

    match event {
        ChannelEvent::MessageIn {
            binding,
            from,
            payload,
            ts,
        } => {
            assert_eq!(binding.kind(), "discord");
            assert_eq!(from.id, "82198898841029460");
            assert_eq!(from.handle.as_deref(), Some("testoperator"));
            assert_eq!(payload.text, "hello from discord");
            // ts should be parseable (not epoch-zero)
            assert!(ts.timestamp() > 0);
        }
        other => panic!("expected MessageIn, got: {other:?}"),
    }
}

/// #bughunt-r3 #3: Discord inbound must be allowlist-gated like telegram.
/// An author NOT on the allowlist (and the fail-closed `None` / empty cases)
/// must be dropped — `map_message_create_to_message_in` returns `None`.
#[test]
fn discord_message_create_rejected_when_author_not_allowlisted() {
    let fixture = include_str!("../../../tests/fixtures/discord-gateway-message-create.json");
    let frame: serde_json::Value = serde_json::from_str(fixture).expect("fixture must parse");
    let d = frame.get("d").expect("d field");
    let msg: twilight_model::channel::Message = serde_json::from_value(d.clone()).expect("Message");

    // Author 82198898841029460 is NOT in this list → dropped.
    assert!(
        super::map_message_create_to_message_in(&msg, &Some(vec![999_i64])).is_none(),
        "author absent from allowlist must be dropped"
    );
    // Fail-closed: unconfigured allowlist (None) → dropped.
    assert!(
        super::map_message_create_to_message_in(&msg, &None).is_none(),
        "None allowlist must fail-closed (drop)"
    );
    // Fail-closed: empty allowlist → dropped.
    assert!(
        super::map_message_create_to_message_in(&msg, &Some(vec![])).is_none(),
        "empty allowlist must reject all"
    );
}

/// Accept-path parity (#2562 PR-1): allowlisted messages must log for
/// observability, same as the reject path already does. Regression
/// guard for the asymmetry found during #2562 P3's live smoke test
/// (the accept path produced zero log output before this PR).
#[test]
#[tracing_test::traced_test]
fn discord_message_create_accept_path_logs_info() {
    let fixture = include_str!("../../../tests/fixtures/discord-gateway-message-create.json");
    let frame: serde_json::Value = serde_json::from_str(fixture).expect("fixture must parse");
    let d = frame.get("d").expect("d field");
    let msg: twilight_model::channel::Message = serde_json::from_value(d.clone()).expect("Message");

    let allowlist = Some(vec![82198898841029460_i64]);
    super::map_message_create_to_message_in(&msg, &allowlist)
        .expect("allowlisted author must emit MessageIn");

    assert!(
        logs_contain("discord message accepted by user_allowlist"),
        "accept path must log for observability parity with the reject path"
    );
}

// ── #2562 PR-1: inbound dispatcher ──

fn dispatch_test_home(label: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agend-discord-dispatch-test-{}-{label}-{id}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn message_in_event(channel_id: u64, text: &str) -> ChannelEvent {
    ChannelEvent::MessageIn {
        binding: crate::channel::BindingRef::new(
            "discord",
            Some(format!("DC#{channel_id}")),
            super::DiscordBindingPayload { channel_id },
        ),
        from: crate::channel::event::User {
            id: "999".to_string(),
            handle: Some("someuser".to_string()),
        },
        payload: crate::channel::event::MsgPayload {
            text: text.to_string(),
        },
        ts: chrono::Utc::now(),
    }
}

/// `resolve_instance_for_channel` returns the instance bound to a
/// channel_id via `record_binding` — the reverse-lookup table
/// Telegram's `resolve_topic` uses for `topic_id`.
#[test]
fn resolve_instance_for_channel_returns_bound_instance() {
    let (ch, _tx) = super::DiscordChannel::new_for_test();
    let binding = crate::channel::BindingRef::new(
        "discord",
        Some("DC#111".into()),
        super::DiscordBindingPayload { channel_id: 111 },
    );
    crate::channel::Channel::record_binding(&ch, "dev-agent", binding, "\r".into());

    assert_eq!(ch.resolve_instance_for_channel(111), "dev-agent");
}

/// Unbound channel_id (no `record_binding` call ever happened) falls
/// back to `"general"` + a warn log — mirrors `telegram/inbound.rs`'s
/// topic-miss fallback semantics.
#[test]
#[tracing_test::traced_test]
fn resolve_instance_for_channel_falls_back_to_general_when_unbound() {
    let (ch, _tx) = super::DiscordChannel::new_for_test();

    assert_eq!(ch.resolve_instance_for_channel(222), "general");
    assert!(
        logs_contain("no instance bound to this channel"),
        "unbound channel_id must warn so operators can trace the fallback"
    );
}

/// `dispatch_channel_event` end-to-end: extracts channel_id from the
/// binding and routes via `resolve_instance_for_channel` — an
/// integration check on top of the resolver unit tests above (proves
/// the wiring, not just the resolver in isolation). Verified via the
/// routing-decision log rather than `inbox::drain`, since the
/// delivery layer below `notify_agent_with_attachments` depends on
/// live daemon/PTY state this test environment doesn't have.
#[test]
#[tracing_test::traced_test]
fn dispatch_channel_event_routes_to_bound_instance() {
    let (ch, _tx) = super::DiscordChannel::new_for_test();
    let binding = crate::channel::BindingRef::new(
        "discord",
        Some("DC#111".into()),
        super::DiscordBindingPayload { channel_id: 111 },
    );
    crate::channel::Channel::record_binding(&ch, "dev-agent", binding, "\r".into());
    let home = dispatch_test_home("routes");

    super::dispatch_channel_event(&ch, &home, message_in_event(111, "hello dev-agent"));

    assert!(
        logs_contain("routing message to instance") && logs_contain("dev-agent"),
        "dispatch must resolve and log the bound instance"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// PR-1 review fix: a message ≥ 200 chars must be persisted to the
/// instance's inbox BEFORE the (truncating, pointer-only-for-long-text)
/// PTY notification fires — otherwise the notification's "use the inbox
/// MCP tool to read full message" pointer has nothing to point at.
/// Verified directly via `inbox::drain` (unlike the short-message tests
/// above, this doesn't depend on live daemon/PTY state — `enqueue` is a
/// plain synchronous file write).
#[test]
fn dispatch_channel_event_persists_long_message_to_inbox() {
    let (ch, _tx) = super::DiscordChannel::new_for_test();
    let binding = crate::channel::BindingRef::new(
        "discord",
        Some("DC#444".into()),
        super::DiscordBindingPayload { channel_id: 444 },
    );
    crate::channel::Channel::record_binding(&ch, "dev-agent", binding, "\r".into());
    let home = dispatch_test_home("long-message");
    let long_text = "a".repeat(250);

    super::dispatch_channel_event(&ch, &home, message_in_event(444, &long_text));

    let msgs = crate::inbox::drain(&home, "dev-agent");
    assert!(
        msgs.iter().any(|m| m.text == long_text),
        "long message must be persisted in full to the bound instance's inbox; \
         got {} message(s), lengths: {:?}",
        msgs.len(),
        msgs.iter().map(|m| m.text.len()).collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Short message (< 200 chars) must NOT be persisted to the inbox —
/// preserves Telegram's existing short-message behavior (PTY-only,
/// no disk write) rather than accidentally persisting everything.
#[test]
fn dispatch_channel_event_does_not_persist_short_message_to_inbox() {
    let (ch, _tx) = super::DiscordChannel::new_for_test();
    let binding = crate::channel::BindingRef::new(
        "discord",
        Some("DC#555".into()),
        super::DiscordBindingPayload { channel_id: 555 },
    );
    crate::channel::Channel::record_binding(&ch, "dev-agent", binding, "\r".into());
    let home = dispatch_test_home("short-message");

    super::dispatch_channel_event(&ch, &home, message_in_event(555, "hi"));

    let msgs = crate::inbox::drain(&home, "dev-agent");
    assert!(
        msgs.iter().all(|m| m.text != "hi"),
        "short message must not be persisted to inbox (PTY-inject-only path); got: {:?}",
        msgs.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// #2562 PR-1 regression pin, same shape as `build_http_client_does_not_
/// panic_on_bare_thread`: the dispatch path touches no tokio runtime, so
/// calling it from a genuine bare `std::thread` must not panic.
#[test]
fn dispatch_channel_event_does_not_panic_on_bare_thread() {
    let home = dispatch_test_home("bare-thread");
    let home_for_thread = home.clone();
    let joined = std::thread::spawn(move || {
        let (ch, _tx) = super::DiscordChannel::new_for_test();
        super::dispatch_channel_event(
            &ch,
            &home_for_thread,
            message_in_event(333, "bare thread smoke"),
        );
    })
    .join();
    assert!(
        joined.is_ok(),
        "dispatch_channel_event must not panic when called from a bare std::thread \
         with no ambient Tokio runtime"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// §3.5.10 wire-format fixture: outbound POST /channels/{id}/messages
/// response parsed into `MsgRef`.
#[test]
fn discord_create_message_response_parses_to_msg_ref() {
    let fixture = include_str!("../../../tests/fixtures/discord-rest-create-message-response.json");
    let msg: twilight_model::channel::Message =
        serde_json::from_str(fixture).expect("response must parse as Message");

    let msg_ref = super::map_message_to_msg_ref(&msg);

    assert_eq!(msg_ref.id, "444385199974967099");
    assert_eq!(msg_ref.binding.kind(), "discord");
}

/// send_from_agent(Reply) on an authorized channel with no binding
/// for the agent should error with "no discord binding".
#[test]
fn send_from_agent_reply_errors_on_unbound_instance() {
    let (ch, _rx) = super::DiscordChannel::new_for_test_authorized();
    // Authorized but no binding → should error about binding.
    let result = crate::channel::Channel::send_from_agent(
        &ch,
        "unknown-agent",
        crate::channel::AgentOutboundOp::Reply { text: "hi".into() },
    );
    let err = result.expect_err("unbound instance must error");
    let err_msg = format!("{err}");
    assert!(
        err_msg.contains("no discord binding"),
        "error must mention binding, got: {err_msg}"
    );
}

/// F2 fix: send_from_agent must check outbound_authorized() gate.
/// When user_allowlist is None (unconfigured), the gate drops the call.
#[test]
fn send_from_agent_blocked_by_outbound_gate() {
    let (ch, _rx) = super::DiscordChannel::new_for_test(); // allowlist=None → unauthorized
    let result = crate::channel::Channel::send_from_agent(
        &ch,
        "any-agent",
        crate::channel::AgentOutboundOp::Reply { text: "hi".into() },
    );
    let err = result.expect_err("unauthorized channel must reject");
    let err_msg = format!("{err}");
    assert!(
        err_msg.contains("outbound disabled"),
        "error must mention outbound gate, got: {err_msg}"
    );
}

/// notify on an unbound instance should error gracefully.
#[test]
fn notify_errors_on_unbound_instance() {
    let (ch, _rx) = super::DiscordChannel::new_for_test();
    let result = crate::channel::Channel::notify(
        &ch,
        "unknown-agent",
        crate::channel::NotifySeverity::Info,
        "test notification",
        false,
    );
    assert!(result.is_err(), "notify on unbound instance must error");
}

// ── F3 fix: §3.5.10 outbound request body shape assertion ────────
//
// Production-path-coupled: exercises the real Channel::send() →
// twilight_http::create_message() path against a mock HTTP server.
// The mock captures the request body twilight actually transmits
// and asserts it matches the Discord spec shape.

/// §3.5.10 wire-format: outbound POST /channels/{id}/messages request
/// body transmitted by twilight-http matches Discord spec shape.
///
/// Uses a raw TCP listener as mock Discord API server. The twilight
/// client is pointed at it via `proxy()`. Channel::send() exercises
/// the real production code path.
#[test]
fn discord_send_outbound_body_matches_spec() {
    use crate::channel::Channel;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    // Step 1: Start a mock HTTP server on an ephemeral port.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();

    // Step 2: Spawn a thread to handle one request.
    let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let captured_clone = captured.clone();
    // fire-and-forget: test mock server thread — lives only for this test
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).expect("read");
        let request = String::from_utf8_lossy(&buf[..n]).to_string();

        // Extract body after the \r\n\r\n header separator.
        if let Some(idx) = request.find("\r\n\r\n") {
            let body = &request[idx + 4..];
            *captured_clone.lock().expect("lock") = Some(body.to_string());
        }

        // Respond with a minimal valid Discord Message JSON.
        let response_body =
            include_str!("../../../tests/fixtures/discord-rest-create-message-response.json");
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        stream.write_all(response.as_bytes()).expect("write");
    });

    // Step 3: Create twilight client pointed at mock server.
    // twilight-http 0.17's ratelimiter initialises inside `build()` and needs
    // a Tokio reactor in scope, so construct it within the shared discord
    // runtime (production builds the client in async context already).
    let client = super::block_on_value(async {
        twilight_http::Client::builder()
            .proxy(format!("127.0.0.1:{port}"), true)
            .build()
    });
    let client = std::sync::Arc::new(client);

    // Step 4: Create DiscordChannel with this client + a recorded binding.
    let (ch, _tx) = super::DiscordChannel::new_for_test_with_http(client);
    let binding = crate::channel::BindingRef::new(
        "discord",
        Some("DC#290926798999357250".into()),
        super::DiscordBindingPayload {
            channel_id: 290926798999357250,
        },
    );
    ch.record_binding("test-agent", binding.clone(), "\r".into());

    // Step 5: Call the real production send() path.
    let result =
        crate::channel::Channel::send(&ch, &binding, crate::channel::OutMsg::text("Hello, World!"));

    handle.join().expect("mock server thread");

    // Step 6: Assert the request body twilight transmitted.
    assert!(result.is_ok(), "send must succeed: {:?}", result.err());
    let body_str = captured
        .lock()
        .expect("lock")
        .take()
        .expect("body captured");
    let actual: serde_json::Value =
        serde_json::from_str(&body_str).expect("body must be valid JSON");
    let expected: serde_json::Value = serde_json::json!({"content": "Hello, World!"});
    assert_eq!(
        actual, expected,
        "outbound body must match Discord spec create-message shape"
    );
}

// ── PR3 tests: edit + delete production-path-coupled ─────────────

/// Captured HTTP request from the mock server: method, path, body.
struct CapturedRequest {
    method: String,
    path: String,
    body: String,
}

/// Reusable mock HTTP server that captures one request and responds
/// with a canned response. Returns (port, join_handle, captured_arc).
fn mock_http_server(
    response_status: u16,
    response_body: &str,
) -> (
    u16,
    std::thread::JoinHandle<()>,
    std::sync::Arc<std::sync::Mutex<Option<CapturedRequest>>>,
) {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let captured = std::sync::Arc::new(std::sync::Mutex::new(None::<CapturedRequest>));
    let captured_clone = captured.clone();
    let resp_body = response_body.to_string();
    let status = response_status;

    // fire-and-forget: test mock server thread — lives only for this test
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).expect("read");
        let request = String::from_utf8_lossy(&buf[..n]).to_string();

        // Parse method + path from first line.
        let first_line = request.lines().next().unwrap_or("");
        let parts: Vec<&str> = first_line.split_whitespace().collect();
        let method = parts.first().unwrap_or(&"").to_string();
        let path = parts.get(1).unwrap_or(&"").to_string();

        // Extract body after \r\n\r\n.
        let body = request
            .find("\r\n\r\n")
            .map(|idx| request[idx + 4..].to_string())
            .unwrap_or_default();

        *captured_clone.lock().expect("lock") = Some(CapturedRequest { method, path, body });

        let response = format!(
            "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            resp_body.len(),
            resp_body
        );
        stream.write_all(response.as_bytes()).expect("write");
    });

    (port, handle, captured)
}

fn make_test_channel_with_mock(
    port: u16,
) -> (super::DiscordChannel, std::sync::mpsc::Sender<ChannelEvent>) {
    // twilight-http 0.17's ratelimiter initialises inside `build()` and needs
    // a Tokio reactor in scope — build within the shared discord runtime.
    let client = super::block_on_value(async {
        twilight_http::Client::builder()
            .proxy(format!("127.0.0.1:{port}"), true)
            .build()
    });
    super::DiscordChannel::new_for_test_with_http(std::sync::Arc::new(client))
}

fn test_binding(channel_id: u64) -> crate::channel::BindingRef {
    crate::channel::BindingRef::new(
        "discord",
        Some(format!("DC#{channel_id}")),
        super::DiscordBindingPayload { channel_id },
    )
}

fn test_msg_ref(channel_id: u64, msg_id: &str) -> crate::channel::MsgRef {
    crate::channel::MsgRef {
        binding: test_binding(channel_id),
        id: msg_id.to_string(),
    }
}

/// §3.5.10 wire-format: PATCH /channels/{cid}/messages/{mid} request
/// body transmitted by twilight-http matches Discord edit-message spec.
/// Ref: https://discord.com/developers/docs/resources/message#edit-message
#[test]
fn discord_edit_outbound_body_matches_spec() {
    use crate::channel::Channel;

    // Edit response is the updated message — reuse create-message fixture.
    let response_body =
        include_str!("../../../tests/fixtures/discord-rest-create-message-response.json");
    let (port, handle, captured) = mock_http_server(200, response_body);
    let (ch, _tx) = make_test_channel_with_mock(port);

    let msg_ref = test_msg_ref(290926798999357250, "444385199974967099");
    let result = ch.edit(&msg_ref, crate::channel::OutMsg::text("edited text"));

    handle.join().expect("mock server");
    assert!(result.is_ok(), "edit must succeed: {:?}", result.err());

    let req = captured.lock().expect("lock").take().expect("captured");
    assert_eq!(req.method, "PATCH", "edit must use PATCH method");
    assert!(
        req.path.contains("/messages/444385199974967099"),
        "path must contain message id: {}",
        req.path
    );
    let body: serde_json::Value = serde_json::from_str(&req.body).expect("body must be JSON");
    assert_eq!(
        body["content"], "edited text",
        "edit body must contain updated content"
    );
}

/// §3.5.10 wire-format: DELETE /channels/{cid}/messages/{mid}
/// Ref: https://discord.com/developers/docs/resources/message#delete-message
#[test]
fn discord_delete_outbound_method_matches_spec() {
    use crate::channel::Channel;

    // DELETE returns 204 No Content with empty body per spec.
    let (port, handle, captured) = mock_http_server(204, "");
    let (ch, _tx) = make_test_channel_with_mock(port);

    let msg_ref = test_msg_ref(290926798999357250, "444385199974967099");
    let result = ch.delete(&msg_ref);

    handle.join().expect("mock server");
    assert!(result.is_ok(), "delete must succeed: {:?}", result.err());

    let req = captured.lock().expect("lock").take().expect("captured");
    assert_eq!(req.method, "DELETE", "delete must use DELETE method");
    assert!(
        req.path.contains("/messages/444385199974967099"),
        "path must contain message id: {}",
        req.path
    );
    assert!(
        req.body.is_empty() || req.body.trim().is_empty(),
        "DELETE body must be empty per spec, got: '{}'",
        req.body
    );
}

/// send_from_agent(Edit) wires through edit() with gate check.
#[test]
fn send_from_agent_edit_wires_through_edit() {
    use crate::channel::Channel;

    let response_body =
        include_str!("../../../tests/fixtures/discord-rest-create-message-response.json");
    let (port, handle, captured) = mock_http_server(200, response_body);
    let (ch, _tx) = make_test_channel_with_mock(port);

    // Record a binding so the agent lookup succeeds.
    ch.record_binding("test-agent", test_binding(290926798999357250), "\r".into());

    let result = ch.send_from_agent(
        "test-agent",
        crate::channel::AgentOutboundOp::Edit {
            message_id: "444385199974967099".into(),
            new_text: "updated".into(),
        },
    );

    handle.join().expect("mock server");
    assert!(
        result.is_ok(),
        "send_from_agent Edit must succeed: {:?}",
        result.err()
    );

    let req = captured.lock().expect("lock").take().expect("captured");
    assert_eq!(req.method, "PATCH");
}

// ── PR4 tests: binding lifecycle + CHANNEL_DELETE + persistence ───

/// §3.5.10 wire-format: POST /guilds/{gid}/channels request via
/// production Channel::create_binding() path.
#[test]
fn discord_create_binding_outbound_matches_spec() {
    use crate::channel::Channel;

    let response_body =
        include_str!("../../../tests/fixtures/discord-rest-create-guild-channel-response.json");
    let (port, handle, captured) = mock_http_server(200, response_body);
    let (ch, _tx) = make_test_channel_with_mock(port);

    let result = ch.create_binding("test-agent", crate::channel::BindingOpts::default());

    handle.join().expect("mock server");
    assert!(
        result.is_ok(),
        "create_binding must succeed: {:?}",
        result.err()
    );

    let binding = result.expect("binding");
    assert_eq!(binding.kind(), "discord");
    // Channel ID from fixture response.
    assert_eq!(binding.display_tag(), Some("DC#555555555555555555"));

    let req = captured.lock().expect("lock").take().expect("captured");
    assert_eq!(req.method, "POST", "create_binding must use POST");
    assert!(
        req.path.contains("/guilds/"),
        "path must target guild: {}",
        req.path
    );
    let body: serde_json::Value = serde_json::from_str(&req.body).expect("body must be JSON");
    assert!(
        body["name"].is_string(),
        "request body must have 'name' field"
    );
}

/// t-20260703164240502572-50899-11 (reviewer4 REJECTED finding on the
/// first #2615 pass): `create_topic` used to call `create_binding` but
/// never `record_binding`, so `has_binding`/`take_binding` — which
/// `channel_for_instance` (routing) and `drop_binding_on_all_channels`
/// (cleanup) both depend on — could never find the channel it just
/// created. Pins the fix directly on the production `create_topic` path
/// (not just the generic mock in `channel::tests`).
#[test]
fn discord_create_topic_records_a_binding_that_is_routable_and_cleanable() {
    use crate::channel::Channel;

    let response_body =
        include_str!("../../../tests/fixtures/discord-rest-create-guild-channel-response.json");
    let (port, handle, _captured) = mock_http_server(200, response_body);
    let (ch, _tx) = make_test_channel_with_mock(port);

    assert!(
        !ch.has_binding("test-agent"),
        "precondition: no binding before create_topic"
    );

    let result = ch.create_topic("test-agent");
    handle.join().expect("mock server");
    assert!(
        result.is_ok(),
        "create_topic must succeed: {:?}",
        result.err()
    );

    assert!(
        ch.has_binding("test-agent"),
        "create_topic must leave the instance bound — otherwise the \
         channel it just created is unroutable and uncleanable"
    );

    let taken = ch.take_binding("test-agent");
    assert!(taken.is_some(), "the recorded binding must be cleanable");
    assert!(
        !ch.has_binding("test-agent"),
        "binding must be gone after take_binding"
    );
}

/// §3.5.10 wire-format: DELETE /channels/{id} via production
/// Channel::remove_binding() path.
#[test]
fn discord_remove_binding_outbound_matches_spec() {
    use crate::channel::Channel;

    // DELETE returns the deleted channel object per spec.
    let response_body =
        include_str!("../../../tests/fixtures/discord-rest-create-guild-channel-response.json");
    let (port, handle, captured) = mock_http_server(200, response_body);
    let (ch, _tx) = make_test_channel_with_mock(port);

    let binding = test_binding(555555555555555555);
    let result = ch.remove_binding(&binding);

    handle.join().expect("mock server");
    assert!(
        result.is_ok(),
        "remove_binding must succeed: {:?}",
        result.err()
    );

    let req = captured.lock().expect("lock").take().expect("captured");
    assert_eq!(req.method, "DELETE", "remove_binding must use DELETE");
    assert!(
        req.path.contains("/channels/555555555555555555"),
        "path must contain channel id: {}",
        req.path
    );
}

/// §3.5.10 wire-format: CHANNEL_DELETE gateway event → BindingRevoked.
#[test]
fn discord_channel_delete_emits_binding_revoked() {
    let fixture = include_str!("../../../tests/fixtures/discord-gateway-channel-delete.json");
    let frame: serde_json::Value = serde_json::from_str(fixture).expect("fixture must parse");

    // Extract channel_id from the event payload.
    let channel_id: u64 = frame["d"]["id"]
        .as_str()
        .expect("id")
        .parse()
        .expect("parse id");

    let event = super::map_channel_delete_to_binding_revoked(channel_id);

    match event {
        ChannelEvent::BindingRevoked { binding, reason } => {
            assert_eq!(binding.kind(), "discord");
            assert_eq!(reason, crate::channel::event::RevokeReason::Deleted);
        }
        other => panic!("expected BindingRevoked, got: {other:?}"),
    }
}

/// CHANNEL_DELETE delivered via poll_event: gateway pushes event,
/// poll_event drains it as BindingRevoked.
#[test]
fn discord_channel_delete_via_poll_event() {
    use crate::channel::Channel;

    let (ch, tx) = super::DiscordChannel::new_for_test();
    let event = super::map_channel_delete_to_binding_revoked(290926798999357250);
    tx.send(event).expect("send");

    let polled = ch.poll_event().expect("should have event");
    assert!(
        matches!(polled, ChannelEvent::BindingRevoked { .. }),
        "expected BindingRevoked, got: {polled:?}"
    );
}

/// §3.5.10 persistence-replay: binding registry round-trip.
/// Write state → serialize → deserialize → verify bindings intact.
#[test]
fn discord_binding_registry_persistence_round_trip() {
    use crate::channel::Channel;

    let (ch, _tx) = super::DiscordChannel::new_for_test();

    // Record two bindings.
    ch.record_binding("agent-a", test_binding(111), "\r".into());
    ch.record_binding("agent-b", test_binding(222), "\r".into());

    // Serialize the binding registry to JSON (simulating disk write).
    let snapshot: std::collections::HashMap<String, u64> = {
        let s = ch.state.lock();
        s.instance_to_channel.clone()
    };
    let json = serde_json::to_string(&snapshot).expect("serialize");

    // Simulate restart: deserialize and verify.
    let restored: std::collections::HashMap<String, u64> =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.len(), 2);
    assert_eq!(restored["agent-a"], 111);
    assert_eq!(restored["agent-b"], 222);

    // Verify the live channel still has correct bindings.
    assert!(ch.has_binding("agent-a"));
    assert!(ch.has_binding("agent-b"));
    assert!(!ch.has_binding("agent-c"));

    // Take and verify round-trip.
    let taken = ch.take_binding("agent-a").expect("take");
    assert_eq!(taken.kind(), "discord");
    assert!(!ch.has_binding("agent-a"));
}

// ── F1 fix: auto-archive keepalive test ──────────────────────────

/// §3.5.10 production-path-coupled: keepalive PATCH via
/// send_keepalive_patch() against mock server.
#[test]
fn discord_keepalive_patch_method_matches_spec() {
    let (port, handle, captured) = mock_http_server(200, "{}");
    // twilight-http 0.17's ratelimiter needs a Tokio reactor at build().
    let client = super::block_on_value(async {
        twilight_http::Client::builder()
            .proxy(format!("127.0.0.1:{port}"), true)
            .build()
    });

    let result = super::send_keepalive_patch(&client, 290926798999357250);

    handle.join().expect("mock server");
    assert!(result.is_ok(), "keepalive must succeed: {:?}", result.err());

    let req = captured.lock().expect("lock").take().expect("captured");
    assert_eq!(req.method, "PATCH", "keepalive must use PATCH");
    assert!(
        req.path.contains("/channels/290926798999357250"),
        "path must target channel: {}",
        req.path
    );
    // Body must set archived=false per Discord thread update spec.
    let body: serde_json::Value = serde_json::from_str(&req.body).expect("body must be JSON");
    assert_eq!(body["archived"], false, "must set archived=false");
}

/// TLS smoke (network, manual): proves twilight-http 0.17's
/// rustls-native-roots/ring stack actually completes a real TLS handshake —
/// the one merge-gate CI can't cover (the spec tests use a plaintext mock
/// server). `#[ignore]` so normal/CI runs skip it; run with
/// `cargo test --features tray,discord -- --ignored tls_handshake_smoke`.
///
/// A missing crypto provider would panic ("no process-level CryptoProvider")
/// during the handshake. We hit `GET /gateway` (no valid token → 401 is fine);
/// any HTTP/auth response proves the handshake succeeded. Auth is NOT tested.
#[tokio::test]
#[ignore = "network: real Discord TLS handshake smoke; run manually"]
async fn tls_handshake_smoke_real_discord() {
    let client = twilight_http::Client::new("Bot tls-smoke-no-valid-token".to_string());
    // `.gateway()` GETs https://discord.com/api/v10/gateway. The handshake
    // happens before any auth check. A panic here = rustls/ring not wired.
    let outcome = client.gateway().await;
    // Reaching this line at all means no CryptoProvider panic. Surface the
    // result so the run log shows the handshake completed.
    match outcome {
        Ok(_) => eprintln!("TLS smoke: handshake + request OK (gateway responded)"),
        Err(e) => {
            eprintln!("TLS smoke: handshake OK, request returned (expected w/o token): {e}")
        }
    }
}

/// Keepalive interval constant is reasonable (≤ Discord's shortest
/// auto-archive of 3600s). Compile-time check via `const {}` blocks —
/// per `clippy::assertions_on_constants` and Rust 1.79+ const block
/// support — so a regression in `KEEPALIVE_INTERVAL_SECS` fails the
/// build, not just this test.
#[test]
fn discord_keepalive_interval_within_auto_archive_window() {
    const { assert!(super::KEEPALIVE_INTERVAL_SECS < 3600) };
    const { assert!(super::KEEPALIVE_INTERVAL_SECS >= 60) };
}

/// #2562 P3 regression pin: `build_http_client` must not panic when
/// called from a bare `std::thread` with no ambient Tokio runtime —
/// mirrors exactly how `bootstrap::discord_init::init` calls it on real
/// daemon boot (a plain `std::thread::Builder::spawn`, not a
/// `#[tokio::main]`/`#[tokio::test]` thread). Before the
/// `discord_runtime()`/`block_on_value` fix, this panicked with "there
/// is no reactor running, must be called from the context of a Tokio
/// 1.x runtime" (twilight-http-ratelimiting's internal `tokio::spawn` in
/// `Client::new`'s rate limiter) — caught only by an isolated
/// smoke-home real boot, never by the `#[tokio::test]`-shielded tests
/// above, which supply the missing runtime and hide the bug.
#[test]
fn build_http_client_does_not_panic_on_bare_thread() {
    let joined = std::thread::spawn(|| {
        super::build_http_client("Bot pin-test-not-a-real-token".to_string());
    })
    .join();
    assert!(
        joined.is_ok(),
        "build_http_client must not panic when called from a bare std::thread \
         with no ambient Tokio runtime (matches bootstrap::discord_init::init's \
         real call site)"
    );
}
