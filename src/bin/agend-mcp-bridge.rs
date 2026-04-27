//! agend-mcp-bridge — zero-state stdio↔TCP relay for MCP tool calls.
//!
//! Sprint 25 P0 Option F: this binary is spawned by agent backends as
//! their MCP server. It holds NO state — every `tools/call` request is
//! forwarded to the daemon API socket which dispatches via `handle_tool`
//! in daemon process context (where ACTIVE_CHANNEL, heartbeat_pair,
//! etc. are registered).
//!
//! MCP protocol methods (initialize, ping, tools/list, notifications)
//! are handled locally — they don't need daemon state.
//!
//! Protocol:
//! - stdin/stdout: Content-Length framed or NDJSON JSON-RPC (MCP spec)
//! - daemon: NDJSON over TCP loopback with cookie auth

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};

fn main() {
    if let Err(e) = run() {
        eprintln!("agend-mcp-bridge: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let home = home_dir();
    let instance = std::env::var("AGEND_INSTANCE_NAME").unwrap_or_default();
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = io::stdout();

    // Lazy persistent TCP connection to daemon API.
    let mut conn: Option<(BufReader<TcpStream>, TcpStream)> = None;

    loop {
        let body = match read_message(&mut reader)? {
            Some(b) => b,
            None => break,
        };

        let req: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(e) => {
                let id = extract_id(&body);
                let resp = format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32700,"message":"Parse error: {e}"}}}}"#
                );
                write_message(&mut stdout, &resp)?;
                continue;
            }
        };

        let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let method = req["method"].as_str().unwrap_or("");

        let response = match method {
            "initialize" => serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "agend-terminal", "version": env!("CARGO_PKG_VERSION") }
                }
            })
            .to_string(),

            "ping" => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}}).to_string(),

            "notifications/initialized" | "notifications/cancelled" => continue,

            "tools/list" => match proxy_tools_list(&home, &mut conn) {
                Ok(r) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": r}).to_string(),
                Err(_) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"tools": []}})
                    .to_string(),
            },

            "tools/call" => {
                let tool = req["params"]["name"].as_str().unwrap_or("");
                let args = &req["params"]["arguments"];

                match proxy_tool_call(&home, &instance, tool, args, &mut conn) {
                    Ok(result) => serde_json::json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {
                            "content": [{"type": "text", "text": serde_json::to_string_pretty(&result).unwrap_or_default()}],
                            "isError": result.get("error").is_some()
                        }
                    }).to_string(),
                    Err(e) => {
                        conn = None;
                        serde_json::json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": {"code": -32603, "message": format!("daemon proxy error: {e}")}
                        }).to_string()
                    }
                }
            }

            m if m.starts_with("notifications/") => continue,

            _ => serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": format!("Method not found: {method}")}
            })
            .to_string(),
        };

        write_message(&mut stdout, &response)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon proxy
// ---------------------------------------------------------------------------

fn proxy_tool_call(
    home: &Path,
    instance: &str,
    tool: &str,
    args: &serde_json::Value,
    conn: &mut Option<(BufReader<TcpStream>, TcpStream)>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    ensure_connection(home, conn)?;
    let (ref mut r, ref mut w) = conn
        .as_mut()
        .expect("connection established by ensure_connection");

    let envelope = serde_json::json!({
        "method": "mcp_tool",
        "params": {"tool": tool, "arguments": args, "instance": instance}
    });
    writeln!(w, "{envelope}")?;
    w.flush()?;

    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        *conn = None;
        return Err("daemon closed connection".into());
    }

    let resp: serde_json::Value = serde_json::from_str(line.trim())?;
    if resp["ok"].as_bool() == Some(true) {
        Ok(resp["result"].clone())
    } else {
        let msg = resp["error"].as_str().unwrap_or("daemon error");
        Err(msg.into())
    }
}

