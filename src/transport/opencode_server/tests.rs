use super::*;
use std::io::{BufRead, Write};
use std::net::TcpListener;
use std::thread;

#[test]
fn locator_rejects_non_loopback_and_https() {
    let mut locator = SessionLocator::opencode(
        "https://127.0.0.1:4096".to_string(),
        Some("session".to_string()),
        "opencode".to_string(),
        "secret".to_string(),
    );
    assert!(Endpoint::parse(&locator).is_err());
    locator.endpoint_url = Some("http://localhost:4096".to_string());
    assert!(Endpoint::parse(&locator).is_err());
    locator.endpoint_url = Some("http://127.0.0.1:4096/api".to_string());
    assert!(Endpoint::parse(&locator).is_err());
    locator.endpoint_url = Some("http://127.0.0.1:4096".to_string());
    assert!(Endpoint::parse(&locator).is_ok());
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn managed_start_does_not_send_credentials_to_a_port_winner() {
    use std::os::unix::fs::PermissionsExt;

    let home = std::env::temp_dir().join(format!("agend-opencode-owner-{}", Uuid::new_v4()));
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let port = listener.local_addr().expect("address").port();
    let fake = home.join("fake-opencode.sh");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::write(&fake, "#!/bin/sh\nsleep 1\n").expect("fake binary");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700))
        .expect("fake executable");
    let previous_binary = std::env::var_os("AGEND_OPENCODE_BINARY");
    std::env::set_var("AGEND_OPENCODE_BINARY", &fake);
    let mut locator = SessionLocator::opencode(
        format!("http://127.0.0.1:{port}"),
        None,
        "opencode".to_string(),
        "must-not-leak".to_string(),
    );
    let result = wait_for_server_until(
        &home,
        "agent",
        &mut locator,
        None,
        Duration::from_millis(300),
    );
    assert!(result.is_err(), "fake child cannot prove server ownership");
    let mut received = false;
    for _ in 0..20 {
        match listener.accept() {
            Ok((mut stream, _)) => {
                received = true;
                let mut bytes = [0_u8; 256];
                let _ = stream.read(&mut bytes);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("listener: {error}"),
        }
    }
    assert!(
        !received,
        "managed startup must not probe an unowned listener"
    );
    stop_instance_server(&home, "agent");
    match previous_binary {
        Some(value) => std::env::set_var("AGEND_OPENCODE_BINARY", value),
        None => std::env::remove_var("AGEND_OPENCODE_BINARY"),
    }
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn persisted_server_identity_rejects_pid_reuse_or_missing_start_token() {
    let pid = std::process::id();
    let token = crate::process::process_start_token(pid).expect("current process token");
    let mut locator = SessionLocator::opencode(
        "http://127.0.0.1:4096".to_string(),
        None,
        "opencode".to_string(),
        "secret".to_string(),
    );
    locator.server_pid = Some(pid);
    locator.server_start_token = Some(token);
    assert!(persisted_server_owned(&locator));
    locator.server_start_token = Some(token.wrapping_add(1));
    assert!(!persisted_server_owned(&locator));
    locator.server_start_token = None;
    assert!(!persisted_server_owned(&locator));
}

#[test]
fn redacted_receipt_locator_matches_current_server_without_widening_credentials() {
    let current = SessionLocator::opencode(
        "http://127.0.0.1:4096".to_string(),
        Some("session".to_string()),
        "opencode".to_string(),
        "current-password".to_string(),
    );
    let mut redacted = current.clone();
    redacted.password = None;
    assert!(OpenCodeNativeShared::same_server(&current, &redacted));

    let mut wrong = current.clone();
    wrong.password = Some("wrong-password".to_string());
    assert!(!OpenCodeNativeShared::same_server(&current, &wrong));

    let mut wrong_session = current.clone();
    wrong_session.password = Some("wrong-password".to_string());
    assert!(!OpenCodeNativeShared::same_session(
        &current,
        &wrong_session
    ));
}

#[test]
fn sse_decoder_handles_split_chunked_unicode_events() {
    let mut decoder = SseDecoder::new(true, Vec::new());
    let body = b"data: {\"type\":\"session.status\",\"properties\":{\"status\":{\"type\":\"busy\"}}}\n\ndata: {\"type\":\"session.idle\"}\n\n";
    let mut wire = format!("{:X}\r\n", body.len()).into_bytes();
    wire.extend_from_slice(body);
    wire.extend_from_slice(b"\r\n0\r\n\r\n");
    let mut out = Vec::new();
    for part in wire.chunks(3) {
        out.extend(decoder.feed(part));
    }
    assert_eq!(out.len(), 2);
    assert!(out[0].contains("session.status"));
    assert!(out[1].contains("session.idle"));
}

#[test]
fn sse_decoder_accepts_crlf_and_bounds_non_chunked_frames() {
    let mut decoder = SseDecoder::new(false, Vec::new());
    let out = decoder.feed(
        b"data: {\"type\":\"server.connected\"}\r\n\r\ndata: {\"type\":\"session.idle\"}\r\n\r\n",
    );
    assert_eq!(out.len(), 2);

    let mut decoder = SseDecoder::new(false, Vec::new());
    assert!(decoder.feed(&vec![b'x'; MAX_BODY + 1]).is_empty());
    assert!(decoder.overflowed);
}

#[test]
fn sse_decoder_splits_mixed_delimiters_in_wire_order() {
    let mut decoder = SseDecoder::new(false, Vec::new());
    let out = decoder
        .feed(b"data: {\"type\":\"session.status\"}\n\ndata: {\"type\":\"session.idle\"}\r\n\r\n");
    assert_eq!(out.len(), 2);
    assert!(out[0].contains("session.status"));
    assert!(out[1].contains("session.idle"));
}

#[test]
fn readiness_observer_requires_the_configured_port() {
    let ready = Arc::new(AtomicBool::new(false));
    observe_server_ready(
        std::io::Cursor::new("server listening on http://127.0.0.1:40960\n"),
        4096,
        Arc::clone(&ready),
    );
    assert!(!ready.load(Ordering::Acquire));

    observe_server_ready(
        std::io::Cursor::new("server listening on http://127.0.0.1:4096\n"),
        4096,
        Arc::clone(&ready),
    );
    assert!(ready.load(Ordering::Acquire));
}

#[test]
fn attach_args_are_session_specific_and_do_not_include_password() {
    let locator = SessionLocator::opencode(
        "http://127.0.0.1:4096".to_string(),
        Some("session-1".to_string()),
        "opencode".to_string(),
        "secret".to_string(),
    );
    let args = OpenCodeNativeShared::attach_args(&locator).expect("attach args");
    assert_eq!(
        args,
        ["attach", "http://127.0.0.1:4096", "--session", "session-1"]
    );
    assert!(!args.iter().any(|arg| arg.contains("secret")));
}

#[test]
fn event_mapping_marks_busy_idle_and_error() {
    let mut adapter = OpenCodeNativeShared::new(Path::new("/tmp/agend"), "agent");
    adapter.locator = Some(SessionLocator::opencode(
        "http://127.0.0.1:4096".to_string(),
        Some("session-1".to_string()),
        "opencode".to_string(),
        "secret".to_string(),
    ));
    let delivery_id = Uuid::new_v4();
    adapter.in_flight = Some(delivery_id);
    adapter.pending.insert(
        delivery_id,
        DeliveryEnvelope::new(
            "agent",
            adapter.locator.clone().expect("locator"),
            DeliveryKind::Prompt,
            "hello",
            None,
        ),
    );
    let idle_before_target = adapter
        .normalize_event(json!({
            "type": "session.idle",
            "properties": {"sessionID": "session-1"}
        }))
        .expect("idle before target");
    assert!(matches!(idle_before_target, BackendEvent::Unknown { .. }));
    assert_eq!(adapter.in_flight, Some(delivery_id));

    let other_message = Uuid::new_v4();
    let unrelated = adapter
        .normalize_event(json!({
            "type": "message.updated",
            "properties": {"sessionID": "session-1", "info": {"id": other_message.to_string()}}
        }))
        .expect("unrelated message");
    assert!(matches!(unrelated, BackendEvent::Unknown { .. }));
    assert_eq!(adapter.in_flight, Some(delivery_id));

    let observed = adapter
            .normalize_event(json!({
                "type": "message.updated",
                "properties": {"sessionID": "session-1", "info": {"id": opencode_message_id(delivery_id)}}
            }))
            .expect("target message");
    assert!(
        matches!(observed, BackendEvent::ObservedInSession { delivery_id: id, .. } if id == delivery_id)
    );

    let busy = adapter
        .normalize_event(json!({
            "type": "session.status",
            "properties": {"sessionID": "session-1", "status": {"type": "busy"}}
        }))
        .expect("busy");
    assert!(matches!(busy, BackendEvent::TurnStarted { .. }));
    let idle = adapter
        .normalize_event(json!({
            "type": "session.idle",
            "properties": {"sessionID": "session-1"}
        }))
        .expect("idle");
    assert!(matches!(idle, BackendEvent::Completed { .. }));
}

#[test]
fn opencode_message_id_roundtrips_only_the_prefixed_wire_identity() {
    let delivery_id =
        Uuid::parse_str("5af6d2a0-f5ca-4bef-8171-bb29202e25d2").expect("fixture UUID");
    let wire_id = opencode_message_id(delivery_id);
    assert_eq!(wire_id, "msg_5af6d2a0f5ca4bef8171bb29202e25d2");
    assert_eq!(
        delivery_id_from_opencode_message_id(&wire_id),
        Some(delivery_id)
    );
    assert_eq!(
        delivery_id_from_opencode_message_id(&delivery_id.to_string()),
        None
    );
    assert!(contains_delivery_target(
        &json!({"info": {"id": wire_id}}),
        delivery_id,
        None
    ));
    assert!(!contains_delivery_target(
        &json!({"info": {"id": opencode_message_id(Uuid::new_v4())}}),
        delivery_id,
        None
    ));
}

#[test]
fn opencode_message_ids_are_monotonic_when_clock_does_not_advance() {
    let now_ms = 1_786_091_572_000;
    let first = next_opencode_message_id_at(None, now_ms).expect("first message id");
    let second = next_opencode_message_id_at(Some(&first), now_ms).expect("second message id");

    for value in [&first, &second] {
        assert!(value.starts_with("msg_"));
        assert_eq!(value.len(), 4 + 12 + 14);
        assert!(value[4..16].bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(value[16..].bytes().all(|byte| byte.is_ascii_alphanumeric()));
    }
    assert!(
        second[4..16] > first[4..16],
        "timestamp prefix must advance"
    );
    assert!(second > first, "same-clock IDs must be strictly ordered");
}

#[test]
fn opencode_message_ids_fail_closed_at_vendor_timestamp_wrap() {
    let before_wrap_ms = OPENCODE_MESSAGE_ID_TIMESTAMP_MASK / 0x1000;
    let before_wrap = next_opencode_message_id_at(None, before_wrap_ms).expect("pre-wrap ID");
    let before_field = opencode_message_id_timestamp(&before_wrap).expect("pre-wrap field");
    assert!(
        next_opencode_message_id_at(Some(&before_wrap), before_wrap_ms + 1)
            .expect_err("natural wrap must fail closed")
            .to_string()
            .contains("timestamp wrapped")
    );
    let exhausted = format!(
        "{OPENCODE_MESSAGE_ID_PREFIX}{OPENCODE_MESSAGE_ID_TIMESTAMP_MASK:012x}00000000000000"
    );
    assert!(
        next_opencode_message_id_at(Some(&exhausted), before_wrap_ms)
            .expect_err("exhausted prefix must fail closed")
            .to_string()
            .contains("prefix is exhausted")
    );
    assert!(before_field > 1);
}

const ROLLOVER_SESSION_ID: &str = "ses_rollover";
const ROLLOVER_LATEST_MESSAGE_ID: &str = "msg_00000000000900000000000000";

fn rollover_server(fail_first_select: bool) -> (u16, thread::JoinHandle<Vec<(String, Value)>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let port = listener.local_addr().expect("address").port();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut requests = Vec::new();
        let mut select_count = 0;
        loop {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "rollover server timed out");
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("rollover listener: {error}"),
            };
            let (header, body) = read_http_request(&stream);
            let request_line = header.lines().next().unwrap_or_default().to_string();
            let body = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
            requests.push((request_line.clone(), body.clone()));
            if request_line.starts_with("GET /global/health ") {
                json_response(
                    &mut stream,
                    "200 OK",
                    json!({"healthy": true, "version": "1.17.5"}),
                );
            } else if request_line.starts_with("GET /session/session-1 ") {
                json_response(&mut stream, "200 OK", json!({"id": "session-1"}));
            } else if request_line.starts_with("POST /session/session-1/fork ") {
                json_response(&mut stream, "200 OK", json!({"id": ROLLOVER_SESSION_ID}));
            } else if request_line.starts_with(&format!(
                "GET /session/{ROLLOVER_SESSION_ID}/message?limit=1 "
            )) {
                json_response(
                    &mut stream,
                    "200 OK",
                    json!([{"info": {"id": ROLLOVER_LATEST_MESSAGE_ID}, "parts": []}]),
                );
            } else if request_line.starts_with("POST /tui/select-session ") {
                assert_eq!(body, json!({"sessionID": ROLLOVER_SESSION_ID}));
                select_count += 1;
                if fail_first_select && select_count == 1 {
                    json_response(
                        &mut stream,
                        "503 Service Unavailable",
                        json!({"data": {"message": "select unavailable"}}),
                    );
                } else {
                    json_response(&mut stream, "200 OK", json!(true));
                }
            } else if request_line.starts_with(&format!(
                "POST /session/{ROLLOVER_SESSION_ID}/prompt_async "
            )) {
                stream
                    .write_all(
                        b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .expect("prompt response");
                stream.flush().expect("prompt flush");
                return requests;
            } else if request_line.starts_with("GET /event ") {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .expect("event response");
                stream.flush().expect("event flush");
            } else {
                panic!("unexpected rollover request: {request_line}");
            }
        }
    });
    (port, server)
}

