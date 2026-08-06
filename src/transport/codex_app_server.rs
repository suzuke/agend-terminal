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
use std::io::Read;

#[cfg(unix)]
const CODEX_PROTOCOL: &str = "v2";
#[cfg(unix)]
const IO_TIMEOUT: Duration = Duration::from_secs(10);

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
            failed.detail = Some("NativeShared readiness failed closed".to_string());
            store.record(failed)?;
            return Err(error);
        }
        if let Some(locator) = self.locator.clone() {
            envelope.session = locator;
        }
        if self.in_flight.is_some() && !matches!(envelope.kind, DeliveryKind::Steer) {
            let mut queued = DeliveryReceipt::for_state(&envelope, DeliveryState::Queued);
            queued.detail = Some("one ordinary turn is already in flight".to_string());
            store.record(queued)?;
            return Err(anyhow::anyhow!(
                "Codex thread already has an ordinary turn in flight"
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
        self.pending.insert(envelope.delivery_id, envelope.clone());
        self.in_flight = Some(envelope.delivery_id);
        let mut receipt = DeliveryReceipt::for_state(&envelope, DeliveryState::ProtocolAccepted);
        receipt.protocol_request_id = Some(backend_request_id.unwrap_or(request_id));
        receipt.tui_visibility = Some("shared_codex_thread".to_string());
        receipt.detail = Some("Codex app-server accepted turn request".to_string());
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
                .args(["app-server", "--listen", &locator.remote_attach_arg()])
                .current_dir(cwd)
                .process_group(0);
            let mut child = command.spawn()?;
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
            let _ = (codex, locator, cwd);
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
        match envelope.kind {
            DeliveryKind::Steer => {
                let turn_id = self.active_turn_id.clone().ok_or_else(|| {
                    anyhow::anyhow!("turn/steer requires an active Codex turn id")
                })?;
                Ok((
                    "turn/steer".to_string(),
                    json!({
                        "threadId": thread_id,
                        "expectedTurnId": turn_id,
                        "input": input,
                    }),
                ))
            }
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
                self.send_request("thread/resume", json!({"threadId": thread_id}))?;
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
            let thread_ids = loaded_thread_ids(&response);
            match thread_ids.as_slice() {
                [thread_id] => return Ok(thread_id.clone()),
                [] if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                [] => {
                    return Err(anyhow::anyhow!(
                        "Codex TUI did not publish a loaded thread before the delivery deadline"
                    ));
                }
                _ => {
                    return Err(anyhow::anyhow!(
                        "Codex app-server has {} loaded threads; refusing ambiguous TUI delivery",
                        thread_ids.len()
                    ));
                }
            }
        }
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

#[cfg(unix)]
fn loaded_thread_ids(response: &Value) -> Vec<String> {
    let mut thread_ids = Vec::new();
    for item in response
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let thread_id = item
            .as_str()
            .or_else(|| item.get("id").and_then(Value::as_str));
        if let Some(thread_id) = thread_id.filter(|thread_id| !thread_id.is_empty()) {
            if !thread_ids.iter().any(|loaded| loaded == thread_id) {
                thread_ids.push(thread_id.to_string());
            }
        }
    }
    thread_ids
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
    locator.endpoint = Some(parent.join(format!(
        "{}-{}.sock",
        super::receipt::safe_component(instance),
        Uuid::new_v4()
    )));
    locator.server_pid = None;
    locator.server_start_token = None;
}

#[cfg(unix)]
fn launch_managed_server(
    home: &Path,
    instance: &str,
    codex: &str,
    locator: &mut SessionLocator,
    cwd: Option<&Path>,
) -> anyhow::Result<()> {
    let child = CodexNativeShared::launch(codex, locator, cwd.unwrap_or_else(|| Path::new(".")))?;
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
    let safe_instance = super::receipt::safe_component(instance);
    let valid_name = endpoint
        .parent()
        .is_some_and(|parent| parent == expected_parent)
        && endpoint
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.strip_prefix(&format!("{safe_instance}-"))
                    .and_then(|uuid| uuid.strip_suffix(".sock"))
                    .and_then(|uuid| Uuid::parse_str(uuid).ok())
                    .is_some_and(|uuid| format!("{safe_instance}-{uuid}.sock") == name)
            });
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
    std::fs::remove_file(endpoint).map_err(|error| {
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
    locator.thread_id = None;
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
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread;

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
                let (_, body) = read_websocket_frame(&mut stream).expect("read client frame");
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
                        write_server_frame(
                            &mut stream,
                            json!({"id": id, "result": {"thread": {"id": "thread-1"}}}),
                        );
                    }
                    "thread/loaded/list" => {
                        write_server_frame(
                            &mut stream,
                            json!({"id": id, "result": {"data": [{"id": "thread-1"}]}}),
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
        assert_eq!(
            receipt.detail.as_deref(),
            Some("NativeShared readiness failed closed")
        );
        let _ = std::fs::remove_dir_all(home);
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
                "trap 'printf term > \"$AGEND_TERM_MARKER\"; exit 0' TERM; while :; do :; done",
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
        let home =
            std::env::temp_dir().join(format!("agend-codex-owned-relaunch-{}", Uuid::new_v4()));
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
        assert!(
            endpoint.as_os_str().as_encoded_bytes().len() < 104,
            "managed Codex socket must fit macOS sockaddr_un.sun_path: {} ({} bytes)",
            endpoint.display(),
            endpoint.as_os_str().as_encoded_bytes().len()
        );
    }

    #[cfg(unix)]
    #[test]
    fn teardown_removes_owned_socket_but_not_other_socket() {
        let suffix = Uuid::new_v4().to_string();
        let home = std::path::PathBuf::from("/tmp").join(format!("c-{}", &suffix[..8]));
        let instance = "a";
        let socket_dir = home.join("transport/codex");
        std::fs::create_dir_all(&socket_dir).expect("socket dir");
        let endpoint = socket_dir.join(format!("a-{}.sock", Uuid::new_v4()));
        let other_endpoint = socket_dir.join("other.sock");
        let listener = UnixListener::bind(&endpoint).expect("owned socket");
        let other_listener = UnixListener::bind(&other_endpoint).expect("other socket");
        drop(listener);

        let term_marker = home.join("term-grace");
        let trap_ready = home.join("trap-ready");
        let child = std::process::Command::new("sh")
            .args([
                "-c",
                "trap 'printf done > \"$AGEND_TERM_MARKER\"; exit 0' TERM; touch \"$AGEND_TRAP_READY\"; while :; do :; done",
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
                    "trap 'printf term > \"$AGEND_TERM_MARKER\"' TERM; touch \"$AGEND_TRAP_READY\"; while :; do :; done",
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
}
