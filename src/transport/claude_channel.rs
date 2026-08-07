//! Claude Code ChannelBridge transport.
//!
//! Claude Channels are an MCP server-side notification capability.  The
//! bridge owns a loopback HTTP listener, forwards authenticated webhook
//! envelopes to Claude over MCP stdio, and records structured `reply` tool
//! receipts in a small durable event log.  It never falls back to PTY.

use super::{
    safe_component, AgentDeliveryTransport, BackendEvent, DeliveryEnvelope, DeliveryReceipt,
    DeliveryState, ReceiptStore, SessionLocator, TransportCapability, TransportMode,
};
use base64::Engine as _;
use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use uuid::Uuid;

pub(crate) const CHANNEL_SERVER_NAME: &str = "agend-claude-channel";
const MIN_CLAUDE_VERSION: (u64, u64, u64) = (2, 1, 80);
const BRIDGE_VERSION: &str = "0.1.0";
const MAX_HEADERS: usize = 64 * 1024;
const MAX_BODY: usize = 1024 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const SSE_READ_TIMEOUT: Duration = Duration::from_secs(20);
const HTTP_PATH: &str = "/webhook";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint {
    port: u16,
}

impl Endpoint {
    fn parse(locator: &SessionLocator) -> anyhow::Result<Self> {
        let raw = locator
            .endpoint_url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Claude ChannelBridge endpoint is missing"))?;
        let rest = raw.strip_prefix("http://").ok_or_else(|| {
            anyhow::anyhow!("Claude ChannelBridge endpoint must use http:// loopback")
        })?;
        let (host, port) = rest
            .rsplit_once(':')
            .ok_or_else(|| anyhow::anyhow!("Claude ChannelBridge endpoint must include a port"))?;
        if host != "127.0.0.1" || port.contains('/') || port.contains('?') || port.contains('#') {
            anyhow::bail!("Claude ChannelBridge endpoint is not loopback");
        }
        let port = port.parse::<u16>()?;
        if port == 0 {
            anyhow::bail!("Claude ChannelBridge endpoint port must be non-zero");
        }
        Ok(Self { port })
    }
}

fn endpoint_address(locator: &SessionLocator) -> anyhow::Result<String> {
    Ok(format!("127.0.0.1:{}", Endpoint::parse(locator)?.port))
}

fn token(locator: &SessionLocator) -> anyhow::Result<&str> {
    locator
        .password
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Claude ChannelBridge bearer token is missing"))
}

fn channel_state_dir(home: &Path, instance: &str) -> PathBuf {
    home.join("transport")
        .join("claude-channel")
        .join(safe_component(instance))
}

fn channel_events_path(home: &Path, instance: &str) -> PathBuf {
    channel_state_dir(home, instance).join("events.jsonl")
}

fn channel_lock_path(home: &Path, instance: &str) -> PathBuf {
    channel_events_path(home, instance).with_extension("jsonl.lock")
}

fn restrict_private(path: &Path, mode: u32) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(mode);
        std::fs::set_permissions(path, permissions)?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
