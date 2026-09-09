//! Codex app-server NativeShared adapter.
//!
//! Codex's Unix app-server endpoint is a WebSocket carrying JSON-RPC messages
//! (the stdio variant is JSONL). This module keeps the wire implementation
//! intentionally small and version-gated: the daemon must prove the
//! initialize response's version/platform capability shape before resuming a
//! thread or sending a turn.

use super::{
    AgentDeliveryTransport, BackendEvent, DeliveryEnvelope, DeliveryKind, DeliveryReceipt,
    DeliveryState, ReceiptStore, SessionLocator, TransportCapability, TransportMode,
};
use serde_json::{json, Value};
use std::collections::HashMap;
#[cfg(unix)]
use std::collections::VecDeque;
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::{Mutex, OnceLock};
#[cfg(unix)]
use std::time::Duration;
use uuid::Uuid;

#[cfg(unix)]
use std::io::{BufRead, Read};

#[cfg(unix)]
const CODEX_PROTOCOL: &str = "v2";
#[cfg(unix)]
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_READINESS_DETAIL_BYTES: usize = 1024;

pub(crate) struct CodexNativeShared {
    home: PathBuf,
    instance: String,
    locator: Option<SessionLocator>,
    ready: bool,
    backend_version: Option<String>,
    next_request_id: u64,
    in_flight: Option<Uuid>,
    active_turn_id: Option<String>,
    pending: HashMap<Uuid, DeliveryEnvelope>,
    #[cfg(unix)]
    events: VecDeque<BackendEvent>,
    #[cfg(unix)]
    writer: Option<std::os::unix::net::UnixStream>,
    #[cfg(unix)]
    reader: Option<std::os::unix::net::UnixStream>,
}

#[cfg(unix)]
struct ManagedServer {
    child: std::process::Child,
    pid: u32,
    start_token: Option<u64>,
}

#[cfg(unix)]
fn managed_servers() -> &'static Mutex<HashMap<String, ManagedServer>> {
    static SERVERS: OnceLock<Mutex<HashMap<String, ManagedServer>>> = OnceLock::new();
    SERVERS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(unix)]
fn server_key(home: &Path, instance: &str) -> String {
    format!("{}\0{}", home.display(), instance)
}

#[cfg(unix)]
fn persisted_server_owned(locator: &SessionLocator) -> bool {
    let (Some(pid), Some(start_token)) = (locator.server_pid, locator.server_start_token) else {
        return false;
    };
    crate::process::process_start_token(pid) == Some(start_token)
}

#[cfg(unix)]
fn in_memory_server_owned(home: &Path, instance: &str, locator: &SessionLocator) -> bool {
    let key = server_key(home, instance);
    let mut servers = managed_servers()
        .lock()
        .expect("Codex server registry lock");
    let Some(server) = servers.get_mut(&key) else {
        return false;
    };
    if server.child.try_wait().ok().flatten().is_some() {
        servers.remove(&key);
        return false;
    }
    locator.server_pid == Some(server.pid)
        && locator.server_start_token.is_some()
        && locator.server_start_token == server.start_token
}

impl CodexNativeShared {
    pub(crate) fn new(home: &Path, instance: &str) -> Self {
        Self {
            home: home.to_path_buf(),
            instance: instance.to_string(),
            locator: None,
            ready: false,
            backend_version: None,
            next_request_id: 1,
            in_flight: None,
            active_turn_id: None,
            pending: HashMap::new(),
            #[cfg(unix)]
            events: VecDeque::new(),
            #[cfg(unix)]
            writer: None,
            #[cfg(unix)]
            reader: None,
        }
    }