fn seed_pre_wrap_delivery(
    home: &Path,
    locator: &SessionLocator,
    before_wrap_ms: u64,
) -> ReceiptStore {
    let pre_wrap_user_id =
        next_opencode_message_id_at(None, before_wrap_ms).expect("pre-wrap user ID");
    let store = ReceiptStore::for_instance(home, "agent").expect("receipt store");
    let pre_wrap_delivery = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "pre-wrap terminal user",
        None,
    );
    store
        .record_queued(&pre_wrap_delivery)
        .expect("pre-wrap queued");
    let mut pre_wrap_receipt =
        DeliveryReceipt::for_state(&pre_wrap_delivery, DeliveryState::Completed);
    pre_wrap_receipt.protocol_request_id = Some(pre_wrap_user_id);
    store.record(pre_wrap_receipt).expect("pre-wrap completed");
    store
}

#[test]
fn prompt_async_rolls_over_before_vendor_timestamp_wrap() {
    let home =
        std::env::temp_dir().join(format!("agend-opencode-wrap-rollover-{}", Uuid::new_v4()));
    let (port, server) = rollover_server(false);
    let locator = SessionLocator::opencode(
        format!("http://127.0.0.1:{port}"),
        Some("session-1".to_string()),
        "opencode".to_string(),
        "secret".to_string(),
    );
    let before_wrap_ms = OPENCODE_MESSAGE_ID_TIMESTAMP_MASK / 0x1000;
    let store = seed_pre_wrap_delivery(&home, &locator, before_wrap_ms);

    let mut adapter = OpenCodeNativeShared::new(&home, "agent");
    adapter.ready = true;
    adapter.locator = Some(locator.clone());
    adapter.message_id_timestamp_ms = Some(before_wrap_ms + 1);
    let envelope = DeliveryEnvelope::new(
        "agent",
        locator,
        DeliveryKind::Prompt,
        "post-wrap delivery",
        None,
    );
    let receipt = adapter
        .deliver_blocking(envelope.clone())
        .expect("post-wrap delivery must roll over");
    assert_eq!(receipt.state, DeliveryState::ProtocolAccepted);
    let wire_message_id = receipt
        .protocol_request_id
        .as_deref()
        .expect("rollover wire message ID");
    assert!(wire_message_id > ROLLOVER_LATEST_MESSAGE_ID);
    assert_eq!(opencode_message_id_timestamp(wire_message_id), Some(10));
    let saved =
        super::super::registry::load_session_locator(&home, "agent").expect("rollover locator");
    assert_eq!(saved.session_id.as_deref(), Some(ROLLOVER_SESSION_ID));
    assert_eq!(
        store
            .latest_opencode_protocol_request_id_for_session(ROLLOVER_SESSION_ID)
            .expect("new-session seed")
            .as_deref(),
        Some(wire_message_id)
    );
    let requests = server.join().expect("rollover server");
    let request_lines = requests
        .iter()
        .map(|(line, _)| line.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        request_lines,
        [
            "POST /session/session-1/fork HTTP/1.1",
            "GET /session/ses_rollover/message?limit=1 HTTP/1.1",
            "POST /tui/select-session HTTP/1.1",
            "POST /session/ses_rollover/prompt_async HTTP/1.1",
        ]
    );
    let prompts = requests
        .iter()
        .filter(|(line, _)| line.contains("/prompt_async "))
        .collect::<Vec<_>>();
    assert_eq!(prompts.len(), 1);
    assert_eq!(
        prompts[0].1.get("parts"),
        Some(&json!([{"type": "text", "text": "post-wrap delivery"}]))
    );
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn prompt_async_replays_pre_submit_rollover_after_reconstruction() {
    let home =
        std::env::temp_dir().join(format!("agend-opencode-rollover-replay-{}", Uuid::new_v4()));
    let before_wrap_ms = OPENCODE_MESSAGE_ID_TIMESTAMP_MASK / 0x1000;
    let (port, server) = rollover_server(true);
    let mut locator = SessionLocator::opencode(
        format!("http://127.0.0.1:{port}"),
        Some("session-1".to_string()),
        "opencode".to_string(),
        "secret".to_string(),
    );
    locator.managed = false;
    let store = seed_pre_wrap_delivery(&home, &locator, before_wrap_ms);

    let mut first_adapter = OpenCodeNativeShared::new(&home, "agent");
    first_adapter.ready = true;
    first_adapter.locator = Some(locator.clone());
    first_adapter.message_id_timestamp_ms = Some(before_wrap_ms + 1);
    let envelope = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "replay exact boundary prompt",
        None,
    );
    first_adapter
        .deliver_blocking(envelope.clone())
        .expect_err("first select-session attempt must fail before prompt submission");
    assert_eq!(
        store
            .latest(envelope.delivery_id)
            .expect("queued replay receipt")
            .expect("replay receipt")
            .state,
        DeliveryState::Queued,
        "a pre-submit rollover failure must remain replayable"
    );

    let mut reconstructed = OpenCodeNativeShared::new(&home, "agent");
    reconstructed.message_id_timestamp_ms = Some(before_wrap_ms + 1);
    reconstructed
        .start_or_attach_blocking(locator, None)
        .expect("fresh adapter must replay the durable pre-submit rollover");
    let receipt = store
        .latest(envelope.delivery_id)
        .expect("accepted replay receipt")
        .expect("replayed receipt");
    assert_eq!(receipt.state, DeliveryState::ProtocolAccepted);
    let message_id = receipt
        .protocol_request_id
        .as_deref()
        .expect("replayed wire message ID");
    assert_eq!(
        opencode_message_id_timestamp(message_id),
        Some(10),
        "replay must retain the fork-scoped wire seed"
    );
    let requests = server.join().expect("replay server");
    assert_eq!(
        requests
            .iter()
            .filter(|(line, _)| line == "POST /session/session-1/fork HTTP/1.1")
            .count(),
        1,
        "reconstruction must resume the durable fork instead of forking again"
    );
    let prompts = requests
        .iter()
        .filter(|(line, _)| line.contains("/prompt_async "))
        .collect::<Vec<_>>();
    assert_eq!(prompts.len(), 1);
    assert_eq!(
        prompts[0].1.get("parts"),
        Some(&json!([{"type": "text", "text": "replay exact boundary prompt"}]))
    );
    assert_eq!(
        super::super::registry::load_session_locator(&home, "agent")
            .expect("replayed locator")
            .session_id
            .as_deref(),
        Some(ROLLOVER_SESSION_ID)
    );
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn prompt_async_retries_the_same_durable_envelope_without_regressing_its_receipt() {
    let home = std::env::temp_dir().join(format!(
        "agend-opencode-rollover-same-envelope-{}",
        Uuid::new_v4()
    ));
    let before_wrap_ms = OPENCODE_MESSAGE_ID_TIMESTAMP_MASK / 0x1000;
    let (port, server) = rollover_server(true);
    let mut locator = SessionLocator::opencode(
        format!("http://127.0.0.1:{port}"),
        Some("session-1".to_string()),
        "opencode".to_string(),
        "secret".to_string(),
    );
    locator.managed = false;
    let store = seed_pre_wrap_delivery(&home, &locator, before_wrap_ms);
    let mut adapter = OpenCodeNativeShared::new(&home, "agent");
    adapter.ready = true;
    adapter.locator = Some(locator.clone());
    adapter.message_id_timestamp_ms = Some(before_wrap_ms + 1);
    let envelope = DeliveryEnvelope::new(
        "agent",
        locator,
        DeliveryKind::Prompt,
        "retry exact durable envelope",
        None,
    );

    adapter
        .deliver_blocking(envelope.clone())
        .expect_err("first select must fail");
    let receipt = adapter
        .deliver_blocking(envelope.clone())
        .expect("same envelope retry must resume the journal");
    assert_eq!(receipt.state, DeliveryState::ProtocolAccepted);
    assert_eq!(
        store
            .latest(envelope.delivery_id)
            .expect("latest")
            .expect("accepted")
            .state,
        DeliveryState::ProtocolAccepted
    );
    let requests = server.join().expect("server");
    assert_eq!(
        requests
            .iter()
            .filter(|(line, _)| line.contains("/fork "))
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|(line, _)| line.contains("/prompt_async "))
            .count(),
        1
    );
    let _ = std::fs::remove_dir_all(home);
}