enum ChannelLogRecord {
    Inbound {
        delivery_id: Uuid,
        chat_id: String,
        sender_id: Option<String>,
        content: String,
        recorded_at: String,
    },
    Reply {
        delivery_id: Uuid,
        chat_id: String,
        text: String,
        reply_id: String,
        recorded_at: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ReplyEvent {
    delivery_id: Uuid,
    chat_id: String,
    text: String,
    reply_id: String,
    recorded_at: String,
}

fn append_log(home: &Path, instance: &str, record: &ChannelLogRecord) -> anyhow::Result<()> {
    let dir = channel_state_dir(home, instance);
    std::fs::create_dir_all(&dir)?;
    restrict_private(&dir, 0o700)?;
    let _lock = crate::store::acquire_file_lock(&channel_lock_path(home, instance))?;
    let path = channel_events_path(home, instance);
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    restrict_private(&path, 0o600)?;
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    file.write_all(&line)?;
    file.sync_data()?;
    Ok(())
}

fn load_log(home: &Path, instance: &str) -> anyhow::Result<Vec<ChannelLogRecord>> {
    let path = channel_events_path(home, instance);
    if !path.exists() {
        return Ok(Vec::new());
    }
    restrict_private(&path, 0o600)?;
    let file = File::open(path)?;
    BufReader::new(file)
        .lines()
        .map(|line| Ok(serde_json::from_str(&line?)?))
        .collect()
}

struct ChannelRuntime {
    home: PathBuf,
    instance: String,
    session_id: String,
    token: String,
    ready: AtomicBool,
    client_version: Mutex<Option<String>>,
    inbound: Mutex<HashMap<String, Uuid>>,
    replies: Mutex<HashMap<Uuid, ReplyEvent>>,
    subscribers: Mutex<Vec<Sender<ReplyEvent>>>,
    mcp_sender: Mutex<Option<SyncSender<Value>>>,
}

impl ChannelRuntime {
    fn new(home: &Path, instance: &str, locator: &SessionLocator) -> anyhow::Result<Self> {
        let mut inbound = HashMap::new();
        let mut replies = HashMap::new();
        for record in load_log(home, instance)? {
            match record {
                ChannelLogRecord::Inbound {
                    delivery_id,
                    chat_id,
                    ..
                } => {
                    inbound.insert(chat_id, delivery_id);
                }
                ChannelLogRecord::Reply {
                    delivery_id,
                    chat_id,
                    text,
                    reply_id,
                    recorded_at,
                } => {
                    replies.insert(
                        delivery_id,
                        ReplyEvent {
                            delivery_id,
                            chat_id,
                            text,
                            reply_id,
                            recorded_at,
                        },
                    );
                }
            }
        }
        Ok(Self {
            home: home.to_path_buf(),
            instance: instance.to_string(),
            session_id: locator
                .session_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Claude ChannelBridge session ID is missing"))?,
            token: token(locator)?.to_string(),
            ready: AtomicBool::new(false),
            client_version: Mutex::new(None),
            inbound: Mutex::new(inbound),
            replies: Mutex::new(replies),
            subscribers: Mutex::new(Vec::new()),
            mcp_sender: Mutex::new(None),
        })
    }

    fn set_sender(&self, sender: SyncSender<Value>) {
        *self.mcp_sender.lock() = Some(sender);
    }

    fn clear_sender(&self) {
        *self.mcp_sender.lock() = None;
    }

    fn set_client_version(&self, version: String) {
        *self.client_version.lock() = Some(version);
        self.ready.store(true, Ordering::Release);
    }

    fn mark_unready(&self) {
        self.ready.store(false, Ordering::Release);
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    fn client_version(&self) -> Option<String> {
        self.client_version.lock().clone()
    }

    fn health_json(&self) -> Value {
        json!({
            "ready": self.is_ready(),
            "backend": "claude",
            "mode": "channel_bridge",
            "backend_version": self.client_version(),
            "session_id": self.session_id,
            "capabilities": {
                "claude/channel": self.is_ready(),
                "tools": self.is_ready(),
            }
        })
    }

    fn remember_inbound(
        &self,
        delivery_id: Uuid,
        chat_id: &str,
        sender_id: Option<&str>,
        content: &str,
    ) -> anyhow::Result<()> {
        append_log(
            &self.home,
            &self.instance,
            &ChannelLogRecord::Inbound {
                delivery_id,
                chat_id: chat_id.to_string(),
                sender_id: sender_id.map(str::to_string),
                content: content.to_string(),
                recorded_at: Utc::now().to_rfc3339(),
            },
        )?;
        self.inbound.lock().insert(chat_id.to_string(), delivery_id);
        Ok(())
    }

    fn delivery_for_chat(&self, chat_id: &str) -> Option<Uuid> {
        self.inbound.lock().get(chat_id).copied()
    }

    fn remember_reply(
        &self,
        delivery_id: Uuid,
        chat_id: &str,
        text: &str,
    ) -> anyhow::Result<ReplyEvent> {
        let event = ReplyEvent {
            delivery_id,
            chat_id: chat_id.to_string(),
            text: text.to_string(),
            reply_id: Uuid::new_v4().to_string(),
            recorded_at: Utc::now().to_rfc3339(),
        };
        append_log(
            &self.home,
            &self.instance,
            &ChannelLogRecord::Reply {
                delivery_id: event.delivery_id,
                chat_id: event.chat_id.clone(),
                text: event.text.clone(),
                reply_id: event.reply_id.clone(),
                recorded_at: event.recorded_at.clone(),
            },
        )?;
        self.replies.lock().insert(delivery_id, event.clone());
        self.subscribers
            .lock()
            .retain(|subscriber| subscriber.send(event.clone()).is_ok());
        Ok(event)
    }

    fn reply_for(&self, delivery_id: Uuid) -> Option<ReplyEvent> {
        self.replies.lock().get(&delivery_id).cloned()
    }

    fn subscribe(&self, after_reply_id: Option<&str>) -> (Receiver<ReplyEvent>, Vec<ReplyEvent>) {
        let (sender, receiver) = mpsc::channel();
        let replies = self.replies.lock();
        let mut replay: Vec<_> = replies.values().cloned().collect();
        replay.sort_by(|left, right| {
            left.recorded_at
                .cmp(&right.recorded_at)
                .then_with(|| left.reply_id.cmp(&right.reply_id))
        });
        if let Some(after_reply_id) = after_reply_id {
            if let Some(position) = replay
                .iter()
                .position(|event| event.reply_id == after_reply_id)
            {
                replay.drain(..=position);
            }
        }
        // Keep the reply and subscriber locks in the same order as
        // remember_reply so a reply cannot land between replay and subscribe.
        self.subscribers.lock().push(sender);
        (receiver, replay)
    }

    fn send_channel_notification(
        &self,
        delivery_id: Uuid,
        chat_id: &str,
        sender_id: Option<&str>,
        content: &str,
    ) -> bool {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/claude/channel",
            "params": {
                "content": content,
                "meta": {
                    "delivery_id": delivery_id.to_string(),
                    "chat_id": chat_id,
                    "sender_id": sender_id.unwrap_or("agend-terminal")
                }
            }
        });
        let sender = self.mcp_sender.lock().clone();
        match sender {
            Some(sender) => match sender.try_send(notification) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
            },
            None => false,
        }
    }
}

fn parse_version(value: &str) -> Option<(u64, u64, u64)> {
    let mut numbers = value
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .take(3)
        .map(|part| part.parse::<u64>().ok());
    Some((
        numbers.next()??,
        numbers.next()??,
        numbers.next().unwrap_or(Some(0))?,
    ))
}

fn supported_claude_version(value: &str) -> bool {
    parse_version(value).is_some_and(|version| version >= MIN_CLAUDE_VERSION)
}

fn json_rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

fn mcp_initialize(message: &Value, runtime: &ChannelRuntime) -> Value {
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let version = message
        .pointer("/params/clientInfo/version")
        .and_then(Value::as_str);
    let Some(version) = version else {
        runtime.mark_unready();
        return json_rpc_error(id, -32001, "Claude Code client version is required");
    };
    if !supported_claude_version(version) {
        runtime.mark_unready();
        return json_rpc_error(
            id,
            -32001,
            "Claude Code Channels require client version 2.1.80 or newer",
        );
    }
    runtime.set_client_version(version.to_string());
    json!({
        "jsonrpc":"2.0",
        "id": id,
        "result": {
            "protocolVersion": message.pointer("/params/protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18"),
            "capabilities": {
                "experimental": {"claude/channel": {}},
                "tools": {}
            },
            "serverInfo": {"name": CHANNEL_SERVER_NAME, "version": BRIDGE_VERSION},
            "instructions": "Messages arrive as <channel source=\"agend-terminal\" chat_id=\"...\" delivery_id=\"...\">. Each delivery has a unique chat_id; reply with the reply tool using the same chat_id and delivery_id."
        }
    })
}