    pub(crate) fn deliver_blocking(
        &mut self,
        mut envelope: DeliveryEnvelope,
    ) -> anyhow::Result<DeliveryReceipt> {
        let store = ReceiptStore::for_instance(&self.home, &self.instance)?;
        store.record_queued(&envelope)?;
        let attach = self
            .start_or_attach_blocking(envelope.session.clone())
            .map(|_| ());
        if let Err(error) = attach {
            let mut failed = DeliveryReceipt::for_state(&envelope, DeliveryState::Failed);
            failed.detail = Some(readiness_failure_detail(&error));
            store.record(failed)?;
            return Err(error);
        }
        if let Some(locator) = self.locator.clone() {
            envelope.session = locator;
        }
        if self.in_flight.is_some()
            && self.active_turn_id.is_none()
            && !matches!(envelope.kind, DeliveryKind::Steer)
        {
            let mut queued = DeliveryReceipt::for_state(&envelope, DeliveryState::Queued);
            queued.detail = Some("the active Codex turn id is not known yet".to_string());
            store.record(queued)?;
            return Err(anyhow::anyhow!(
                "Codex active turn is not ready for turn/steer"
            ));
        }

        let (method, params) = match self.turn_request(&envelope) {
            Ok(request) => request,
            Err(error) => {
                let mut failed = DeliveryReceipt::for_state(&envelope, DeliveryState::Failed);
                failed.detail =
                    Some("delivery kind is not valid for the current Codex state".to_string());
                store.record(failed)?;
                return Err(error);
            }
        };
        let steers_active_turn = method == "turn/steer";
        let request_id = self.next_request_id.to_string();
        let response = match self.send_request(&method, params) {
            Ok(response) => response,
            Err(error) => {
                let protocol_rejected = error.to_string().starts_with("Codex JSON-RPC ");
                let state = if protocol_rejected {
                    DeliveryState::Failed
                } else {
                    DeliveryState::Ambiguous
                };
                let mut receipt = DeliveryReceipt::for_state(&envelope, state);
                receipt.detail = Some(if protocol_rejected {
                    "Codex rejected the turn request before acceptance".to_string()
                } else {
                    "Codex request outcome is ambiguous after transport failure; reconcile before retry"
                        .to_string()
                });
                store.record(receipt)?;
                return Err(error);
            }
        };
        let backend_request_id = response
            .get("turn")
            .and_then(|turn| turn.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                response
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        if !steers_active_turn {
            self.active_turn_id = backend_request_id.clone();
            self.pending.insert(envelope.delivery_id, envelope.clone());
            self.in_flight = Some(envelope.delivery_id);
        }
        let mut receipt = DeliveryReceipt::for_state(&envelope, DeliveryState::ProtocolAccepted);
        receipt.protocol_request_id = Some(backend_request_id.unwrap_or(request_id));
        receipt.tui_visibility = Some("shared_codex_thread".to_string());
        receipt.detail = Some(if steers_active_turn {
            "Codex app-server accepted turn/steer".to_string()
        } else {
            "Codex app-server accepted turn/start".to_string()
        });
        store.record(receipt.clone())?;
        Ok(receipt)
    }

    #[allow(dead_code)]
    pub(crate) fn remote_attach_args(locator: &SessionLocator) -> Vec<String> {
        vec!["--remote".to_string(), locator.remote_attach_arg()]
    }

    /// Start a Codex app-server for a session. The caller owns the child and
    /// must keep it alive with the agent lifecycle. This helper is not used as
    /// a fallback from a failed attach.
    #[allow(dead_code)]
    pub(crate) fn launch(
        codex: &str,
        locator: &SessionLocator,
        cwd: &Path,
        config_args: &[String],
    ) -> anyhow::Result<std::process::Child> {
        #[cfg(unix)]
        {
            let endpoint = locator
                .endpoint
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Codex NativeShared endpoint is missing"))?;
            if endpoint.exists() {
                return Err(anyhow::anyhow!(
                    "refusing to replace existing Codex app-server socket {}",
                    endpoint.display()
                ));
            }
            let parent = endpoint
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("Codex socket must have a dedicated parent directory")
                })?;
            let temp_root = std::env::temp_dir();
            if parent == Path::new("/") || parent == temp_root.as_path() {
                return Err(anyhow::anyhow!(
                    "refusing to make the shared socket parent private: {}",
                    parent.display()
                ));
            }
            std::fs::create_dir_all(parent)?;
            let mut parent_permissions = std::fs::metadata(parent)?.permissions();
            use std::os::unix::fs::PermissionsExt;
            parent_permissions.set_mode(0o700);
            std::fs::set_permissions(parent, parent_permissions)?;
            use std::os::unix::process::CommandExt;
            let mut command = std::process::Command::new(codex);
            command
                .args(config_args)
                .args(["app-server", "--listen", &locator.remote_attach_arg()])
                .current_dir(cwd)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .process_group(0);
            let mut child = command.spawn()?;
            if let Some(stdout) = child.stdout.take() {
                // fire-and-forget: drain the managed server stream so it cannot
                // block or write through the TUI owner's terminal; exits at EOF.
                if let Err(error) = std::thread::Builder::new()
                    .name("codex-app-server-out".to_string())
                    .spawn(move || drain_server_output(stdout, "stdout"))
                {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error.into());
                }
            }
            if let Some(stderr) = child.stderr.take() {
                // fire-and-forget: drain the managed server stream into tracing;
                // TUI mode routes tracing to app.log rather than the terminal.
                if let Err(error) = std::thread::Builder::new()
                    .name("codex-app-server-err".to_string())
                    .spawn(move || drain_server_output(stderr, "stderr"))
                {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error.into());
                }
            }
            let started = wait_for_socket(endpoint, &mut child)?;
            if !started {
                let _ = child.kill();
                return Err(anyhow::anyhow!(
                    "Codex app-server did not create its Unix socket"
                ));
            }
            let mut permissions = std::fs::metadata(endpoint)?.permissions();
            permissions.set_mode(0o600);
            std::fs::set_permissions(endpoint, permissions)?;
            Ok(child)
        }
        #[cfg(not(unix))]
        {
            let _ = (codex, locator, cwd, config_args);
            Err(anyhow::anyhow!("Codex NativeShared requires Unix sockets"))
        }
    }

    fn turn_request(&self, envelope: &DeliveryEnvelope) -> anyhow::Result<(String, Value)> {
        let thread_id = envelope
            .session
            .thread_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Codex NativeShared requires a thread_id"))?;
        let input = json!([{"type": "text", "text": envelope.body}]);
        if matches!(envelope.kind, DeliveryKind::Steer)
            || (self.in_flight.is_some() && !matches!(envelope.kind, DeliveryKind::Interrupt))
        {
            let turn_id = self
                .active_turn_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("turn/steer requires an active Codex turn id"))?;
            return Ok((
                "turn/steer".to_string(),
                json!({
                    "threadId": thread_id,
                    "expectedTurnId": turn_id,
                    "input": input,
                }),
            ));
        }
        match envelope.kind {
            DeliveryKind::Interrupt => Err(anyhow::anyhow!(
                "interrupt requires an explicit Codex protocol operation; ordinary delivery cannot infer it"
            )),
            DeliveryKind::Prompt
            | DeliveryKind::Notification
            | DeliveryKind::Task
            | DeliveryKind::Query
            | DeliveryKind::Handoff => Ok((
                "turn/start".to_string(),
                json!({
                    "threadId": thread_id,
                    "input": input,
                    "clientUserMessageId": envelope.delivery_id.to_string(),
                }),
            )),
            DeliveryKind::Steer => unreachable!("steer is handled before ordinary turn/start"),
        }
    }

    fn start_or_attach_blocking(
        &mut self,
        locator: SessionLocator,
    ) -> anyhow::Result<TransportCapability> {
        self.start_or_attach_blocking_with_cwd(locator, None)
    }

    fn start_or_attach_blocking_with_cwd(
        &mut self,
        locator: SessionLocator,
        _cwd: Option<&Path>,
    ) -> anyhow::Result<TransportCapability> {
        if locator.backend != "codex" {
            return Err(anyhow::anyhow!("NativeShared locator backend is not codex"));
        }
        if locator.endpoint.is_none() {
            return Err(anyhow::anyhow!("Codex NativeShared requires an endpoint"));
        }
        if self.ready && self.locator.as_ref() == Some(&locator) {
            return Ok(TransportCapability {
                backend: "codex".to_string(),
                mode: TransportMode::NativeShared,
                ready: true,
                backend_version: self.backend_version.clone(),
                degraded_reason: None,
            });
        }
        self.ready = false;
        self.locator = Some(locator.clone());
        #[cfg(unix)]
        {
            let mut locator = locator;
            self.connect(&locator)?;
            let initialize = self.send_request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "agend-terminal",
                        "title": "AgEnD structured transport",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": {
                        "experimentalApi": true,
                    },
                }),
            )?;
            let version = validate_initialize_response(&initialize)?;
            self.send_notification("initialized", json!({}))?;
            let had_thread_id = locator
                .thread_id
                .as_deref()
                .filter(|thread_id| !thread_id.is_empty())
                .map(str::to_string);
            if let Some(thread_id) = had_thread_id.as_deref() {
                self.send_request(
                    "thread/resume",
                    json!({"threadId": thread_id, "excludeTurns": true}),
                )?;
            } else {
                locator.thread_id = Some(self.discover_loaded_tui_thread()?);
            }
            self.locator = Some(locator.clone());
            self.backend_version = Some(version.clone());
            self.ready = true;
            super::registry::save_session_locator(&self.home, &self.instance, &locator)?;
            Ok(TransportCapability {
                backend: "codex".to_string(),
                mode: TransportMode::NativeShared,
                ready: true,
                backend_version: Some(version),
                degraded_reason: None,
            })
        }
        #[cfg(not(unix))]
        {
            Err(anyhow::anyhow!("Codex NativeShared requires Unix sockets"))
        }
    }

    #[cfg(unix)]
    fn discover_loaded_tui_thread(&mut self) -> anyhow::Result<String> {
        let deadline = std::time::Instant::now() + IO_TIMEOUT;
        loop {
            let response = self.send_request("thread/loaded/list", json!({}))?;
            let loaded_threads = loaded_threads(&response);
            match loaded_threads.as_slice() {
                [thread] if !thread.is_known_non_user() => return Ok(thread.id.clone()),
                [thread] => {
                    return Err(anyhow::anyhow!(
                        "Codex app-server has no real user TUI thread (loaded {})",
                        thread.id
                    ));
                }
                [] if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                [] => {
                    return Err(anyhow::anyhow!(
                        "Codex TUI did not publish a loaded thread before the delivery deadline"
                    ));
                }
                _ => {
                    let real_user_ids = self.real_user_loaded_thread_ids(loaded_threads)?;
                    match real_user_ids.as_slice() {
                        [thread_id] => return Ok(thread_id.clone()),
                        [] => {
                            return Err(anyhow::anyhow!(
                                "Codex app-server has no real user TUI thread among loaded threads"
                            ));
                        }
                        _ => {
                            return Err(anyhow::anyhow!(
                                "Codex app-server has {} loaded threads ({}); refusing ambiguous TUI delivery",
                                real_user_ids.len(),
                                ambiguous_thread_ids_preview(&real_user_ids)
                            ));
                        }
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    fn real_user_loaded_thread_ids(
        &mut self,
        loaded_threads: Vec<LoadedThread>,
    ) -> anyhow::Result<Vec<String>> {
        loaded_threads
            .into_iter()
            .filter_map(|thread| {
                if thread.is_known_non_user() {
                    return None;
                }
                if thread.is_known_user() {
                    return Some(Ok(thread.id));
                }
                let thread_id = thread.id.clone();
                let response = self.send_request("thread/read", json!({"threadId": thread_id}));
                match response {
                    Ok(response) => match thread_read_user_classification(&response) {
                        Some(true) => Some(Ok(thread.id)),
                        Some(false) => None,
                        None => Some(Err(anyhow::anyhow!(
                            "Codex app-server thread/read for {thread_id} omitted or returned unrecognized thread classification metadata"
                        ))),
                    },
                    Err(error) => Some(Err(error)),
                }
            })
            .collect()
    }

    #[cfg(unix)]
    fn update_pending_state(
        &self,
        delivery_id: Uuid,
        state: DeliveryState,
        detail: &str,
        backend_event: Option<&str>,
    ) -> anyhow::Result<()> {
        let Some(envelope) = self.pending.get(&delivery_id) else {
            return Ok(());
        };
        let store = ReceiptStore::for_instance(&self.home, &self.instance)?;
        let mut receipt = DeliveryReceipt::for_state(envelope, state);
        if let Some(previous) = store.latest(delivery_id)? {
            receipt.protocol_request_id = previous.protocol_request_id;
            receipt.tui_visibility = previous.tui_visibility;
        }
        receipt.backend_event = backend_event.map(str::to_string);
        receipt.detail = Some(detail.to_string());
        store.record(receipt)
    }

    #[cfg(unix)]
    fn connect(&mut self, locator: &SessionLocator) -> anyhow::Result<()> {
        let endpoint = locator
            .endpoint
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Codex Unix socket endpoint is missing"))?;
        // UnixStream has no portable connect-timeout constructor. The endpoint
        // is a local filesystem socket, so connect is bounded by the local IPC
        // operation; read/write timeouts below bound all protocol waits.
        let stream = std::os::unix::net::UnixStream::connect(endpoint)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        let reader_stream = stream.try_clone()?;
        self.writer = Some(stream);
        self.reader = Some(reader_stream);
        self.websocket_handshake()?;
        Ok(())
    }

    #[cfg(unix)]
    fn websocket_handshake(&mut self) -> anyhow::Result<()> {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce)?;
        let key = STANDARD.encode(nonce);
        let request = format!(
            "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Codex app-server is not connected"))?;
        writer.write_all(request.as_bytes())?;
        writer.flush()?;

        let reader = self
            .reader
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Codex app-server is not connected"))?;
        let mut response = Vec::new();
        let mut last_four = [0_u8; 4];
        loop {
            let mut byte = [0_u8; 1];
            reader.read_exact(&mut byte)?;
            response.push(byte[0]);
            last_four.rotate_left(1);
            last_four[3] = byte[0];
            if last_four == *b"\r\n\r\n" {
                break;
            }
            if response.len() > 16 * 1024 {
                return Err(anyhow::anyhow!(
                    "Codex WebSocket handshake response is too large"
                ));
            }
        }
        let response = String::from_utf8(response)?;
        let mut lines = response.lines();
        let status = lines.next().unwrap_or_default();
        if !status.contains(" 101 ") {
            return Err(anyhow::anyhow!(
                "Codex Unix socket did not upgrade to WebSocket: {status}"
            ));
        }
        let mut upgrade = false;
        let mut connection = false;
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("upgrade")
                    && value.trim().eq_ignore_ascii_case("websocket")
                {
                    upgrade = true;
                }
                if name.eq_ignore_ascii_case("connection")
                    && value.to_ascii_lowercase().contains("upgrade")
                {
                    connection = true;
                }
            }
        }
        if !upgrade || !connection {
            return Err(anyhow::anyhow!(
                "Codex Unix socket WebSocket handshake omitted upgrade headers"
            ));
        }
        Ok(())
    }

    #[cfg(unix)]
    fn send_control_frame(&mut self, opcode: u8, body: &[u8]) -> anyhow::Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Codex app-server is not connected"))?;
        let mut frame = vec![0x80 | opcode];
        append_masked_payload(&mut frame, body)?;
        writer.write_all(&frame)?;
        writer.flush()?;
        Ok(())
    }

    #[cfg(unix)]
    fn send_notification(&mut self, method: &str, params: Value) -> anyhow::Result<()> {
        // Codex omits the JSON-RPC 2.0 marker on the wire; the WebSocket
        // message itself carries the JSON-RPC envelope.
        let frame = json!({"method": method, "params": params});
        self.write_frame(&frame)
    }

    #[cfg(unix)]
    fn send_request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        let frame = json!({"id": id, "method": method, "params": params});
        self.write_frame(&frame)?;
        loop {
            let value = self.read_frame()?;
            if value.get("id") == Some(&json!(id)) {
                if let Some(error) = value.get("error") {
                    return Err(anyhow::anyhow!("Codex JSON-RPC {method} failed: {error}"));
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
            self.observe_frame(value)?;
        }
    }

    #[cfg(not(unix))]
    fn send_request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let _ = (method, params);
        Err(anyhow::anyhow!(
            "Codex NativeShared requires Unix sockets; refusing structured delivery without a PTY fallback"
        ))
    }

    #[cfg(unix)]
    fn write_frame(&mut self, frame: &Value) -> anyhow::Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Codex app-server is not connected"))?;
        let body = serde_json::to_vec(frame)?;
        let mut header = Vec::with_capacity(body.len() + 16);
        header.push(0x81); // FIN + text frame
        append_masked_payload(&mut header, &body)?;
        writer.write_all(&header)?;
        writer.flush()?;
        Ok(())
    }

    #[cfg(unix)]
    fn read_frame(&mut self) -> anyhow::Result<Value> {
        loop {
            let payload = {
                let reader = self
                    .reader
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("Codex app-server is not connected"))?;
                read_websocket_frame(reader)?
            };
            if payload.0 == 0x8 {
                self.ready = false;
                return Err(anyhow::anyhow!("Codex app-server closed the WebSocket"));
            }
            if payload.0 == 0x9 {
                self.send_control_frame(0xA, &payload.1)?;
                continue;
            }
            if payload.0 != 0x1 {
                return Err(anyhow::anyhow!(
                    "Codex app-server returned unsupported WebSocket opcode {}",
                    payload.0
                ));
            }
            if payload.1.is_empty() {
                return Err(anyhow::anyhow!("Codex app-server returned an empty frame"));
            }
            return Ok(serde_json::from_slice(&payload.1)?);
        }
    }

    #[cfg(unix)]
    fn observe_frame(&mut self, value: Value) -> anyhow::Result<()> {
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        let id = value.get("id").and_then(Value::as_u64);
        if let (Some(id), Some(method)) = (id, method.clone()) {
            // Reverse requests must be visible to the structured event plane;
            // reject them explicitly rather than hanging the app-server.
            let response = json!({
                "id": id,
                "error": {"code": -32601, "message": "AgEnD reverse request policy has no handler"}
            });
            self.write_frame(&response)?;
            self.events.push_back(BackendEvent::ReverseRequest {
                request_id: id.to_string(),
                method,
                params: value.get("params").cloned().unwrap_or(Value::Null),
            });
            return Ok(());
        }
        let event = self.normalize_notification(&value);
        self.events.push_back(event);
        Ok(())
    }

    #[cfg(unix)]
    fn normalize_notification(&mut self, value: &Value) -> BackendEvent {
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let delivery_id = self.in_flight;
        match method {
            "turn/started" => {
                if let Some(id) = delivery_id {
                    let _ = self.update_pending_state(
                        id,
                        DeliveryState::TurnStarted,
                        "Codex emitted turn/started",
                        Some("turn/started"),
                    );
                    BackendEvent::TurnStarted {
                        delivery_id: id,
                        turn_id: value
                            .pointer("/params/turn/id")
                            .and_then(Value::as_str)
                            .map(|turn_id| {
                                self.active_turn_id = Some(turn_id.to_string());
                                turn_id.to_string()
                            }),
                    }
                } else {
                    BackendEvent::Unknown {
                        method: Some(method.to_string()),
                    }
                }
            }
            "turn/completed" | "turn/complete" => {
                if let Some(id) = delivery_id {
                    let _ = self.update_pending_state(
                        id,
                        DeliveryState::Completed,
                        "Codex emitted turn completion",
                        Some(method),
                    );
                    self.in_flight = None;
                    self.active_turn_id = None;
                    self.pending.remove(&id);
                    BackendEvent::Completed {
                        delivery_id: id,
                        event: method.to_string(),
                    }
                } else {
                    BackendEvent::Unknown {
                        method: Some(method.to_string()),
                    }
                }
            }
            "error" => {
                if let Some(id) = delivery_id {
                    let _ = self.update_pending_state(
                        id,
                        DeliveryState::Failed,
                        "Codex emitted an error notification",
                        Some("error"),
                    );
                    self.in_flight = None;
                    self.active_turn_id = None;
                    self.pending.remove(&id);
                }
                BackendEvent::Failed {
                    delivery_id,
                    reason: "Codex emitted an error notification".to_string(),
                }
            }
            "initialized" => BackendEvent::Ready,
            _ if delivery_id.is_some() => {
                let id = delivery_id.unwrap_or_else(Uuid::nil);
                let _ = self.update_pending_state(
                    id,
                    DeliveryState::ObservedInSession,
                    "Codex emitted an event for the active delivery",
                    Some(method),
                );
                BackendEvent::ObservedInSession {
                    delivery_id: id,
                    event: method.to_string(),
                }
            }
            _ => BackendEvent::Unknown {
                method: Some(method.to_string()),
            },
        }
    }

    #[cfg(unix)]
    fn next_event_blocking(&mut self) -> anyhow::Result<BackendEvent> {
        if let Some(event) = self.events.pop_front() {
            return Ok(event);
        }
        let value = self.read_frame()?;
        self.observe_frame(value)?;
        self.events
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("Codex event stream produced no event"))
    }
}

