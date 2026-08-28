//! Shared NDJSON client and response contract for the MCP bridge and tool CLI.
//!
//! The two user-facing surfaces deliberately share the daemon envelope and
//! transport behavior.  Keeping this module dependency-light also lets the
//! standalone `agend-mcp-bridge` binary include it without linking the main
//! binary's module graph.

#![allow(dead_code)]

use serde_json::{json, Value};
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CONNECT_ATTEMPTS: usize = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const LIST_RETRY_INTERVAL: Duration = Duration::from_millis(100);
const LIST_RETRY_TIMEOUT: Duration = Duration::from_secs(30);

/// Ordered interpretation of a daemon response.  The order is intentional:
/// a result carrying `error` is a tool error even when the outer envelope says
/// `ok:true`, while an accepted background operation has no result field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseClass {
    Ok,
    Accepted,
    ToolError,
    Refused,
    Indeterminate,
    ProtocolError,
}

pub fn classify_response(response: &Value) -> ResponseClass {
    let Some(ok) = response.get("ok").and_then(Value::as_bool) else {
        return ResponseClass::ProtocolError;
    };

    if !ok {
        if response
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(is_indeterminate_status)
            || response.get("err_class").and_then(Value::as_str) == Some("protocol")
        {
            ResponseClass::Indeterminate
        } else {
            ResponseClass::Refused
        }
    } else if response
        .get("result")
        .and_then(|result| result.get("error"))
        .is_some()
    {
        ResponseClass::ToolError
    } else if response.get("status").and_then(Value::as_str) == Some("accepted_in_progress") {
        ResponseClass::Accepted
    } else if response.get("result").is_some() {
        ResponseClass::Ok
    } else {
        ResponseClass::ProtocolError
    }
}

fn is_indeterminate_status(status: &str) -> bool {
    matches!(status, "in_progress" | "oversized" | "handler_errored")
}

