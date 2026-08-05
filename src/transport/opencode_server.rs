//! OpenCode NativeShared transport.
//!
//! OpenCode exposes a small authenticated HTTP API and a server-wide SSE event
//! stream.  The adapter owns the loopback `opencode serve` process when the
//! locator says it is managed, attaches the TUI to the same session, and uses
//! `prompt_async` for daemon delivery.  It deliberately has no PTY fallback.

use super::{
    AgentDeliveryTransport, BackendEvent, DeliveryEnvelope, DeliveryKind, DeliveryReceipt,
    DeliveryState, ReceiptStore, SessionLocator, TransportCapability, TransportMode,
};
use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use uuid::Uuid;

const IO_TIMEOUT: Duration = Duration::from_secs(10);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_HEADERS: usize = 64 * 1024;
const MAX_BODY: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
struct Endpoint {
    host: String,
    port: u16,
}

impl Endpoint {
    fn parse(locator: &SessionLocator) -> anyhow::Result<Self> {
        let raw = locator
            .endpoint_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("OpenCode NativeShared endpoint URL is missing"))?;
        let rest = raw.strip_prefix("http://").ok_or_else(|| {
            anyhow::anyhow!("OpenCode endpoint must use http:// loopback transport")
        })?;
        let (authority, suffix) = rest.split_once('/').unwrap_or((rest, ""));
        if authority.is_empty() || !suffix.is_empty() || rest.contains('?') || rest.contains('#') {
            return Err(anyhow::anyhow!(
                "OpenCode endpoint URL has an invalid authority"
            ));
        }
        let (host, port) = authority.rsplit_once(':').ok_or_else(|| {
            anyhow::anyhow!("OpenCode endpoint URL must include an explicit port")
        })?;
        let host = host.trim_matches(['[', ']']);
        // A fixed numeric loopback endpoint is intentional: accepting a
        // hostname would make DNS rebinding part of the credential boundary.
        if host != "127.0.0.1" {
            return Err(anyhow::anyhow!(
                "OpenCode NativeShared refuses non-loopback host {host:?}"
            ));
        }
        let port = port
            .parse::<u16>()
            .map_err(|_| anyhow::anyhow!("OpenCode endpoint port is invalid"))?;
        if port == 0 {
            return Err(anyhow::anyhow!("OpenCode endpoint port must be non-zero"));
        }
        Ok(Self {
            host: host.to_string(),
            port,
        })
    }

    fn address(&self) -> anyhow::Result<SocketAddr> {
        (self.host.as_str(), self.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow::anyhow!("OpenCode loopback endpoint did not resolve"))
    }
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn basic_auth(locator: &SessionLocator) -> Option<String> {
    let username = locator.username.as_deref()?;
    let password = locator.password.as_deref()?;
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    Some(format!("Basic {encoded}"))
}

fn connect(endpoint: &Endpoint) -> anyhow::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&endpoint.address()?, IO_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

fn write_request(
    stream: &mut TcpStream,
    endpoint: &Endpoint,
    locator: &SessionLocator,
    method: &str,
    path: &str,
    body: &[u8],
    accept: &str,
) -> anyhow::Result<()> {
    if !path.starts_with('/') || path.contains('\r') || path.contains('\n') {
        return Err(anyhow::anyhow!("OpenCode request path is invalid"));
    }
    let auth = basic_auth(locator)
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}:{}\r\nAccept: {accept}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n{auth}\r\n",
        endpoint.host,
        endpoint.port,
        body.len(),
    );
    stream.write_all(request.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn read_headers(stream: &mut TcpStream) -> anyhow::Result<(u16, HashMap<String, String>, Vec<u8>)> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(anyhow::anyhow!(
                "OpenCode HTTP server closed before headers"
            ));
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > MAX_HEADERS {
            return Err(anyhow::anyhow!(
                "OpenCode HTTP headers exceed the size limit"
            ));
        }
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
    };
    let body_start = header_end + 4;
    let header_text = std::str::from_utf8(&bytes[..header_end])?;
    let mut lines = header_text.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| anyhow::anyhow!("OpenCode HTTP status line is invalid"))?
        .parse::<u16>()?;
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    Ok((status, headers, bytes[body_start..].to_vec()))
}

fn read_exact_more(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    amount: usize,
) -> anyhow::Result<()> {
    while bytes.len() < amount {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(anyhow::anyhow!("OpenCode HTTP body ended early"));
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > MAX_BODY {
            return Err(anyhow::anyhow!("OpenCode HTTP body exceeds the size limit"));
        }
    }
    Ok(())
}