fn readiness_failure_detail(error: &anyhow::Error) -> String {
    let mut detail = format!("NativeShared readiness failed closed: {error}");
    if detail.len() <= MAX_READINESS_DETAIL_BYTES {
        return detail;
    }
    let suffix = "...";
    let mut end = MAX_READINESS_DETAIL_BYTES - suffix.len();
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    detail.truncate(end);
    detail.push_str(suffix);
    detail
}

#[cfg(unix)]
fn validate_initialize_response(response: &Value) -> anyhow::Result<String> {
    let version = response
        .get("userAgent")
        .and_then(Value::as_str)
        .or_else(|| {
            response
                .get("serverInfo")
                .and_then(|info| info.get("version"))
                .and_then(Value::as_str)
        })
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("Codex initialize response omitted backend version/userAgent")
        })?;
    for field in ["codexHome", "platformFamily", "platformOs"] {
        let present = response
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .is_some();
        if !present {
            return Err(anyhow::anyhow!(
                "Codex app-server protocol {CODEX_PROTOCOL} omitted initialize capability {field}"
            ));
        }
    }
    Ok(version.to_string())
}

/// #3535: truncated preview of ambiguous loaded thread ids for the refusal
/// diagnostic. The guard verdict (refuse when != 1) lives in
/// `discover_loaded_tui_thread` and is untouched.
///
/// This only renders the ids the helper already holds, truncated to 8 chars
/// per id so the receipt detail + event-log let the operator compare against
/// visible TUI threads without persisting full identifiers.
#[cfg(unix)]
fn ambiguous_thread_ids_preview(thread_ids: &[String]) -> String {
    thread_ids
        .iter()
        .map(|thread_id| thread_id.chars().take(8).collect::<String>())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(unix)]