fn rollover_reconcile_server(
    message_found: bool,
    wire_message_id: String,
) -> (u16, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
    let port = listener.local_addr().expect("address").port();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..5 {
            let (mut stream, _) = listener.accept().expect("accept");
            let (header, body) = read_http_request(&stream);
            let request_line = header.lines().next().unwrap_or_default().to_string();
            requests.push(request_line.clone());
            if request_line.starts_with("GET /global/health ") {
                json_response(
                    &mut stream,
                    "200 OK",
                    json!({"healthy": true, "version": "1.17.5"}),
                );
            } else if request_line.starts_with("GET /session/session-1 ")
                || request_line.starts_with(&format!("GET /session/{ROLLOVER_SESSION_ID} "))
            {
                let session_id = if request_line.starts_with("GET /session/session-1 ") {
                    "session-1"
                } else {
                    ROLLOVER_SESSION_ID
                };
                json_response(&mut stream, "200 OK", json!({"id": session_id}));
            } else if request_line.starts_with("GET /event ") {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .expect("event response");
                stream.flush().expect("event flush");
            } else if request_line.starts_with("POST /tui/select-session ") {
                assert_eq!(
                    serde_json::from_slice::<Value>(&body).expect("select body"),
                    json!({"sessionID": ROLLOVER_SESSION_ID})
                );
                json_response(&mut stream, "200 OK", json!(true));
            } else if request_line.starts_with(&format!(
                "GET /session/{ROLLOVER_SESSION_ID}/message/{wire_message_id} "
            )) {
                if message_found {
                    json_response(
                        &mut stream,
                        "200 OK",
                        json!({"info": {"id": wire_message_id}, "parts": []}),
                    );
                } else {
                    json_response(
                        &mut stream,
                        "404 Not Found",
                        json!({"data": {"message": "message not found"}}),
                    );
                }
            } else {
                panic!("unexpected reconciliation request: {request_line}");
            }
        }
        requests
    });
    (port, server)
}

fn seed_submit_attempted_rollover(
    home: &Path,
    port: u16,
    wire_message_id: &str,
) -> (SessionLocator, DeliveryEnvelope, ReceiptStore) {
    let mut source = SessionLocator::opencode(
        format!("http://127.0.0.1:{port}"),
        Some("session-1".to_string()),
        "opencode".to_string(),
        "secret".to_string(),
    );
    source.managed = false;
    let envelope = DeliveryEnvelope::new(
        "agent",
        source.clone(),
        DeliveryKind::Prompt,
        "never blindly retry this prompt",
        None,
    );
    let store = ReceiptStore::for_instance(home, "agent").expect("receipt store");
    store.record_queued(&envelope).expect("queued");
    save_rollover_journal(
        home,
        "agent",
        &RolloverJournal {
            version: ROLLOVER_JOURNAL_VERSION,
            delivery_id: envelope.delivery_id,
            phase: RolloverPhase::SubmitAttempted {
                source_session_id: "session-1".to_string(),
                target_session_id: ROLLOVER_SESSION_ID.to_string(),
                wire_message_id: wire_message_id.to_string(),
            },
        },
    )
    .expect("submit-attempted journal");
    let mut target = source;
    target.session_id = Some(ROLLOVER_SESSION_ID.to_string());
    (target, envelope, store)
}