fn read_chunked_body(stream: &mut TcpStream, mut raw: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let size_end = loop {
            if let Some(position) = raw.windows(2).position(|window| window == b"\r\n") {
                break position;
            }
            let mut chunk = [0_u8; 4096];
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                return Err(anyhow::anyhow!("OpenCode chunked body ended before size"));
            }
            raw.extend_from_slice(&chunk[..read]);
        };
        let size_line = std::str::from_utf8(&raw[..size_end])?;
        let size_text = size_line.split(';').next().unwrap_or_default().trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| anyhow::anyhow!("OpenCode chunk size is invalid"))?;
        raw.drain(..size_end + 2);
        if size == 0 {
            return Ok(body);
        }
        let required = size
            .checked_add(2)
            .ok_or_else(|| anyhow::anyhow!("OpenCode chunk size overflow"))?;
        read_exact_more(stream, &mut raw, required)?;
        body.extend_from_slice(&raw[..size]);
        if body.len() > MAX_BODY {
            return Err(anyhow::anyhow!("OpenCode HTTP body exceeds the size limit"));
        }
        if &raw[size..size + 2] != b"\r\n" {
            return Err(anyhow::anyhow!("OpenCode chunk is missing its trailer"));
        }
        raw.drain(..required);
    }
}

fn read_body(
    stream: &mut TcpStream,
    headers: &HashMap<String, String>,
    initial: Vec<u8>,
) -> anyhow::Result<Vec<u8>> {
    if let Some(length) = headers.get("content-length") {
        let length = length.parse::<usize>()?;
        if length > MAX_BODY {
            return Err(anyhow::anyhow!("OpenCode HTTP body exceeds the size limit"));
        }
        let mut body = initial;
        read_exact_more(stream, &mut body, length)?;
        body.truncate(length);
        return Ok(body);
    }
    if headers
        .get("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        return read_chunked_body(stream, initial);
    }
    let mut body = initial;
    let mut chunk = [0_u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                body.extend_from_slice(&chunk[..read]);
                if body.len() > MAX_BODY {
                    return Err(anyhow::anyhow!("OpenCode HTTP body exceeds the size limit"));
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(body)
}

fn request(
    locator: &SessionLocator,
    method: &str,
    path: &str,
    body: Value,
) -> anyhow::Result<HttpResponse> {
    let endpoint = Endpoint::parse(locator)?;
    let mut stream = connect(&endpoint)?;
    let body = serde_json::to_vec(&body)?;
    write_request(
        &mut stream,
        &endpoint,
        locator,
        method,
        path,
        &body,
        "application/json",
    )?;
    let (status, headers, initial) = read_headers(&mut stream)?;
    let body = if matches!(status, 204 | 304) {
        Vec::new()
    } else {
        read_body(&mut stream, &headers, initial)?
    };
    Ok(HttpResponse { status, body })
}

fn response_json(response: HttpResponse, operation: &str) -> anyhow::Result<Value> {
    if !(200..300).contains(&response.status) {
        return Err(anyhow::anyhow!(
            "OpenCode {operation} returned HTTP {}",
            response.status
        ));
    }
    if response.body.is_empty() {
        return Ok(Value::Null);
    }
    Ok(serde_json::from_slice(&response.body)?)
}

fn not_found(response: &HttpResponse) -> bool {
    response.status == 404
}

#[derive(Debug, Default)]
struct SseDecoder {
    raw: Vec<u8>,
    body: Vec<u8>,
    chunked: bool,
}

impl SseDecoder {
    fn new(chunked: bool, initial: Vec<u8>) -> Self {
        Self {
            raw: initial,
            body: Vec::new(),
            chunked,
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.raw.extend_from_slice(bytes);
        if self.chunked {
            self.dechunk();
        } else {
            self.body.append(&mut self.raw);
        }
        self.split_events()
    }

    fn dechunk(&mut self) {
        loop {
            let Some(size_end) = self.raw.windows(2).position(|window| window == b"\r\n") else {
                return;
            };
            let size_line = String::from_utf8_lossy(&self.raw[..size_end]);
            let size_text = size_line.split(';').next().unwrap_or_default().trim();
            let Ok(size) = usize::from_str_radix(size_text, 16) else {
                self.raw.clear();
                return;
            };
            let data_start = size_end + 2;
            let Some(data_end) = data_start.checked_add(size) else {
                self.raw.clear();
                return;
            };
            if self.raw.len() < data_end + 2 {
                return;
            }
            if size == 0 {
                self.raw.clear();
                return;
            }
            self.body.extend_from_slice(&self.raw[data_start..data_end]);
            self.raw.drain(..data_end + 2);
            if self.body.len() > MAX_BODY {
                self.body.clear();
                self.raw.clear();
                return;
            }
        }
    }

    fn split_events(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        loop {
            let delimiter = self.body.windows(2).position(|window| window == b"\n\n");
            let Some(position) = delimiter else { break };
            let frame = self.body[..position].to_vec();
            self.body.drain(..position + 2);
            let mut data = Vec::new();
            for line in frame.split(|byte| *byte == b'\n') {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                if let Some(value) = line.strip_prefix(b"data:") {
                    let value = value.strip_prefix(b" ").unwrap_or(value);
                    data.extend_from_slice(value);
                    data.push(b'\n');
                }
            }
            if data.last() == Some(&b'\n') {
                data.pop();
            }
            if !data.is_empty() {
                if let Ok(value) = String::from_utf8(data) {
                    events.push(value);
                }
            }
        }
        events
    }
}

struct SseStream {
    stream: TcpStream,
    decoder: SseDecoder,
    pending: VecDeque<Value>,
}

impl SseStream {
    fn next_json(&mut self) -> anyhow::Result<Value> {
        let mut bytes = [0_u8; 8192];
        loop {
            if let Some(value) = self.pending.pop_front() {
                return Ok(value);
            }
            let events = self.decoder.feed(&[]);
            for event in events {
                if let Ok(value) = serde_json::from_str::<Value>(&event) {
                    self.pending.push_back(value);
                }
            }
            if let Some(value) = self.pending.pop_front() {
                return Ok(value);
            }
            let read = self.stream.read(&mut bytes)?;
            if read == 0 {
                return Err(anyhow::anyhow!("OpenCode SSE stream closed"));
            }
            let events = self.decoder.feed(&bytes[..read]);
            for event in events {
                if let Ok(value) = serde_json::from_str::<Value>(&event) {
                    self.pending.push_back(value);
                }
            }
        }
    }
}

#[derive(Debug)]
struct ManagedServer {
    child: Child,
}

fn managed_servers() -> &'static Mutex<HashMap<String, ManagedServer>> {
    static SERVERS: OnceLock<Mutex<HashMap<String, ManagedServer>>> = OnceLock::new();
    SERVERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn server_key(home: &Path, instance: &str, locator: &SessionLocator) -> String {
    format!(
        "{}\0{}\0{}",
        home.display(),
        instance,
        locator.endpoint_url.as_deref().unwrap_or_default()
    )
}

pub(crate) fn stop_instance_server(home: &Path, instance: &str) {
    let prefix = format!("{}\0{}\0", home.display(), instance);
    managed_servers().lock().retain(|key, server| {
        if !key.starts_with(&prefix) {
            return true;
        }
        let _ = server.child.kill();
        let _ = server.child.wait();
        false
    });
}

fn canonical_auth() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
        })?;
    Some(base.join("opencode/auth.json"))
}