fn proxy_tools_list(
    home: &Path,
    conn: &mut Option<(BufReader<TcpStream>, TcpStream)>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    ensure_connection(home, conn)?;
    let (ref mut r, ref mut w) = conn
        .as_mut()
        .expect("connection established by ensure_connection");

    let envelope = serde_json::json!({"method": "mcp_tools_list", "params": {}});
    writeln!(w, "{envelope}")?;
    w.flush()?;

    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        *conn = None;
        return Err("daemon closed connection".into());
    }
    let resp: serde_json::Value = serde_json::from_str(line.trim())?;
    if resp["ok"].as_bool() == Some(true) {
        Ok(resp["result"].clone())
    } else {
        Err("daemon error on tools_list".into())
    }
}

fn ensure_connection(
    home: &Path,
    conn: &mut Option<(BufReader<TcpStream>, TcpStream)>,
) -> Result<(), Box<dyn std::error::Error>> {
    if conn.is_some() {
        return Ok(());
    }
    *conn = Some(connect_daemon(home)?);
    Ok(())
}

fn connect_daemon(
    home: &Path,
) -> Result<(BufReader<TcpStream>, TcpStream), Box<dyn std::error::Error>> {
    let run_dir = find_run_dir(home)?;
    let port = read_port_file(&run_dir)?;
    let cookie = read_cookie_file(&run_dir)?;

    let stream = TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));

    let writer = stream.try_clone()?;
    let mut rdr = BufReader::new(stream);

    // Cookie handshake — include PID for daemon-side liveness tracking
    // (Sprint 25 P1 F1 PID-watch invalidation)
    let hex: String = cookie.iter().map(|b| format!("{b:02x}")).collect();
    let pid = std::process::id();
    let mut w = writer.try_clone()?;
    writeln!(w, r#"{{"auth":"{hex}","pid":{pid}}}"#)?;
    w.flush()?;

    let mut resp = String::new();
    rdr.read_line(&mut resp)?;
    if !resp.contains(r#""ok":true"#) && !resp.contains(r#""ok": true"#) {
        return Err(format!("auth rejected: {}", resp.trim()).into());
    }

    Ok((rdr, writer))
}

// ---------------------------------------------------------------------------
// MCP framing (Content-Length + NDJSON auto-detect)
// ---------------------------------------------------------------------------

fn read_message(reader: &mut impl BufRead) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('{') {
            return Ok(Some(trimmed.to_string()));
        }
        if let Some(val) = trimmed.strip_prefix("Content-Length:") {
            let len: usize = val.trim().parse()?;
            let mut sep = String::new();
            reader.read_line(&mut sep)?;
            if len == 0 {
                continue;
            }
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body)?;
            return Ok(Some(String::from_utf8(body)?));
        }
    }
}

fn write_message(stdout: &mut io::Stdout, json: &str) -> io::Result<()> {
    write!(stdout, "Content-Length: {}\r\n\r\n{}", json.len(), json)?;
    stdout.flush()
}

// ---------------------------------------------------------------------------
// Minimal filesystem helpers (NO crate:: dependencies — zero state)
// ---------------------------------------------------------------------------

fn home_dir() -> PathBuf {
    if let Ok(h) = std::env::var("AGEND_HOME") {
        return PathBuf::from(h);
    }
    let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let new_path = base.join(".agend");
    let legacy = base.join(".agend-terminal");
    if new_path.exists() || !legacy.exists() {
        new_path
    } else {
        legacy
    }
}

fn find_run_dir(home: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let run_base = home.join("run");
    for entry in std::fs::read_dir(&run_base)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let p = entry.path();
            if p.join("api.port").exists() {
                return Ok(p);
            }
        }
    }
    Err("no active daemon run dir".into())
}

fn read_port_file(run_dir: &Path) -> Result<u16, Box<dyn std::error::Error>> {
    Ok(std::fs::read_to_string(run_dir.join("api.port"))?
        .trim()
        .parse()?)
}

fn read_cookie_file(run_dir: &Path) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    let mut f = std::fs::File::open(run_dir.join("api.cookie"))?;
    let mut buf = [0u8; 32];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

fn extract_id(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("id").cloned())
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".to_string())
}