#[test]
fn reconstruction_accepts_a_proven_submit_attempt_without_retrying_prompt() {
    let home = std::env::temp_dir().join(format!(
        "agend-opencode-rollover-reconcile-present-{}",
        Uuid::new_v4()
    ));
    let wire_message_id = "msg_00000000000a00000000000000".to_string();
    let (port, server) = rollover_reconcile_server(true, wire_message_id.clone());
    let (locator, envelope, store) = seed_submit_attempted_rollover(&home, port, &wire_message_id);

    let mut reconstructed = OpenCodeNativeShared::new(&home, "agent");
    reconstructed
        .start_or_attach_blocking(locator, None)
        .expect("stable message proof must recover acceptance");
    let receipt = store
        .latest(envelope.delivery_id)
        .expect("latest")
        .expect("accepted receipt");
    assert_eq!(receipt.state, DeliveryState::ProtocolAccepted);
    assert_eq!(
        receipt.backend_session_id.as_deref(),
        Some(ROLLOVER_SESSION_ID)
    );
    assert_eq!(
        receipt.protocol_request_id.as_deref(),
        Some(&*wire_message_id)
    );
    assert!(
        load_rollover_journal(&home, "agent")
            .expect("journal lookup")
            .is_none(),
        "a reconciled attempt must clear the journal"
    );
    let requests = server.join().expect("server");
    assert!(requests.iter().all(|line| !line.contains("prompt_async")));
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn reconstruction_marks_an_absent_submit_attempt_ambiguous_without_retry() {
    let home = std::env::temp_dir().join(format!(
        "agend-opencode-rollover-reconcile-absent-{}",
        Uuid::new_v4()
    ));
    let wire_message_id = "msg_00000000000a00000000000000".to_string();
    let (port, server) = rollover_reconcile_server(false, wire_message_id.clone());
    let (locator, envelope, store) = seed_submit_attempted_rollover(&home, port, &wire_message_id);

    let mut reconstructed = OpenCodeNativeShared::new(&home, "agent");
    reconstructed
        .start_or_attach_blocking(locator, None)
        .expect_err("absence cannot prove that retry is safe");
    let receipt = store
        .latest(envelope.delivery_id)
        .expect("latest")
        .expect("ambiguous receipt");
    assert_eq!(receipt.state, DeliveryState::Ambiguous);
    assert_eq!(
        receipt.backend_session_id.as_deref(),
        Some(ROLLOVER_SESSION_ID)
    );
    assert!(
        load_rollover_journal(&home, "agent")
            .expect("journal lookup")
            .is_none(),
        "an explicit ambiguous receipt replaces the journal"
    );
    let requests = server.join().expect("server");
    assert!(requests.iter().all(|line| !line.contains("prompt_async")));
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn same_envelope_retry_preserves_an_ambiguous_submit_reconciliation() {
    let home = std::env::temp_dir().join(format!(
        "agend-opencode-rollover-reconcile-retry-{}",
        Uuid::new_v4()
    ));
    let wire_message_id = "msg_00000000000a00000000000000".to_string();
    let (port, server) = rollover_reconcile_server(false, wire_message_id.clone());
    let (_, envelope, store) = seed_submit_attempted_rollover(&home, port, &wire_message_id);

    let mut reconstructed = OpenCodeNativeShared::new(&home, "agent");
    reconstructed
        .deliver_blocking(envelope.clone())
        .expect_err("absence cannot be retried as a fresh prompt");
    assert_eq!(
        store
            .latest(envelope.delivery_id)
            .expect("latest")
            .expect("ambiguous receipt")
            .state,
        DeliveryState::Ambiguous,
        "the delivery wrapper must not overwrite Ambiguous with Failed"
    );
    let requests = server.join().expect("server");
    assert!(requests.iter().all(|line| !line.contains("prompt_async")));
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn non_2xx_response_diagnostics_are_bounded_and_redacted() {
    let error = response_json(
            HttpResponse {
                status: 400,
                body: br#"{"data":{"message":"Expected a string starting with msg"},"token":"do-not-log"}"#
                    .to_vec(),
            },
            "prompt_async",
        )
        .expect_err("non-2xx response");
    let detail = error.to_string();
    assert!(detail.contains("Expected a string starting with msg"));
    assert!(!detail.contains("do-not-log"));
}

#[test]
fn session_only_events_do_not_regress_or_complete_a_delivery() {
    let home = std::env::temp_dir().join(format!("agend-opencode-events-{}", Uuid::new_v4()));
    let mut adapter = OpenCodeNativeShared::new(&home, "agent");
    let locator = SessionLocator::opencode(
        "http://127.0.0.1:4096".to_string(),
        Some("session-1".to_string()),
        "opencode".to_string(),
        "secret".to_string(),
    );
    adapter.locator = Some(locator.clone());
    let envelope = DeliveryEnvelope::new("agent", locator, DeliveryKind::Prompt, "hello", None);
    let delivery_id = envelope.delivery_id;
    adapter.pending.insert(delivery_id, envelope.clone());
    adapter.in_flight = Some(delivery_id);
    let _ = adapter
        .normalize_event(json!({
            "type": "session.status",
            "properties": {"sessionID": "session-1", "status": {"type": "idle"}}
        }))
        .expect("idle");
    assert_eq!(adapter.in_flight, Some(delivery_id));
    assert!(!adapter.target_confirmed.contains(&delivery_id));

    let store = ReceiptStore::for_instance(&home, "agent").expect("store");
    store.record_queued(&envelope).expect("queued");
    let mut accepted = DeliveryReceipt::for_state(&envelope, DeliveryState::ProtocolAccepted);
    accepted.protocol_request_id = Some(opencode_message_id(delivery_id));
    store.record(accepted).expect("accepted");
    adapter
        .update_state(
            delivery_id,
            DeliveryState::Completed,
            "test completion",
            Some("test"),
        )
        .expect("complete");
    adapter
        .update_state(
            delivery_id,
            DeliveryState::ObservedInSession,
            "late event",
            Some("test"),
        )
        .expect("monotonic");
    assert_eq!(
        store
            .latest(delivery_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::Completed
    );
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn busy_collision_parks_for_redrive_while_interrupt_stays_terminal() {
    let home = std::env::temp_dir().join(format!("agend-opencode-collision-{}", Uuid::new_v4()));
    let locator = SessionLocator::opencode(
        "http://127.0.0.1:4096".to_string(),
        Some("session-1".to_string()),
        "opencode".to_string(),
        "secret".to_string(),
    );
    let mut adapter = OpenCodeNativeShared::new(&home, "agent");
    adapter.locator = Some(locator.clone());
    adapter.ready = true;
    let in_flight = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "first",
        None,
    );
    adapter.in_flight = Some(in_flight.delivery_id);
    adapter.pending.insert(in_flight.delivery_id, in_flight);

    // A busy ordinary delivery parks durably Queued for redrive — it is NOT
    // a terminal failure.
    let envelope =
        DeliveryEnvelope::new("agent", locator.clone(), DeliveryKind::Prompt, "next", None);
    let delivery_id = envelope.delivery_id;
    let error = adapter
        .deliver_blocking(envelope)
        .expect_err("busy ordinary delivery parks");
    assert_eq!(
        error.to_string(),
        "OpenCode session already has an ordinary turn in flight"
    );
    let receipt = ReceiptStore::for_instance(&home, "agent")
        .expect("store")
        .latest(delivery_id)
        .expect("latest")
        .expect("receipt");
    assert_eq!(receipt.state, DeliveryState::Queued);
    assert_eq!(adapter.parked.len(), 1);
    assert_eq!(adapter.parked[0].envelope.delivery_id, delivery_id);
    assert_eq!(adapter.parked[0].attempts, 1);

    // Steer/interrupt rejection stays terminal and never parks.
    let envelope = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Interrupt,
        "next",
        None,
    );
    let delivery_id = envelope.delivery_id;
    let error = adapter
        .deliver_blocking(envelope)
        .expect_err("interrupt stays terminal");
    assert_eq!(
        error.to_string(),
        "OpenCode NativeShared requires an explicit prompt operation"
    );
    let receipt = ReceiptStore::for_instance(&home, "agent")
        .expect("store")
        .latest(delivery_id)
        .expect("latest")
        .expect("receipt");
    assert_eq!(receipt.state, DeliveryState::Failed);
    assert_eq!(adapter.parked.len(), 1);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn parked_redrive_attempt_cap_fails_closed() {
    let home = std::env::temp_dir().join(format!("agend-opencode-park-cap-{}", Uuid::new_v4()));
    let locator = SessionLocator::opencode(
        "http://127.0.0.1:4096".to_string(),
        Some("session-1".to_string()),
        "opencode".to_string(),
        "secret".to_string(),
    );
    let mut adapter = OpenCodeNativeShared::new(&home, "agent");
    adapter.locator = Some(locator.clone());
    adapter.ready = true;
    let in_flight = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "first",
        None,
    );
    adapter.in_flight = Some(in_flight.delivery_id);
    adapter
        .pending
        .insert(in_flight.delivery_id, in_flight.clone());
    let in_flight_id = in_flight.delivery_id;

    // Same delivery id colliding while busy parks up to the cap, then fails
    // closed — all without touching the network (the busy gate precedes it).
    let envelope = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "retry me",
        None,
    );
    let delivery_id = envelope.delivery_id;
    for _ in 0..MAX_PARKED_REDRIVE_ATTEMPTS {
        let error = adapter
            .deliver_blocking(envelope.clone())
            .expect_err("busy collision parks");
        assert_eq!(
            error.to_string(),
            "OpenCode session already has an ordinary turn in flight"
        );
    }
    let store = ReceiptStore::for_instance(&home, "agent").expect("store");
    assert_eq!(
        store
            .latest(delivery_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::Queued
    );
    assert_eq!(adapter.parked.len(), 1);
    assert_eq!(adapter.parked[0].attempts, MAX_PARKED_REDRIVE_ATTEMPTS);
    let error = adapter
        .deliver_blocking(envelope)
        .expect_err("cap exhausts fail-closed");
    assert_eq!(
        error.to_string(),
        "OpenCode parked redrive attempts exhausted; delivery failed closed",
        "the fail-closed signal must be distinguishable from the park signal"
    );
    assert_eq!(
        store
            .latest(delivery_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::Failed
    );
    assert!(adapter.parked.is_empty());

    // The redrive path enforces the same cap without submitting: an
    // over-attempt intent found in the queue is failed closed on completion.
    let stale = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "stale park",
        None,
    );
    let stale_id = stale.delivery_id;
    store.record_queued(&stale).expect("queued");
    adapter.parked.push_back(ParkedDelivery {
        envelope: stale,
        attempts: MAX_PARKED_REDRIVE_ATTEMPTS + 1,
        parked_at: Instant::now(),
    });
    adapter
        .complete(in_flight_id, "session.idle")
        .expect("complete");
    assert_eq!(
        store
            .latest(stale_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::Failed
    );
    assert!(adapter.parked.is_empty());
    let _ = std::fs::remove_dir_all(home);
}

/// Mock OpenCode server for redrive tests: answers health + session lookup,
/// accepts the event stream, and captures `prompt_async` bodies in order.
fn redrive_capture_server(
    prompt_count: usize,
) -> (
    u16,
    thread::JoinHandle<Vec<String>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (prompt_tx, prompt_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let mut texts = Vec::new();
        for _ in 0..(3 + prompt_count) {
            let (mut stream, _) = listener.accept().expect("accept");
            let (header, body) = read_http_request(&stream);
            let request_line = header.lines().next().unwrap_or_default().to_string();
            if request_line.starts_with("GET /global/health ") {
                json_response(
                    &mut stream,
                    "200 OK",
                    json!({"healthy": true, "version": "1.17.5"}),
                );
            } else if request_line.starts_with("GET /session/session-1 ") {
                json_response(&mut stream, "200 OK", json!({"id": "session-1"}));
            } else if request_line.starts_with("GET /event ") {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n"
                )
                .expect("event headers");
                stream.flush().expect("event flush");
            } else if request_line.starts_with("POST /session/session-1/prompt_async ") {
                let prompt = serde_json::from_slice::<Value>(&body).expect("prompt json");
                let text = prompt
                    .pointer("/parts/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                texts.push(text);
                prompt_tx.send(body).expect("prompt capture");
                stream
                    .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .expect("prompt response");
                stream.flush().expect("prompt flush");
            } else {
                panic!("unexpected request: {request_line}");
            }
        }
        texts
    });
    (port, server, prompt_rx)
}

fn redrive_locator(port: u16) -> SessionLocator {
    let mut locator = SessionLocator::opencode(
        format!("http://127.0.0.1:{port}"),
        Some("session-1".to_string()),
        "opencode".to_string(),
        "secret".to_string(),
    );
    locator.managed = false;
    locator
}

#[test]
fn busy_parked_delivery_redrives_to_completed_after_idle() {
    let (port, server, _prompt_rx) = redrive_capture_server(2);
    let home =
        std::env::temp_dir().join(format!("agend-opencode-redrive-single-{}", Uuid::new_v4()));
    let locator = redrive_locator(port);
    let mut adapter = OpenCodeNativeShared::new(&home, "agent");
    adapter
        .start_or_attach_blocking(locator.clone(), None)
        .expect("attach");

    let first = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "first",
        None,
    );
    let first_id = first.delivery_id;
    let receipt = adapter.deliver_blocking(first).expect("first prompt");
    assert_eq!(receipt.state, DeliveryState::ProtocolAccepted);
    assert_eq!(adapter.in_flight, Some(first_id));

    let second = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "second",
        None,
    );
    let second_id = second.delivery_id;
    assert!(adapter.deliver_blocking(second).is_err());
    let store = ReceiptStore::for_instance(&home, "agent").expect("store");
    assert_eq!(
        store
            .latest(second_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::Queued
    );

    // Idle completion re-drives the parked delivery through the normal
    // submit path: Queued -> ProtocolAccepted with a fresh wire identity.
    let event = adapter
        .complete(first_id, "session.idle")
        .expect("complete");
    assert!(matches!(
        event,
        BackendEvent::Completed { delivery_id: id, .. } if id == first_id
    ));
    assert_eq!(adapter.in_flight, Some(second_id));
    assert!(adapter.parked.is_empty());
    let receipt = store.latest(second_id).expect("latest").expect("receipt");
    assert_eq!(receipt.state, DeliveryState::ProtocolAccepted);
    assert!(receipt.protocol_request_id.is_some());

    // The re-driven turn completes normally.
    let event = adapter
        .complete(second_id, "session.idle")
        .expect("complete");
    assert!(matches!(
        event,
        BackendEvent::Completed { delivery_id: id, .. } if id == second_id
    ));
    assert_eq!(
        store
            .latest(second_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::Completed
    );
    assert_eq!(
        store
            .latest(first_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::Completed
    );

    let texts = server.join().expect("server");
    assert_eq!(texts, vec!["first".to_string(), "second".to_string()]);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn parked_deliveries_redrive_fifo() {
    let (port, server, _prompt_rx) = redrive_capture_server(3);
    let home = std::env::temp_dir().join(format!("agend-opencode-redrive-fifo-{}", Uuid::new_v4()));
    let locator = redrive_locator(port);
    let mut adapter = OpenCodeNativeShared::new(&home, "agent");
    adapter
        .start_or_attach_blocking(locator.clone(), None)
        .expect("attach");

    let first = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "first",
        None,
    );
    let first_id = first.delivery_id;
    adapter.deliver_blocking(first).expect("first prompt");
    let second = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "second",
        None,
    );
    let second_id = second.delivery_id;
    let third = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "third",
        None,
    );
    let third_id = third.delivery_id;
    assert!(adapter.deliver_blocking(second).is_err());
    assert!(adapter.deliver_blocking(third).is_err());
    assert_eq!(adapter.parked.len(), 2);

    // First completion re-drives the head of the queue only; the session is
    // busy again so the remainder stays parked in order.
    adapter
        .complete(first_id, "session.idle")
        .expect("complete");
    assert_eq!(adapter.in_flight, Some(second_id));
    assert_eq!(adapter.parked.len(), 1);
    assert_eq!(adapter.parked[0].envelope.delivery_id, third_id);

    // Second completion re-drives the next parked delivery.
    adapter
        .complete(second_id, "session.idle")
        .expect("complete");
    assert_eq!(adapter.in_flight, Some(third_id));
    assert!(adapter.parked.is_empty());

    adapter
        .complete(third_id, "session.idle")
        .expect("complete");
    assert_eq!(adapter.in_flight, None);
    let store = ReceiptStore::for_instance(&home, "agent").expect("store");
    for id in [first_id, second_id, third_id] {
        assert_eq!(
            store.latest(id).expect("latest").expect("receipt").state,
            DeliveryState::Completed,
            "delivery {id} must complete"
        );
    }

    let texts = server.join().expect("server");
    assert_eq!(
        texts,
        vec![
            "first".to_string(),
            "second".to_string(),
            "third".to_string()
        ]
    );
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn restore_probe_failure_clears_gate_as_ambiguous() {
    let home = std::env::temp_dir().join(format!("agend-opencode-restore-{}", Uuid::new_v4()));
    let locator = SessionLocator::opencode(
        "http://127.0.0.1:4096".to_string(),
        None,
        "opencode".to_string(),
        "secret".to_string(),
    );
    let envelope =
        DeliveryEnvelope::new("agent", locator, DeliveryKind::Prompt, "restore me", None);
    let delivery_id = envelope.delivery_id;
    let store = ReceiptStore::for_instance(&home, "agent").expect("store");
    store.record_queued(&envelope).expect("queued");
    store
        .record(DeliveryReceipt::for_state(
            &envelope,
            DeliveryState::ProtocolAccepted,
        ))
        .expect("accepted");

    let mut adapter = OpenCodeNativeShared::new(&home, "agent");
    adapter.pending.insert(delivery_id, envelope);
    adapter.in_flight = Some(delivery_id);
    adapter.restore_pending_state().expect("restore");
    assert_eq!(adapter.in_flight, None);
    assert_eq!(
        store
            .latest(delivery_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::Ambiguous
    );
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn restore_reconciles_msg_prefixed_history_target() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
    let port = listener.local_addr().expect("address").port();
    let delivery_id = Uuid::new_v4();
    let wire_message_id = "msg_1700000000000000000".to_string();
    let expected_wire_message_id = wire_message_id.clone();
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            let (header, _) = read_http_request(&stream);
            let request_line = header.lines().next().unwrap_or_default();
            if request_line.starts_with("GET /session/session-1/message?limit=100 ") {
                json_response(
                    &mut stream,
                    "200 OK",
                    json!([{"info": {"id": wire_message_id}}]),
                );
            } else if request_line.starts_with("GET /session/status ") {
                json_response(
                    &mut stream,
                    "200 OK",
                    json!({"session-1": {"type": "idle"}}),
                );
            } else {
                panic!("unexpected request: {request_line}");
            }
        }
    });

    let home = std::env::temp_dir().join(format!(
        "agend-opencode-restore-msg-target-{}",
        Uuid::new_v4()
    ));
    let locator = SessionLocator::opencode(
        format!("http://127.0.0.1:{port}"),
        Some("session-1".to_string()),
        "opencode".to_string(),
        "secret".to_string(),
    );
    let mut envelope = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "restore me",
        None,
    );
    envelope.delivery_id = delivery_id;
    let store = ReceiptStore::for_instance(&home, "agent").expect("store");
    store.record_queued(&envelope).expect("queued");
    let mut accepted = DeliveryReceipt::for_state(&envelope, DeliveryState::ProtocolAccepted);
    accepted.protocol_request_id = Some(expected_wire_message_id);
    store.record(accepted).expect("accepted");

    let mut adapter = OpenCodeNativeShared::new(&home, "agent");
    adapter.locator = Some(locator);
    adapter.pending.insert(delivery_id, envelope);
    adapter.in_flight = Some(delivery_id);
    adapter.restore_pending_state().expect("restore");
    assert_eq!(adapter.in_flight, None);
    let receipt = store.latest(delivery_id).expect("latest").expect("receipt");
    assert_eq!(receipt.delivery_id, delivery_id);
    assert_eq!(receipt.state, DeliveryState::Completed);
    server.join().expect("server");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn restore_idle_without_target_history_proof_is_ambiguous() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let port = listener.local_addr().expect("address").port();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut responses = 0;
        while responses < 2 && Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("accept: {error}"),
            };
            let (header, _) = read_http_request(&stream);
            let request_line = header.lines().next().unwrap_or_default();
            if request_line.starts_with("GET /session/session-1/message?limit=100 ") {
                json_response(
                    &mut stream,
                    "200 OK",
                    json!([{"id": Uuid::new_v4().to_string()}]),
                );
            } else if request_line.starts_with("GET /session/status ") {
                json_response(
                    &mut stream,
                    "200 OK",
                    json!({"session-1": {"type": "idle"}}),
                );
            } else {
                panic!("unexpected request: {request_line}");
            }
            responses += 1;
        }
    });

    let home = std::env::temp_dir().join(format!(
        "agend-opencode-restore-target-proof-{}",
        Uuid::new_v4()
    ));
    let locator = SessionLocator::opencode(
        format!("http://127.0.0.1:{port}"),
        Some("session-1".to_string()),
        "opencode".to_string(),
        "secret".to_string(),
    );
    let envelope = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "restore me",
        None,
    );
    let delivery_id = envelope.delivery_id;
    let store = ReceiptStore::for_instance(&home, "agent").expect("store");
    store.record_queued(&envelope).expect("queued");
    store
        .record(DeliveryReceipt::for_state(
            &envelope,
            DeliveryState::ProtocolAccepted,
        ))
        .expect("accepted");

    let mut adapter = OpenCodeNativeShared::new(&home, "agent");
    adapter.locator = Some(locator);
    adapter.restore_pending_state().expect("restore");
    assert_eq!(adapter.in_flight, None);
    assert_eq!(
        store
            .latest(delivery_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::Ambiguous
    );
    server.join().expect("server");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn restore_unknown_status_clears_gate_as_ambiguous_without_recurrence() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
    let port = listener.local_addr().expect("address").port();
    let delivery_id = Uuid::new_v4();
    let wire_message_id = opencode_message_id(delivery_id);
    let expected_wire = wire_message_id.clone();
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            let (header, _) = read_http_request(&stream);
            let request_line = header.lines().next().unwrap_or_default();
            if request_line.starts_with("GET /session/session-1/message?limit=100 ") {
                json_response(
                    &mut stream,
                    "200 OK",
                    json!([{"info": {"id": expected_wire}}]),
                );
            } else if request_line.starts_with("GET /session/status ") {
                json_response(
                    &mut stream,
                    "200 OK",
                    json!({"session-1": {"type": "starting"}}),
                );
            } else {
                panic!("unexpected request: {request_line}");
            }
        }
    });

    let home = std::env::temp_dir().join(format!("agend-oc-unknown-clear-{}", Uuid::new_v4()));
    let locator = SessionLocator::opencode(
        format!("http://127.0.0.1:{port}"),
        Some("session-1".to_string()),
        "opencode".to_string(),
        "secret".to_string(),
    );
    let mut envelope = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "restore me",
        None,
    );
    envelope.delivery_id = delivery_id;
    let store = ReceiptStore::for_instance(&home, "agent").expect("store");
    store.record_queued(&envelope).expect("queued");
    let mut accepted = DeliveryReceipt::for_state(&envelope, DeliveryState::ProtocolAccepted);
    accepted.protocol_request_id = Some(wire_message_id);
    store.record(accepted).expect("accepted");

    let mut adapter = OpenCodeNativeShared::new(&home, "agent");
    adapter.locator = Some(locator);
    adapter.pending.insert(delivery_id, envelope);
    adapter.in_flight = Some(delivery_id);
    adapter.restore_pending_state().expect("restore");
    assert_eq!(
        adapter.in_flight, None,
        "an unknown status must not install a permanent in-flight gate"
    );
    assert!(!adapter.pending.contains_key(&delivery_id));
    assert!(!adapter.target_confirmed.contains(&delivery_id));
    assert_eq!(
        store
            .latest(delivery_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::Ambiguous
    );

    // An SSE reconnect reruns restore. The Ambiguous receipt is terminal, so
    // it is never a restore candidate and the bogus gate cannot reappear.
    adapter.restore_pending_state().expect("restore rerun");
    assert_eq!(
        adapter.in_flight, None,
        "a reconnect rerun must not re-install the gate"
    );
    assert!(!adapter.pending.contains_key(&delivery_id));

    server.join().expect("server");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn restore_unknown_status_reopens_gate_for_a_fresh_delivery() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
    let port = listener.local_addr().expect("address").port();
    let restored_id = Uuid::new_v4();
    let restored_wire = opencode_message_id(restored_id);
    let (prompt_tx, prompt_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().expect("accept");
            let (header, body) = read_http_request(&stream);
            let request_line = header.lines().next().unwrap_or_default();
            if request_line.starts_with("GET /session/session-1/message?limit=100 ") {
                json_response(
                    &mut stream,
                    "200 OK",
                    json!([{"info": {"id": restored_wire}}]),
                );
            } else if request_line.starts_with("GET /session/status ") {
                json_response(
                    &mut stream,
                    "200 OK",
                    json!({"session-1": {"type": "starting"}}),
                );
            } else if request_line.starts_with("POST /session/session-1/prompt_async ") {
                let prompt = serde_json::from_slice::<Value>(&body).expect("prompt json");
                prompt_tx.send(prompt).expect("prompt capture");
                stream
                    .write_all(
                        b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .expect("prompt response");
                stream.flush().expect("prompt flush");
            } else {
                panic!("unexpected request: {request_line}");
            }
        }
    });

    let home = std::env::temp_dir().join(format!("agend-oc-unknown-reopen-{}", Uuid::new_v4()));
    let locator = SessionLocator::opencode(
        format!("http://127.0.0.1:{port}"),
        Some("session-1".to_string()),
        "opencode".to_string(),
        "secret".to_string(),
    );
    let mut restored = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "restore me",
        None,
    );
    restored.delivery_id = restored_id;
    let store = ReceiptStore::for_instance(&home, "agent").expect("store");
    store.record_queued(&restored).expect("queued");
    let mut accepted = DeliveryReceipt::for_state(&restored, DeliveryState::ProtocolAccepted);
    accepted.protocol_request_id = Some(opencode_message_id(restored_id));
    store.record(accepted).expect("accepted");

    let mut adapter = OpenCodeNativeShared::new(&home, "agent");
    adapter.locator = Some(locator.clone());
    adapter.pending.insert(restored_id, restored);
    adapter.in_flight = Some(restored_id);
    adapter.restore_pending_state().expect("restore");
    assert_eq!(adapter.in_flight, None);

    // The gate is open, so the next delivery submits instead of parking
    // forever behind a phantom in-flight turn.
    adapter.ready = true;
    let fresh = DeliveryEnvelope::new(
        "agent",
        locator,
        DeliveryKind::Prompt,
        "after restore",
        None,
    );
    let fresh_id = fresh.delivery_id;
    let receipt = adapter
        .deliver_blocking(fresh)
        .expect("fresh delivery submits");
    assert_eq!(receipt.state, DeliveryState::ProtocolAccepted);
    assert_eq!(adapter.in_flight, Some(fresh_id));
    assert!(adapter.parked.is_empty());
    assert_eq!(
        store
            .latest(fresh_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::ProtocolAccepted
    );
    let prompt = prompt_rx.recv().expect("prompt captured");
    assert_eq!(
        prompt.pointer("/parts/0/text").and_then(Value::as_str),
        Some("after restore")
    );

    server.join().expect("server");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn parked_queue_ages_to_fail_closed_without_completion_event() {
    // The status probe can never prove the claimed turn is active (unknown),
    // so the bounded aging must advance attempts and fail closed. Three
    // sweeps reach the probe: attempts 2, attempts 3, then the cap.
    let (port, server) = status_probe_server(vec!["starting", "starting", "starting"]);
    let home = std::env::temp_dir().join(format!("agend-opencode-park-aging-{}", Uuid::new_v4()));
    let locator = redrive_locator(port);
    let mut adapter = OpenCodeNativeShared::new(&home, "agent");
    adapter.locator = Some(locator.clone());
    adapter.ready = true;
    // A turn that claims the session but will never emit a completion event.
    let stuck = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "stuck",
        None,
    );
    adapter.in_flight = Some(stuck.delivery_id);
    adapter.pending.insert(stuck.delivery_id, stuck);

    let parked = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "queued",
        None,
    );
    let parked_id = parked.delivery_id;
    assert!(adapter.deliver_blocking(parked).is_err());
    assert_eq!(adapter.parked.len(), 1);
    assert_eq!(adapter.parked[0].attempts, 1);

    // A young park is a legitimately busy collision: the sweep must not
    // steal an attempt from it (and must not probe).
    adapter.sweep_parked_aging().expect("young sweep");
    assert_eq!(adapter.parked.len(), 1);
    assert_eq!(adapter.parked[0].attempts, 1);

    let store = ReceiptStore::for_instance(&home, "agent").expect("store");
    // Without any completion event, each aging window advances exactly one
    // attempt; the queue stays bounded and fails closed at the cap.
    for expected in 2..=MAX_PARKED_REDRIVE_ATTEMPTS {
        age_parked(&mut adapter);
        adapter.sweep_parked_aging().expect("aging sweep");
        assert_eq!(
            adapter.parked.len(),
            1,
            "still parked at attempt {expected}"
        );
        assert_eq!(adapter.parked[0].attempts, expected);
        assert_eq!(
            store
                .latest(parked_id)
                .expect("latest")
                .expect("receipt")
                .state,
            DeliveryState::Queued
        );
    }
    age_parked(&mut adapter);
    adapter.sweep_parked_aging().expect("final sweep");
    assert!(
        adapter.parked.is_empty(),
        "aging must reach the fail-closed cap"
    );
    assert_eq!(
        store
            .latest(parked_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::Failed
    );
    server.join().expect("server");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn aging_sweep_spares_a_genuinely_busy_turn_until_completion() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (prompt_tx, prompt_rx) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        // Call 1: the aging sweep probes a genuinely busy turn.
        // Call 2: the real completion event then re-drives the parked prompt.
        for step in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            let (header, body) = read_http_request(&stream);
            let request_line = header.lines().next().unwrap_or_default();
            if step == 0 {
                assert!(
                    request_line.starts_with("GET /session/status "),
                    "unexpected probe request: {request_line}"
                );
                json_response(
                    &mut stream,
                    "200 OK",
                    json!({"session-1": {"type": "busy"}}),
                );
            } else {
                assert!(
                    request_line.starts_with("POST /session/session-1/prompt_async "),
                    "unexpected redrive request: {request_line}"
                );
                let prompt = serde_json::from_slice::<Value>(&body).expect("prompt json");
                prompt_tx.send(prompt).expect("prompt capture");
                stream
                    .write_all(
                        b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .expect("prompt response");
                stream.flush().expect("prompt flush");
            }
        }
    });

    let home = std::env::temp_dir().join(format!("agend-opencode-park-busy-{}", Uuid::new_v4()));
    let locator = redrive_locator(port);
    let mut adapter = OpenCodeNativeShared::new(&home, "agent");
    adapter.locator = Some(locator.clone());
    adapter.ready = true;
    // A turn that is genuinely still running; its completion event will come.
    let long_turn = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "long turn",
        None,
    );
    let long_id = long_turn.delivery_id;
    adapter.in_flight = Some(long_id);
    adapter.pending.insert(long_id, long_turn);

    let parked = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "queued",
        None,
    );
    let parked_id = parked.delivery_id;
    assert!(adapter.deliver_blocking(parked).is_err());
    assert_eq!(adapter.parked.len(), 1);
    assert_eq!(adapter.parked[0].attempts, 1);

    // Aged past the window, but the probe proves the turn is still busy: the
    // sweep must not consume an attempt or fail the delivery closed.
    age_parked(&mut adapter);
    adapter.sweep_parked_aging().expect("busy sweep");
    assert_eq!(
        adapter.parked.len(),
        1,
        "a busy turn keeps its queue parked"
    );
    assert_eq!(
        adapter.parked[0].attempts, 1,
        "a busy turn must not consume an attempt"
    );
    assert!(
        adapter.parked[0].parked_at.elapsed() < PARKED_REDRIVE_AGING,
        "the busy probe must refresh the aging window"
    );
    assert_eq!(adapter.in_flight, Some(long_id));

    // The completion event arrives: redrive proceeds normally.
    adapter.complete(long_id, "session.idle").expect("complete");
    assert_eq!(adapter.in_flight, Some(parked_id));
    assert!(adapter.parked.is_empty());
    let store = ReceiptStore::for_instance(&home, "agent").expect("store");
    assert_eq!(
        store
            .latest(parked_id)
            .expect("latest")
            .expect("receipt")
            .state,
        DeliveryState::ProtocolAccepted
    );
    let prompt = prompt_rx.recv().expect("prompt captured");
    assert_eq!(
        prompt.pointer("/parts/0/text").and_then(Value::as_str),
        Some("queued")
    );
    server.join().expect("server");
    let _ = std::fs::remove_dir_all(home);
}