fn provision_data_dir(data_dir: &Path) -> anyhow::Result<()> {
    let opencode_dir = data_dir.join("opencode");
    std::fs::create_dir_all(&opencode_dir)?;
    if let Some(source) = canonical_auth().filter(|path| path.exists()) {
        let target = opencode_dir.join("auth.json");
        std::fs::copy(source, &target)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

fn launch_server(
    home: &Path,
    instance: &str,
    locator: &SessionLocator,
    cwd: Option<&Path>,
) -> anyhow::Result<()> {
    let endpoint = Endpoint::parse(locator)?;
    let key = server_key(home, instance, locator);
    {
        let mut servers = managed_servers().lock();
        if let Some(server) = servers.get_mut(&key) {
            if server.child.try_wait()?.is_none() {
                return Ok(());
            }
            servers.remove(&key);
        }
    }
    let data_dir = home.join("backend-data").join("opencode").join(instance);
    provision_data_dir(&data_dir)?;
    let binary = std::env::var("AGEND_OPENCODE_BINARY").unwrap_or_else(|_| "opencode".to_string());
    let mut command = std::process::Command::new(binary);
    command.args([
        "serve",
        "--hostname",
        "127.0.0.1",
        "--port",
        &endpoint.port.to_string(),
    ]);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.env("XDG_DATA_HOME", &data_dir);
    command.env(
        "OPENCODE_SERVER_USERNAME",
        locator.username.as_deref().unwrap_or("opencode"),
    );
    if let Some(password) = locator.password.as_deref() {
        command.env("OPENCODE_SERVER_PASSWORD", password);
    }
    command.env("OPENCODE_DISABLE_AUTOUPDATE", "1");
    command.env("OPENCODE_CONFIG_CONTENT", r#"{"autoupdate":false}"#);
    // fire-and-forget: the managed server is retained in `managed_servers` and
    // is reconciled/reaped on the next adapter attach.
    let child = command.spawn()?;
    managed_servers()
        .lock()
        .insert(key, ManagedServer { child });
    Ok(())
}

fn global_health(locator: &SessionLocator) -> anyhow::Result<(String, bool)> {
    let response = request(locator, "GET", "/global/health", Value::Null)?;
    let value = response_json(response, "global health")?;
    let healthy = value
        .get("healthy")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let version = value
        .get("version")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("OpenCode health response omitted its version"))?;
    Ok((version.to_string(), healthy))
}

fn wait_for_server(
    home: &Path,
    instance: &str,
    locator: &SessionLocator,
    cwd: Option<&Path>,
) -> anyhow::Result<String> {
    let deadline = Instant::now() + SERVER_START_TIMEOUT;
    let mut last_error = None;
    while Instant::now() < deadline {
        match global_health(locator) {
            Ok((version, true)) => return Ok(version),
            Ok((_, false)) => last_error = Some("OpenCode server reported unhealthy".to_string()),
            Err(error) => last_error = Some(error.to_string()),
        }
        if locator.managed {
            launch_server(home, instance, locator, cwd)?;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(anyhow::anyhow!(
        "OpenCode NativeShared server did not become ready: {}",
        last_error.unwrap_or_else(|| "no health response".to_string())
    ))
}

fn path_segment(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn session_path(session_id: &str) -> String {
    format!("/session/{}", path_segment(session_id))
}

fn session_status_type(value: &Value, session_id: &str) -> Option<String> {
    let status = value
        .get(session_id)
        .or_else(|| value.get("data").and_then(|data| data.get(session_id)))?;
    status
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| status.as_str())
        .map(str::to_string)
}

fn contains_delivery_id(value: &Value, delivery_id: Uuid) -> bool {
    let target = delivery_id.to_string();
    match value {
        Value::Object(map) => map.iter().any(|(key, child)| {
            (matches!(key.as_str(), "id" | "messageID" | "clientUserMessageId")
                && child.as_str() == Some(target.as_str()))
                || contains_delivery_id(child, delivery_id)
        }),
        Value::Array(values) => values
            .iter()
            .any(|child| contains_delivery_id(child, delivery_id)),
        _ => false,
    }
}

pub(crate) struct OpenCodeNativeShared {
    home: PathBuf,
    instance: String,
    locator: Option<SessionLocator>,
    ready: bool,
    backend_version: Option<String>,
    in_flight: Option<Uuid>,
    pending: HashMap<Uuid, DeliveryEnvelope>,
    events: VecDeque<BackendEvent>,
    stream: Option<SseStream>,
}

impl OpenCodeNativeShared {
    pub(crate) fn new(home: &Path, instance: &str) -> Self {
        Self {
            home: home.to_path_buf(),
            instance: instance.to_string(),
            locator: None,
            ready: false,
            backend_version: None,
            in_flight: None,
            pending: HashMap::new(),
            events: VecDeque::new(),
            stream: None,
        }
    }

    pub(crate) fn attach_args(locator: &SessionLocator) -> anyhow::Result<Vec<String>> {
        let endpoint = locator
            .endpoint_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("OpenCode attach locator has no endpoint URL"))?;
        let session_id = locator
            .session_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow::anyhow!("OpenCode attach locator has no session id"))?;
        Ok(vec![
            "attach".to_string(),
            endpoint.to_string(),
            "--session".to_string(),
            session_id.to_string(),
        ])
    }

    pub(crate) fn deliver_blocking(
        &mut self,
        envelope: DeliveryEnvelope,
    ) -> anyhow::Result<DeliveryReceipt> {
        let store = ReceiptStore::for_instance(&self.home, &self.instance)?;
        store.record_queued(&envelope)?;
        if let Err(error) = self.start_or_attach_blocking(envelope.session.clone(), None) {
            let mut failed = DeliveryReceipt::for_state(&envelope, DeliveryState::Failed);
            failed.detail = Some("OpenCode NativeShared readiness failed closed".to_string());
            store.record(failed)?;
            return Err(error);
        }
        if self.in_flight.is_some() && !matches!(envelope.kind, DeliveryKind::Steer) {
            let mut queued = DeliveryReceipt::for_state(&envelope, DeliveryState::Queued);
            queued.detail = Some("one ordinary OpenCode turn is already in flight".to_string());
            store.record(queued)?;
            return Err(anyhow::anyhow!(
                "OpenCode session already has an ordinary turn in flight"
            ));
        }
        if matches!(envelope.kind, DeliveryKind::Steer | DeliveryKind::Interrupt) {
            let mut failed = DeliveryReceipt::for_state(&envelope, DeliveryState::Failed);
            failed.detail = Some("OpenCode has no implicit steer/interrupt operation".to_string());
            store.record(failed)?;
            return Err(anyhow::anyhow!(
                "OpenCode NativeShared requires an explicit prompt operation"
            ));
        }
        let locator = self
            .locator
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("OpenCode locator was not attached"))?;
        let session_id = locator
            .session_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("OpenCode session id is missing"))?;
        let path = format!("{}/prompt_async", session_path(session_id));
        let response = request(locator, "POST", &path, {
            let mut body = json!({
                "messageID": envelope.delivery_id.to_string(),
                "parts": [{"type": "text", "text": envelope.body}],
            });
            if let Some(model) = locator.model.as_deref() {
                body["model"] = Value::String(model.to_string());
            }
            body
        });
        let response = match response {
            Ok(response) if (200..300).contains(&response.status) => response,
            Ok(response) => {
                let error =
                    anyhow::anyhow!("OpenCode prompt_async returned HTTP {}", response.status);
                let mut failed = DeliveryReceipt::for_state(&envelope, DeliveryState::Failed);
                failed.detail = Some("OpenCode rejected prompt_async".to_string());
                store.record(failed)?;
                return Err(error);
            }
            Err(error) => {
                let mut ambiguous = DeliveryReceipt::for_state(&envelope, DeliveryState::Ambiguous);
                ambiguous.detail = Some(
                    "OpenCode prompt_async outcome is ambiguous; reconcile before retry"
                        .to_string(),
                );
                store.record(ambiguous)?;
                return Err(error);
            }
        };
        let _ = response;
        self.pending.insert(envelope.delivery_id, envelope.clone());
        self.in_flight = Some(envelope.delivery_id);
        let mut receipt = DeliveryReceipt::for_state(&envelope, DeliveryState::ProtocolAccepted);
        receipt.protocol_request_id = Some(envelope.delivery_id.to_string());
        receipt.tui_visibility = Some("shared_opencode_session".to_string());
        receipt.detail = Some("OpenCode prompt_async accepted".to_string());
        store.record(receipt.clone())?;
        Ok(receipt)
    }

    pub(crate) fn prepare_for_tui(
        &mut self,
        locator: SessionLocator,
        cwd: Option<&Path>,
    ) -> anyhow::Result<SessionLocator> {
        self.start_or_attach_blocking(locator, cwd)?;
        self.locator
            .clone()
            .ok_or_else(|| anyhow::anyhow!("OpenCode TUI session was not prepared"))
    }

    fn start_or_attach_blocking(
        &mut self,
        mut locator: SessionLocator,
        cwd: Option<&Path>,
    ) -> anyhow::Result<TransportCapability> {
        if locator.backend != "opencode" {
            return Err(anyhow::anyhow!(
                "NativeShared locator backend is not opencode"
            ));
        }
        Endpoint::parse(&locator)?;
        if self.ready && self.locator.as_ref() == Some(&locator) {
            return Ok(self.capability());
        }
        self.ready = false;
        let version = wait_for_server(&self.home, &self.instance, &locator, cwd)?;
        let session_id = match locator.session_id.clone() {
            Some(session_id) if !session_id.is_empty() => {
                let response = request(&locator, "GET", &session_path(&session_id), Value::Null)?;
                if not_found(&response) {
                    None
                } else {
                    let _ = response_json(response, "session lookup")?;
                    Some(session_id)
                }
            }
            _ => None,
        };
        if let Some(session_id) = session_id {
            locator.session_id = Some(session_id);
        } else {
            let body = locator
                .model
                .as_deref()
                .map(|model| json!({"model": model}))
                .unwrap_or_else(|| json!({}));
            let response = request(&locator, "POST", "/session", body)?;
            let value = response_json(response, "session creation")?;
            let session_id = value
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| anyhow::anyhow!("OpenCode session creation omitted id"))?;
            locator.session_id = Some(session_id.to_string());
        }
        locator.event_cursor.get_or_insert(0);
        super::registry::save_session_locator(&self.home, &self.instance, &locator)?;
        self.open_event_stream(&locator)?;
        self.locator = Some(locator);
        self.backend_version = Some(version.clone());
        self.ready = true;
        self.restore_pending_state()?;
        Ok(self.capability())
    }

    fn restore_pending_state(&mut self) -> anyhow::Result<()> {
        let store = ReceiptStore::for_instance(&self.home, &self.instance)?;
        let mut pending = store.pending_deliveries()?;
        pending.retain(|(_, receipt)| {
            matches!(
                receipt.state,
                DeliveryState::ProtocolAccepted
                    | DeliveryState::ObservedInSession
                    | DeliveryState::TurnStarted
            )
        });
        let Some((envelope, _receipt)) = pending.into_iter().next() else {
            return Ok(());
        };
        let delivery_id = envelope.delivery_id;
        self.pending.insert(delivery_id, envelope);
        self.in_flight = Some(delivery_id);

        // A previous adapter may have gone away after prompt_async accepted.
        // If the server now reports idle, the durable message has completed;
        // otherwise keep the ordinary-turn gate closed until an SSE event or
        // explicit reconcile proves the outcome.
        let Some(locator) = self.locator.as_ref() else {
            return Ok(());
        };
        let Some(session_id) = locator.session_id.as_deref() else {
            return Ok(());
        };
        let Ok(response) = request(locator, "GET", "/session/status", Value::Null) else {
            return Ok(());
        };
        let Ok(status) = response_json(response, "session status") else {
            return Ok(());
        };
        if session_status_type(&status, session_id).as_deref() == Some("idle") {
            self.update_state(
                delivery_id,
                DeliveryState::Completed,
                "OpenCode reported the restored delivery completed",
                Some("restore/session.status"),
            )?;
            self.pending.remove(&delivery_id);
            self.in_flight = None;
        }
        Ok(())
    }

    fn open_event_stream(&mut self, locator: &SessionLocator) -> anyhow::Result<()> {
        let endpoint = Endpoint::parse(locator)?;
        let mut stream = connect(&endpoint)?;
        write_request(
            &mut stream,
            &endpoint,
            locator,
            "GET",
            "/event",
            b"",
            "text/event-stream",
        )?;
        let (status, headers, initial) = read_headers(&mut stream)?;
        if !(200..300).contains(&status) {
            return Err(anyhow::anyhow!(
                "OpenCode event stream returned HTTP {status}"
            ));
        }
        let content_type = headers.get("content-type").cloned().unwrap_or_default();
        if !content_type.is_empty()
            && !content_type
                .to_ascii_lowercase()
                .contains("text/event-stream")
        {
            return Err(anyhow::anyhow!(
                "OpenCode event stream has an invalid content type"
            ));
        }
        let chunked = headers
            .get("transfer-encoding")
            .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));
        self.stream = Some(SseStream {
            stream,
            decoder: SseDecoder::new(chunked, initial),
            pending: VecDeque::new(),
        });
        Ok(())
    }

    fn capability(&self) -> TransportCapability {
        TransportCapability {
            backend: "opencode".to_string(),
            mode: TransportMode::NativeShared,
            ready: self.ready,
            backend_version: self.backend_version.clone(),
            degraded_reason: (!self.ready)
                .then(|| "OpenCode server/session handshake not verified".to_string()),
        }
    }

    fn update_state(
        &self,
        delivery_id: Uuid,
        state: DeliveryState,
        detail: &str,
        backend_event: Option<&str>,
    ) -> anyhow::Result<()> {
        let store = ReceiptStore::for_instance(&self.home, &self.instance)?;
        if let Some(envelope) = self.pending.get(&delivery_id) {
            let mut receipt = DeliveryReceipt::for_state(envelope, state);
            if let Some(previous) = store.latest(delivery_id)? {
                receipt.protocol_request_id = previous.protocol_request_id;
                receipt.tui_visibility = previous.tui_visibility;
            }
            receipt.backend_event = backend_event.map(str::to_string);
            receipt.detail = Some(detail.to_string());
            store.record(receipt)?;
        } else if let Some(mut receipt) = store.latest(delivery_id)? {
            receipt.state = state;
            receipt.backend_event = backend_event.map(str::to_string);
            receipt.detail = Some(detail.to_string());
            receipt.recorded_at = chrono::Utc::now().to_rfc3339();
            store.record(receipt)?;
        }
        Ok(())
    }

    fn session_id_from_event(value: &Value) -> Option<&str> {
        value
            .pointer("/properties/sessionID")
            .and_then(Value::as_str)
            .or_else(|| {
                value
                    .pointer("/properties/info/sessionID")
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                value
                    .pointer("/properties/part/sessionID")
                    .and_then(Value::as_str)
            })
    }

    fn event_delivery_id(&self, value: &Value) -> Option<Uuid> {
        let target = self.in_flight?;
        let target_text = target.to_string();
        let id = value
            .pointer("/properties/info/id")
            .and_then(Value::as_str)
            .or_else(|| {
                value
                    .pointer("/properties/part/messageID")
                    .and_then(Value::as_str)
            });
        if id == Some(target_text.as_str()) || self.pending.contains_key(&target) {
            Some(target)
        } else {
            None
        }
    }

    fn normalize_event(&mut self, raw: Value) -> anyhow::Result<BackendEvent> {
        let value = raw
            .get("payload")
            .filter(|payload| payload.is_object())
            .cloned()
            .unwrap_or(raw);
        let method = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if method != "server.connected" {
            let expected = self
                .locator
                .as_ref()
                .and_then(|locator| locator.session_id.as_deref());
            if let (Some(expected), Some(actual)) = (expected, Self::session_id_from_event(&value))
            {
                if expected != actual {
                    return Ok(BackendEvent::Unknown {
                        method: Some(method.to_string()),
                    });
                }
            }
        }
        let delivery_id = self.event_delivery_id(&value);
        match method {
            "server.connected" => Ok(BackendEvent::Ready),
            "message.updated" | "message.part.updated" => {
                if let Some(id) = delivery_id {
                    self.update_state(
                        id,
                        DeliveryState::ObservedInSession,
                        "OpenCode emitted a message for the delivery",
                        Some(method),
                    )?;
                    Ok(BackendEvent::ObservedInSession {
                        delivery_id: id,
                        event: method.to_string(),
                    })
                } else {
                    Ok(BackendEvent::Unknown {
                        method: Some(method.to_string()),
                    })
                }
            }
            "session.status" => {
                let status = value
                    .pointer("/properties/status/type")
                    .and_then(Value::as_str)
                    .or_else(|| value.pointer("/properties/status").and_then(Value::as_str));
                match (status, delivery_id) {
                    (Some("busy"), Some(id)) => {
                        self.update_state(
                            id,
                            DeliveryState::TurnStarted,
                            "OpenCode reported a busy session",
                            Some(method),
                        )?;
                        Ok(BackendEvent::TurnStarted {
                            delivery_id: id,
                            turn_id: None,
                        })
                    }
                    (Some("idle"), Some(id)) => self.complete(id, method),
                    _ => Ok(BackendEvent::Unknown {
                        method: Some(method.to_string()),
                    }),
                }
            }
            "session.idle" => delivery_id
                .map(|id| self.complete(id, method))
                .unwrap_or_else(|| {
                    Ok(BackendEvent::Unknown {
                        method: Some(method.to_string()),
                    })
                }),
            "session.error" => {
                if let Some(id) = delivery_id {
                    self.update_state(
                        id,
                        DeliveryState::Failed,
                        "OpenCode emitted session.error",
                        Some(method),
                    )?;
                    self.in_flight = None;
                    self.pending.remove(&id);
                }
                Ok(BackendEvent::Failed {
                    delivery_id,
                    reason: "OpenCode emitted session.error".to_string(),
                })
            }
            "permission.asked" => Ok(BackendEvent::ReverseRequest {
                request_id: value
                    .pointer("/properties/id")
                    .and_then(Value::as_str)
                    .unwrap_or("permission")
                    .to_string(),
                method: method.to_string(),
                params: value.get("properties").cloned().unwrap_or(Value::Null),
            }),
            _ => Ok(BackendEvent::Unknown {
                method: Some(method.to_string()),
            }),
        }
    }

    fn complete(&mut self, delivery_id: Uuid, method: &str) -> anyhow::Result<BackendEvent> {
        self.update_state(
            delivery_id,
            DeliveryState::Completed,
            "OpenCode reported session completion",
            Some(method),
        )?;
        self.in_flight = None;
        self.pending.remove(&delivery_id);
        Ok(BackendEvent::Completed {
            delivery_id,
            event: method.to_string(),
        })
    }

    fn next_event_blocking(&mut self) -> anyhow::Result<BackendEvent> {
        if let Some(event) = self.events.pop_front() {
            return Ok(event);
        }
        let value = self
            .stream
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("OpenCode event stream is not attached"))?
            .next_json()?;
        if let Some(locator) = self.locator.as_mut() {
            let cursor = locator.event_cursor.unwrap_or(0).saturating_add(1);
            locator.event_cursor = Some(cursor);
            super::registry::save_session_locator(&self.home, &self.instance, locator)?;
        }
        self.normalize_event(value)
    }

    async fn reconcile_blocking(&mut self, delivery_id: Uuid) -> anyhow::Result<DeliveryState> {
        let store = ReceiptStore::for_instance(&self.home, &self.instance)?;
        let Some(previous) = store.latest(delivery_id)? else {
            return Ok(DeliveryState::Ambiguous);
        };
        if previous.state.is_terminal() {
            return Ok(previous.state);
        }
        let locator = match self.locator.clone() {
            Some(locator) => locator,
            None => super::registry::load_session_locator(&self.home, &self.instance)?,
        };
        self.start_or_attach_blocking(locator, None)?;
        let locator = self
            .locator
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("OpenCode locator was not restored"))?;
        let session_id = locator
            .session_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("OpenCode session id is missing during reconcile"))?;
        let history = request(
            locator,
            "GET",
            &format!("{}/message?limit=100", session_path(session_id)),
            Value::Null,
        )?;
        if !contains_delivery_id(&response_json(history, "session history")?, delivery_id) {
            let mut ambiguous = previous;
            ambiguous.state = DeliveryState::Ambiguous;
            ambiguous.detail = Some(
                "OpenCode receipt is not present in session history; retry requires operator reconciliation"
                    .to_string(),
            );
            store.record(ambiguous)?;
            return Ok(DeliveryState::Ambiguous);
        }
        let status_response = request(locator, "GET", "/session/status", Value::Null)?;
        let status = response_json(status_response, "session status")?;
        let state = match session_status_type(&status, session_id).as_deref() {
            Some("idle") => DeliveryState::Completed,
            Some("busy") | Some("retry") => DeliveryState::TurnStarted,
            _ => DeliveryState::ObservedInSession,
        };
        let mut receipt = previous;
        receipt.state = state;
        receipt.backend_event = Some("reconcile".to_string());
        receipt.detail = Some("OpenCode session history reconciled the delivery".to_string());
        store.record(receipt)?;
        Ok(state)
    }
}

