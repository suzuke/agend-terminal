use super::*;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::thread;

const STDIO_PROBE_MARKER: &str = "AGEND_CODEX_STDIO_LEAK_MARKER";

#[test]
fn codex_launch_stdio_probe_helper() {
    if std::env::var_os("AGEND_CODEX_STDIO_PROBE_CHILD").is_none() {
        return;
    }
    let helper_ran =
        std::env::var_os("AGEND_CODEX_STDIO_PROBE_RAN").expect("probe helper sentinel path");
    std::fs::write(helper_ran, b"ran").expect("record probe helper execution");

    let root = std::env::temp_dir().join(format!("agend-codex-stdio-{}", Uuid::new_v4()));
    let endpoint = root.join("transport/codex/probe.sock");
    let executable = root.join("fake-codex");
    std::fs::create_dir_all(&root).expect("probe root");
    std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nendpoint=${{3#unix://}}\nprintf '{STDIO_PROBE_MARKER} stdout\\n'\nprintf '{STDIO_PROBE_MARKER} stderr\\n' >&2\ntouch \"$endpoint\"\ntrap 'exit 0' TERM\nwhile :; do sleep 1; done\n"
            ),
        )
        .expect("probe executable");
    let mut permissions = std::fs::metadata(&executable)
        .expect("probe metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&executable, permissions).expect("probe permissions");

    let locator = SessionLocator::codex(endpoint, None);
    let mut child = CodexNativeShared::launch(
        executable.to_str().expect("UTF-8 executable"),
        &locator,
        &root,
        &[],
    )
    .expect("launch fake Codex app-server");
    std::thread::sleep(Duration::from_millis(50));
    child.kill().expect("stop probe child");
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn codex_app_server_output_does_not_escape_to_parent_terminal() {
    let helper_ran =
        std::env::temp_dir().join(format!("agend-codex-stdio-helper-ran-{}", Uuid::new_v4()));
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "transport::codex_app_server::tests::codex_launch_stdio_probe_helper",
            "--nocapture",
        ])
        .env("AGEND_CODEX_STDIO_PROBE_CHILD", "1")
        .env("AGEND_CODEX_STDIO_PROBE_RAN", &helper_ran)
        .output()
        .expect("run isolated stdio probe");

    assert!(helper_ran.exists(), "stdio probe helper did not run");
    let _ = std::fs::remove_file(helper_ran);
    assert!(
        output.status.success(),
        "stdio probe helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !combined.contains(STDIO_PROBE_MARKER),
        "Codex app-server output escaped to the parent terminal: {combined}"
    );
}

/// #3402 regression: the managed app-server is the first Codex process to
/// read workspace config, so its own argv must carry invocation-only MCP
/// and project-trust overrides before the `app-server` subcommand.
#[test]
fn managed_app_server_launch_carries_config_before_subcommand_3402() {
    let root = std::env::temp_dir().join(format!("agend-codex-trust-{}", Uuid::new_v4()));
    let home = root.join("home");
    let workspace = root.join("workspace");
    let endpoint = root.join("transport/codex/trust.sock");
    let executable = root.join("fake-codex");
    let args_log = executable.with_extension("args");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::write(
            &executable,
            "#!/bin/sh\n: > \"$0.args\"\nfor arg in \"$@\"; do\n  printf '%s\\n' \"$arg\" >> \"$0.args\"\n  endpoint=\"$arg\"\ndone\nendpoint=${endpoint#unix://}\ntouch \"$endpoint\"\ntrap 'exit 0' TERM\nsleep 5\n",
        )
        .expect("fake Codex executable");
    let mut permissions = std::fs::metadata(&executable)
        .expect("fake Codex metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&executable, permissions).expect("fake Codex permissions");

    let locator = SessionLocator::codex(endpoint.clone(), None);
    let prepared = prepare_managed_tui(
        &home,
        "codex-3402-server",
        executable.to_str().expect("UTF-8 executable"),
        locator,
        Some(&workspace),
    )
    .expect("launch managed fake Codex app-server");

    let argv = std::fs::read_to_string(&args_log).expect("captured app-server argv");
    let args: Vec<&str> = argv.lines().collect();
    let app_server_at = args
        .iter()
        .position(|arg| *arg == "app-server")
        .expect("app-server subcommand");
    let trust_at = args
        .iter()
        .position(|arg| arg.starts_with("projects={"))
        .unwrap_or_else(|| panic!("#3402: app-server trust override missing; argv={args:?}"));
    let mcp_at = args
        .iter()
        .position(|arg| arg.starts_with("mcp_servers.agend-terminal.command="))
        .unwrap_or_else(|| panic!("#3402: app-server MCP override missing; argv={args:?}"));
    assert!(trust_at < app_server_at, "#3402: argv={args:?}");
    assert!(mcp_at < app_server_at, "#3402: argv={args:?}");
    assert!(
        args[trust_at].contains(
            &workspace
                .canonicalize()
                .expect("canonical workspace")
                .to_string_lossy()
                .to_string()
        ),
        "#3402: trust must target the canonical workspace; argv={args:?}"
    );

    let mut server = managed_servers()
        .lock()
        .expect("managed server lock")
        .remove(&server_key(&home, "codex-3402-server"))
        .expect("managed fake server");
    server.child.kill().expect("stop managed fake server");
    let _ = server.child.wait();
    let _ = std::fs::remove_file(endpoint);
    assert!(prepared.managed);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn drain_server_output_continues_past_invalid_utf8_to_eof() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    struct EofTrackingReader {
        inner: std::io::Cursor<Vec<u8>>,
        reached_eof: Arc<AtomicBool>,
    }

    impl std::io::Read for EofTrackingReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let read = self.inner.read(buffer)?;
            if read == 0 {
                self.reached_eof.store(true, Ordering::Release);
            }
            Ok(read)
        }
    }

    let reached_eof = Arc::new(AtomicBool::new(false));
    let reader = EofTrackingReader {
        inner: std::io::Cursor::new(b"first\n\xff\xfe\x80\nlast\n".to_vec()),
        reached_eof: Arc::clone(&reached_eof),
    };

    drain_server_output(reader, "stderr");

    assert!(
        reached_eof.load(Ordering::Acquire),
        "invalid UTF-8 must not stop the drain before EOF"
    );
}