/// Deterministic `/session/status` probe server: answers each accepted
/// connection with the next status type from `types` (`{"session-1": {"type":
/// <type>}}`), then exits.
fn status_probe_server(types: Vec<&'static str>) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
    let port = listener.local_addr().expect("address").port();
    let server = thread::spawn(move || {
        for status_type in types {
            let (mut stream, _) = listener.accept().expect("accept");
            let (header, _) = read_http_request(&stream);
            let request_line = header.lines().next().unwrap_or_default();
            assert!(
                request_line.starts_with("GET /session/status "),
                "unexpected probe request: {request_line}"
            );
            json_response(
                &mut stream,
                "200 OK",
                json!({"session-1": {"type": status_type}}),
            );
        }
    });
    (port, server)
}

fn age_parked(adapter: &mut OpenCodeNativeShared) {
    let aged = Instant::now()
        .checked_sub(PARKED_REDRIVE_AGING + Duration::from_secs(1))
        .expect("representable instant");
    for parked in adapter.parked.iter_mut() {
        parked.parked_at = aged;
    }
}

fn read_http_request(mut stream: &TcpStream) -> (String, Vec<u8>) {
    stream
        .set_nonblocking(false)
        .expect("blocking accepted stream");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).expect("request headers");
        assert!(read > 0, "request ended before headers");
        bytes.extend_from_slice(&chunk[..read]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
    };
    let header = String::from_utf8(bytes[..header_end].to_vec()).expect("request header");
    let content_length = header
        .lines()
        .find_map(|line| {
            line.strip_prefix("Content-Length:")
                .or_else(|| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = bytes[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).expect("request body");
        assert!(read > 0, "request body ended early");
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    (header, body)
}

fn json_response(stream: &mut TcpStream, status: &str, value: Value) {
    let body = serde_json::to_vec(&value).expect("response json");
    write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("response headers");
    stream.write_all(&body).expect("response body");
    stream.flush().expect("response flush");
}

#[test]
fn prompt_async_wire_and_sse_stream_share_one_session() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
    let port = listener.local_addr().expect("address").port();
    let (prompt_tx, prompt_rx) = std::sync::mpsc::channel();
    let delivery_id = Uuid::nil();
    let event_message_id = opencode_message_id(delivery_id);
    let persisted_seed_ms = current_opencode_timestamp().saturating_add(60_000);
    let persisted_low =
        next_opencode_message_id_at(None, persisted_seed_ms).expect("persisted low ID");
    let persisted_high =
        next_opencode_message_id_at(None, persisted_seed_ms + 1_000).expect("persisted high ID");
    let persisted_wire_message_id = persisted_high.clone();
    let server_persisted_wire_message_id = persisted_wire_message_id.clone();
    let server = thread::spawn(move || {
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().expect("accept");
            let (header, body) = read_http_request(&stream);
            let request_line = header.lines().next().unwrap_or_default().to_string();
            if request_line.starts_with("GET /global/health ") {
                json_response(
                    &mut stream,
                    "200 OK",
                    json!({"healthy": true, "version": "1.17.5"}),
                );
            } else if request_line.starts_with("GET /session/session-1 ") {
                json_response(&mut stream, "200 OK", json!({"id": "session-1"}));
            } else if request_line.starts_with("GET /event ") {
                write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n"
                    )
                    .expect("event headers");
                let events = [
                    json!({"type": "server.connected"}),
                    json!({"type": "message.updated", "properties": {"sessionID": "session-1", "info": {"id": event_message_id}}}),
                    json!({"type": "session.status", "properties": {"sessionID": "session-1", "status": {"type": "busy"}}}),
                    json!({"type": "session.idle", "properties": {"sessionID": "session-1"}}),
                ];
                for event in events {
                    let payload = format!("data: {}\n\n", event);
                    write!(stream, "{:X}\r\n", payload.len()).expect("event size");
                    stream.write_all(payload.as_bytes()).expect("event payload");
                    stream.write_all(b"\r\n").expect("event trailer");
                }
                stream.flush().expect("event flush");
            } else if request_line.starts_with("POST /session/session-1/prompt_async ") {
                let prompt = serde_json::from_slice::<Value>(&body).expect("prompt json");
                let message_id = prompt
                    .get("messageID")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !message_id.starts_with("msg_")
                    || message_id <= server_persisted_wire_message_id.as_str()
                {
                    json_response(
                        &mut stream,
                        "400 Bad Request",
                        json!({"data": {"message": "Expected an ID newer than the persisted request"}, "token": "do-not-log"}),
                    );
                } else {
                    prompt_tx.send((header, body)).expect("prompt capture");
                    stream
                            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .expect("prompt response");
                }
                stream.flush().expect("prompt flush");
            }
        }
    });

    let home = std::env::temp_dir().join(format!("agend-opencode-wire-{}", Uuid::new_v4()));
    let locator = SessionLocator::opencode(
        format!("http://127.0.0.1:{port}"),
        Some("session-1".to_string()),
        "opencode".to_string(),
        "secret".to_string(),
    );
    let mut locator = locator;
    locator.managed = false;
    let store = ReceiptStore::for_instance(&home, "agent").expect("receipt store");
    for (index, protocol_request_id) in [&persisted_low, &persisted_high].into_iter().enumerate() {
        let seed = DeliveryEnvelope::new(
            "agent",
            locator.clone(),
            DeliveryKind::Prompt,
            format!("persisted-seed-{index}"),
            None,
        );
        store.record_queued(&seed).expect("seed queued");
        let mut receipt = DeliveryReceipt::for_state(&seed, DeliveryState::Completed);
        receipt.protocol_request_id = Some(protocol_request_id.clone());
        store.record(receipt).expect("seed completed");
    }
    let mut adapter = OpenCodeNativeShared::new(&home, "agent");
    adapter
        .start_or_attach_blocking(locator.clone(), None)
        .expect("attach");
    let mut envelope =
        DeliveryEnvelope::new("agent", locator, DeliveryKind::Prompt, "hello\n世界", None);
    envelope.delivery_id = delivery_id;
    let receipt = adapter.deliver_blocking(envelope).expect("prompt");
    assert_eq!(receipt.state, DeliveryState::ProtocolAccepted);
    assert_eq!(receipt.delivery_id, delivery_id);
    let (header, body) = prompt_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("prompt request");
    assert!(header.contains("Authorization: Basic "));
    let prompt = serde_json::from_slice::<Value>(&body).expect("prompt json");
    let message_id = prompt
        .get("messageID")
        .and_then(Value::as_str)
        .expect("wire message id");
    assert!(message_id.starts_with("msg_"));
    assert_eq!(message_id.len(), 4 + 12 + 14);
    assert!(message_id > persisted_wire_message_id.as_str());
    assert_eq!(receipt.protocol_request_id.as_deref(), Some(message_id));
    assert_eq!(
        prompt.get("parts"),
        Some(&json!([{"type": "text", "text": "hello\n世界"}]))
    );
    assert!(matches!(
        adapter.next_event_blocking().expect("connected"),
        BackendEvent::Ready
    ));
    assert!(matches!(
        adapter.next_event_blocking().expect("observed"),
        BackendEvent::ObservedInSession { delivery_id: id, .. } if id == delivery_id
    ));
    assert!(
        matches!(adapter.next_event_blocking().expect("busy"), BackendEvent::TurnStarted { delivery_id: id, .. } if id == delivery_id)
    );
    assert!(
        matches!(adapter.next_event_blocking().expect("idle"), BackendEvent::Completed { delivery_id: id, .. } if id == delivery_id)
    );
    server.join().expect("server");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn resident_event_loop_consumes_target_receipt_without_manual_polling() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let port = listener.local_addr().expect("address").port();
    let prompt_seen = Arc::new(AtomicBool::new(false));
    let event_sent = Arc::new(AtomicBool::new(false));
    let server_stop = Arc::new(AtomicBool::new(false));
    let server_prompt_seen = Arc::clone(&prompt_seen);
    let server_event_sent = Arc::clone(&event_sent);
    let server_stop_flag = Arc::clone(&server_stop);
    let delivery_id = Uuid::new_v4();
    let wire_message_id = opencode_message_id(delivery_id);
    let server = thread::spawn(move || {
        let mut handlers = Vec::new();
        while !server_stop_flag.load(Ordering::Acquire) {
            let (stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("accept: {error}"),
            };
            stream
                .set_nonblocking(false)
                .expect("accepted request stream must be blocking");
            let prompt_seen = Arc::clone(&server_prompt_seen);
            let event_sent = Arc::clone(&server_event_sent);
            let stop = Arc::clone(&server_stop_flag);
            let wire_message_id = wire_message_id.clone();
            handlers.push(thread::spawn(move || {
                    let (header, body) = read_http_request(&stream);
                    let request_line = header.lines().next().unwrap_or_default().to_string();
                    if request_line.starts_with("GET /global/health ") {
                        let mut stream = stream;
                        json_response(
                            &mut stream,
                            "200 OK",
                            json!({"healthy": true, "version": "1.17.5"}),
                        );
                    } else if request_line.starts_with("GET /session/session-1 ") {
                        let mut stream = stream;
                        json_response(&mut stream, "200 OK", json!({"id": "session-1"}));
                    } else if request_line.starts_with("POST /session/session-1/prompt_async ") {
                        assert!(!body.is_empty(), "prompt body must be present");
                        prompt_seen.store(true, Ordering::Release);
                        let mut stream = stream;
                        stream
                            .write_all(
                                b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .expect("prompt response");
                        stream.flush().expect("prompt flush");
                    } else if request_line.starts_with("GET /event ") {
                        let mut stream = stream;
                        write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n"
                        )
                        .expect("event headers");
                        let connected = format!(
                            "data: {}\n\n",
                            json!({"type": "server.connected"})
                        );
                        write!(stream, "{:X}\r\n", connected.len()).expect("connected size");
                        stream
                            .write_all(connected.as_bytes())
                            .expect("connected event");
                        stream.write_all(b"\r\n").expect("connected trailer");
                        stream.flush().expect("connected flush");
                        while !stop.load(Ordering::Acquire) {
                            if prompt_seen.load(Ordering::Acquire)
                                && !event_sent.swap(true, Ordering::AcqRel)
                            {
                                let events = [
                                    json!({"type": "message.updated", "properties": {"sessionID": "session-1", "info": {"id": wire_message_id}}}),
                                    json!({"type": "session.status", "properties": {"sessionID": "session-1", "status": {"type": "busy"}}}),
                                    json!({"type": "session.idle", "properties": {"sessionID": "session-1"}}),
                                ];
                                for event in events {
                                    let payload = format!("data: {}\n\n", event);
                                    write!(stream, "{:X}\r\n", payload.len())
                                        .expect("event size");
                                    stream
                                        .write_all(payload.as_bytes())
                                        .expect("event payload");
                                    stream.write_all(b"\r\n").expect("event trailer");
                                }
                                stream.flush().expect("event flush");
                            }
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    } else {
                        panic!("unexpected request: {request_line}");
                    }
                }));
        }
        for handler in handlers {
            handler.join().expect("request handler");
        }
    });

    let home = std::env::temp_dir().join(format!("agend-opencode-resident-{}", Uuid::new_v4()));
    let locator = SessionLocator::opencode(
        format!("http://127.0.0.1:{port}"),
        Some("session-1".to_string()),
        "opencode".to_string(),
        "secret".to_string(),
    );
    let mut locator = locator;
    locator.managed = false;
    let mut envelope = DeliveryEnvelope::new(
        "agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "resident hello",
        None,
    );
    envelope.delivery_id = delivery_id;

    prepare_resident_tui(&home, "agent", locator, None).expect("resident attach");
    let receipt = deliver_resident(&home, "agent", envelope).expect("resident prompt");
    assert_eq!(receipt.state, DeliveryState::ProtocolAccepted);

    let store = ReceiptStore::for_instance(&home, "agent").expect("receipt store");
    let deadline = Instant::now() + Duration::from_secs(2);
    let completed = loop {
        if store
            .latest(delivery_id)
            .expect("latest receipt")
            .is_some_and(|receipt| receipt.state == DeliveryState::Completed)
        {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(
        completed,
        "resident SSE worker must settle the target receipt"
    );

    server_stop.store(true, Ordering::Release);
    stop_instance_server(&home, "agent");
    server.join().expect("server");
    assert!(event_sent.load(Ordering::Acquire));
    let _ = std::fs::remove_dir_all(home);
}

// This smoke helper is intentionally not run by default. It documents the
// wire shape expected by the live acceptance test without starting a real
// OpenCode binary in the unit-test process.
#[allow(dead_code)]
fn _read_request_line(listener: &TcpListener) -> String {
    let (stream, _) = listener.accept().expect("accept");
    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).expect("request line");
    line
}

#[allow(dead_code)]
fn _server_thread(listener: TcpListener) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let _ = _read_request_line(&listener);
    })
}

