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
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
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
    overflowed: bool,
}

impl SseDecoder {
    fn new(chunked: bool, initial: Vec<u8>) -> Self {
        Self {
            raw: initial,
            body: Vec::new(),
            chunked,
            overflowed: false,
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        if self.overflowed {
            return Vec::new();
        }
        self.raw.extend_from_slice(bytes);
        if self.raw.len() > MAX_BODY {
            self.raw.clear();
            self.body.clear();
            self.overflowed = true;
            return Vec::new();
        }
        if self.chunked {
            self.dechunk();
        } else {
            self.body.append(&mut self.raw);
            if self.body.len() > MAX_BODY {
                self.body.clear();
                self.overflowed = true;
                return Vec::new();
            }
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
                self.overflowed = true;
                return;
            };
            let data_start = size_end + 2;
            let Some(data_end) = data_start.checked_add(size) else {
                self.raw.clear();
                self.overflowed = true;
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
                self.overflowed = true;
                return;
            }
        }
    }

    fn split_events(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        loop {
            let crlf = self
                .body
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|position| (position, 4));
            let lf = self
                .body
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|position| (position, 2));
            let delimiter = match (crlf, lf) {
                (Some(crlf), Some(lf)) => Some(if crlf.0 < lf.0 { crlf } else { lf }),
                (Some(delimiter), None) | (None, Some(delimiter)) => Some(delimiter),
                (None, None) => None,
            };
            let Some((position, delimiter_len)) = delimiter else {
                break;
            };
            let frame = self.body[..position].to_vec();
            self.body.drain(..position + delimiter_len);
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
    fn next_json(&mut self) -> anyhow::Result<Option<Value>> {
        let mut bytes = [0_u8; 8192];
        loop {
            if let Some(value) = self.pending.pop_front() {
                return Ok(Some(value));
            }
            let events = self.decoder.feed(&[]);
            if self.decoder.overflowed {
                return Err(anyhow::anyhow!("OpenCode SSE frame exceeds the size limit"));
            }
            for event in events {
                match serde_json::from_str::<Value>(&event) {
                    Ok(value) => self.pending.push_back(value),
                    Err(error) => tracing::warn!(
                        error = %error,
                        event_bytes = event.len(),
                        "ignoring unparseable OpenCode SSE event"
                    ),
                }
            }
            if let Some(value) = self.pending.pop_front() {
                return Ok(Some(value));
            }
            let read = match self.stream.read(&mut bytes) {
                Ok(0) => return Err(anyhow::anyhow!("OpenCode SSE stream closed")),
                Ok(read) => read,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None)
                }
                Err(error) => return Err(error.into()),
            };
            // A quiet resident stream must yield the adapter lock regularly so
            // delivery can submit a prompt while the event reader waits.
            // `TcpStream` timeouts are normal polling, not stream failure.
            let events = self.decoder.feed(&bytes[..read]);
            if self.decoder.overflowed {
                return Err(anyhow::anyhow!("OpenCode SSE frame exceeds the size limit"));
            }
            for event in events {
                if let Ok(value) = serde_json::from_str::<Value>(&event) {
                    self.pending.push_back(value);
                }
            }
            if self.pending.is_empty() {
                return Ok(None);
            }
        }
    }
}

#[derive(Debug)]
struct ManagedServer {
    child: Child,
    pid: u32,
    start_token: Option<u64>,
    ready: Arc<AtomicBool>,
}