fn write_server_frame(stream: &mut std::os::unix::net::UnixStream, value: Value) {
    let body = serde_json::to_vec(&value).expect("serialize frame");
    let mut frame = vec![0x81_u8];
    match body.len() {
        0..=125 => frame.push(body.len() as u8),
        126..=65_535 => {
            frame.push(126);
            frame.extend_from_slice(&(body.len() as u16).to_be_bytes());
        }
        _ => {
            frame.push(127);
            frame.extend_from_slice(&(body.len() as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(&body);
    stream.write_all(&frame).expect("write server frame");
    stream.flush().expect("flush server frame");
}

fn run_fake_codex(endpoint: &Path) -> thread::JoinHandle<()> {
    run_fake_codex_with_loaded_threads(endpoint, vec!["thread-1".to_string()])
}

fn run_fake_codex_with_loaded_threads(
    endpoint: &Path,
    loaded_threads: Vec<String>,
) -> thread::JoinHandle<()> {
    run_fake_codex_with_loaded_threads_and_reads(endpoint, loaded_threads, Vec::new())
}

fn run_fake_codex_with_loaded_threads_and_reads(
    endpoint: &Path,
    loaded_threads: Vec<String>,
    thread_reads: Vec<(String, Option<bool>, Option<String>)>,
) -> thread::JoinHandle<()> {
    let listener = UnixListener::bind(endpoint).expect("bind fake Codex socket");
    // fire-and-forget: the fake app-server owns the socket until the client drains events.
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept fake Codex client");
        let mut handshake = Vec::new();
        let mut suffix = [0_u8; 4];
        loop {
            let mut byte = [0_u8; 1];
            stream.read_exact(&mut byte).expect("read handshake");
            handshake.push(byte[0]);
            suffix.rotate_left(1);
            suffix[3] = byte[0];
            if suffix == *b"\r\n\r\n" {
                break;
            }
        }
        assert!(String::from_utf8_lossy(&handshake).contains("Upgrade: websocket"));
        stream
                .write_all(
                    b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
                )
                .expect("write handshake");
        stream.flush().expect("flush handshake");

        loop {
            // #3535: a refused discovery never sends `turn/start`, so the
            // client may drop the connection right after `thread/loaded/list`.
            // Treat that EOF as a clean test shutdown, not a failure — the
            // `turn/start` arm below still owns the happy-path `break`.
            let (_, body) = match read_websocket_frame(&mut stream) {
                Ok(frame) => frame,
                Err(_) => break,
            };
            let request: Value = serde_json::from_slice(&body).expect("decode client frame");
            let method = request
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let id = request.get("id").cloned();
            match method {
                "initialize" => write_server_frame(
                    &mut stream,
                    json!({
                        "id": id,
                        "result": {
                            "userAgent": "codex/0.146.0",
                            "codexHome": "/tmp/codex",
                            "platformFamily": "unix",
                            "platformOs": "macos"
                        }
                    }),
                ),
                "initialized" => {}
                "thread/resume" => {
                    assert_eq!(
                        request.pointer("/params/excludeTurns"),
                        Some(&Value::Bool(true)),
                        "readiness must rejoin the live thread without downloading turn history"
                    );
                    write_server_frame(
                        &mut stream,
                        json!({"id": id, "result": {"thread": {"id": "thread-1"}}}),
                    );
                }
                "thread/loaded/list" => {
                    let data: Vec<Value> = loaded_threads
                        .iter()
                        .map(|thread_id| json!({"id": thread_id}))
                        .collect();
                    write_server_frame(&mut stream, json!({"id": id, "result": {"data": data}}));
                }
                "thread/read" => {
                    let thread_id = request
                        .pointer("/params/threadId")
                        .and_then(Value::as_str)
                        .expect("thread/read threadId");
                    let (ephemeral, source) = thread_reads
                        .iter()
                        .find(|(id, _, _)| id == thread_id)
                        .map(|(_, ephemeral, source)| (*ephemeral, source.as_deref()))
                        .unwrap_or((Some(false), Some("user")));
                    let mut thread = json!({"id": thread_id});
                    if let Some(ephemeral) = ephemeral {
                        thread["ephemeral"] = Value::Bool(ephemeral);
                    }
                    if let Some(source) = source {
                        thread["threadSource"] = Value::String(source.to_string());
                    }
                    write_server_frame(
                        &mut stream,
                        json!({
                            "id": id,
                            "result": {"thread": thread}
                        }),
                    );
                }
                "thread/start" => {
                    panic!("daemon must not pre-create the visible TUI thread");
                }
                "turn/start" => {
                    write_server_frame(
                        &mut stream,
                        json!({"id": id, "result": {"turn": {"id": "turn-1"}}}),
                    );
                    write_server_frame(
                        &mut stream,
                        json!({"method": "turn/started", "params": {"turn": {"id": "turn-1"}}}),
                    );
                    write_server_frame(
                        &mut stream,
                        json!({"method": "turn/completed", "params": {"turn": {"id": "turn-1"}}}),
                    );
                    break;
                }
                _ => panic!("unexpected method from client: {method}"),
            }
        }
    })
}

#[test]
fn first_delivery_discovers_the_tui_created_thread_without_precreating_one() {
    let home =
        std::env::temp_dir().join(format!("agend-codex-managed-bootstrap-{}", Uuid::new_v4()));
    let endpoint = std::env::temp_dir().join(format!("a-{}.sock", Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("home");
    let server = run_fake_codex(&endpoint);
    let locator = SessionLocator::codex(endpoint.clone(), None);
    let mut adapter = CodexNativeShared::new(&home, "codex-agent");

    let envelope = DeliveryEnvelope::new(
        "codex-agent",
        locator,
        DeliveryKind::Prompt,
        "hello",
        Some("corr-managed".to_string()),
    );
    let accepted = adapter
        .deliver_blocking(envelope)
        .expect("managed thread must accept a structured turn");
    assert_eq!(accepted.state, DeliveryState::ProtocolAccepted);
    assert_eq!(
        accepted.tui_visibility.as_deref(),
        Some("shared_codex_thread")
    );
    let persisted = super::super::registry::load_session_locator(&home, "codex-agent")
        .expect("first delivery must persist the discovered TUI thread");
    assert_eq!(persisted.thread_id.as_deref(), Some("thread-1"));

    server.join().expect("fake server");
    let _ = std::fs::remove_file(endpoint);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn websocket_handshake_turn_and_event_receipts_are_structured() {
    let home = std::env::temp_dir().join(format!("agend-codex-native-{}", Uuid::new_v4()));
    // macOS limits Unix-domain socket paths to SUN_LEN; keep this test
    // endpoint short even though the temporary home path is descriptive.
    let endpoint = std::env::temp_dir().join(format!("a-{}.sock", Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("home");
    let server = run_fake_codex(&endpoint);
    let locator = SessionLocator::codex(endpoint.clone(), Some("thread-1".to_string()));
    let envelope = DeliveryEnvelope::new(
        "codex-agent",
        locator,
        DeliveryKind::Prompt,
        "hello",
        Some("corr-1".to_string()),
    );
    let delivery_id = envelope.delivery_id;
    let mut adapter = CodexNativeShared::new(&home, "codex-agent");
    let accepted = adapter.deliver_blocking(envelope).expect("accepted");
    assert_eq!(accepted.state, DeliveryState::ProtocolAccepted);
    assert_eq!(adapter.active_turn_id.as_deref(), Some("turn-1"));
    assert!(matches!(
        adapter.next_event_blocking().expect("started"),
        BackendEvent::TurnStarted { .. }
    ));
    assert!(matches!(
        adapter.next_event_blocking().expect("completed"),
        BackendEvent::Completed { .. }
    ));
    let store = ReceiptStore::for_instance(&home, "codex-agent").expect("store");
    assert_eq!(
        store.latest(delivery_id).expect("latest").map(|r| r.state),
        Some(DeliveryState::Completed)
    );
    let receipt = store
        .latest(delivery_id)
        .expect("latest receipt")
        .expect("completed receipt");
    assert_eq!(receipt.protocol_request_id.as_deref(), Some("turn-1"));
    assert_eq!(receipt.backend_event.as_deref(), Some("turn/completed"));
    server.join().expect("fake server");
    let _ = std::fs::remove_file(endpoint);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn ordinary_delivery_steers_the_known_active_turn_without_replacing_its_owner() {
    let home = std::env::temp_dir().join(format!("agend-codex-steer-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("home");
    let locator = SessionLocator::codex(
        std::env::temp_dir().join(format!("unused-{}.sock", Uuid::new_v4())),
        Some("thread-1".to_string()),
    );
    let original = DeliveryEnvelope::new(
        "codex-agent",
        locator.clone(),
        DeliveryKind::Prompt,
        "original turn",
        Some("corr-original".to_string()),
    );
    let original_id = original.delivery_id;
    let steered = DeliveryEnvelope::new(
        "codex-agent",
        locator.clone(),
        DeliveryKind::Notification,
        "new assignment",
        Some("corr-steer".to_string()),
    );
    let steered_id = steered.delivery_id;
    let (client, mut server) = std::os::unix::net::UnixStream::pair().expect("socket pair");
    // fire-and-forget: the fake peer is joined after its single response.
    let peer = thread::spawn(move || {
        let Ok((_, body)) = read_websocket_frame(&mut server) else {
            return None;
        };
        let request: Value = serde_json::from_slice(&body).expect("decode request");
        write_server_frame(
            &mut server,
            json!({"id": request.get("id"), "result": {"turn": {"id": "turn-1"}}}),
        );
        Some(request)
    });

    let mut adapter = CodexNativeShared::new(&home, "codex-agent");
    adapter.ready = true;
    adapter.locator = Some(locator);
    adapter.writer = Some(client.try_clone().expect("clone client"));
    adapter.reader = Some(client);
    adapter.in_flight = Some(original_id);
    adapter.active_turn_id = Some("turn-1".to_string());
    adapter.pending.insert(original_id, original);

    let accepted = adapter.deliver_blocking(steered).expect("steer accepted");
    drop(adapter.writer.take());
    drop(adapter.reader.take());
    let request = peer.join().expect("peer").expect("steer request");

    assert_eq!(accepted.state, DeliveryState::ProtocolAccepted);
    assert_eq!(request.get("method"), Some(&json!("turn/steer")));
    assert_eq!(
        request.pointer("/params/expectedTurnId"),
        Some(&json!("turn-1"))
    );
    assert_eq!(adapter.in_flight, Some(original_id));
    assert!(adapter.pending.contains_key(&original_id));
    assert!(!adapter.pending.contains_key(&steered_id));

    let store = ReceiptStore::for_instance(&home, "codex-agent").expect("store");
    assert_eq!(
        store.latest(steered_id).expect("latest").map(|r| r.state),
        Some(DeliveryState::ProtocolAccepted)
    );
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn readiness_failure_records_a_failed_closed_receipt() {
    let home =
        std::env::temp_dir().join(format!("agend-codex-failed-readiness-{}", Uuid::new_v4()));
    let envelope = DeliveryEnvelope::new(
        "codex-agent",
        SessionLocator::codex(
            std::env::temp_dir().join(format!("missing-{}.sock", Uuid::new_v4())),
            Some("thread-1".to_string()),
        ),
        DeliveryKind::Prompt,
        "hello",
        Some("corr-failed".to_string()),
    );
    let delivery_id = envelope.delivery_id;
    let mut adapter = CodexNativeShared::new(&home, "codex-agent");
    assert!(adapter.deliver_blocking(envelope).is_err());

    let store = ReceiptStore::for_instance(&home, "codex-agent").expect("store");
    let receipt = store
        .latest(delivery_id)
        .expect("latest receipt")
        .expect("failed readiness receipt");
    assert_eq!(receipt.state, DeliveryState::Failed);
    let detail = receipt.detail.expect("failed readiness detail");
    assert!(
        detail.starts_with("NativeShared readiness failed closed: "),
        "receipt must preserve the readiness failure class: {detail}"
    );
    assert!(
        detail.contains("No such file or directory"),
        "receipt must preserve the exact readiness cause: {detail}"
    );
    assert!(detail.len() <= 1024, "receipt detail must remain bounded");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn readiness_failure_detail_is_utf8_safe_and_bounded() {
    let error = anyhow::anyhow!("{}", "界".repeat(MAX_READINESS_DETAIL_BYTES));

    let detail = readiness_failure_detail(&error);

    assert!(detail.len() <= MAX_READINESS_DETAIL_BYTES);
    assert!(detail.starts_with("NativeShared readiness failed closed: "));
    assert!(detail.ends_with("..."));
}

#[cfg(unix)]
#[test]
fn persisted_server_identity_rejects_pid_reuse_or_missing_start_token() {
    let pid = std::process::id();
    let token = crate::process::process_start_token(pid).expect("current process token");
    let mut locator = SessionLocator::codex(
        std::env::temp_dir().join("codex-managed.sock"),
        Some("thread-1".to_string()),
    );
    locator.managed = true;
    locator.server_pid = Some(pid);
    locator.server_start_token = Some(token);
    assert!(persisted_server_owned(&locator));
    locator.server_start_token = Some(token.wrapping_add(1));
    assert!(!persisted_server_owned(&locator));
    locator.server_start_token = None;
    assert!(!persisted_server_owned(&locator));
}

#[cfg(unix)]
#[test]
fn stop_owned_process_rejects_identity_mismatch_without_signaling() {
    let home = std::env::temp_dir().join(format!("agend-codex-identity-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("home");
    let marker = home.join("term-marker");
    let mut child = std::process::Command::new("sh")
            .args([
                "-c",
                "ulimit -t 30; trap 'printf term > \"$AGEND_TERM_MARKER\"; exit 0' TERM; while :; do :; done",
            ])
            .env("AGEND_TERM_MARKER", &marker)
            .spawn()
            .expect("identity fixture");
    let pid = child.id();
    let token = crate::process::process_start_token(pid).expect("start token");

    let result = stop_owned_process(pid, token.wrapping_add(1), Some(&mut child));

    assert!(result.is_err(), "identity mismatch must fail closed");
    assert!(
        crate::process::process_start_token(pid).is_some(),
        "mismatched identity must not stop the live child"
    );
    assert!(!marker.exists(), "mismatched identity must not signal TERM");
    child.kill().expect("cleanup identity fixture");
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(home);
}

#[cfg(unix)]
#[test]
fn missing_socket_reaps_owned_server_before_relaunch() {
    let home = std::env::temp_dir().join(format!("agend-codex-owned-relaunch-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("home");
    let instance = "a";
    let endpoint = home
        .join("transport/codex")
        .join(format!("a-{}.sock", Uuid::new_v4()));
    let child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("owned server fixture");
    let pid = child.id();
    let start_token = crate::process::process_start_token(pid).expect("start token");
    let mut locator = SessionLocator::codex(endpoint, None);
    locator.managed = true;
    locator.server_pid = Some(pid);
    locator.server_start_token = Some(start_token);
    super::super::registry::save_session_locator(&home, instance, &locator)
        .expect("persist locator");
    managed_servers()
        .lock()
        .expect("server registry lock")
        .insert(
            server_key(&home, instance),
            ManagedServer {
                child,
                pid,
                start_token: Some(start_token),
            },
        );

    let result = prepare_managed_tui(&home, instance, "/bin/false", locator, None);
    assert!(result.is_err(), "false must not create a Codex socket");
    assert!(
        crate::process::process_start_token(pid).is_none(),
        "owned child must be reaped before a replacement launch"
    );
    let _ = std::fs::remove_dir_all(home);
}

#[cfg(unix)]
#[test]
fn managed_endpoint_for_long_instance_fits_macos_sun_len() {
    let home = Path::new("/Users/suzuke/.agend-terminal");
    let instance = "archfix-codex-native-smoke-072903";
    let mut locator = SessionLocator::codex(PathBuf::new(), None);

    rotate_managed_endpoint(home, instance, &mut locator);

    let endpoint = locator.endpoint.expect("managed endpoint");
    let endpoint_name = endpoint
        .file_name()
        .and_then(|name| name.to_str())
        .expect("UTF-8 endpoint name");
    assert!(
        endpoint.as_os_str().as_encoded_bytes().len() < 104,
        "managed Codex socket must fit macOS sockaddr_un.sun_path: {} ({} bytes)",
        endpoint.display(),
        endpoint.as_os_str().as_encoded_bytes().len()
    );
    assert!(valid_managed_endpoint_name(instance, endpoint_name));
    assert!(!valid_managed_endpoint_name(
        "another-instance",
        endpoint_name
    ));
}

#[cfg(unix)]
#[test]
fn teardown_removes_owned_socket_but_not_other_socket() {
    let suffix = Uuid::new_v4().to_string();
    let home = std::path::PathBuf::from("/tmp").join(format!("c-{}", &suffix[..8]));
    let instance = "a";
    let socket_dir = home.join("transport/codex");
    std::fs::create_dir_all(&socket_dir).expect("socket dir");
    let endpoint = socket_dir.join(managed_endpoint_name(instance, Uuid::new_v4()));
    let other_endpoint = socket_dir.join("other.sock");
    let listener = UnixListener::bind(&endpoint).expect("owned socket");
    let other_listener = UnixListener::bind(&other_endpoint).expect("other socket");
    drop(listener);

    let term_marker = home.join("term-grace");
    let trap_ready = home.join("trap-ready");
    let child = std::process::Command::new("sh")
            .args([
                "-c",
                "ulimit -t 30; trap 'printf done > \"$AGEND_TERM_MARKER\"; exit 0' TERM; touch \"$AGEND_TRAP_READY\"; while :; do :; done",
            ])
            .env("AGEND_TERM_MARKER", &term_marker)
            .env("AGEND_TRAP_READY", &trap_ready)
            .spawn()
            .expect("owned server fixture");
    let pid = child.id();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !trap_ready.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "owned server fixture did not install TERM trap"
        );
        thread::sleep(std::time::Duration::from_millis(10));
    }
    let start_token = crate::process::process_start_token(pid).expect("start token");
    let mut locator = SessionLocator::codex(endpoint.clone(), None);
    locator.managed = true;
    locator.server_pid = Some(pid);
    locator.server_start_token = Some(start_token);
    super::super::registry::save_session_locator(&home, instance, &locator)
        .expect("persist locator");
    managed_servers()
        .lock()
        .expect("server registry lock")
        .insert(
            server_key(&home, instance),
            ManagedServer {
                child,
                pid,
                start_token: Some(start_token),
            },
        );

    stop_instance_server(&home, instance).expect("owned teardown");
    assert!(term_marker.exists(), "owned child must receive TERM grace");
    assert!(!endpoint.exists(), "owned stale socket must be removed");
    assert!(other_endpoint.exists(), "unowned socket must remain");
    drop(other_listener);
    let _ = std::fs::remove_dir_all(home);
}

#[cfg(unix)]
#[test]
fn shared_process_group_stop_does_not_kill_caller() {
    let suffix = Uuid::new_v4().to_string();
    let home = std::path::PathBuf::from("/tmp").join(format!("c-{}", &suffix[..8]));
    let instance = "a";
    let socket_dir = home.join("transport/codex");
    std::fs::create_dir_all(&socket_dir).expect("socket dir");
    let endpoint = socket_dir.join(format!("a-{}.sock", Uuid::new_v4()));
    let listener = UnixListener::bind(&endpoint).expect("owned socket");
    drop(listener);

    let term_marker = home.join("term-grace");
    let trap_ready = home.join("trap-ready");
    let caller_pgid = unsafe { libc::getpgrp() };
    let child = unsafe {
        use std::os::unix::process::CommandExt;
        std::process::Command::new("sh")
                .args([
                    "-c",
                    "ulimit -t 30; trap 'printf term > \"$AGEND_TERM_MARKER\"' TERM; touch \"$AGEND_TRAP_READY\"; while :; do :; done",
                ])
                .env("AGEND_TERM_MARKER", &term_marker)
                .env("AGEND_TRAP_READY", &trap_ready)
                .pre_exec(move || {
                    if libc::setpgid(0, caller_pgid) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                })
                .spawn()
                .expect("shared-group fixture")
    };
    let pid = child.id();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !trap_ready.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "shared-group fixture did not install TERM trap"
        );
        thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_ne!(
        crate::process::process_group_id(pid),
        Some(pid),
        "fixture must share the caller process group"
    );
    let start_token = crate::process::process_start_token(pid).expect("start token");
    let mut locator = SessionLocator::codex(endpoint.clone(), None);
    locator.managed = true;
    locator.server_pid = Some(pid);
    locator.server_start_token = Some(start_token);
    super::super::registry::save_session_locator(&home, instance, &locator)
        .expect("persist locator");
    managed_servers()
        .lock()
        .expect("Codex server registry lock")
        .insert(
            server_key(&home, instance),
            ManagedServer {
                child,
                pid,
                start_token: Some(start_token),
            },
        );

    stop_instance_server(&home, instance).expect("shared-group teardown");
    assert!(term_marker.exists(), "shared-group child must receive TERM");
    assert!(
        crate::process::process_start_token(pid).is_none(),
        "shared-group child must be reaped"
    );
    assert!(!endpoint.exists(), "owned socket must be removed");
    assert!(crate::process::is_pid_alive(std::process::id()));
    let _ = std::fs::remove_dir_all(home);
}

#[cfg(unix)]
#[test]
fn managed_endpoint_cleanup_rejects_symlink() {
    use std::os::unix::fs::symlink;

    let suffix = Uuid::new_v4().to_string();
    let home = std::path::PathBuf::from("/tmp").join(format!("c-{}", &suffix[..8]));
    let instance = "a";
    let socket_dir = home.join("transport/codex");
    std::fs::create_dir_all(&socket_dir).expect("socket dir");
    let target = home.join("target");
    let endpoint = socket_dir.join(format!("a-{}.sock", Uuid::new_v4()));
    std::fs::write(&target, "must survive").expect("target");
    symlink(&target, &endpoint).expect("endpoint symlink");
    let locator = SessionLocator::codex(endpoint.clone(), None);

    assert!(remove_managed_endpoint(&home, instance, &locator).is_err());
    assert!(std::fs::symlink_metadata(&endpoint)
        .expect("endpoint metadata")
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read_to_string(target).expect("target contents"),
        "must survive"
    );
    let _ = std::fs::remove_dir_all(home);
}

#[cfg(unix)]
#[test]
fn managed_endpoint_cleanup_rejects_real_socket_outside_namespace() {
    let suffix = Uuid::new_v4().to_string();
    let home = std::path::PathBuf::from("/tmp").join(format!("c-{}", &suffix[..8]));
    let instance = "a";
    std::fs::create_dir_all(home.join("transport/codex")).expect("socket dir");
    let endpoint = home.join("outside.sock");
    let listener = UnixListener::bind(&endpoint).expect("outside socket");
    drop(listener);
    let locator = SessionLocator::codex(endpoint.clone(), None);

    assert!(remove_managed_endpoint(&home, instance, &locator).is_err());
    assert!(endpoint.exists(), "outside socket must survive cleanup");
    let _ = std::fs::remove_file(endpoint);
    let _ = std::fs::remove_dir_all(home);
}

#[cfg(unix)]
#[test]
fn managed_socket_final_unlink_is_idempotent_when_endpoint_disappears() {
    let endpoint = std::env::temp_dir().join(format!(
        "agend-codex-missing-managed-socket-{}.sock",
        Uuid::new_v4()
    ));

    assert!(
        remove_managed_socket_file(&endpoint).is_ok(),
        "a socket disappearing after validation already reached the desired end state"
    );
}

#[cfg(unix)]
#[test]
fn managed_socket_final_unlink_keeps_other_errors_fail_closed() {
    let endpoint = std::env::temp_dir().join(format!(
        "agend-codex-non-socket-endpoint-{}",
        Uuid::new_v4()
    ));
    std::fs::create_dir(&endpoint).expect("endpoint fixture directory");

    let error = remove_managed_socket_file(&endpoint)
        .expect_err("a non-ENOENT unlink error must stay fatal");
    assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
    std::fs::remove_dir(&endpoint).expect("remove endpoint fixture directory");
}

#[test]
fn initialize_version_and_capability_shape_fail_closed() {
    let missing_version =
        json!({"codexHome": "/tmp", "platformFamily": "unix", "platformOs": "macos"});
    assert!(validate_initialize_response(&missing_version).is_err());
    let missing_platform = json!({"userAgent": "codex/0.146.0", "codexHome": "/tmp"});
    assert!(validate_initialize_response(&missing_platform).is_err());
    let null_platform = json!({
        "userAgent": "codex/0.146.0",
        "codexHome": "/tmp",
        "platformFamily": null,
        "platformOs": "macos"
    });
    assert!(validate_initialize_response(&null_platform).is_err());
}

/// #3571 RED: an ephemeral/system loaded thread must not make discovery
/// reject the one real user thread. The test drives the real delivery path
/// and exposes the thread/read metadata only after the loaded-list response.
#[test]
fn ephemeral_system_thread_is_filtered_from_tui_discovery_3571() {
    let home = std::env::temp_dir().join(format!("agend-codex-ephemeral-{}", Uuid::new_v4()));
    let endpoint = std::env::temp_dir().join(format!("a-{}.sock", Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("home");
    let server = run_fake_codex_with_loaded_threads_and_reads(
        &endpoint,
        vec!["thread-system".to_string(), "thread-user".to_string()],
        vec![
            (
                "thread-system".to_string(),
                Some(true),
                Some("system".to_string()),
            ),
            (
                "thread-user".to_string(),
                Some(false),
                Some("user".to_string()),
            ),
        ],
    );
    let locator = SessionLocator::codex(endpoint.clone(), None);
    let mut adapter = CodexNativeShared::new(&home, "codex-agent");
    let envelope = DeliveryEnvelope::new(
        "codex-agent",
        locator,
        DeliveryKind::Prompt,
        "hello",
        Some("corr-ephemeral".to_string()),
    );

    let accepted = adapter
        .deliver_blocking(envelope)
        .expect("the real user thread must be selected");
    assert_eq!(accepted.state, DeliveryState::ProtocolAccepted);
    let persisted = super::super::registry::load_session_locator(&home, "codex-agent")
        .expect("discovery must persist the selected thread");
    assert_eq!(persisted.thread_id.as_deref(), Some("thread-user"));

    server.join().expect("fake server");
    let _ = std::fs::remove_file(endpoint);
    let _ = std::fs::remove_dir_all(home);
}

/// #3571 negative control: two real user threads remain ambiguous and must
/// keep the fail-closed refusal boundary.
#[test]
fn two_real_user_threads_remain_ambiguous_3571() {
    let home = std::env::temp_dir().join(format!("agend-codex-two-users-{}", Uuid::new_v4()));
    let endpoint = std::env::temp_dir().join(format!("a-{}.sock", Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("home");
    let server = run_fake_codex_with_loaded_threads_and_reads(
        &endpoint,
        vec!["thread-user-a".to_string(), "thread-user-b".to_string()],
        vec![
            (
                "thread-user-a".to_string(),
                Some(false),
                Some("user".to_string()),
            ),
            (
                "thread-user-b".to_string(),
                Some(false),
                Some("user".to_string()),
            ),
        ],
    );
    let locator = SessionLocator::codex(endpoint.clone(), None);
    let mut adapter = CodexNativeShared::new(&home, "codex-agent");
    let envelope = DeliveryEnvelope::new(
        "codex-agent",
        locator,
        DeliveryKind::Prompt,
        "hello",
        Some("corr-two-users".to_string()),
    );

    let error = adapter
        .deliver_blocking(envelope)
        .expect_err("two real user threads must remain refused");
    assert!(
        error.to_string().contains("2 loaded threads"),
        "ambiguity count must survive filtering: {error}"
    );

    drop(adapter);
    server.join().expect("fake server");
    let _ = std::fs::remove_file(endpoint);
    let _ = std::fs::remove_dir_all(home);
}

/// PR #3581 correction RED: successful `thread/read` without classification
/// metadata remains an unresolved candidate, so it must keep discovery closed
/// even when another candidate is positively identified as the user thread.
#[test]
fn unknown_thread_read_metadata_keeps_tui_discovery_fail_closed_3581() {
    let home = std::env::temp_dir().join(format!("agend-codex-unknown-{}", Uuid::new_v4()));
    let endpoint = std::env::temp_dir().join(format!("a-{}.sock", Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("home");
    let server = run_fake_codex_with_loaded_threads_and_reads(
        &endpoint,
        vec!["thread-user".to_string(), "thread-unknown".to_string()],
        vec![
            (
                "thread-user".to_string(),
                Some(false),
                Some("user".to_string()),
            ),
            ("thread-unknown".to_string(), None, None),
        ],
    );
    let locator = SessionLocator::codex(endpoint.clone(), None);
    let mut adapter = CodexNativeShared::new(&home, "codex-agent");
    let envelope = DeliveryEnvelope::new(
        "codex-agent",
        locator,
        DeliveryKind::Prompt,
        "hello",
        Some("corr-unknown".to_string()),
    );

    let error = adapter
        .deliver_blocking(envelope)
        .expect_err("unresolved thread metadata must keep discovery fail-closed");
    assert!(
        error.to_string().contains("metadata"),
        "refusal must identify the unresolved metadata: {error}"
    );

    drop(adapter);
    server.join().expect("fake server");
    let _ = std::fs::remove_file(endpoint);
    let _ = std::fs::remove_dir_all(home);
}

/// #3535: the ambiguous-delivery refusal must carry truncated thread ids
/// (first 8 chars) so the receipt detail + event-log let the operator tell
/// a stale leftover from a genuine double-open. The guard verdict itself
/// (refuse when != 1) is unchanged — only the diagnostic payload grows.
#[test]
fn ambiguous_delivery_refusal_carries_truncated_thread_ids() {
    let home = std::env::temp_dir().join(format!("agend-codex-ambiguous-ids-{}", Uuid::new_v4()));
    let endpoint = std::env::temp_dir().join(format!("a-{}.sock", Uuid::new_v4()));
    std::fs::create_dir_all(&home).expect("home");
    let server = run_fake_codex_with_loaded_threads(
        &endpoint,
        vec![
            "thread-aaaa1111bbbb2222".to_string(),
            "thread-bbbb3333cccc4444".to_string(),
        ],
    );
    let locator = SessionLocator::codex(endpoint.clone(), None);
    let mut adapter = CodexNativeShared::new(&home, "codex-agent");

    let envelope = DeliveryEnvelope::new(
        "codex-agent",
        locator,
        DeliveryKind::Prompt,
        "hello",
        Some("corr-ambiguous".to_string()),
    );
    let delivery_id = envelope.delivery_id;
    let error = adapter
        .deliver_blocking(envelope)
        .expect_err("two loaded threads must stay refused");
    let message = error.to_string();
    assert!(
        message.contains("2 loaded threads"),
        "count must survive: {message}"
    );
    assert!(
        message.contains("thread-a") && message.contains("thread-b"),
        "truncated ids must be present: {message}"
    );
    assert!(
        !message.contains("aaaa1111bbbb2222") && !message.contains("bbbb3333cccc4444"),
        "full ids must not leak: {message}"
    );

    let store = ReceiptStore::for_instance(&home, "codex-agent").expect("store");
    let receipt = store
        .latest(delivery_id)
        .expect("latest receipt")
        .expect("failed receipt");
    assert_eq!(receipt.state, DeliveryState::Failed);
    let detail = receipt.detail.as_deref().unwrap_or_default();
    assert!(
        detail.contains("thread-a") && detail.contains("thread-b"),
        "receipt detail must carry the truncated ids: {detail}"
    );

    // #3535: the refusal happens at discovery, so no `turn/start` ever
    // reaches the fake server — drop the client first so its read loop
    // sees EOF and exits instead of hanging the join.
    drop(adapter);
    server.join().expect("fake server");
    let _ = std::fs::remove_file(endpoint);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn reconcile_preserves_ambiguity_after_restart() {
    let home = std::env::temp_dir().join(format!("agend-codex-reconcile-{}", Uuid::new_v4()));
    let envelope = DeliveryEnvelope::new(
        "codex-agent",
        SessionLocator::codex(PathBuf::from("/tmp/missing.sock"), Some("thread-1".into())),
        DeliveryKind::Prompt,
        "hello",
        None,
    );
    let store = ReceiptStore::for_instance(&home, "codex-agent").expect("store");
    store.record_queued(&envelope).expect("queued");
    store
        .record(DeliveryReceipt::for_state(
            &envelope,
            DeliveryState::ProtocolAccepted,
        ))
        .expect("accepted");

    let mut adapter = CodexNativeShared::new(&home, "codex-agent");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let reconciled = runtime
        .block_on(adapter.reconcile(envelope.delivery_id))
        .expect("reconcile");
    assert_eq!(reconciled, DeliveryState::Ambiguous);
    let _ = std::fs::remove_dir_all(home);
}