/// #3414 RED (lead review of 06faebad). Clearing `locator.session_id` in
/// `prepare_opencode_tui_session` is not enough: `resident_adapter` returns an
/// ALREADY-RESIDENT worker for the same key and drops the freshly prepared
/// locator on the floor, so the old session id survives a fresh restart.
///
/// The fix only ever worked for the cold path — no resident worker — which is
/// exactly the case a RESTART is not.
mod fresh_restart_resident_session_3414 {
    use super::super::*;
    use crate::backend::SpawnMode;

    #[allow(clippy::expect_used)]
    fn scratch_home(tag: &str) -> std::path::PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let home = std::env::temp_dir().join(format!(
            "agend-3414-resident-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&home).expect("scratch home");
        home
    }

    /// A hermetic stand-in for `opencode serve`: enough of the HTTP surface for
    /// `start_or_attach_blocking` to complete, and nothing else.
    ///
    /// Paired with `managed: false` on the locator, which is what stops
    /// `wait_for_server` from ever launching a child
    /// (`opencode_server.rs:888`). The test therefore needs no installed
    /// binary and no network beyond loopback.
    ///
    /// Answers exactly what the adapter asks for:
    /// - `GET /global/health`   -> healthy + a version (required, else Err)
    /// - `GET /session/{id}`    -> 404 for the STALE id, 200 for one we issued
    /// - `POST /session`        -> a fresh id, recorded so the test can assert it
    /// - `GET /event`           -> an SSE stream held open
    struct FakeOpenCode {
        port: u16,
        created: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
    }

    impl FakeOpenCode {
        #[allow(clippy::expect_used)]
        fn start(known_session: Option<&str>) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake opencode");
            let port = listener.local_addr().expect("addr").port();
            listener
                .set_nonblocking(true)
                .expect("nonblocking fake listener");
            let created: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let known = known_session.map(str::to_string);
            let worker_created = Arc::clone(&created);
            let worker_stop = Arc::clone(&stop);
            // fire-and-forget: bounded by `stop`, which every test sets in Drop.
            std::thread::spawn(move || {
                let mut issued: Vec<String> = Vec::new();
                if let Some(known) = known.clone() {
                    issued.push(known);
                }
                while !worker_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let mut known_ids = issued.clone();
                            known_ids.extend(worker_created.lock().clone());
                            Self::serve_one(stream, known_ids, Arc::clone(&worker_created));
                            issued = worker_created.lock().clone();
                            if let Some(known) = known.clone() {
                                issued.push(known);
                            }
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                port,
                created,
                stop,
            }
        }

        fn serve_one(
            mut stream: std::net::TcpStream,
            known_ids: Vec<String>,
            created: Arc<Mutex<Vec<String>>>,
        ) {
            use std::io::{BufRead, BufReader, Write};
            let peek = match stream.try_clone() {
                Ok(s) => s,
                Err(_) => return,
            };
            let mut reader = BufReader::new(peek);
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).is_err() {
                return;
            }
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or_default().to_string();
            let path = parts.next().unwrap_or_default().to_string();
            // Drain headers so the peer's write completes.
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) if line.trim().is_empty() => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let respond = |stream: &mut std::net::TcpStream, code: &str, body: String| {
                let _ = write!(
                    stream,
                    "HTTP/1.1 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.flush();
            };
            if path == "/global/health" {
                respond(
                    &mut stream,
                    "200 OK",
                    "{\"healthy\":true,\"version\":\"0.0.0-test\"}".to_string(),
                );
            } else if path == "/event" {
                // Hold the SSE stream open; the adapter only needs it to connect.
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n"
                );
                let _ = stream.flush();
                std::thread::sleep(std::time::Duration::from_millis(200));
            } else if method == "POST" && path == "/session" {
                let id = format!("ses-fake-{}", created.lock().len());
                created.lock().push(id.clone());
                respond(&mut stream, "200 OK", format!("{{\"id\":\"{id}\"}}"));
            } else if let Some(id) = path.strip_prefix("/session/") {
                if known_ids.iter().any(|k| k == id) {
                    respond(&mut stream, "200 OK", format!("{{\"id\":\"{id}\"}}"));
                } else {
                    respond(&mut stream, "404 Not Found", "{}".to_string());
                }
            } else {
                respond(&mut stream, "200 OK", "{}".to_string());
            }
        }
    }

    impl Drop for FakeOpenCode {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
        }
    }

    /// Seed the resident map with a worker already holding a stale session, the
    /// state a live OpenCode instance is in when a restart arrives. `join: None`
    /// keeps the fixture free of a real event-loop thread.
    #[allow(clippy::expect_used)]
    fn seed_resident(home: &std::path::Path, instance: &str, session_id: &str, port: u16) {
        let mut adapter = OpenCodeNativeShared::new(home, instance);
        let mut locator = SessionLocator::opencode(
            format!("http://127.0.0.1:{port}"),
            Some(session_id.to_string()),
            "opencode".to_string(),
            "secret".to_string(),
        );
        // Unmanaged: `wait_for_server` only launches a child when `managed`
        // (opencode_server.rs:888), so this keeps the test off any installed
        // binary while still driving the real attach path.
        locator.managed = false;
        // `prepare_opencode_tui_session` resolves the locator from DISK
        // (`locator_for_instance`), not from the resident adapter, so the
        // fixture has to persist it or the production path would fall back to
        // a default managed locator and try to launch a real server.
        crate::transport::registry::save_session_locator(home, instance, &locator)
            .expect("persist fixture locator");
        adapter.locator = Some(locator);
        resident_workers().lock().insert(
            resident_key(home, instance),
            ResidentWorker {
                adapter: Arc::new(Mutex::new(adapter)),
                stop: Arc::new(AtomicBool::new(false)),
                join: None,
            },
        );
    }

    fn drop_resident(home: &std::path::Path, instance: &str) {
        resident_workers()
            .lock()
            .remove(&resident_key(home, instance));
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn fresh_restart_does_not_reuse_a_resident_session_3414() {
        let home = scratch_home("fresh");
        let fake = FakeOpenCode::start(Some("stale-session"));
        seed_resident(&home, "dev", "stale-session", fake.port);

        let prepared = crate::transport::prepare_opencode_tui_session(
            &home,
            "dev",
            None,
            &[],
            SpawnMode::Fresh,
        )
        .expect("fresh preparation");

        // NOT `None`: clearing session_id happens BEFORE resident preparation,
        // and preparation then creates a new conversation via POST /session.
        // The contract is that the OLD session is not inherited — asserting
        // `None` would have been asserting the intermediate state, not the
        // observable one.
        assert_ne!(
            prepared.session_id.as_deref(),
            Some("stale-session"),
            "#3414: a fresh restart must not inherit the resident worker's session"
        );
        assert!(
            prepared.session_id.is_some(),
            "#3414: fresh preparation must still produce a session to talk to"
        );
        assert_eq!(
            fake.created.lock().len(),
            1,
            "#3414: fresh must create exactly one NEW session on the server"
        );
        drop_resident(&home, "dev");
        std::fs::remove_dir_all(&home).ok();
    }

    /// The mirror: Resume is the whole point of keeping a resident worker, so it
    /// must still reuse the session it already has.
    #[test]
    #[allow(clippy::expect_used)]
    fn resume_restart_still_reuses_the_resident_session_3414() {
        let home = scratch_home("resume");
        let fake = FakeOpenCode::start(Some("stale-session"));
        seed_resident(&home, "dev", "stale-session", fake.port);

        let prepared = crate::transport::prepare_opencode_tui_session(
            &home,
            "dev",
            None,
            &[],
            SpawnMode::Resume,
        )
        .expect("resume preparation");

        assert_eq!(
            prepared.session_id.as_deref(),
            Some("stale-session"),
            "#3414: resume must keep attaching to the existing session"
        );
        drop_resident(&home, "dev");
        std::fs::remove_dir_all(&home).ok();
    }
}

