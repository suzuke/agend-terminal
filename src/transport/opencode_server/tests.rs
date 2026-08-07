use super::*;
use std::io::BufRead;
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
fn busy_collision_and_interrupt_are_terminal_failures_not_fake_queue_entries() {
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

    for kind in [DeliveryKind::Prompt, DeliveryKind::Interrupt] {
        let envelope = DeliveryEnvelope::new("agent", locator.clone(), kind, "next", None);
        let delivery_id = envelope.delivery_id;
        assert!(adapter.deliver_blocking(envelope).is_err());
        let receipt = ReceiptStore::for_instance(&home, "agent")
            .expect("store")
            .latest(delivery_id)
            .expect("latest")
            .expect("receipt");
        assert_eq!(receipt.state, DeliveryState::Failed);
    }
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

fn read_http_request(mut stream: &TcpStream) -> (String, Vec<u8>) {
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
                if !message_id.starts_with("msg_") || message_id <= "msg_fdb5a96c3001auuSsUN6in29xP"
                {
                    json_response(
                        &mut stream,
                        "400 Bad Request",
                        json!({"data": {"message": "Expected a lexicographically newer msg_ ID"}, "token": "do-not-log"}),
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
    assert!(message_id > "msg_fdb5a96c3001auuSsUN6in29xP");
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