fn reply_tool_definition() -> Value {
    json!({
        "name": "reply",
        "description": "Send a structured reply back over the AgEnD Claude channel",
        "inputSchema": {
            "type": "object",
            "properties": {
                "chat_id": {"type":"string", "description":"The conversation to reply in"},
                "delivery_id": {"type":"string", "description":"The inbound delivery UUID"},
                "text": {"type":"string", "description":"The reply text"}
            },
            "required": ["chat_id", "text"]
        }
    })
}

fn mcp_message(message: Value, runtime: &ChannelRuntime) -> Option<Value> {
    let method = message.get("method").and_then(Value::as_str)?;
    match method {
        "initialize" => Some(mcp_initialize(&message, runtime)),
        "notifications/initialized" => None,
        "ping" => Some(json!({
            "jsonrpc":"2.0",
            "id": message.get("id").cloned().unwrap_or(Value::Null),
            "result": {}
        })),
        "tools/list" => Some(json!({
            "jsonrpc":"2.0",
            "id": message.get("id").cloned().unwrap_or(Value::Null),
            "result": {"tools": [reply_tool_definition()]}
        })),
        "tools/call" => {
            let id = message.get("id").cloned().unwrap_or(Value::Null);
            let name = message.pointer("/params/name").and_then(Value::as_str);
            if name != Some("reply") {
                return Some(json_rpc_error(
                    id,
                    -32601,
                    "unknown Claude ChannelBridge tool",
                ));
            }
            let arguments = message
                .pointer("/params/arguments")
                .and_then(Value::as_object);
            let Some(arguments) = arguments else {
                return Some(json_rpc_error(
                    id,
                    -32602,
                    "reply arguments must be an object",
                ));
            };
            let Some(chat_id) = arguments.get("chat_id").and_then(Value::as_str) else {
                return Some(json_rpc_error(id, -32602, "reply.chat_id is required"));
            };
            let Some(text) = arguments.get("text").and_then(Value::as_str) else {
                return Some(json_rpc_error(id, -32602, "reply.text is required"));
            };
            let mapped = runtime.delivery_for_chat(chat_id);
            let delivery_id = match arguments.get("delivery_id").and_then(Value::as_str) {
                Some(value) => match Uuid::parse_str(value) {
                    Ok(value) if mapped == Some(value) => value,
                    Ok(_) => {
                        return Some(json_rpc_error(
                            id,
                            -32602,
                            "reply.delivery_id does not match chat_id",
                        ));
                    }
                    Err(_) => {
                        return Some(json_rpc_error(id, -32602, "reply.delivery_id is invalid"))
                    }
                },
                None => match mapped {
                    Some(value) => value,
                    None => return Some(json_rpc_error(id, -32602, "reply.chat_id is unknown")),
                },
            };
            match runtime.remember_reply(delivery_id, chat_id, text) {
                Ok(event) => Some(json!({
                    "jsonrpc":"2.0",
                    "id": id,
                    "result": {
                        "content": [{"type":"text","text":"sent"}],
                        "structuredContent": {"delivery_id": event.delivery_id, "reply_id": event.reply_id}
                    }
                })),
                Err(error) => Some(json_rpc_error(
                    id,
                    -32002,
                    &format!("reply receipt failed: {error}"),
                )),
            }
        }
        _ if message.get("id").is_some() => Some(json_rpc_error(
            message.get("id").cloned().unwrap_or(Value::Null),
            -32601,
            "unknown Claude ChannelBridge method",
        )),
        _ => None,
    }
}

fn read_mcp_message<R: Read>(reader: &mut BufReader<R>) -> anyhow::Result<Option<Value>> {
    let mut first = String::new();
    loop {
        if reader.read_line(&mut first)? == 0 {
            return Ok(None);
        }
        if !first.trim().is_empty() {
            break;
        }
        first.clear();
    }
    if first.len() > MAX_BODY {
        anyhow::bail!("MCP message exceeds the size limit");
    }
    if !first.trim_start().starts_with('{') {
        anyhow::bail!("MCP stdio requires one JSON object per line");
    }
    Ok(Some(serde_json::from_str(first.trim())?))
}

