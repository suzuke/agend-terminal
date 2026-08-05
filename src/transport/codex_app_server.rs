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
use std::collections::{HashMap, VecDeque};
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};
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
    events: VecDeque<BackendEvent>,
    #[cfg(unix)]
    writer: Option<std::os::unix::net::UnixStream>,
    #[cfg(unix)]
    reader: Option<std::os::unix::net::UnixStream>,
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
            events: VecDeque::new(),
            #[cfg(unix)]
            writer: None,
            #[cfg(unix)]
            reader: None,
        }
    }

    pub(crate) fn deliver_blocking(
        &mut self,
        envelope: DeliveryEnvelope,
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
            let mut child = std::process::Command::new(codex)
                .args(["app-server", "--listen", &locator.remote_attach_arg()])
                .current_dir(cwd)
                .spawn()?;
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
        if locator.backend != "codex" {
            return Err(anyhow::anyhow!("NativeShared locator backend is not codex"));
        }
        if locator.endpoint.is_none() || locator.thread_id.as_deref().unwrap_or_default().is_empty()
        {
            return Err(anyhow::anyhow!(
                "Codex NativeShared requires both endpoint and thread_id"
            ));
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
            self.send_request(
                "thread/resume",
                json!({"threadId": locator.thread_id.as_deref()}),
            )?;
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