struct ResidentWorker {
    adapter: Arc<Mutex<OpenCodeNativeShared>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

fn managed_servers() -> &'static Mutex<HashMap<String, ManagedServer>> {
    static SERVERS: OnceLock<Mutex<HashMap<String, ManagedServer>>> = OnceLock::new();
    SERVERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn resident_workers() -> &'static Mutex<HashMap<String, ResidentWorker>> {
    static WORKERS: OnceLock<Mutex<HashMap<String, ResidentWorker>>> = OnceLock::new();
    WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn resident_key(home: &Path, instance: &str) -> String {
    format!("{}\0{}", home.display(), instance)
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
    let workers = {
        let mut all = resident_workers().lock();
        all.remove(&resident_key(home, instance))
            .into_iter()
            .collect::<Vec<_>>()
    };
    for mut worker in workers {
        worker.stop.store(true, Ordering::Release);
        if let Some(join) = worker.join.take() {
            let _ = join.join();
        }
    }
    let prefix = format!("{}\0{}\0", home.display(), instance);
    managed_servers().lock().retain(|key, server| {
        if !key.starts_with(&prefix) {
            return true;
        }
        let _ = server.child.kill();
        let _ = server.child.wait();
        false
    });
    // A daemon restart drops the in-memory Child handle, but the locator keeps
    // the verified PID/start identity. Re-check that identity before signaling
    // so instance deletion cannot orphan a managed server or kill a recycled
    // unrelated PID.
    if let Ok(locator) = super::registry::load_session_locator(home, instance) {
        if locator.managed {
            if let Some(pid) = locator
                .server_pid
                .filter(|_| persisted_server_owned(&locator))
            {
                crate::process::terminate(pid);
                for _ in 0..5 {
                    if crate::process::process_start_token(pid).is_none() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                if persisted_server_owned(&locator) {
                    crate::process::kill_process_tree(pid);
                }
            }
        }
    }
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700))?;
        std::fs::set_permissions(&opencode_dir, std::fs::Permissions::from_mode(0o700))?;
    }
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
    locator: &mut SessionLocator,
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
    let data_dir = crate::agent::opencode_data_dir(home, instance);
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
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
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
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    // fire-and-forget: the managed server is retained in `managed_servers` and
    // is reconciled/reaped on the next adapter attach.
    let mut child = command.spawn()?;
    let pid = child.id();
    let start_token = crate::process::process_start_token(pid);
    let expected_port = endpoint.port;
    let ready = Arc::new(AtomicBool::new(false));
    if let Some(stdout) = child.stdout.take() {
        let ready = Arc::clone(&ready);
        // fire-and-forget: this reader only observes the child readiness line
        // and exits when the managed server closes stdout.
        std::thread::Builder::new()
            .name("opencode-server-ready".to_string())
            .spawn(move || observe_server_ready(stdout, expected_port, ready))?;
    }
    if let Some(stderr) = child.stderr.take() {
        let ready = Arc::clone(&ready);
        // fire-and-forget: this reader only observes the child readiness line
        // and exits when the managed server closes stderr.
        std::thread::Builder::new()
            .name("opencode-server-ready-err".to_string())
            .spawn(move || observe_server_ready(stderr, expected_port, ready))?;
    }
    locator.server_pid = Some(pid);
    locator.server_start_token = start_token;
    super::registry::save_session_locator(home, instance, locator)?;
    managed_servers().lock().insert(
        key,
        ManagedServer {
            child,
            pid,
            start_token,
            ready,
        },
    );
    Ok(())
}

fn observe_server_ready<R: Read>(reader: R, expected_port: u16, ready: Arc<AtomicBool>) {
    use std::io::BufRead;
    for line in std::io::BufReader::new(reader)
        .lines()
        .map_while(Result::ok)
    {
        let observed_port = line
            .split_once("server listening on http://127.0.0.1:")
            .and_then(|(_, suffix)| suffix.split_whitespace().next())
            .and_then(|port| port.parse::<u16>().ok());
        if observed_port == Some(expected_port) {
            ready.store(true, Ordering::Release);
        }
    }
}

fn in_memory_server_owned(home: &Path, instance: &str, locator: &SessionLocator) -> bool {
    let key = server_key(home, instance, locator);
    let mut servers = managed_servers().lock();
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

fn persisted_server_owned(locator: &SessionLocator) -> bool {
    let (Some(pid), Some(start_token)) = (locator.server_pid, locator.server_start_token) else {
        return false;
    };
    crate::process::process_start_token(pid) == Some(start_token)
}

fn server_ready(home: &Path, instance: &str, locator: &SessionLocator) -> bool {
    if in_memory_server_owned(home, instance, locator) {
        return managed_servers()
            .lock()
            .get(&server_key(home, instance, locator))
            .is_some_and(|server| server.ready.load(Ordering::Acquire));
    }
    persisted_server_owned(locator)
}

fn rotate_managed_endpoint(locator: &mut SessionLocator) -> anyhow::Result<()> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    locator.endpoint_url = Some(format!("http://127.0.0.1:{port}"));
    locator.server_pid = None;
    locator.server_start_token = None;
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
    locator: &mut SessionLocator,
    cwd: Option<&Path>,
) -> anyhow::Result<String> {
    wait_for_server_until(home, instance, locator, cwd, SERVER_START_TIMEOUT)
}