fn write_mcp_message<W: Write>(writer: &mut W, message: &Value) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *writer, message)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_http_request(stream: &mut TcpStream) -> anyhow::Result<HttpRequest> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            anyhow::bail!("HTTP request closed before headers");
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > MAX_HEADERS {
            anyhow::bail!("HTTP headers exceed the size limit");
        }
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
    };
    let header_text = std::str::from_utf8(&bytes[..header_end])?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("HTTP request line is missing"))?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("HTTP method is missing"))?
        .to_string();
    let path = request_parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("HTTP path is missing"))?
        .to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length = headers
        .get("content-length")
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(0);
    if length > MAX_BODY {
        anyhow::bail!("HTTP body exceeds the size limit");
    }
    let body_start = header_end + 4;
    let mut body = bytes[body_start..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            anyhow::bail!("HTTP body ended early");
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> anyhow::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

fn bearer_ok(request: &HttpRequest, expected: &str) -> bool {
    request
        .headers
        .get("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|value| value == expected)
}

fn json_response(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap_or_else(|_| b"{\"error\":\"serialization failure\"}".to_vec())
}

fn handle_http(mut stream: TcpStream, runtime: Arc<ChannelRuntime>) -> anyhow::Result<()> {
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
    let request = read_http_request(&mut stream)?;
    if !bearer_ok(&request, &runtime.token) {
        write_http_response(
            &mut stream,
            401,
            "application/json",
            br#"{"error":"unauthorized"}"#,
        )?;
        return Ok(());
    }
    if request.method == "GET" && request.path == "/health" {
        write_http_response(
            &mut stream,
            200,
            "application/json",
            &json_response(runtime.health_json()),
        )?;
        return Ok(());
    }
    if request.method == "GET" && request.path == "/events" {
        let cursor = request.headers.get("last-event-id").map(String::as_str);
        let (receiver, replay) = runtime.subscribe(cursor);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n: connected\n\n"
        )?;
        for event in replay {
            write_sse_event(&mut stream, &event)?;
        }
        stream.flush()?;
        loop {
            match receiver.recv_timeout(Duration::from_secs(15)) {
                Ok(event) => {
                    write_sse_event(&mut stream, &event)?;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    stream.write_all(b": keepalive\n\n")?;
                    stream.flush()?;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        return Ok(());
    }
    if request.method == "GET" && request.path.starts_with("/receipts/") {
        let id = request
            .path
            .trim_start_matches("/receipts/")
            .parse::<Uuid>();
        let Ok(id) = id else {
            write_http_response(
                &mut stream,
                400,
                "application/json",
                br#"{"error":"invalid delivery_id"}"#,
            )?;
            return Ok(());
        };
        match runtime.reply_for(id) {
            Some(event) => write_http_response(
                &mut stream,
                200,
                "application/json",
                &json_response(json!({"reply":event})),
            )?,
            None => {
                write_http_response(&mut stream, 404, "application/json", br#"{"reply":null}"#)?
            }
        }
        return Ok(());
    }
    if request.method == "POST" && (request.path == HTTP_PATH || request.path == "/") {
        if !runtime.is_ready() {
            write_http_response(
                &mut stream,
                503,
                "application/json",
                br#"{"error":"channel_not_ready"}"#,
            )?;
            return Ok(());
        }
        let payload: Value = match serde_json::from_slice(&request.body) {
            Ok(payload) => payload,
            Err(_) => {
                write_http_response(
                    &mut stream,
                    400,
                    "application/json",
                    br#"{"error":"webhook body must be JSON"}"#,
                )?;
                return Ok(());
            }
        };
        let Some(delivery_id) = payload
            .get("delivery_id")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<Uuid>().ok())
        else {
            write_http_response(
                &mut stream,
                400,
                "application/json",
                br#"{"error":"webhook delivery_id is required and must be a UUID"}"#,
            )?;
            return Ok(());
        };
        let Some(chat_id) = payload
            .get("chat_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            write_http_response(
                &mut stream,
                400,
                "application/json",
                br#"{"error":"webhook chat_id is required"}"#,
            )?;
            return Ok(());
        };
        let Some(content) = payload
            .get("text")
            .or_else(|| payload.get("content"))
            .and_then(Value::as_str)
        else {
            write_http_response(
                &mut stream,
                400,
                "application/json",
                br#"{"error":"webhook text is required"}"#,
            )?;
            return Ok(());
        };
        let sender_id = payload.get("sender_id").and_then(Value::as_str);
        runtime.remember_inbound(delivery_id, chat_id, sender_id, content)?;
        if !runtime.send_channel_notification(delivery_id, chat_id, sender_id, content) {
            write_http_response(
                &mut stream,
                503,
                "application/json",
                br#"{"error":"channel_queue_unavailable"}"#,
            )?;
            return Ok(());
        }
        write_http_response(
            &mut stream,
            202,
            "application/json",
            &json_response(json!({"accepted":true,"delivery_id":delivery_id,"chat_id":chat_id})),
        )?;
        return Ok(());
    }
    write_http_response(
        &mut stream,
        404,
        "application/json",
        br#"{"error":"not_found"}"#,
    )?;
    Ok(())
}

fn write_sse_event(stream: &mut TcpStream, event: &ReplyEvent) -> anyhow::Result<()> {
    write!(
        stream,
        "id: {}\nevent: reply\ndata: {}\n\n",
        event.reply_id,
        serde_json::to_string(event)?
    )?;
    stream.flush()?;
    Ok(())
}

fn run_http_listener(listener: TcpListener, runtime: Arc<ChannelRuntime>, stop: Arc<AtomicBool>) {
    let _ = listener.set_nonblocking(true);
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let runtime = Arc::clone(&runtime);
                // fire-and-forget: each HTTP/SSE connection owns its blocking socket until the client closes it; the listener remains available for webhooks.
                let _ = thread::Builder::new()
                    .name("claude-channel-http-client".to_string())
                    .spawn(move || {
                        let _ = handle_http(stream, runtime);
                    });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
}

pub(crate) fn run_channel_server(home: &Path, instance: &str) -> anyhow::Result<()> {
    let locator = prepare_claude_channel(home, instance)?;
    let address = endpoint_address(&locator)?;
    let listener = TcpListener::bind(&address)?;
    let runtime = Arc::new(ChannelRuntime::new(home, instance, &locator)?);
    let stop = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::sync_channel::<Value>(64);
    runtime.set_sender(sender.clone());
    let http_stop = Arc::clone(&stop);
    let http_runtime = Arc::clone(&runtime);
    // fire-and-forget: retained JoinHandle joins the HTTP listener during bridge shutdown.
    let http_join = thread::Builder::new()
        .name("claude-channel-http".to_string())
        .spawn(move || run_http_listener(listener, http_runtime, http_stop))?;

    // fire-and-forget: retained JoinHandle joins the MCP writer during bridge shutdown.
    let writer_join = thread::Builder::new()
        .name("claude-channel-mcp-writer".to_string())
        .spawn(move || {
            let stdout = io::stdout();
            let mut writer = io::BufWriter::new(stdout.lock());
            while let Ok(message) = receiver.recv() {
                if write_mcp_message(&mut writer, &message).is_err() {
                    break;
                }
            }
        })?;

    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    while let Some(message) = read_mcp_message(&mut reader)? {
        if let Some(response) = mcp_message(message, &runtime) {
            if sender.send(response).is_err() {
                break;
            }
        }
    }
    stop.store(true, Ordering::Release);
    runtime.clear_sender();
    drop(sender);
    let _ = http_join.join();
    let _ = writer_join.join();
    Ok(())
}

fn random_token() -> anyhow::Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

pub(crate) fn legacy_pty_opt_in(home: &Path, instance: &str) -> bool {
    crate::fleet::FleetConfig::load(&crate::fleet::fleet_yaml_path(home))
        .ok()
        .and_then(|fleet| fleet.resolve_instance(instance))
        .and_then(|resolved| resolved.env.get("AGEND_TRANSPORT_MODE").cloned())
        .is_some_and(|mode| mode.eq_ignore_ascii_case("legacy_pty"))
}

pub(crate) fn prepare_claude_channel(
    home: &Path,
    instance: &str,
) -> anyhow::Result<SessionLocator> {
    if let Some(locator) = super::registry::claude_attach_locator(home, instance)? {
        return Ok(locator);
    }
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let locator = SessionLocator::claude(
        format!("http://127.0.0.1:{port}"),
        format!("claude-{}", Uuid::new_v4()),
        random_token()?,
    );
    super::registry::save_session_locator(home, instance, &locator)?;
    Ok(locator)
}

pub(crate) fn channel_server_entry(home: &Path, instance: &str) -> anyhow::Result<Value> {
    let executable = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "agend-terminal".to_string());
    Ok(json!({
        "command": executable,
        "args": ["channel-bridge", "--instance", instance],
        "env": {
            "AGEND_HOME": home.display().to_string(),
            "AGEND_INSTANCE_NAME": instance
        }
    }))
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn client_request(
    locator: &SessionLocator,
    method: &str,
    path: &str,
    body: &[u8],
    accept: &str,
) -> anyhow::Result<HttpResponse> {
    let address = endpoint_address(locator)?;
    let mut stream = TcpStream::connect_timeout(&address.parse()?, HTTP_TIMEOUT)?;
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
    let token = token(locator)?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nAccept: {accept}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            anyhow::bail!("Claude ChannelBridge closed before HTTP headers");
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > MAX_HEADERS {
            anyhow::bail!("Claude ChannelBridge headers exceed the size limit");
        }
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
    };
    let header_text = std::str::from_utf8(&bytes[..header_end])?;
    let mut lines = header_text.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| anyhow::anyhow!("Claude ChannelBridge status is invalid"))?
        .parse::<u16>()?;
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length = headers
        .get("content-length")
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(0);
    if length > MAX_BODY {
        anyhow::bail!("Claude ChannelBridge response exceeds the size limit");
    }
    let mut response_body = bytes[header_end + 4..].to_vec();
    while response_body.len() < length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            anyhow::bail!("Claude ChannelBridge response ended early");
        }
        response_body.extend_from_slice(&chunk[..read]);
    }
    response_body.truncate(length);
    Ok(HttpResponse {
        status,
        body: response_body,
    })
}

