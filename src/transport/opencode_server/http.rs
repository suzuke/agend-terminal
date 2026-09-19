//! Raw HTTP/1.1 plumbing for the OpenCode NativeShared transport.
//!
//! Split out of `opencode_server.rs` (verbatim move, no behavior change) to
//! hold that file under the repo-wide 2500-LOC anti-monolith ceiling enforced
//! by `tests/src_file_size_invariant.rs`.

use crate::transport::SessionLocator;
use base64::Engine as _;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const MAX_HEADERS: usize = 64 * 1024;
pub(crate) const MAX_BODY: usize = 16 * 1024 * 1024;
pub(crate) const MAX_ERROR_DETAIL: usize = 2048;

#[derive(Debug, Clone)]
pub(crate) struct Endpoint {
    pub(crate) host: String,
    pub(crate) port: u16,
}

impl Endpoint {
    pub(crate) fn parse(locator: &SessionLocator) -> anyhow::Result<Self> {
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

    pub(crate) fn address(&self) -> anyhow::Result<SocketAddr> {
        (self.host.as_str(), self.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow::anyhow!("OpenCode loopback endpoint did not resolve"))
    }
}

#[derive(Debug)]
pub(crate) struct HttpResponse {
    pub(crate) status: u16,
    pub(crate) body: Vec<u8>,
}

pub(crate) fn basic_auth(locator: &SessionLocator) -> Option<String> {
    let username = locator.username.as_deref()?;
    let password = locator.password.as_deref()?;
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    Some(format!("Basic {encoded}"))
}

pub(crate) fn connect(endpoint: &Endpoint) -> anyhow::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&endpoint.address()?, IO_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

pub(crate) fn write_request(
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

pub(crate) fn read_headers(
    stream: &mut TcpStream,
) -> anyhow::Result<(u16, HashMap<String, String>, Vec<u8>)> {
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

pub(crate) fn read_exact_more(
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

pub(crate) fn read_chunked_body(
    stream: &mut TcpStream,
    mut raw: Vec<u8>,
) -> anyhow::Result<Vec<u8>> {
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

pub(crate) fn read_body(
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

pub(crate) fn request(
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

pub(crate) fn response_json(response: HttpResponse, operation: &str) -> anyhow::Result<Value> {
    if !(200..300).contains(&response.status) {
        return Err(anyhow::anyhow!(response_error_detail(&response, operation)));
    }
    if response.body.is_empty() {
        return Ok(Value::Null);
    }
    Ok(serde_json::from_slice(&response.body)?)
}

pub(crate) fn response_error_detail(response: &HttpResponse, operation: &str) -> String {
    let body = response_body_detail(&response.body);
    if body.is_empty() {
        format!("OpenCode {operation} returned HTTP {}", response.status)
    } else {
        format!(
            "OpenCode {operation} returned HTTP {}: {body}",
            response.status
        )
    }
}

pub(crate) fn response_body_detail(body: &[u8]) -> String {
    if body.is_empty() {
        return String::new();
    }
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return String::new();
    };
    let message = value
        .pointer("/data/message")
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str));
    message.map(truncate_error_detail).unwrap_or_default()
}

pub(crate) fn truncate_error_detail(value: &str) -> String {
    let value = value.trim();
    if value.len() <= MAX_ERROR_DETAIL {
        return value.to_string();
    }
    let mut end = MAX_ERROR_DETAIL;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

pub(crate) fn not_found(response: &HttpResponse) -> bool {
    response.status == 404
}