fn wait_for_server_until(
    home: &Path,
    instance: &str,
    locator: &mut SessionLocator,
    cwd: Option<&Path>,
    timeout: Duration,
) -> anyhow::Result<String> {
    let deadline = Instant::now() + timeout;
    let mut last_error = None;
    if locator.managed && !persisted_server_owned(locator) {
        if locator.server_pid.is_some() || locator.server_start_token.is_some() {
            rotate_managed_endpoint(locator)?;
        }
        launch_server(home, instance, locator, cwd)?;
    }
    while Instant::now() < deadline {
        if locator.managed && !server_ready(home, instance, locator) {
            if !persisted_server_owned(locator) && !in_memory_server_owned(home, instance, locator)
            {
                rotate_managed_endpoint(locator)?;
                launch_server(home, instance, locator, cwd)?;
            }
            last_error = Some("managed OpenCode child has not proved it owns the endpoint".into());
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        match global_health(locator) {
            Ok((version, true)) => return Ok(version),
            Ok((_, false)) => last_error = Some("OpenCode server reported unhealthy".to_string()),
            Err(error) => last_error = Some(error.to_string()),
        }
        if locator.managed
            && !persisted_server_owned(locator)
            && !in_memory_server_owned(home, instance, locator)
        {
            rotate_managed_endpoint(locator)?;
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
    /// Delivery IDs that have been observed in a message-specific event or
    /// session history. Session-level status is never enough by itself.
    target_confirmed: HashSet<Uuid>,
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
            target_confirmed: HashSet::new(),
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
        if matches!(envelope.kind, DeliveryKind::Steer | DeliveryKind::Interrupt) {
            let mut failed = DeliveryReceipt::for_state(&envelope, DeliveryState::Failed);
            failed.detail = Some("OpenCode has no implicit steer/interrupt operation".to_string());
            store.record(failed)?;
            return Err(anyhow::anyhow!(
                "OpenCode NativeShared requires an explicit prompt operation"
            ));
        }
        if self.in_flight.is_some() {
            let mut failed = DeliveryReceipt::for_state(&envelope, DeliveryState::Failed);
            failed.detail = Some(
                "OpenCode ordinary turn is already in flight; no durable queue accepted this delivery"
                    .to_string(),
            );
            store.record(failed)?;
            return Err(anyhow::anyhow!(
                "OpenCode session already has an ordinary turn in flight"
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
        if self
            .ready
            .then_some(self.locator.as_ref())
            .flatten()
            .is_some_and(|current| Self::same_session(current, &locator))
        {
            return Ok(self.capability());
        }
        self.ready = false;
        let version = wait_for_server(&self.home, &self.instance, &mut locator, cwd)?;
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

    fn same_session(current: &SessionLocator, requested: &SessionLocator) -> bool {
        current.backend == requested.backend
            && current.endpoint_url == requested.endpoint_url
            && current.session_id == requested.session_id
            && current.username == requested.username
            && current.password == requested.password
            && current.model == requested.model
            && current.managed == requested.managed
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
        pending.sort_by(|(left, _), (right, _)| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.delivery_id.cmp(&right.delivery_id))
        });
        let Some((envelope, _receipt)) = pending.into_iter().next() else {
            return Ok(());
        };
        let delivery_id = envelope.delivery_id;
        self.pending.insert(delivery_id, envelope);
        self.in_flight = Some(delivery_id);

        // A previous adapter may have gone away after prompt_async accepted.
        // Session-level idle is not target evidence: history must first prove
        // this exact delivery exists, otherwise recovery is Ambiguous and the
        // ordinary-turn gate is reopened for explicit reconciliation.
        let Some(locator) = self.locator.clone() else {
            return self.mark_ambiguous_and_clear(
                delivery_id,
                "OpenCode restored delivery has no locator",
            );
        };
        let Some(session_id) = locator.session_id.as_deref() else {
            return self.mark_ambiguous_and_clear(
                delivery_id,
                "OpenCode restored delivery has no session identity",
            );
        };
        let history = match request(
            &locator,
            "GET",
            &format!("{}/message?limit=100", session_path(session_id)),
            Value::Null,
        )
        .and_then(|response| response_json(response, "restored session history"))
        {
            Ok(history) => history,
            Err(_) => {
                return self.mark_ambiguous_and_clear(
                    delivery_id,
                    "OpenCode could not prove the restored delivery exists in session history",
                )
            }
        };
        if !contains_delivery_id(&history, delivery_id) {
            return self.mark_ambiguous_and_clear(
                delivery_id,
                "OpenCode restored delivery is absent from session history",
            );
        }
        self.target_confirmed.insert(delivery_id);
        let status = match request(&locator, "GET", "/session/status", Value::Null)
            .and_then(|response| response_json(response, "session status"))
        {
            Ok(status) => status,
            Err(_) => {
                return self.mark_ambiguous_and_clear(
                    delivery_id,
                    "OpenCode could not prove the restored delivery session status",
                )
            }
        };
        match session_status_type(&status, session_id).as_deref() {
            Some("idle") => {
                self.update_state(
                    delivery_id,
                    DeliveryState::Completed,
                    "OpenCode reported the restored delivery completed after target history proof",
                    Some("restore/session.status"),
                )?;
                self.pending.remove(&delivery_id);
                self.target_confirmed.remove(&delivery_id);
                self.in_flight = None;
            }
            Some("busy") | Some("retry") => {
                self.update_state(
                    delivery_id,
                    DeliveryState::TurnStarted,
                    "OpenCode restored a target-confirmed active delivery",
                    Some("restore/session.status"),
                )?;
            }
            _ => {
                self.update_state(
                    delivery_id,
                    DeliveryState::ObservedInSession,
                    "OpenCode restored a target-confirmed delivery with unknown status",
                    Some("restore/session.status"),
                )?;
            }
        }
        Ok(())
    }

    fn mark_ambiguous_and_clear(&mut self, delivery_id: Uuid, detail: &str) -> anyhow::Result<()> {
        self.update_state(
            delivery_id,
            DeliveryState::Ambiguous,
            detail,
            Some("restore/insufficient-target-proof"),
        )?;
        self.pending.remove(&delivery_id);
        self.target_confirmed.remove(&delivery_id);
        if self.in_flight == Some(delivery_id) {
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
        stream.set_read_timeout(Some(Duration::from_millis(250)))?;
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
        let previous = store.latest(delivery_id)?;
        if previous.as_ref().is_some_and(|previous| {
            previous.state.is_terminal()
                || Self::state_rank(state) < Self::state_rank(previous.state)
        }) {
            return Ok(());
        }
        if let Some(envelope) = self.pending.get(&delivery_id) {
            let mut receipt = DeliveryReceipt::for_state(envelope, state);
            if let Some(previous) = previous {
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

    fn state_rank(state: DeliveryState) -> u8 {
        match state {
            DeliveryState::Queued => 0,
            DeliveryState::ProtocolAccepted => 1,
            DeliveryState::ObservedInSession => 2,
            DeliveryState::TurnStarted => 3,
            DeliveryState::Completed | DeliveryState::Failed | DeliveryState::Ambiguous => 4,
        }
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
                    .pointer("/properties/info/messageID")
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                value
                    .pointer("/properties/part/messageID")
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                value
                    .pointer("/properties/part/messageId")
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                value
                    .pointer("/properties/messageID")
                    .and_then(Value::as_str)
            });
        if id == Some(target_text.as_str()) {
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
                    self.target_confirmed.insert(id);
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
                let confirmed_id = delivery_id.or_else(|| {
                    self.in_flight
                        .filter(|id| self.target_confirmed.contains(id))
                });
                match (status, confirmed_id) {
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
                .or_else(|| {
                    self.in_flight
                        .filter(|id| self.target_confirmed.contains(id))
                })
                .map(|id| self.complete(id, method))
                .unwrap_or_else(|| {
                    Ok(BackendEvent::Unknown {
                        method: Some(method.to_string()),
                    })
                }),
            "session.error" => {
                let confirmed_id = delivery_id.or_else(|| {
                    self.in_flight
                        .filter(|id| self.target_confirmed.contains(id))
                });
                if let Some(id) = confirmed_id {
                    self.update_state(
                        id,
                        DeliveryState::Failed,
                        "OpenCode emitted session.error",
                        Some(method),
                    )?;
                    self.in_flight = None;
                    self.pending.remove(&id);
                    self.target_confirmed.remove(&id);
                }
                Ok(BackendEvent::Failed {
                    delivery_id: confirmed_id,
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
        self.target_confirmed.remove(&delivery_id);
        Ok(BackendEvent::Completed {
            delivery_id,
            event: method.to_string(),
        })
    }

    fn poll_event_blocking(&mut self) -> anyhow::Result<Option<BackendEvent>> {
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(event));
        }
        let Some(value) = self
            .stream
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("OpenCode event stream is not attached"))?
            .next_json()?
        else {
            return Ok(None);
        };
        if self
            .stream
            .as_ref()
            .is_some_and(|stream| stream.decoder.overflowed)
        {
            return Err(anyhow::anyhow!("OpenCode SSE frame exceeds the size limit"));
        }
        if let Some(locator) = self.locator.as_mut() {
            let cursor = locator.event_cursor.unwrap_or(0).saturating_add(1);
            locator.event_cursor = Some(cursor);
            super::registry::save_session_locator(&self.home, &self.instance, locator)?;
        }
        self.normalize_event(value).map(Some)
    }

    fn next_event_blocking(&mut self) -> anyhow::Result<BackendEvent> {
        loop {
            if let Some(event) = self.poll_event_blocking()? {
                return Ok(event);
            }
        }
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
            self.mark_ambiguous_and_clear(
                delivery_id,
                "OpenCode receipt is not present in session history; retry requires operator reconciliation",
            )?;
            return Ok(DeliveryState::Ambiguous);
        }
        self.target_confirmed.insert(delivery_id);
        let status_response = request(locator, "GET", "/session/status", Value::Null)?;
        let status = response_json(status_response, "session status")?;
        let state = match session_status_type(&status, session_id).as_deref() {
            Some("idle") => DeliveryState::Completed,
            Some("busy") | Some("retry") => DeliveryState::TurnStarted,
            _ => DeliveryState::ObservedInSession,
        };
        self.update_state(
            delivery_id,
            state,
            "OpenCode session history reconciled the target delivery",
            Some("reconcile"),
        )?;
        if state == DeliveryState::Completed {
            self.pending.remove(&delivery_id);
            self.target_confirmed.remove(&delivery_id);
            self.in_flight = None;
        }
        Ok(state)
    }
}

/// Get the resident OpenCode adapter for an instance. The adapter and its SSE
/// reader outlive an individual delivery, so event receipts are consumed on
/// the production path instead of only by tests that call `next_event`.
fn resident_adapter(
    home: &Path,
    instance: &str,
    locator: SessionLocator,
    cwd: Option<&Path>,
) -> anyhow::Result<Arc<Mutex<OpenCodeNativeShared>>> {
    let key = resident_key(home, instance);
    let mut workers = resident_workers().lock();
    if let Some(worker) = workers.get(&key) {
        return Ok(Arc::clone(&worker.adapter));
    }

    let mut adapter = OpenCodeNativeShared::new(home, instance);
    adapter.start_or_attach_blocking(locator, cwd)?;
    let adapter = Arc::new(Mutex::new(adapter));
    let stop = Arc::new(AtomicBool::new(false));
    let worker_adapter = Arc::clone(&adapter);
    let worker_stop = Arc::clone(&stop);
    // fire-and-forget: ownership transfers to `ResidentWorker`; cleanup sets
    // the stop flag and joins the retained handle before server teardown.
    let join = std::thread::Builder::new()
        .name(format!("opencode-events-{instance}"))
        .spawn(move || resident_event_loop(worker_adapter, worker_stop))?;
    workers.insert(
        key,
        ResidentWorker {
            adapter: Arc::clone(&adapter),
            stop,
            join: Some(join),
        },
    );
    Ok(adapter)
}

fn resident_event_loop(adapter: Arc<Mutex<OpenCodeNativeShared>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        let result = adapter.lock().poll_event_blocking();
        match result {
            Ok(Some(_)) | Ok(None) => {}
            Err(error) => {
                tracing::debug!(error = %error, "OpenCode resident event stream disconnected");
                let reconnected = {
                    let mut adapter = adapter.lock();
                    adapter.ready = false;
                    adapter.stream = None;
                    adapter
                        .locator
                        .clone()
                        .map(|locator| adapter.start_or_attach_blocking(locator, None).is_ok())
                        .unwrap_or(false)
                };
                if !reconnected {
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
    }
}

pub(crate) fn deliver_resident(
    home: &Path,
    instance: &str,
    envelope: DeliveryEnvelope,
) -> anyhow::Result<DeliveryReceipt> {
    let locator = envelope.session.clone();
    let adapter = resident_adapter(home, instance, locator, None)?;
    let result = adapter.lock().deliver_blocking(envelope);
    result
}

pub(crate) fn prepare_resident_tui(
    home: &Path,
    instance: &str,
    locator: SessionLocator,
    cwd: Option<&Path>,
) -> anyhow::Result<SessionLocator> {
    let adapter = resident_adapter(home, instance, locator, cwd)?;
    let result = adapter
        .lock()
        .locator
        .clone()
        .ok_or_else(|| anyhow::anyhow!("OpenCode TUI session was not prepared"));
    result
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

    #[cfg(unix)]
    #[test]
    fn managed_start_does_not_send_credentials_to_a_port_winner() {
        use std::os::unix::fs::PermissionsExt;

        static ENV_LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        let _guard = ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .expect("env lock");
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

    #[cfg(unix)]
    #[test]
    fn managed_teardown_kills_isolated_term_ignoring_server() {
        use std::os::unix::fs::PermissionsExt;

        static ENV_LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        let _guard = ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .expect("env lock");
        let home = std::env::temp_dir().join(format!("agend-opencode-teardown-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&home).expect("home");
        let fake = home.join("fake-opencode.sh");
        std::fs::write(&fake, "#!/bin/sh\ntrap '' TERM\nwhile :; do :; done\n")
            .expect("fake binary");
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700))
            .expect("fake executable");
        let previous_binary = std::env::var_os("AGEND_OPENCODE_BINARY");
        std::env::set_var("AGEND_OPENCODE_BINARY", &fake);
        let mut locator = SessionLocator::opencode(
            "http://127.0.0.1:4096".to_string(),
            None,
            "opencode".to_string(),
            "secret".to_string(),
        );
        locator.managed = true;
        launch_server(&home, "agent", &mut locator, None).expect("launch fake server");
        let pid = locator.server_pid.expect("server pid");
        let key = server_key(&home, "agent", &locator);
        managed_servers()
            .lock()
            .remove(&key)
            .expect("managed server");
        assert!(crate::process::process_start_token(pid).is_some());

        stop_instance_server(&home, "agent");
        assert!(
            crate::process::process_start_token(pid).is_none(),
            "isolated term-ignoring server must be killed during teardown"
        );
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
        let out = decoder.feed(
            b"data: {\"type\":\"session.status\"}\n\ndata: {\"type\":\"session.idle\"}\r\n\r\n",
        );
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
                "properties": {"sessionID": "session-1", "info": {"id": delivery_id.to_string()}}
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
        accepted.protocol_request_id = Some(delivery_id.to_string());
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
        let home =
            std::env::temp_dir().join(format!("agend-opencode-collision-{}", Uuid::new_v4()));
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
        let delivery_id = Uuid::new_v4();
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
                        json!({"type": "message.updated", "properties": {"sessionID": "session-1", "info": {"id": delivery_id.to_string()}}}),
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
        let mut envelope =
            DeliveryEnvelope::new("agent", locator, DeliveryKind::Prompt, "hello\n世界", None);
        envelope.delivery_id = delivery_id;
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
                let prompt_seen = Arc::clone(&server_prompt_seen);
                let event_sent = Arc::clone(&server_event_sent);
                let stop = Arc::clone(&server_stop_flag);
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
                                    json!({"type": "message.updated", "properties": {"sessionID": "session-1", "info": {"id": delivery_id.to_string()}}}),
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
}