fn health_probe(locator: &SessionLocator) -> anyhow::Result<TransportCapability> {
    let response = client_request(locator, "GET", "/health", &[], "application/json")?;
    if response.status != 200 {
        anyhow::bail!(
            "Claude ChannelBridge health returned HTTP {}",
            response.status
        );
    }
    let value: Value = serde_json::from_slice(&response.body)?;
    let version = value
        .get("backend_version")
        .and_then(Value::as_str)
        .map(str::to_string);
    let ready = value.get("ready").and_then(Value::as_bool) == Some(true)
        && value
            .pointer("/capabilities/claude~1channel")
            .and_then(Value::as_bool)
            == Some(true)
        && value
            .pointer("/capabilities/tools")
            .and_then(Value::as_bool)
            == Some(true)
        && version.as_deref().is_some_and(supported_claude_version);
    Ok(TransportCapability {
        backend: "claude".to_string(),
        mode: TransportMode::ChannelBridge,
        ready,
        backend_version: version,
        degraded_reason: if ready {
            None
        } else {
            Some("Claude ChannelBridge capability/version probe failed".to_string())
        },
    })
}

fn response_detail(response: &HttpResponse) -> String {
    serde_json::from_slice::<Value>(&response.body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("HTTP {}", response.status))
}

fn complete_receipt(home: &Path, instance: &str, event: &ReplyEvent) -> anyhow::Result<()> {
    let store = ReceiptStore::for_instance(home, instance)?;
    let Some(previous) = store.latest(event.delivery_id)? else {
        return Ok(());
    };
    if previous.state.is_terminal() {
        return Ok(());
    }
    let envelope = DeliveryEnvelope::new(
        instance,
        SessionLocator::claude(
            "http://127.0.0.1:1".to_string(),
            "reconcile".to_string(),
            "reconcile".to_string(),
        ),
        super::DeliveryKind::Notification,
        "reconcile",
        Some(event.chat_id.clone()),
    );
    let mut receipt = DeliveryReceipt::for_state(&envelope, DeliveryState::Completed);
    receipt.delivery_id = event.delivery_id;
    receipt.payload_digest = previous.payload_digest;
    receipt.protocol_request_id = previous.protocol_request_id;
    receipt.backend_event = Some("claude_reply".to_string());
    store.record(receipt)
}

struct SseStream {
    reader: BufReader<TcpStream>,
}