/// #3515 follow-up: the cross-transport claim, tested on the OpenCode production
/// path rather than asserted.
///
/// The batching fix works by pre-signalling every instance's resident workers
/// before any of them is joined, and `transport::signal_instance_transport_stop`
/// is the entry the shutdown loop calls. Review r1 accepted the scope split (the
/// linearity is cross-transport; bounding OpenCode's own join needs its own
/// lifecycle design) but noted the OpenCode half had no regression of its own —
/// the only batching test injects Claude `event_workers`.
///
/// This pins both halves of what that entry must do here: the flag is SET, and
/// the worker is still REGISTERED. Signalling must not double as removal —
/// `stop_instance_server` still owns removing and joining it, which is what
/// keeps the speed-up from turning into skipped cleanup.
#[test]
fn signal_instance_transport_stop_signals_opencode_resident_worker_3515() {
    let home = std::env::temp_dir().join(format!("agend-opencode-signal-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("home");
    let instance = "opencode-agent";
    let stop = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&stop);
    let thread_stop = Arc::clone(&stop);
    let join = std::thread::Builder::new()
        .name("test-opencode-resident".to_string())
        .spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        })
        .expect("spawn stand-in resident worker");
    resident_workers().lock().insert(
        resident_key(&home, instance),
        ResidentWorker {
            adapter: Arc::new(parking_lot::Mutex::new(OpenCodeNativeShared::new(
                &home, instance,
            ))),
            stop,
            join: Some(join),
        },
    );

    crate::transport::signal_instance_transport_stop(&home, instance);

    assert!(
        observed.load(Ordering::Acquire),
        "the shutdown pre-signal must reach an OpenCode resident worker, not only \
         the Claude channel one (#3515 follow-up: a mixed fleet keeps the linearity \
         otherwise)"
    );
    assert!(
        resident_workers()
            .lock()
            .contains_key(&resident_key(&home, instance)),
        "pre-signalling must not double as removal — `stop_instance_server` still \
         owns removing and joining the worker"
    );

    stop_instance_server(&home, instance);
    let _ = std::fs::remove_dir_all(&home);
}