/// A transport/protocol failure, distinguished by the point at which it was
/// observed so callers can avoid retrying a request after it was written.
#[derive(Debug)]
pub struct WireError {
    kind: WireErrorKind,
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireErrorKind {
    NoDaemon,
    Refused,
    Connect,
    Write,
    Read,
    Protocol,
}

impl WireError {
    fn new(kind: WireErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> WireErrorKind {
        self.kind
    }
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for WireError {}

/// Persistent daemon API client.  The bridge reuses one instance for its
/// stdio session; the CLI creates one per invocation.
#[derive(Default)]
pub struct Client {
    conn: Option<(BufReader<TcpStream>, TcpStream)>,
}

impl Client {
    /// Send one request.  Connection establishment is retried a bounded
    /// number of times; once the envelope is written, any read failure is
    /// returned as indeterminate and is never transparently replayed here.
    pub fn request(&mut self, home: &Path, envelope: &Value) -> Result<Value, WireError> {
        if self.conn.is_none() {
            self.conn = Some(connect_with_retry(home)?);
        }

        let request = with_request_id(envelope);
        let Some((reader, writer)) = self.conn.as_mut() else {
            return Err(WireError::new(
                WireErrorKind::NoDaemon,
                "daemon connection was not established",
            ));
        };

        if let Err(error) = writeln!(writer, "{request}").and_then(|_| writer.flush()) {
            self.conn = None;
            return Err(WireError::new(
                WireErrorKind::Write,
                format!("daemon request write failed: {error}"),
            ));
        }

        let mut line = String::new();
        if let Err(error) = reader.read_line(&mut line) {
            self.conn = None;
            return Err(WireError::new(
                WireErrorKind::Read,
                format!("daemon response read failed: {error}"),
            ));
        }
        if line.trim().is_empty() {
            self.conn = None;
            return Err(WireError::new(
                WireErrorKind::Read,
                "daemon closed connection before a response",
            ));
        }
        serde_json::from_str(line.trim()).map_err(|error| {
            WireError::new(
                WireErrorKind::Protocol,
                format!("daemon response was not valid JSON: {error}"),
            )
        })
    }

    /// Send a bridge request with one transparent reconnect after a
    /// transport failure. The request id is minted before the first attempt
    /// and therefore stays stable across the retry; the daemon can deduplicate
    /// a request whose response was lost in transit.
    pub fn request_with_retry(
        &mut self,
        home: &Path,
        envelope: &Value,
    ) -> Result<Value, WireError> {
        let envelope = with_request_id(envelope);
        match self.request(home, &envelope) {
            Ok(response) => Ok(response),
            Err(error)
                if matches!(
                    error.kind(),
                    WireErrorKind::Connect | WireErrorKind::Write | WireErrorKind::Read
                ) =>
            {
                self.conn = None;
                self.request(home, &envelope)
            }
            Err(error) => Err(error),
        }
    }

    /// Fetch the role-filtered live tool catalogue.  Application-level
    /// refusals return immediately; only connection failures are retried.
    pub fn tools_list(&mut self, home: &Path, instance: &str) -> Result<Value, WireError> {
        let timeout_ms = std::env::var("AGEND_BRIDGE_TOOLS_LIST_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(LIST_RETRY_TIMEOUT);
        let deadline = Instant::now() + timeout_ms;
        let envelope = tools_list_envelope(instance);
        loop {
            match self.request(home, &envelope) {
                Ok(response) => {
                    if response.get("ok").and_then(Value::as_bool) == Some(true)
                        && response.get("result").is_none()
                    {
                        return Err(WireError::new(
                            WireErrorKind::Protocol,
                            "daemon tools list response omitted result",
                        ));
                    }
                    return Ok(response);
                }
                Err(error) => {
                    let retryable = matches!(
                        error.kind(),
                        WireErrorKind::Connect | WireErrorKind::Write | WireErrorKind::Read
                    );
                    if !retryable || Instant::now() >= deadline {
                        return Err(error);
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    std::thread::sleep(LIST_RETRY_INTERVAL.min(remaining));
                }
            }
        }
    }
}

/// Build the canonical CLI/MCP tool envelope.  `request_id` is validated by
/// the CLI before this function is called; bridge callers pass `None` and get
/// a fresh UUID at the request boundary.
pub fn tool_envelope(
    instance: &str,
    tool: &str,
    arguments: Value,
    request_id: Option<&str>,
) -> Value {
    let mut request = json!({
        "method": "mcp_tool",
        "params": {
            "tool": tool,
            "arguments": arguments,
            "instance": instance,
            "transport": "cli"
        }
    });
    if let Some(request_id) = request_id {
        request["request_id"] = Value::String(request_id.to_string());
    }
    request
}

/// Build the pre-existing MCP bridge envelope.  Unlike the experimental CLI
/// surface it deliberately has no `transport` discriminator.
pub fn mcp_tool_envelope(instance: &str, tool: &str, arguments: &Value) -> Value {
    json!({
        "method": "mcp_tool",
        "params": {"tool": tool, "arguments": arguments, "instance": instance}
    })
}

pub fn tools_list_envelope(instance: &str) -> Value {
    json!({
        "method": "mcp_tools_list",
        "params": {"instance": instance}
    })
}

pub fn with_request_id(envelope: &Value) -> Value {
    if envelope.get("request_id").and_then(Value::as_str).is_some() {
        return envelope.clone();
    }
    let mut request = envelope.clone();
    if let Some(object) = request.as_object_mut() {
        object.insert(
            "request_id".to_string(),
            Value::String(uuid::Uuid::new_v4().to_string()),
        );
    }
    request
}

fn connect_with_retry(home: &Path) -> Result<(BufReader<TcpStream>, TcpStream), WireError> {
    let mut last_error = None;
    for attempt in 0..CONNECT_ATTEMPTS {
        match connect_once(home) {
            Ok(connection) => return Ok(connection),
            Err(error) if error.kind() == WireErrorKind::Connect => {
                last_error = Some(error);
                if attempt + 1 < CONNECT_ATTEMPTS {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        WireError::new(
            WireErrorKind::Connect,
            "daemon connection attempts exhausted without an error",
        )
    }))
}

fn connect_once(home: &Path) -> Result<(BufReader<TcpStream>, TcpStream), WireError> {
    let run_dir = find_run_dir(home)?;
    let port = read_port_file(&run_dir)?;
    let cookie = read_cookie_file(&run_dir)?;
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).map_err(|error| {
        WireError::new(
            WireErrorKind::Connect,
            format!("connect to daemon API failed: {error}"),
        )
    })?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let writer = stream.try_clone().map_err(|error| {
        WireError::new(
            WireErrorKind::Connect,
            format!("clone daemon API stream failed: {error}"),
        )
    })?;
    let mut reader = BufReader::new(stream);
    let mut handshake_writer = writer.try_clone().map_err(|error| {
        WireError::new(
            WireErrorKind::Connect,
            format!("clone daemon handshake stream failed: {error}"),
        )
    })?;
    writeln!(
        handshake_writer,
        r#"{{"auth":"{}","pid":{}}}"#,
        hex(&cookie),
        std::process::id()
    )
    .and_then(|_| handshake_writer.flush())
    .map_err(|error| {
        WireError::new(
            WireErrorKind::Connect,
            format!("daemon auth write failed: {error}"),
        )
    })?;
    let mut auth_response = String::new();
    reader.read_line(&mut auth_response).map_err(|error| {
        WireError::new(
            WireErrorKind::Connect,
            format!("daemon auth response read failed: {error}"),
        )
    })?;
    let authenticated = serde_json::from_str::<Value>(auth_response.trim())
        .ok()
        .and_then(|value| value.get("ok").and_then(Value::as_bool))
        .unwrap_or(false);
    if !authenticated {
        return Err(WireError::new(
            WireErrorKind::Refused,
            format!("daemon authentication rejected: {}", auth_response.trim()),
        ));
    }
    Ok((reader, writer))
}

pub(crate) fn find_run_dir(home: &Path) -> Result<PathBuf, WireError> {
    let run_base = home.join("run");
    let entries = std::fs::read_dir(&run_base).map_err(|error| {
        WireError::new(
            WireErrorKind::NoDaemon,
            format!("no active daemon run dir: {error}"),
        )
    })?;
    for entry in entries.flatten() {
        if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if !path.join("api.port").exists() {
            continue;
        }
        let alive = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
            .map(pid_is_alive)
            .unwrap_or(true);
        if alive {
            return Ok(path);
        }
    }
    Err(WireError::new(
        WireErrorKind::NoDaemon,
        "no active daemon run dir",
    ))
}

fn read_port_file(run_dir: &Path) -> Result<u16, WireError> {
    std::fs::read_to_string(run_dir.join("api.port"))
        .map_err(|error| WireError::new(WireErrorKind::Refused, format!("read api.port: {error}")))?
        .trim()
        .parse()
        .map_err(|error| WireError::new(WireErrorKind::Refused, format!("parse api.port: {error}")))
}

fn read_cookie_file(run_dir: &Path) -> Result<[u8; 32], WireError> {
    let mut file = std::fs::File::open(run_dir.join("api.cookie")).map_err(|error| {
        WireError::new(WireErrorKind::Refused, format!("read api.cookie: {error}"))
    })?;
    let mut cookie = [0u8; 32];
    file.read_exact(&mut cookie).map_err(|error| {
        WireError::new(WireErrorKind::Refused, format!("read api.cookie: {error}"))
    })?;
    let mut trailing = [0u8; 1];
    if file.read(&mut trailing).unwrap_or(0) != 0 {
        return Err(WireError::new(
            WireErrorKind::Refused,
            "api.cookie has unexpected trailing bytes",
        ));
    }
    Ok(cookie)
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn pid_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        pid != 0 && unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_classification_is_ordered() {
        assert_eq!(
            classify_response(&json!({"ok": true, "result": {"error": "bad"}})),
            ResponseClass::ToolError
        );
        assert_eq!(
            classify_response(&json!({"ok": true, "status": "accepted_in_progress"})),
            ResponseClass::Accepted
        );
        assert_eq!(
            classify_response(&json!({"ok": false, "status": "in_progress"})),
            ResponseClass::Indeterminate
        );
        assert_eq!(
            classify_response(&json!({"ok": false, "error": "disabled"})),
            ResponseClass::Refused
        );
    }

    #[test]
    fn accepted_in_progress_with_result_error_is_tool_error() {
        assert_eq!(
            classify_response(&json!({
                "ok": true,
                "status": "accepted_in_progress",
                "result": {"error": "handler failed"}
            })),
            ResponseClass::ToolError
        );
    }

    #[test]
    fn request_id_is_injected_once() {
        let request = with_request_id(&json!({"method": "mcp_tool"}));
        assert!(request["request_id"].as_str().is_some());
        assert_eq!(with_request_id(&request), request);
    }

    #[test]
    fn mcp_tool_envelope_omits_cli_transport_discriminator() {
        let request = mcp_tool_envelope("agent-a", "send", &json!({"message": "hello"}));
        assert_eq!(request["method"], "mcp_tool");
        assert_eq!(request["params"]["instance"], "agent-a");
        assert_eq!(request["params"]["tool"], "send");
        assert!(
            request["params"].get("transport").is_none(),
            "legacy MCP bridge requests must not claim transport=cli: {request}"
        );
    }
}