impl SseStream {
    fn connect(locator: &SessionLocator, last_event_id: Option<&str>) -> anyhow::Result<Self> {
        let address = endpoint_address(locator)?;
        let mut stream = TcpStream::connect_timeout(&address.parse()?, HTTP_TIMEOUT)?;
        stream.set_read_timeout(Some(SSE_READ_TIMEOUT))?;
        stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
        let token = token(locator)?;
        let cursor = last_event_id
            .map(|value| format!("Last-Event-ID: {value}\r\n"))
            .unwrap_or_default();
        write!(
            stream,
            "GET /events HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nAccept: text/event-stream\r\nConnection: keep-alive\r\n{cursor}\r\n"
        )?;
        stream.flush()?;
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status)?;
        if !status.contains(" 200 ") {
            anyhow::bail!("Claude ChannelBridge SSE returned {status:?}");
        }
        loop {
            let mut header = String::new();
            reader.read_line(&mut header)?;
            if header == "\r\n" || header == "\n" {
                break;
            }
        }
        Ok(Self { reader })
    }

    fn next(&mut self) -> anyhow::Result<Option<ReplyEvent>> {
        let mut data = String::new();
        loop {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                return Ok(None);
            }
            if line == "\n" || line == "\r\n" {
                if data.is_empty() {
                    continue;
                }
                return Ok(Some(serde_json::from_str(data.trim())?));
            }
            if let Some(value) = line.strip_prefix("data:") {
                data.push_str(value.trim_start());
            }
        }
    }
}

struct EventWorker {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

fn event_workers() -> &'static Mutex<HashMap<String, EventWorker>> {
    static WORKERS: OnceLock<Mutex<HashMap<String, EventWorker>>> = OnceLock::new();
    WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn worker_key(home: &Path, instance: &str) -> String {
    format!("{}\0{}", home.display(), instance)
}

fn ensure_event_worker(home: &Path, instance: &str, locator: &SessionLocator) {
    let key = worker_key(home, instance);
    let mut workers = event_workers().lock();
    if workers.contains_key(&key) {
        return;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let home = home.to_path_buf();
    let instance = instance.to_string();
    let locator = locator.clone();
    let mut last_event_id = None::<String>;
    let mut seen_reply_ids = HashSet::new();
    // fire-and-forget: this resident listener owns reconnect/reconciliation for the daemon lifetime; cleanup stops and joins it.
    let join = thread::Builder::new()
        .name("claude-channel-events".to_string())
        .spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match SseStream::connect(&locator, last_event_id.as_deref()) {
                    Ok(mut stream) => loop {
                        if thread_stop.load(Ordering::Acquire) {
                            return;
                        }
                        match stream.next() {
                            Ok(Some(event)) => {
                                if seen_reply_ids.contains(&event.reply_id) {
                                    // This event was reconciled successfully on an
                                    // earlier connection; it is safe to advance
                                    // past the duplicate replay.
                                    last_event_id = Some(event.reply_id);
                                } else if complete_receipt(&home, &instance, &event).is_ok() {
                                    // Advance the cursor only after receipt
                                    // persistence succeeds. A failed write must
                                    // be replayed after reconnect.
                                    seen_reply_ids.insert(event.reply_id.clone());
                                    last_event_id = Some(event.reply_id);
                                } else {
                                    break;
                                }
                            }
                            Ok(None) | Err(_) => break,
                        }
                    },
                    Err(_) => thread::sleep(Duration::from_millis(250)),
                }
            }
        })
        .expect("Claude ChannelBridge event worker thread must start");
    workers.insert(
        key,
        EventWorker {
            stop,
            join: Some(join),
        },
    );
}

pub(crate) fn stop_instance_state(home: &Path, instance: &str) {
    let key = worker_key(home, instance);
    if let Some(mut worker) = event_workers().lock().remove(&key) {
        worker.stop.store(true, Ordering::Release);
        if let Some(join) = worker.join.take() {
            let _ = join.join();
        }
    }
    let _ = std::fs::remove_dir_all(channel_state_dir(home, instance));
    if matches!(
        super::registry::claude_attach_locator(home, instance),
        Ok(Some(_))
    ) {
        let _ = super::registry::remove_session_locator(home, instance);
    }
}

pub(crate) fn deliver_resident(
    home: &Path,
    instance: &str,
    envelope: DeliveryEnvelope,
) -> anyhow::Result<DeliveryReceipt> {
    let locator = prepare_claude_channel(home, instance)?;
    ensure_event_worker(home, instance, &locator);
    let store = ReceiptStore::for_instance(home, instance)?;
    store.record_queued(&envelope)?;
    let capability = health_probe(&locator).map_err(|error| {
        let mut receipt = DeliveryReceipt::for_state(&envelope, DeliveryState::Failed);
        receipt.detail = Some(format!(
            "Claude ChannelBridge readiness failed closed: {error}"
        ));
        let _ = store.record(receipt);
        error
    })?;
    if !capability.ready {
        let mut receipt = DeliveryReceipt::for_state(&envelope, DeliveryState::Failed);
        receipt.detail = capability.degraded_reason;
        store.record(receipt)?;
        anyhow::bail!("Claude ChannelBridge capability probe failed closed");
    }
    let chat_id = chat_id_for_delivery(&envelope.instance, envelope.delivery_id);
    let payload = json!({
        "delivery_id": envelope.delivery_id,
        "chat_id": chat_id,
        "sender_id": "agend-terminal",
        "text": envelope.body,
    });
    let response = client_request(
        &locator,
        "POST",
        HTTP_PATH,
        &serde_json::to_vec(&payload)?,
        "application/json",
    )?;
    if response.status != 202 {
        let mut receipt = DeliveryReceipt::for_state(&envelope, DeliveryState::Failed);
        receipt.detail = Some(format!(
            "Claude ChannelBridge webhook rejected: {}",
            response_detail(&response)
        ));
        store.record(receipt)?;
        anyhow::bail!("Claude ChannelBridge webhook rejected: {}", response.status);
    }
    let mut receipt = DeliveryReceipt::for_state(&envelope, DeliveryState::ProtocolAccepted);
    receipt.protocol_request_id = Some(envelope.delivery_id.to_string());
    receipt.backend_event = Some("webhook_accepted".to_string());
    if let Some(previous) = store.latest(envelope.delivery_id)? {
        if previous.state.is_terminal() {
            // Claude may call `reply` while the webhook handler is still
            // writing its 202 response. Preserve that terminal receipt rather
            // than appending a late ProtocolAccepted regression.
            return Ok(previous);
        }
    }
    store.record(receipt.clone())?;
    if let Ok(response) = client_request(
        &locator,
        "GET",
        &format!("/receipts/{}", envelope.delivery_id),
        &[],
        "application/json",
    ) {
        if response.status == 200 {
            if let Ok(value) = serde_json::from_slice::<Value>(&response.body) {
                if let Ok(event) = serde_json::from_value::<ReplyEvent>(value["reply"].clone()) {
                    let _ = complete_receipt(home, instance, &event);
                }
            }
        }
    }
    Ok(receipt)
}