#[derive(Debug)]
struct LoadedThread {
    id: String,
    ephemeral: Option<bool>,
    source: Option<String>,
}

#[cfg(unix)]
impl LoadedThread {
    fn is_known_non_user(&self) -> bool {
        self.ephemeral == Some(true) || self.source.as_deref() == Some("system")
    }

    fn is_known_user(&self) -> bool {
        self.ephemeral == Some(false) && self.source.as_deref() == Some("user")
    }
}

#[cfg(unix)]
fn loaded_threads(response: &Value) -> Vec<LoadedThread> {
    let mut loaded_threads = Vec::new();
    for item in response
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(thread_id) = item
            .as_str()
            .or_else(|| item.get("id").and_then(Value::as_str))
            .filter(|thread_id| !thread_id.is_empty())
        else {
            continue;
        };
        if !loaded_threads
            .iter()
            .any(|loaded: &LoadedThread| loaded.id == thread_id)
        {
            loaded_threads.push(LoadedThread {
                id: thread_id.to_string(),
                ephemeral: item.get("ephemeral").and_then(Value::as_bool),
                source: item
                    .get("threadSource")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
    }
    loaded_threads
}

#[cfg(unix)]
fn thread_read_user_classification(response: &Value) -> Option<bool> {
    let thread = response
        .pointer("/result/thread")
        .or_else(|| response.get("thread"))?;
    let loaded = LoadedThread {
        id: String::new(),
        ephemeral: thread.get("ephemeral").and_then(Value::as_bool),
        source: thread
            .get("threadSource")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    if loaded.is_known_user() {
        Some(true)
    } else if loaded.is_known_non_user() {
        Some(false)
    } else {
        None
    }
}

#[cfg(unix)]
fn append_masked_payload(output: &mut Vec<u8>, body: &[u8]) -> anyhow::Result<()> {
    let length = body.len();
    if length > 16 * 1024 * 1024 {
        return Err(anyhow::anyhow!(
            "Codex WebSocket payload exceeds the 16 MiB limit"
        ));
    }
    match length {
        0..=125 => output.push(0x80 | length as u8),
        126..=65_535 => {
            output.push(0x80 | 126);
            output.extend_from_slice(&(length as u16).to_be_bytes());
        }
        _ => {
            output.push(0x80 | 127);
            output.extend_from_slice(&(length as u64).to_be_bytes());
        }
    }
    let mut mask = [0_u8; 4];
    getrandom::fill(&mut mask)?;
    output.extend_from_slice(&mask);
    output.extend(
        body.iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );
    Ok(())
}

#[cfg(unix)]
fn read_websocket_frame(
    reader: &mut std::os::unix::net::UnixStream,
) -> anyhow::Result<(u8, Vec<u8>)> {
    let mut header = [0_u8; 2];
    reader.read_exact(&mut header)?;
    if header[0] & 0x80 == 0 {
        return Err(anyhow::anyhow!(
            "Codex WebSocket fragmented frames are unsupported"
        ));
    }
    let opcode = header[0] & 0x0F;
    let masked = header[1] & 0x80 != 0;
    let mut length = u64::from(header[1] & 0x7F);
    if length == 126 {
        let mut extended = [0_u8; 2];
        reader.read_exact(&mut extended)?;
        length = u64::from(u16::from_be_bytes(extended));
    } else if length == 127 {
        let mut extended = [0_u8; 8];
        reader.read_exact(&mut extended)?;
        length = u64::from_be_bytes(extended);
    }
    let length = usize::try_from(length)
        .map_err(|_| anyhow::anyhow!("Codex WebSocket frame is too large"))?;
    if length > 16 * 1024 * 1024 {
        return Err(anyhow::anyhow!(
            "Codex WebSocket frame exceeds the 16 MiB limit"
        ));
    }
    if opcode >= 0x8 && length > 125 {
        return Err(anyhow::anyhow!(
            "Codex WebSocket control frame exceeds the 125-byte limit"
        ));
    }
    let mut mask = [0_u8; 4];
    if masked {
        reader.read_exact(&mut mask)?;
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Ok((opcode, payload))
}

#[cfg(unix)]
fn drain_server_output<R: Read>(reader: R, stream: &'static str) {
    let mut reader = std::io::BufReader::new(reader);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        match reader.read_until(b'\n', &mut bytes) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(stream, %error, "Codex app-server output drain failed");
                break;
            }
        }
        let line = String::from_utf8_lossy(&bytes);
        let line = line.trim_end_matches(['\r', '\n']);
        if stream == "stderr" {
            tracing::warn!(stream, %line, "Codex app-server output");
        } else {
            tracing::debug!(stream, %line, "Codex app-server output");
        }
    }
}

#[cfg(unix)]
fn wait_for_socket(endpoint: &Path, child: &mut std::process::Child) -> anyhow::Result<bool> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if endpoint.exists() {
            return Ok(true);
        }
        if let Some(status) = child.try_wait()? {
            return Err(anyhow::anyhow!(
                "Codex app-server exited before readiness: {status}"
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(false)
}

#[cfg(unix)]
fn rotate_managed_endpoint(home: &Path, instance: &str, locator: &mut SessionLocator) {
    let parent = home.join("transport").join("codex");
    locator.endpoint = Some(parent.join(managed_endpoint_name(instance, Uuid::new_v4())));
    locator.server_pid = None;
    locator.server_start_token = None;
}

#[cfg(unix)]
fn managed_endpoint_name(instance: &str, uuid: Uuid) -> String {
    format!(
        "{}{}.sock",
        managed_endpoint_prefix(instance),
        uuid.simple()
    )
}

#[cfg(unix)]
fn managed_endpoint_prefix(instance: &str) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(instance.as_bytes());
    format!("c{}-", hex::encode(&digest[..6]))
}

#[cfg(unix)]
fn valid_managed_endpoint_name(instance: &str, name: &str) -> bool {
    let compact = name
        .strip_prefix(&managed_endpoint_prefix(instance))
        .and_then(|uuid| uuid.strip_suffix(".sock"))
        .and_then(|uuid| Uuid::parse_str(uuid).ok())
        .is_some_and(|uuid| managed_endpoint_name(instance, uuid) == name);
    if compact {
        return true;
    }

    // Accept the pre-bounded form so a new daemon can clean up a locator
    // persisted by the previous binary after an unclean shutdown.
    let safe_instance = super::receipt::safe_component(instance);
    name.strip_prefix(&format!("{safe_instance}-"))
        .and_then(|uuid| uuid.strip_suffix(".sock"))
        .and_then(|uuid| Uuid::parse_str(uuid).ok())
        .is_some_and(|uuid| format!("{safe_instance}-{uuid}.sock") == name)
}

#[cfg(unix)]
fn launch_managed_server(
    home: &Path,
    instance: &str,
    codex: &str,
    locator: &mut SessionLocator,
    cwd: Option<&Path>,
) -> anyhow::Result<()> {
    let config_args = crate::mcp_config::codex_managed_config_args(home, Some(instance), cwd)?;
    let cwd = cwd.unwrap_or_else(|| Path::new("."));
    let child = CodexNativeShared::launch(codex, locator, cwd, &config_args)?;
    let pid = child.id();
    let start_token = crate::process::process_start_token(pid);
    locator.managed = true;
    locator.server_pid = Some(pid);
    locator.server_start_token = start_token;
    if let Err(error) = super::registry::save_session_locator(home, instance, locator) {
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    managed_servers()
        .lock()
        .expect("Codex server registry lock")
        .insert(
            server_key(home, instance),
            ManagedServer {
                child,
                pid,
                start_token,
            },
        );
    Ok(())
}

#[cfg(unix)]
fn remove_managed_socket_file(endpoint: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(endpoint) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn remove_managed_endpoint(
    home: &Path,
    instance: &str,
    locator: &SessionLocator,
) -> anyhow::Result<()> {
    use std::os::unix::fs::FileTypeExt;

    let Some(endpoint) = locator.endpoint.as_ref() else {
        return Ok(());
    };
    let expected_parent = home.join("transport").join("codex");
    let valid_name = endpoint
        .parent()
        .is_some_and(|parent| parent == expected_parent)
        && endpoint
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| valid_managed_endpoint_name(instance, name));
    if !valid_name {
        return Err(anyhow::anyhow!(
            "refusing to remove Codex endpoint outside managed namespace: {}",
            endpoint.display()
        ));
    }
    let metadata = match std::fs::symlink_metadata(endpoint) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(anyhow::anyhow!(
                "cannot inspect Codex managed socket {}: {error}",
                endpoint.display()
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(anyhow::anyhow!(
            "refusing to remove symlink at Codex managed socket {}",
            endpoint.display()
        ));
    }
    if !metadata.file_type().is_socket() {
        return Err(anyhow::anyhow!(
            "refusing to remove non-socket Codex managed endpoint {}",
            endpoint.display()
        ));
    }
    remove_managed_socket_file(endpoint).map_err(|error| {
        anyhow::anyhow!(
            "cannot remove Codex managed socket {}: {error}",
            endpoint.display()
        )
    })
}

#[cfg(unix)]
fn stop_owned_process(
    pid: u32,
    start_token: u64,
    child: Option<&mut std::process::Child>,
) -> anyhow::Result<()> {
    let mut child = child;
    if let Some(child) = child.as_deref_mut() {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
    }
    match crate::process::process_start_token(pid) {
        None => return Ok(()),
        Some(observed) if observed != start_token => {
            return Err(anyhow::anyhow!(
                "Codex managed server PID {pid} changed identity before teardown"
            ));
        }
        Some(_) => {}
    }
    crate::process::terminate(pid);
    for _ in 0..5 {
        if let Some(child) = child.as_deref_mut() {
            if child.try_wait()?.is_some() {
                return Ok(());
            }
        }
        if crate::process::process_start_token(pid).is_none() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if let Some(child) = child.as_deref_mut() {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
    }
    if crate::process::process_start_token(pid) == Some(start_token) {
        if crate::process::process_group_id(pid) == Some(pid) {
            if crate::process::process_start_token(pid) == Some(start_token) {
                crate::process::kill_process_tree(pid);
            }
        } else if crate::process::process_start_token(pid) == Some(start_token) {
            crate::process::kill_process(pid);
        }
        for _ in 0..5 {
            if let Some(child) = child.as_deref_mut() {
                if child.try_wait()?.is_some() {
                    return Ok(());
                }
            }
            if crate::process::process_start_token(pid).is_none() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    if let Some(child) = child {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
    }
    if let Some(observed) = crate::process::process_start_token(pid) {
        return Err(anyhow::anyhow!(
            "Codex managed server PID {pid} remained alive with identity {observed}"
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn stop_instance_server(home: &Path, instance: &str) -> anyhow::Result<()> {
    let key = server_key(home, instance);
    let locator = super::registry::load_session_locator(home, instance).ok();
    let persisted_owned = locator
        .as_ref()
        .is_some_and(|locator| locator.managed && persisted_server_owned(locator));
    let mut in_memory_owned = false;
    let mut in_memory_identity = None;
    if let Some(mut server) = managed_servers()
        .lock()
        .expect("Codex server registry lock")
        .remove(&key)
    {
        in_memory_owned = locator.as_ref().is_some_and(|locator| {
            locator.managed
                && locator.server_pid == Some(server.pid)
                && locator.server_start_token == server.start_token
        });
        in_memory_identity = server
            .start_token
            .map(|start_token| (server.pid, start_token));
        if let Some(start_token) = server.start_token {
            stop_owned_process(server.pid, start_token, Some(&mut server.child))?;
            let _ = server.child.wait()?;
        } else if server.child.try_wait()?.is_none() {
            server.child.kill()?;
            let _ = server.child.wait()?;
        }
    }
    if persisted_owned {
        let locator = locator
            .as_ref()
            .expect("persisted ownership requires a locator");
        let pid = locator
            .server_pid
            .expect("persisted ownership requires a server PID");
        let start_token = locator
            .server_start_token
            .expect("persisted ownership requires a start token");
        if in_memory_identity != Some((pid, start_token)) {
            stop_owned_process(pid, start_token, None)?;
        }
    }
    if persisted_owned || in_memory_owned {
        if let Some(locator) = locator.as_ref() {
            remove_managed_endpoint(home, instance, locator)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn stop_instance_server(_home: &Path, _instance: &str) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn prepare_managed_tui(
    home: &Path,
    instance: &str,
    codex: &str,
    mut locator: SessionLocator,
    cwd: Option<&Path>,
) -> anyhow::Result<SessionLocator> {
    locator.managed = true;
    let server_owned = locator
        .endpoint
        .as_ref()
        .is_some_and(|_| persisted_server_owned(&locator))
        || in_memory_server_owned(home, instance, &locator);
    if server_owned {
        stop_instance_server(home, instance)?;
    }
    rotate_managed_endpoint(home, instance, &mut locator);
    launch_managed_server(home, instance, codex, &mut locator, cwd)?;
    Ok(locator)
}

#[cfg(not(unix))]
pub(crate) fn prepare_managed_tui(
    _home: &Path,
    _instance: &str,
    _codex: &str,
    _locator: SessionLocator,
    _cwd: Option<&Path>,
) -> anyhow::Result<SessionLocator> {
    Err(anyhow::anyhow!("Codex NativeShared requires Unix sockets"))
}

#[async_trait::async_trait]
impl AgentDeliveryTransport for CodexNativeShared {
    fn mode(&self) -> TransportMode {
        TransportMode::NativeShared
    }

    async fn start_or_attach(
        &mut self,
        locator: SessionLocator,
    ) -> anyhow::Result<TransportCapability> {
        self.start_or_attach_blocking(locator)
    }

    async fn deliver(&mut self, envelope: DeliveryEnvelope) -> anyhow::Result<DeliveryReceipt> {
        self.deliver_blocking(envelope)
    }

    async fn next_event(&mut self) -> anyhow::Result<BackendEvent> {
        #[cfg(unix)]
        {
            self.next_event_blocking()
        }
        #[cfg(not(unix))]
        {
            Err(anyhow::anyhow!("Codex NativeShared requires Unix sockets"))
        }
    }

    async fn reconcile(&mut self, delivery_id: Uuid) -> anyhow::Result<DeliveryState> {
        let store = ReceiptStore::for_instance(&self.home, &self.instance)?;
        let state = store
            .latest(delivery_id)?
            .map(|receipt| receipt.state)
            .unwrap_or(DeliveryState::Ambiguous);
        if state.is_terminal() {
            return Ok(state);
        }
        if matches!(
            state,
            DeliveryState::Queued | DeliveryState::ProtocolAccepted | DeliveryState::TurnStarted
        ) {
            // A process/connection restart after acceptance is not proof of
            // failure. Preserve the ambiguity and require history/event
            // reconciliation before any retry.
            if !self.ready {
                if let Some(locator) = self.locator.clone() {
                    if self.start_or_attach_blocking(locator).is_err() {
                        return Ok(DeliveryState::Ambiguous);
                    }
                } else {
                    return Ok(DeliveryState::Ambiguous);
                }
            }
            return Ok(DeliveryState::Ambiguous);
        }
        Ok(state)
    }

    async fn health(&self) -> TransportCapability {
        TransportCapability {
            backend: "codex".to_string(),
            mode: TransportMode::NativeShared,
            ready: self.ready,
            backend_version: self.backend_version.clone(),
            degraded_reason: (!self.ready)
                .then(|| "Codex app-server handshake not verified".to_string()),
        }
    }
}

#[cfg(all(test, unix))]
#[path = "codex_app_server/tests.rs"]
mod tests;