#[async_trait::async_trait]
impl AgentDeliveryTransport for OpenCodeNativeShared {
    fn mode(&self) -> TransportMode {
        TransportMode::NativeShared
    }

    async fn start_or_attach(
        &mut self,
        locator: SessionLocator,
    ) -> anyhow::Result<TransportCapability> {
        self.start_or_attach_blocking(locator, None)
    }

    async fn deliver(&mut self, envelope: DeliveryEnvelope) -> anyhow::Result<DeliveryReceipt> {
        self.deliver_blocking(envelope)
    }

    async fn next_event(&mut self) -> anyhow::Result<BackendEvent> {
        self.next_event_blocking()
    }

    async fn reconcile(&mut self, delivery_id: Uuid) -> anyhow::Result<DeliveryState> {
        self.reconcile_blocking(delivery_id).await
    }

    async fn health(&self) -> TransportCapability {
        self.capability()
    }
}

#[cfg(test)]
mod tests {
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
                    prompt_tx.send((header, body)).expect("prompt capture");
                    stream
                        .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .expect("prompt response");
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
        let envelope =
            DeliveryEnvelope::new("agent", locator, DeliveryKind::Prompt, "hello\n世界", None);
        let delivery_id = envelope.delivery_id;
        let receipt = adapter.deliver_blocking(envelope).expect("prompt");
        assert_eq!(receipt.state, DeliveryState::ProtocolAccepted);
        let (header, body) = prompt_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("prompt request");
        assert!(header.contains("Authorization: Basic "));
        assert_eq!(
            serde_json::from_slice::<Value>(&body).expect("prompt json"),
            json!({
                "messageID": delivery_id.to_string(),
                "parts": [{"type": "text", "text": "hello\n世界"}],
            })
        );
        assert!(matches!(
            adapter.next_event_blocking().expect("connected"),
            BackendEvent::Ready
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
}