fn chat_id_for_delivery(instance: &str, delivery_id: Uuid) -> String {
    format!("agend:{instance}:{delivery_id}")
}

#[async_trait::async_trait]
impl AgentDeliveryTransport for ClaudeChannelBridge {
    fn mode(&self) -> TransportMode {
        TransportMode::ChannelBridge
    }

    async fn start_or_attach(
        &mut self,
        locator: SessionLocator,
    ) -> anyhow::Result<TransportCapability> {
        self.locator = Some(locator);
        self.health_blocking()
    }

    async fn deliver(&mut self, envelope: DeliveryEnvelope) -> anyhow::Result<DeliveryReceipt> {
        deliver_resident(&self.home, &self.instance, envelope)
    }

    async fn next_event(&mut self) -> anyhow::Result<BackendEvent> {
        let locator = self.locator()?;
        let mut stream = SseStream::connect(&locator, None)?;
        match stream.next()? {
            Some(event) => Ok(BackendEvent::Completed {
                delivery_id: event.delivery_id,
                event: "claude_reply".to_string(),
            }),
            None => Err(anyhow::anyhow!("Claude ChannelBridge SSE closed")),
        }
    }

    async fn reconcile(&mut self, delivery_id: Uuid) -> anyhow::Result<DeliveryState> {
        let locator = self.locator()?;
        let store = ReceiptStore::for_instance(&self.home, &self.instance)?;
        let Some(previous) = store.latest(delivery_id)? else {
            return Ok(DeliveryState::Ambiguous);
        };
        if previous.state.is_terminal() {
            return Ok(previous.state);
        }
        let response = client_request(
            &locator,
            "GET",
            &format!("/receipts/{delivery_id}"),
            &[],
            "application/json",
        )?;
        if response.status == 200 {
            let value: Value = serde_json::from_slice(&response.body)?;
            if let Ok(event) = serde_json::from_value::<ReplyEvent>(value["reply"].clone()) {
                complete_receipt(&self.home, &self.instance, &event)?;
                return Ok(DeliveryState::Completed);
            }
        }
        Ok(previous.state)
    }

    async fn health(&self) -> TransportCapability {
        self.health_blocking()
            .unwrap_or_else(|error| TransportCapability {
                backend: "claude".to_string(),
                mode: TransportMode::ChannelBridge,
                ready: false,
                backend_version: None,
                degraded_reason: Some(error.to_string()),
            })
    }
}

pub(crate) struct ClaudeChannelBridge {
    home: PathBuf,
    instance: String,
    locator: Option<SessionLocator>,
}

impl ClaudeChannelBridge {
    fn locator(&self) -> anyhow::Result<SessionLocator> {
        self.locator
            .clone()
            .map(Ok)
            .unwrap_or_else(|| prepare_claude_channel(&self.home, &self.instance))
    }

    fn health_blocking(&self) -> anyhow::Result<TransportCapability> {
        health_probe(&self.locator()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::transport::{mode_for_backend, mode_for_instance};
    use std::fs;

    fn home(tag: &str) -> std::path::PathBuf {
        let home =
            std::env::temp_dir().join(format!("agend-claude-channel-{}-{}", tag, Uuid::new_v4()));
        fs::create_dir_all(&home).expect("home");
        home
    }

    #[test]
    fn claude_uses_channel_bridge_by_default() {
        assert_eq!(
            mode_for_backend(&Backend::ClaudeCode),
            TransportMode::ChannelBridge
        );
    }

    #[test]
    fn explicit_legacy_pty_is_the_only_claude_fallback() {
        let home = home("legacy");
        fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  claude-agent:\n    backend: claude\n    env:\n      AGEND_TRANSPORT_MODE: legacy_pty\n",
        )
        .expect("fleet");
        assert_eq!(
            mode_for_instance(&home, "claude-agent"),
            TransportMode::LegacyPty
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn channel_locator_persists_across_daemon_restart() {
        let home = home("locator");
        let first = prepare_claude_channel(&home, "claude-agent").expect("initial locator");
        let second = prepare_claude_channel(&home, "claude-agent").expect("restart locator");
        assert_eq!(
            first, second,
            "restart must reuse endpoint, token, and session"
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn existing_non_claude_locator_is_not_replaced() {
        let home = home("foreign-locator");
        let locator = SessionLocator::opencode(
            "http://127.0.0.1:43123".to_string(),
            Some("opencode-session".to_string()),
            "agend".to_string(),
            "secret".to_string(),
        );
        super::super::registry::save_session_locator(&home, "claude-agent", &locator)
            .expect("foreign locator");
        assert!(prepare_claude_channel(&home, "claude-agent").is_err());
        let stored = super::super::registry::load_session_locator(&home, "claude-agent")
            .expect("stored locator");
        assert_eq!(stored.backend, "opencode");
        stop_instance_state(&home, "claude-agent");
        assert!(
            super::super::registry::load_session_locator(&home, "claude-agent").is_ok(),
            "Claude cleanup must not remove another backend's locator"
        );
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn persisted_claude_locator_keeps_channel_mode() {
        let home = home("mode");
        let locator = prepare_claude_channel(&home, "claude-agent").expect("locator");
        fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  claude-agent:\n    backend: claude\n",
        )
        .expect("fleet");
        assert_eq!(
            mode_for_instance(&home, "claude-agent"),
            TransportMode::ChannelBridge
        );
        assert_eq!(locator.backend, "claude");
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn registry_delivery_uses_channel_bridge_without_pty_fallback() {
        let home = home("registry-delivery");
        let listener = TcpListener::bind("127.0.0.1:0").expect("fake channel listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let port = listener.local_addr().expect("listener address").port();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let server = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut request = [0_u8; 4096];
                        let read = stream.read(&mut request).unwrap_or(0);
                        let request = String::from_utf8_lossy(&request[..read]);
                        let path = request.split_whitespace().nth(1).unwrap_or("/");
                        let (status, body) = if path == "/health" {
                            (
                                "200 OK",
                                json!({
                                    "ready": true,
                                    "backend_version": "2.1.89",
                                    "capabilities": {"claude/channel": true, "tools": true}
                                })
                                .to_string(),
                            )
                        } else if path == "/webhook" {
                            ("202 Accepted", json!({"accepted": true}).to_string())
                        } else {
                            ("404 Not Found", json!({"reply": null}).to_string())
                        };
                        let response = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        let locator = SessionLocator::claude(
            format!("http://127.0.0.1:{port}"),
            "claude-registry-session".to_string(),
            "registry-token".to_string(),
        );
        super::super::registry::save_session_locator(&home, "claude-agent", &locator)
            .expect("locator");
        fs::write(
            crate::fleet::fleet_yaml_path(&home),
            "instances:\n  claude-agent:\n    backend: claude\n",
        )
        .expect("fleet");
        let legacy_called = Arc::new(AtomicBool::new(false));
        let legacy_called_by_closure = Arc::clone(&legacy_called);
        let receipt = super::super::registry::deliver_notification(
            &home,
            "claude-agent",
            "registry delivery",
            move |_, _, _| {
                legacy_called_by_closure.store(true, Ordering::Release);
                Ok(())
            },
        )
        .expect("ChannelBridge delivery");
        assert_eq!(receipt.state, DeliveryState::ProtocolAccepted);
        assert!(!legacy_called.load(Ordering::Acquire));
        let stored = ReceiptStore::for_instance(&home, "claude-agent")
            .expect("receipt store")
            .latest(receipt.delivery_id)
            .expect("receipt lookup")
            .expect("stored receipt");
        assert_eq!(stored.state, DeliveryState::ProtocolAccepted);
        stop_instance_state(&home, "claude-agent");
        stop.store(true, Ordering::Release);
        let _ = server.join();
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn channel_server_entry_declares_authenticated_local_bridge() {
        let home = home("entry");
        let entry = channel_server_entry(&home, "claude-agent").expect("server entry");
        assert_eq!(entry["env"]["AGEND_INSTANCE_NAME"], "claude-agent");
        assert_eq!(entry["args"][0], "channel-bridge");
        assert_eq!(entry["env"]["AGEND_HOME"], home.display().to_string());
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn channel_wire_notification_uses_delivery_and_chat_metadata() {
        let delivery_id = Uuid::new_v4();
        let value = json!({
            "jsonrpc":"2.0",
            "method":"notifications/claude/channel",
            "params": {
                "content":"hello",
                "meta": {"delivery_id":delivery_id.to_string(),"chat_id":"chat-1","sender_id":"agend-terminal"}
            }
        });
        assert_eq!(value["method"], "notifications/claude/channel");
        assert_eq!(
            value["params"]["meta"]["delivery_id"],
            delivery_id.to_string()
        );
        assert_eq!(value["params"]["meta"]["chat_id"], "chat-1");
    }

    #[test]
    fn channel_initialize_rejects_old_or_missing_client_version() {
        let home = home("version");
        let locator = prepare_claude_channel(&home, "claude-agent").expect("locator");
        let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime");
        let missing = mcp_initialize(&json!({"jsonrpc":"2.0","id":1,"params":{}}), &runtime);
        assert_eq!(missing["error"]["code"], -32001);
        let old = mcp_initialize(
            &json!({"jsonrpc":"2.0","id":2,"params":{"clientInfo":{"version":"2.1.79"}}}),
            &runtime,
        );
        assert_eq!(old["error"]["code"], -32001);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn reply_requires_delivery_and_chat_correlation() {
        let home = home("correlation");
        let locator = prepare_claude_channel(&home, "claude-agent").expect("locator");
        let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime");
        let response = mcp_message(
            json!({
                "jsonrpc":"2.0",
                "id":1,
                "method":"tools/call",
                "params": {
                    "name":"reply",
                    "arguments": {
                        "chat_id":"unknown-chat",
                        "delivery_id":Uuid::new_v4(),
                        "text":"must be rejected"
                    }
                }
            }),
            &runtime,
        )
        .expect("tool response");
        assert_eq!(response["error"]["code"], -32602);
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn concurrent_deliveries_keep_distinct_chat_correlations() {
        let home = home("two-in-flight");
        let locator = prepare_claude_channel(&home, "claude-agent").expect("locator");
        let runtime = ChannelRuntime::new(&home, "claude-agent", &locator).expect("runtime");
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let first_chat = chat_id_for_delivery("claude-agent", first);
        let second_chat = chat_id_for_delivery("claude-agent", second);
        assert_ne!(first_chat, second_chat);
        runtime
            .remember_inbound(first, &first_chat, None, "first")
            .expect("first inbound");
        runtime
            .remember_inbound(second, &second_chat, None, "second")
            .expect("second inbound");
        assert_eq!(runtime.delivery_for_chat(&first_chat), Some(first));
        assert_eq!(runtime.delivery_for_chat(&second_chat), Some(second));
        let _ = fs::remove_dir_all(home);
    }
}
